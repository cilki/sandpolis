//! A protocol-agnostic service interface over probe devices.
//!
//! The health subsystem drives this; everything underneath — the Docker API,
//! virsh — is the probe subsystem's business. A caller names a device and one of
//! its service protocols and gets its service instances (containers or virtual
//! machines) back, along with start/stop/restart controls.
//!
//! Every request is self-contained (device + protocol + operation) rather than
//! opening a session and issuing operations against it, mirroring
//! [`crate::filesystem`]. Actions answer with a refreshed listing so a caller's
//! view updates in the same round trip.
//!
//! Credentials never leave the server. Callers send a device id, which the server
//! resolves against [`REGISTERED_DEVICES`](crate::REGISTERED_DEVICES).

use crate::ProbeType;
use serde::{Deserialize, Serialize};

/// The lifecycle state of a container or virtual machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ServiceState {
    Running,
    Paused,
    Stopped,
    /// Anything else; the raw state text stays in the entry's `status`.
    Other,
}

impl ServiceState {
    pub fn label(&self) -> &'static str {
        match self {
            ServiceState::Running => "running",
            ServiceState::Paused => "paused",
            ServiceState::Stopped => "stopped",
            ServiceState::Other => "other",
        }
    }
}

/// One Docker container on a device.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContainerInfo {
    /// Container id (short form).
    pub id: String,
    /// Primary name, leading '/' stripped.
    pub name: String,
    pub image: String,
    pub state: ServiceState,
    /// Human-readable status, e.g. "Up 2 hours".
    pub status: Option<String>,
}

/// One libvirt domain on a device.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DomainInfo {
    /// Domain name; virsh addresses domains by it.
    pub name: String,
    pub uuid: Option<String>,
    pub state: ServiceState,
    /// Raw virsh state text, e.g. "shut off".
    pub status: Option<String>,
}

/// An operation against a probe device's services.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ProbeServiceOp {
    List,
    Start {
        id: String,
    },
    /// Stop a service. For libvirt, `force` powers the domain off (`virsh
    /// destroy`) instead of requesting a graceful shutdown; Docker always stops
    /// gracefully with a timeout and ignores it.
    Stop {
        id: String,
        force: bool,
    },
    Restart {
        id: String,
    },
}

/// A request naming the device, the protocol to reach it by, and the operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeServiceRequest {
    pub device_id: u64,
    pub protocol: ProbeType,
    pub op: ProbeServiceOp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ProbeServiceResponse {
    /// The device's containers; also the answer to a successful action.
    Containers(Vec<ContainerInfo>),
    /// The device's domains; also the answer to a successful action.
    Domains(Vec<DomainInfo>),
    Failed(String),
}

#[cfg(feature = "server")]
mod server {
    use super::*;
    use crate::{REGISTERED_DEVICES, RegisteredDevice};
    use anyhow::Result;
    use sandpolis_instance::network::{
        RegisterResponders, ResponderRegistration, StreamRegistry, StreamResponder,
    };
    use sandpolis_macros::Stream;
    use std::collections::HashMap;
    use std::sync::LazyLock;
    use tokio::sync::{Mutex, mpsc::Sender};

    /// Docker engine handles, keyed by device. A handle is just an HTTP client,
    /// but constructing one re-reads TLS material from disk, so they outlive the
    /// one-shot streams that use them; a failed operation evicts its entry so the
    /// next request reconnects.
    static DOCKER_CONNECTIONS: LazyLock<Mutex<HashMap<u64, crate::docker::DockerEngine>>> =
        LazyLock::new(Default::default);

    /// Look a registered device up by id.
    fn device(device_id: u64) -> Result<RegisteredDevice> {
        REGISTERED_DEVICES
            .read()
            .ok()
            .and_then(|devices| devices.iter().find(|d| d.id.body() == device_id).cloned())
            .ok_or_else(|| anyhow::anyhow!("device {device_id} is not registered"))
    }

    async fn docker_engine(device_id: u64) -> Result<crate::docker::DockerEngine> {
        if let Some(engine) = DOCKER_CONNECTIONS.lock().await.get(&device_id) {
            return Ok(engine.clone());
        }

        let device = device(device_id)?;
        let config = device
            .device
            .docker
            .ok_or_else(|| anyhow::anyhow!("device has no Docker configuration"))?;
        let engine = crate::docker::DockerEngine::connect(&config)?;
        DOCKER_CONNECTIONS
            .lock()
            .await
            .insert(device_id, engine.clone());
        Ok(engine)
    }

    async fn dispatch_docker(device_id: u64, op: ProbeServiceOp) -> Result<ProbeServiceResponse> {
        let engine = docker_engine(device_id).await?;
        match &op {
            ProbeServiceOp::List => {}
            ProbeServiceOp::Start { id } => engine.start(id).await?,
            ProbeServiceOp::Stop { id, .. } => engine.stop(id).await?,
            ProbeServiceOp::Restart { id } => engine.restart(id).await?,
        }
        Ok(ProbeServiceResponse::Containers(engine.list().await?))
    }

