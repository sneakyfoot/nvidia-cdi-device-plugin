use clap::Parser;
use glob::glob;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    net::{UnixListener, UnixStream},
    select,
    sync::{mpsc, watch, Mutex},
    task::JoinHandle,
    time::sleep,
};
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tonic::{
    async_trait,
    transport::{Endpoint, Server},
    Request, Response, Status,
};
use tower::service_fn;
use hyper_util::rt::TokioIo;

pub mod k8s {
    tonic::include_proto!("v1beta1");
}

const DEFAULT_KUBELET_DIR: &str = "/var/lib/kubelet/device-plugins";
const DEFAULT_SOCKET_NAME: &str = "nvidia-cdi-device-plugin.sock";
const DEFAULT_RESOURCE_NAME: &str = "nvidia.com/gpu";
const DEVICE_PLUGIN_VERSION: &str = "v1beta1";
const DEVICE_GLOB: &str = "/dev/nvidia[0-9]*";

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Kubernetes resource name to advertise (must match CDI kind)
    #[arg(long, default_value = DEFAULT_RESOURCE_NAME)]
    resource_name: String,

    /// kubelet device plugin directory
    #[arg(long, default_value = DEFAULT_KUBELET_DIR)]
    kubelet_dir: String,

    /// unix domain socket name for this plugin
    #[arg(long, default_value = DEFAULT_SOCKET_NAME)]
    socket_name: String,
}

fn discover_devices(resource_name: &str) -> anyhow::Result<BTreeMap<String, k8s::Device>> {
    let mut devs = BTreeMap::new();
    let pattern = DEVICE_GLOB;

    for (idx, _path) in glob(pattern)?.flatten().enumerate() {
        let id = format!("{resource_name}={idx}");
        devs.insert(
            id.clone(),
            k8s::Device {
                id,
                health: "Healthy".to_string(),
                topology: None,
            },
        );
    }

    if devs.is_empty() {
        eprintln!("warning: no devices found matching {}", pattern);
    }

    Ok(devs)
}

#[derive(Clone)]
struct NvidiaCdiDevicePlugin {
    resource_name: String,
    devices: BTreeMap<String, k8s::Device>,
    shutdown: watch::Receiver<bool>,
}

impl NvidiaCdiDevicePlugin {
    fn new(resource_name: String, shutdown: watch::Receiver<bool>) -> anyhow::Result<Self> {
        Ok(Self {
            devices: discover_devices(&resource_name)?,
            resource_name,
            shutdown,
        })
    }
}

#[async_trait]
impl k8s::device_plugin_server::DevicePlugin for NvidiaCdiDevicePlugin {
    async fn get_device_plugin_options(
        &self,
        _request: Request<k8s::Empty>,
    ) -> Result<Response<k8s::DevicePluginOptions>, Status> {
        Ok(Response::new(k8s::DevicePluginOptions {
            pre_start_required: false,
            get_preferred_allocation_available: false,
        }))
    }

    type ListAndWatchStream = ReceiverStream<Result<k8s::ListAndWatchResponse, Status>>;

    async fn list_and_watch(
        &self,
        _request: Request<k8s::Empty>,
    ) -> Result<Response<Self::ListAndWatchStream>, Status> {
        let devices: Vec<k8s::Device> = self.devices.values().cloned().collect();
        println!(
            "ListAndWatch for {} advertising {} devices",
            self.resource_name,
            devices.len()
        );
        let (tx, rx) = mpsc::channel(1);

        tx.send(Ok(k8s::ListAndWatchResponse { devices }))
            .await
            .map_err(|_| Status::internal("failed to send initial device list"))?;

        // Keep the stream open until shutdown, mimicking the Go plugin's blocking behavior.
        let mut shutdown = self.shutdown.clone();
        let tx_hold = tx.clone();
        tokio::spawn(async move {
            loop {
                if *shutdown.borrow() {
                    break;
                }
                if shutdown.changed().await.is_err() {
                    break;
                }
            }
            drop(tx_hold);
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn allocate(
        &self,
        request: Request<k8s::AllocateRequest>,
    ) -> Result<Response<k8s::AllocateResponse>, Status> {
        let mut container_responses =
            Vec::with_capacity(request.get_ref().container_requests.len());

        for creq in &request.get_ref().container_requests {
            let mut cdi_devices = Vec::with_capacity(creq.devices_ids.len());

            for dev_id in &creq.devices_ids {
                if !self.devices.contains_key(dev_id) {
                    return Err(Status::invalid_argument(format!(
                        "unknown device ID {dev_id}"
                    )));
                }

                cdi_devices.push(k8s::CdiDevice {
                    name: dev_id.clone(),
                });
            }

            container_responses.push(k8s::ContainerAllocateResponse {
                envs: Default::default(),
                mounts: vec![],
                devices: vec![],
                annotations: Default::default(),
                cdi_devices,
            });
        }

        Ok(Response::new(k8s::AllocateResponse {
            container_responses,
        }))
    }

    async fn get_preferred_allocation(
        &self,
        request: Request<k8s::PreferredAllocationRequest>,
    ) -> Result<Response<k8s::PreferredAllocationResponse>, Status> {
        let mut out = k8s::PreferredAllocationResponse {
            container_responses: Vec::new(),
        };

        for creq in &request.get_ref().container_requests {
            let available = &creq.available_device_i_ds;
            let size = creq.allocation_size as usize;
            let chosen = available.iter().take(size).cloned().collect();

            out.container_responses
                .push(k8s::ContainerPreferredAllocationResponse {
                    device_i_ds: chosen,
                });
        }

        Ok(Response::new(out))
    }

    async fn pre_start_container(
        &self,
        _request: Request<k8s::PreStartContainerRequest>,
    ) -> Result<Response<k8s::PreStartContainerResponse>, Status> {
        Ok(Response::new(k8s::PreStartContainerResponse {}))
    }
}

async fn start_device_plugin_server(
    plugin: NvidiaCdiDevicePlugin,
    socket_path: PathBuf,
) -> anyhow::Result<JoinHandle<()>> {
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }

    let uds = UnixListener::bind(&socket_path)?;
    let incoming = UnixListenerStream::new(uds);
    let service = k8s::device_plugin_server::DevicePluginServer::new(plugin);

    let handle = tokio::spawn(async move {
        if let Err(err) = Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming)
            .await
        {
            eprintln!("gRPC server crashed: {err}");
        }
    });

    Ok(handle)
}

async fn wait_for_socket(socket_path: &Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match UnixStream::connect(socket_path).await {
            Ok(_) => return Ok(()),
            Err(err) => {
                if Instant::now() >= deadline {
                    return Err(anyhow::anyhow!(
                        "timeout waiting for gRPC server to start: {err}"
                    ));
                }
            }
        }
        sleep(Duration::from_millis(200)).await;
    }
}

