fn main() -> Result<(), Box<dyn ::std::error::Error>> {
    let protoc_path = protoc_bin_vendored::protoc_bin_path()?;

    let mut config = prost_build::Config::new();
    config.protoc_executable(protoc_path);

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_with_config(
            config,
            &["proto/dra.proto", "proto/registration.proto"],
            &["proto"],
        )?;

    println!("cargo:rerun-if-changed=proto/dra.proto");
    println!("cargo:rerun-if-changed=proto/registration.proto");
    Ok(())
}