    async fn dispatch_libvirt(device_id: u64, op: ProbeServiceOp) -> Result<ProbeServiceResponse> {
        let device = device(device_id)?;
        let config = device
            .device
            .libvirt
            .ok_or_else(|| anyhow::anyhow!("device has no libvirt configuration"))?;
        match &op {
            ProbeServiceOp::List => {}
            ProbeServiceOp::Start { id } => crate::libvirt::start(&config, id).await?,
            ProbeServiceOp::Stop { id, force: false } => {
                crate::libvirt::shutdown(&config, id).await?
            }
            ProbeServiceOp::Stop { id, force: true } => {
                crate::libvirt::destroy(&config, id).await?
            }
            ProbeServiceOp::Restart { id } => crate::libvirt::reboot(&config, id).await?,
        }
        Ok(ProbeServiceResponse::Domains(
            crate::libvirt::list(&config).await?,
        ))
    }

    /// Run one operation, translating errors into [`ProbeServiceResponse::Failed`].
    async fn dispatch(request: ProbeServiceRequest) -> ProbeServiceResponse {
        let ProbeServiceRequest {
            device_id,
            protocol,
            op,
        } = request;

        let result = match protocol {
            ProbeType::Docker => {
                let result = dispatch_docker(device_id, op).await;
                if result.is_err() {
                    // The cached engine may be what's broken, so don't hand it
                    // to the next request.
                    DOCKER_CONNECTIONS.lock().await.remove(&device_id);
                }
                result
            }
            ProbeType::Libvirt => dispatch_libvirt(device_id, op).await,
            other => Err(anyhow::anyhow!(
                "{} is not a service protocol",
                other.display_name()
            )),
        };

        result.unwrap_or_else(|e| ProbeServiceResponse::Failed(e.to_string()))
    }

    /// Server side of the probe service stream.
    #[derive(Stream, Default)]
    pub struct ProbeServiceStreamResponder;

    impl StreamResponder for ProbeServiceStreamResponder {
        type In = ProbeServiceRequest;
        type Out = ProbeServiceResponse;

        async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
            // A graceful shutdown can take tens of seconds, and responders run
            // on the socket receive path.
            tokio::spawn(async move {
                let _ = sender.send(dispatch(request).await).await;
            });
            Ok(())
        }
    }

    /// Registers [`ProbeServiceStreamResponder`] on each connection.
    pub struct ProbeServiceResponderRegistration;

    impl RegisterResponders for ProbeServiceResponderRegistration {
        fn register_responders(&self, registry: &StreamRegistry) {
            registry.register_responder(ProbeServiceStreamResponder::default);
        }
    }

    inventory::submit!(ResponderRegistration(&ProbeServiceResponderRegistration));
}

#[cfg(feature = "server")]
pub use server::ProbeServiceStreamResponder;

/// Client-side access to the interface above.
///
/// Kept as a module rather than flattened because an all-in-one build enables
/// both features (see [`crate::filesystem::client`]).
#[cfg(feature = "client")]
pub mod client {
    use super::*;
    use anyhow::Result;
    use sandpolis_instance::network::InstanceConnection;
    use sandpolis_instance::network::StreamRequester;
    use sandpolis_instance::network::stream::StreamMessage;
    use sandpolis_macros::Stream;
    use std::collections::HashMap;
    use std::sync::{Arc, LazyLock, RwLock};
    use tokio::sync::mpsc::Sender;

    /// What the client knows about one device's services, keyed by device id.
    ///
    /// A global rather than a bevy resource because `bind_text` projections get
    /// no world access (see [`crate::filesystem::client::PROBE_FS_VIEWS`]).
    pub static PROBE_SERVICE_VIEWS: LazyLock<Arc<RwLock<HashMap<u64, ProbeServiceView>>>> =
        LazyLock::new(Default::default);

    /// The last thing a device's services told us.
    #[derive(Clone, Debug, Default)]
    pub struct ProbeServiceView {
        pub containers: Option<Vec<ContainerInfo>>,
        pub domains: Option<Vec<DomainInfo>>,
        /// Set while a request is outstanding, cleared when one answers.
        pub busy: bool,
        /// Why the last request failed, if it did.
        pub error: Option<String>,
    }

    /// Read one device's view.
    pub fn view(device_id: u64) -> Option<ProbeServiceView> {
        PROBE_SERVICE_VIEWS.read().ok()?.get(&device_id).cloned()
    }

    fn update(device_id: u64, f: impl FnOnce(&mut ProbeServiceView)) {
        if let Ok(mut views) = PROBE_SERVICE_VIEWS.write() {
            f(views.entry(device_id).or_default());
        }
    }