async fn register_with_kubelet(
    kubelet_dir: &str,
    socket_name: &str,
    resource_name: &str,
) -> anyhow::Result<()> {
    let kubelet_socket = Path::new(kubelet_dir).join("kubelet.sock");

    let channel = Endpoint::try_from("http://[::]:50051")?
        .connect_with_connector(service_fn(move |_| {
            let path = kubelet_socket.clone();
            async move { UnixStream::connect(path).await.map(TokioIo::new) }
        }))
        .await?;

    let mut client = k8s::registration_client::RegistrationClient::new(channel);

    let req = k8s::RegisterRequest {
        version: DEVICE_PLUGIN_VERSION.to_string(),
        endpoint: socket_name.to_string(),
        resource_name: resource_name.to_string(),
        options: Some(k8s::DevicePluginOptions {
            pre_start_required: false,
            get_preferred_allocation_available: false,
        }),
    };

    client.register(req).await?;
    Ok(())
}

async fn maintain_registration(
    kubelet_dir: String,
    socket_name: String,
    resource_name: String,
    plugin: NvidiaCdiDevicePlugin,
    socket_path: PathBuf,
    server_handle: Arc<Mutex<JoinHandle<()>>>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if *shutdown.borrow() {
                break;
            }

            // If kubelet cleaned up the socket, restart the gRPC server to re-bind the path.
            if !socket_path.exists() {
                {
                    let handle = server_handle.lock().await;
                    handle.abort();
                }
                match start_device_plugin_server(plugin.clone(), socket_path.clone()).await {
                    Ok(new_handle) => {
                        let mut guard = server_handle.lock().await;
                        *guard = new_handle;
                    }
                    Err(err) => {
                        eprintln!("failed to restart device plugin server: {err}");
                    }
                }
            }

            if let Err(err) =
                register_with_kubelet(&kubelet_dir, &socket_name, &resource_name).await
            {
                eprintln!("registration with kubelet failed: {err}");
            }

            select! {
                _ = sleep(Duration::from_secs(10)) => {},
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if !args.resource_name.contains('/') {
        anyhow::bail!("resource-name must be fully qualified, e.g. nvidia.com/gpu");
    }

    let socket_path = Path::new(&args.kubelet_dir).join(&args.socket_name);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let plugin = NvidiaCdiDevicePlugin::new(args.resource_name.clone(), shutdown_rx.clone())?;
    let device_count = plugin.devices.len();
    let plugin_for_server = plugin.clone();
    let server = start_device_plugin_server(plugin_for_server, socket_path.clone()).await?;
    let server_handle = Arc::new(Mutex::new(server));

    wait_for_socket(&socket_path, Duration::from_secs(5)).await?;
    register_with_kubelet(&args.kubelet_dir, &args.socket_name, &args.resource_name).await?;
    let reg_task = maintain_registration(
        args.kubelet_dir.clone(),
        args.socket_name.clone(),
        args.resource_name.clone(),
        plugin,
        socket_path.clone(),
        server_handle.clone(),
        shutdown_rx,
    )
    .await;

    println!(
        "nvidia CDI device plugin running. resource={} devices={}",
        args.resource_name, device_count
    );

    tokio::signal::ctrl_c().await?;
    println!("shutdown requested, stopping server");
    let _ = shutdown_tx.send(true);
    {
        let handle = server_handle.lock().await;
        handle.abort();
    }
    reg_task.abort();

    Ok(())
}
