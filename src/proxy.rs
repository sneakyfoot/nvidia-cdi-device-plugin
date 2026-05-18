// HTTP/2 byte-level proxy that rewrites the `:authority` pseudo-header on
// requests from kubelet's Go-grpc-go client to a sane value before forwarding
// to a downstream tonic server.
//
// Why: kubelet's grpc-go client over a Unix domain socket sends the UDS path
// (e.g. "/var/lib/kubelet/plugins_registry/gpu.nvidia.com-reg.sock") as the
// HTTP/2 `:authority` pseudo-header. The `h2` crate that backs tonic validates
// `:authority` as an RFC 9113 URI authority component and rejects the path
// with RST_STREAM(PROTOCOL_ERROR) before the request ever reaches the gRPC
// service. Upstream is open at hyperium/hyper#3750 and hyperium/tonic#243.
//
// Approach:
//   - Listen on the kubelet-facing UDS socket.
//   - For each accepted connection, dial a downstream `127.0.0.1:port` TCP
//     socket where tonic is listening, then bidirectionally forward bytes.
//   - On the upstream direction (kubelet -> tonic), parse HTTP/2 frames and
//     for each HEADERS frame (with any trailing CONTINUATION frames),
//     HPACK-decode the header block, replace `:authority` with `localhost`,
//     and HPACK-re-encode using only literal-without-indexing-new-name (the
//     simplest stateless representation). The downstream direction is
//     forwarded byte-for-byte.
//
// HPACK state: kubelet's encoder may use indexed references that depend on
// its prior dynamic-table updates, so we MUST decode statefully with one
// `fluke_hpack::Decoder` per connection. Our emitted re-encoding does not
// touch the dynamic table, so the downstream decoder's table also stays
// empty without coordination.

use anyhow::{Context, Result, anyhow};
use fluke_hpack::Decoder;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};

const HTTP2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const FRAME_HEADERS: u8 = 0x1;
const FRAME_CONTINUATION: u8 = 0x9;
const FLAG_END_HEADERS: u8 = 0x4;
const FLAG_PADDED: u8 = 0x8;
const FLAG_PRIORITY: u8 = 0x20;

/// Bidirectional HTTP/2 proxy from a kubelet UDS connection to a downstream
/// TCP connection. Rewrites `:authority` on the upstream direction.
pub async fn proxy(mut kubelet: UnixStream, mut tonic: TcpStream) -> Result<()> {
    let (mut k_r, mut k_w) = kubelet.split();
    let (mut t_r, mut t_w) = tonic.split();

    let upstream = async {
        // Forward the HTTP/2 connection preface verbatim.
        let mut preface = [0u8; 24];
        k_r.read_exact(&mut preface)
            .await
            .context("reading HTTP/2 preface")?;
        if preface != HTTP2_PREFACE {
            return Err(anyhow!("bad HTTP/2 preface from kubelet"));
        }
        t_w.write_all(&preface).await?;

        let mut hpack = Decoder::new();
        loop {
            let mut hdr = [0u8; 9];
            k_r.read_exact(&mut hdr).await?;
            let length =
                ((hdr[0] as usize) << 16) | ((hdr[1] as usize) << 8) | (hdr[2] as usize);
            let ty = hdr[3];
            let flags = hdr[4];
            let stream_id =
                u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]) & 0x7FFF_FFFF;
            let mut payload = vec![0u8; length];
            k_r.read_exact(&mut payload).await?;

            if ty == FRAME_HEADERS && stream_id != 0 {
                let (rewritten_frames, end_stream) =
                    rewrite_headers(&mut hpack, &mut k_r, flags, stream_id, &payload).await?;
                for f in &rewritten_frames {
                    t_w.write_all(f).await?;
                }
                let _ = end_stream;
            } else {
                t_w.write_all(&hdr).await?;
                t_w.write_all(&payload).await?;
            }
        }
        // unreachable in practice; loop exits via Err propagation.
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };

    let downstream = async {
        tokio::io::copy(&mut t_r, &mut k_w).await?;
        Ok::<(), anyhow::Error>(())
    };

    // Either direction ending closes the proxy. EOF / broken pipe is normal
    // when kubelet drops the connection, so don't surface those as errors.
    tokio::select! {
        r = upstream => normalize(r),
        r = downstream => normalize(r),
    }
}