    /// Client side of the probe service stream: folds responses into
    /// [`PROBE_SERVICE_VIEWS`] so the GUI can render them without holding a
    /// session.
    #[derive(Stream)]
    pub struct ProbeServiceStreamRequester {
        /// Which device's view to fold responses into. The response carries no
        /// device id, so the requester remembers what it asked about.
        pub device_id: u64,
    }

    impl StreamRequester for ProbeServiceStreamRequester {
        type In = ProbeServiceResponse;
        type Out = ProbeServiceRequest;

        async fn on_message(&self, response: Self::In, _: Sender<Self::Out>) -> Result<()> {
            let device_id = self.device_id;
            update(device_id, |view| {
                view.busy = false;
                match response {
                    ProbeServiceResponse::Containers(containers) => {
                        view.error = None;
                        view.containers = Some(containers);
                    }
                    ProbeServiceResponse::Domains(domains) => {
                        view.error = None;
                        view.domains = Some(domains);
                    }
                    ProbeServiceResponse::Failed(reason) => {
                        tracing::warn!(device_id, %reason, "Probe service request failed");
                        view.error = Some(reason);
                    }
                }
            });
            Ok(())
        }
    }

    /// How long to keep a one-shot stream registered so its answer arrives before
    /// the stream is released. Longer than the filesystem's window because a
    /// graceful VM shutdown is slow.
    const RESPONSE_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

    /// Send one service operation to the server that reaches `device_id`.
    ///
    /// One-shot, like [`crate::filesystem::client::request`]: the stream lives
    /// just long enough to carry the answer back into [`PROBE_SERVICE_VIEWS`].
    pub fn request(
        conn: Arc<InstanceConnection>,
        device_id: u64,
        protocol: ProbeType,
        op: ProbeServiceOp,
    ) {
        update(device_id, |view| {
            view.busy = true;
        });
        sandpolis_client::sync::spawn(async move {
            let (id, tx) = conn.register_stream(ProbeServiceStreamRequester { device_id });
            let payload = match serde_cbor::to_vec(&ProbeServiceRequest {
                device_id,
                protocol,
                op,
            }) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to encode probe service request");
                    update(device_id, |view| view.busy = false);
                    return;
                }
            };
            let _ = tx.send(StreamMessage::local(id, payload)).await;
            tokio::time::sleep(RESPONSE_WINDOW).await;
            conn.close_stream(id);
        });
    }

    /// The connection that reaches `device_id`.
    pub use crate::filesystem::client::connection_for;

    fn send(device_id: u64, protocol: ProbeType, op: ProbeServiceOp) {
        if let Some(conn) = connection_for(device_id) {
            request(conn, device_id, protocol, op);
        } else {
            tracing::warn!(device_id, "No server connection; cannot reach services");
        }
    }

    /// Ask for the device's containers/domains, if a connection is available.
    pub fn list(device_id: u64, protocol: ProbeType) {
        send(device_id, protocol, ProbeServiceOp::List);
    }

    pub fn start(device_id: u64, protocol: ProbeType, id: String) {
        send(device_id, protocol, ProbeServiceOp::Start { id });
    }

    pub fn stop(device_id: u64, protocol: ProbeType, id: String, force: bool) {
        send(device_id, protocol, ProbeServiceOp::Stop { id, force });
    }

    pub fn restart(device_id: u64, protocol: ProbeType, id: String) {
        send(device_id, protocol, ProbeServiceOp::Restart { id });
    }

    /// A requester that hands each response to a caller-owned channel instead of
    /// the shared view; this is what the CLI waits on. In its own module because
    /// it must be named after the stream (the name is the wire tag) and the
    /// view-folding requester already claims that name here.
    mod once {
        use super::*;

        #[derive(Stream)]
        pub struct ProbeServiceStreamRequester {
            pub tx: tokio::sync::mpsc::UnboundedSender<ProbeServiceResponse>,
        }

        impl StreamRequester for ProbeServiceStreamRequester {
            type In = ProbeServiceResponse;
            type Out = ProbeServiceRequest;

            async fn on_message(&self, response: Self::In, _: Sender<Self::Out>) -> Result<()> {
                let _ = self.tx.send(response);
                Ok(())
            }
        }
    }

    /// Send one operation and wait for its answer.
    pub async fn request_once(
        conn: Arc<InstanceConnection>,
        device_id: u64,
        protocol: ProbeType,
        op: ProbeServiceOp,
        timeout: std::time::Duration,
    ) -> Result<ProbeServiceResponse> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (id, stream_tx) = conn.register_stream(once::ProbeServiceStreamRequester { tx });
        let payload = serde_cbor::to_vec(&ProbeServiceRequest {
            device_id,
            protocol,
            op,
        })?;
        stream_tx.send(StreamMessage::local(id, payload)).await?;
        let response = tokio::time::timeout(timeout, rx.recv()).await;
        conn.close_stream(id);
        response
            .map_err(|_| anyhow::anyhow!("timed out waiting for the server"))?
            .ok_or_else(|| anyhow::anyhow!("stream closed without a response"))
    }
}