fn normalize(r: Result<()>) -> Result<()> {
    match r {
        Ok(()) => Ok(()),
        Err(e) => {
            if let Some(io) = e.downcast_ref::<std::io::Error>() {
                use std::io::ErrorKind::*;
                if matches!(
                    io.kind(),
                    UnexpectedEof | BrokenPipe | ConnectionReset | ConnectionAborted
                ) {
                    return Ok(());
                }
            }
            Err(e)
        }
    }
}

/// Read the whole header block (across HEADERS + CONTINUATION frames),
/// decode HPACK, rewrite `:authority`, re-encode, and return the bytes of
/// the replacement HEADERS frame (always a single frame with END_HEADERS set,
/// because our re-encoding is tiny).
async fn rewrite_headers<R>(
    hpack: &mut Decoder<'static>,
    reader: &mut R,
    initial_flags: u8,
    stream_id: u32,
    initial_payload: &[u8],
) -> Result<(Vec<Vec<u8>>, bool)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    // Strip PADDED + PRIORITY prefixes from the header block fragment.
    let mut block = Vec::new();
    let mut flags = initial_flags;
    push_fragment(&mut block, flags, initial_payload)?;

    while flags & FLAG_END_HEADERS == 0 {
        let mut hdr = [0u8; 9];
        reader.read_exact(&mut hdr).await?;
        let length =
            ((hdr[0] as usize) << 16) | ((hdr[1] as usize) << 8) | (hdr[2] as usize);
        let ty = hdr[3];
        let cont_flags = hdr[4];
        let mut payload = vec![0u8; length];
        reader.read_exact(&mut payload).await?;
        if ty != FRAME_CONTINUATION {
            return Err(anyhow!("expected CONTINUATION, got frame type {:#x}", ty));
        }
        block.extend_from_slice(&payload);
        flags = cont_flags;
    }

    let mut headers: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    hpack
        .decode_with_cb(&block, |n, v| {
            headers.push((n.into_owned(), v.into_owned()));
        })
        .map_err(|e| anyhow!("HPACK decode failed: {e:?}"))?;

    for (n, v) in headers.iter_mut() {
        if n.as_slice() == b":authority" {
            *v = b"localhost".to_vec();
        }
    }

    let new_block = encode_block(&headers);

    // Preserve END_STREAM from the original HEADERS flags; force END_HEADERS;
    // drop PADDED + PRIORITY (we don't carry their payload bytes).
    let new_flags = (initial_flags & !(FLAG_PADDED | FLAG_PRIORITY)) | FLAG_END_HEADERS;
    let frame = build_frame(FRAME_HEADERS, new_flags, stream_id, &new_block);
    Ok((vec![frame], initial_flags & 0x1 != 0))
}

fn push_fragment(out: &mut Vec<u8>, flags: u8, payload: &[u8]) -> Result<()> {
    let mut p = payload;
    if flags & FLAG_PADDED != 0 {
        if p.is_empty() {
            return Err(anyhow!("PADDED HEADERS frame is empty"));
        }
        let pad_len = p[0] as usize;
        p = &p[1..];
        if pad_len > p.len() {
            return Err(anyhow!("PADDED HEADERS pad length exceeds payload"));
        }
        p = &p[..p.len() - pad_len];
    }
    if flags & FLAG_PRIORITY != 0 {
        if p.len() < 5 {
            return Err(anyhow!("PRIORITY HEADERS frame too short"));
        }
        p = &p[5..];
    }
    out.extend_from_slice(p);
    Ok(())
}

fn build_frame(ty: u8, flags: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    let mut frame = Vec::with_capacity(9 + len);
    frame.push(((len >> 16) & 0xFF) as u8);
    frame.push(((len >> 8) & 0xFF) as u8);
    frame.push((len & 0xFF) as u8);
    frame.push(ty);
    frame.push(flags);
    let sid = stream_id.to_be_bytes();
    frame.extend_from_slice(&sid);
    frame.extend_from_slice(payload);
    frame
}

/// HPACK-encode a header block using only "Literal Header Field without
/// Indexing -- New Name" (RFC 7541 6.2.2), no Huffman. Tonic's HPACK decoder
/// accepts this and doesn't touch its dynamic table — keeping the proxy
/// stateless on the emit side.
fn encode_block(headers: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in headers {
        out.push(0x00); // literal-without-indexing, new name (index = 0)
        encode_string(&mut out, name);
        encode_string(&mut out, value);
    }
    out
}

fn encode_string(out: &mut Vec<u8>, s: &[u8]) {
    encode_int(out, 0x00, 7, s.len()); // H = 0, raw length
    out.extend_from_slice(s);
}

/// RFC 7541 5.1 integer encoding with the given N-bit prefix.
fn encode_int(out: &mut Vec<u8>, prefix: u8, n: u8, value: usize) {
    let max = (1usize << n) - 1;
    if value < max {
        out.push(prefix | (value as u8));
    } else {
        out.push(prefix | (max as u8));
        let mut v = value - max;
        while v >= 128 {
            out.push(((v & 0x7F) | 0x80) as u8);
            v >>= 7;
        }
        out.push(v as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluke_hpack::{Decoder, Encoder};

    /// Sanity check: encode a header block the way Go grpc-go would
    /// (using fluke_hpack's encoder), feed it through the rewrite path,
    /// and verify the result decodes to the expected headers with
    /// :authority replaced.
    #[test]
    fn rewrites_authority_with_stateful_decode() {
        let mut enc = Encoder::new();
        let original = [
            (b":method".as_slice(), b"POST".as_slice()),
            (b":scheme".as_slice(), b"http".as_slice()),
            (
                b":authority".as_slice(),
                b"/var/lib/kubelet/plugins_registry/gpu.nvidia.com-reg.sock".as_slice(),
            ),
            (b":path".as_slice(), b"/pluginregistration.Registration/GetInfo".as_slice()),
            (b"content-type".as_slice(), b"application/grpc".as_slice()),
            (b"user-agent".as_slice(), b"grpc-go/1.71.0".as_slice()),
            (b"te".as_slice(), b"trailers".as_slice()),
        ];
        let encoded = enc.encode(original.iter().copied());

        let mut dec = Decoder::new();
        let mut headers: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        dec.decode_with_cb(&encoded, |n, v| {
            headers.push((n.into_owned(), v.into_owned()));
        })
        .unwrap();
        for (n, v) in headers.iter_mut() {
            if n.as_slice() == b":authority" {
                *v = b"localhost".to_vec();
            }
        }
        let rewritten = encode_block(&headers);

        // Round-trip the rewritten block through a fresh decoder (as tonic
        // would) and check it parses cleanly with :authority replaced.
        let mut out = Vec::new();
        let mut sink = Decoder::new();
        sink.decode_with_cb(&rewritten, |n, v| {
            out.push((n.into_owned(), v.into_owned()));
        })
        .unwrap();

        let auth = out
            .iter()
            .find(|(n, _)| n.as_slice() == b":authority")
            .expect(":authority missing");
        assert_eq!(auth.1.as_slice(), b"localhost");
        // Every other header preserved.
        for (orig_n, orig_v) in original.iter() {
            if *orig_n == b":authority" {
                continue;
            }
            let got = out
                .iter()
                .find(|(n, _)| n.as_slice() == *orig_n)
                .unwrap_or_else(|| panic!("header {} missing", String::from_utf8_lossy(orig_n)));
            assert_eq!(got.1.as_slice(), *orig_v);
        }
    }

    /// Subsequent HEADERS frames may reference dynamic-table entries the
    /// encoder added on previous frames; the proxy must decode statefully.
    #[test]
    fn stateful_across_frames() {
        let mut enc = Encoder::new();
        let frame1 = enc.encode([
            (b":authority".as_slice(), b"/var/lib/kubelet/plugins_registry/x.sock".as_slice()),
            (b"x-custom".as_slice(), b"hello".as_slice()),
        ]);
        // Second frame: encoder may now use indexed references for repeat headers.
        let frame2 = enc.encode([
            (b":authority".as_slice(), b"/var/lib/kubelet/plugins_registry/x.sock".as_slice()),
            (b"x-custom".as_slice(), b"hello".as_slice()),
        ]);

        let mut dec = Decoder::new();
        for frame in &[frame1, frame2] {
            let mut hs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            dec.decode_with_cb(frame, |n, v| {
                hs.push((n.into_owned(), v.into_owned()));
            })
            .unwrap();
            for (n, v) in hs.iter_mut() {
                if n.as_slice() == b":authority" {
                    *v = b"localhost".to_vec();
                }
            }
            let rewritten = encode_block(&hs);
            let mut sink = Decoder::new();
            let mut out = Vec::new();
            sink.decode_with_cb(&rewritten, |n, v| {
                out.push((n.into_owned(), v.into_owned()));
            })
            .unwrap();
            assert_eq!(
                out.iter()
                    .find(|(n, _)| n.as_slice() == b":authority")
                    .unwrap()
                    .1
                    .as_slice(),
                b"localhost"
            );
        }
    }
}
