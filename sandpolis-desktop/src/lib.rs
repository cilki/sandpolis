use anyhow::Result;
use native_db::*;
use native_model::Model;
use sandpolis_instance::InstanceId;
use sandpolis_instance::InstanceManager;
use sandpolis_instance::database::DatabaseManager;
use sandpolis_macros::data;

pub mod screenshot;
pub mod session;

#[cfg(feature = "probe")]
pub mod vnc;

#[cfg(feature = "probe")]
pub mod rdp;

#[cfg(feature = "client")]
pub mod client;

#[cfg(all(feature = "client", not(target_os = "android")))]
pub mod cli;

sandpolis_instance::register_layer!("Desktop", "Remote desktop viewing and control");

#[cfg(feature = "agent")]
mod agent;

/// Screen capture, adapted from rustdesk's `scrap`.
#[cfg(feature = "agent")]
pub mod capture;

/// Keyboard/mouse injection, adapted from rustdesk's fork of `enigo`.
#[cfg(feature = "agent")]
pub mod input;

#[cfg(feature = "agent")]
mod platform;

/// A capturable desktop (display) discovered on an agent.
#[data(defaults)]
pub struct DesktopData {
    #[secondary_key]
    pub _instance_id: InstanceId,

    /// Stable name used to identify the display when streaming
    #[secondary_key]
    pub name: String,

    /// Display width in pixels
    pub width: u32,

    /// Display height in pixels
    pub height: u32,

    /// Whether this is the primary display
    pub primary: bool,

    /// Display scale factor
    pub scale_factor: f64,
}

#[derive(Clone)]
pub struct DesktopManager {
    #[allow(dead_code)]
    database: DatabaseManager,
    #[allow(dead_code)]
    pub instance_id: InstanceId,

    /// Agent-side display collector.
    #[cfg(feature = "agent")]
    pub displays: std::sync::Arc<tokio::sync::Mutex<agent::DesktopDisplayCollector>>,
}

impl DesktopManager {
    pub async fn new(database: DatabaseManager, instance: InstanceManager) -> Result<Self> {
        Ok(Self {
            #[cfg(feature = "agent")]
            displays: std::sync::Arc::new(tokio::sync::Mutex::new(
                agent::DesktopDisplayCollector::new(
                    database.realm(sandpolis_instance::realm::RealmName::default())?,
                    instance.instance_id,
                )?,
            )),
            instance_id: instance.instance_id,
            database,
        })
    }

    /// Add the subsystem's background services to the agent's runner.
    ///
    /// Only an agent has a local desktop to enumerate, so this is where display
    /// scanning happens; a server or client never touches the display server.
    #[cfg(feature = "agent")]
    pub fn register_services(&self, runner: &mut sandpolis_instance::service::ServiceRunner) {
        runner.register(sandpolis_agent::CollectorService::new(
            self.displays.clone(),
            "Desktop",
            "displays",
            "Enumerates the host's capturable displays",
            std::time::Duration::from_secs(60),
        ));
    }
}

/// Static handler for registering desktop stream responders.
#[cfg(feature = "agent")]
pub struct DesktopResponderRegistration;

#[cfg(feature = "agent")]
impl sandpolis_instance::network::RegisterResponders for DesktopResponderRegistration {
    fn register_responders(&self, registry: &sandpolis_instance::network::StreamRegistry) {
        registry.register_responder(session::DesktopStreamResponder::default);
        registry.register_responder(screenshot::DesktopScreenshotResponder::default);
    }
}

#[cfg(feature = "agent")]
inventory::submit!(sandpolis_instance::network::ResponderRegistration(
    &DesktopResponderRegistration
));

/// Static handler for registering VNC probe responders.
///
/// Separate from [`DesktopResponderRegistration`] because probes are reached
/// only from servers, whereas the capture responders above only exist on agents.
#[cfg(all(feature = "server", feature = "probe"))]
pub struct DesktopProbeResponderRegistration;

#[cfg(all(feature = "server", feature = "probe"))]
impl sandpolis_instance::network::RegisterResponders for DesktopProbeResponderRegistration {
    fn register_responders(&self, registry: &sandpolis_instance::network::StreamRegistry) {
        registry.register_responder(vnc::VncStreamResponder::default);
        registry.register_responder(rdp::RdpStreamResponder::default);
    }
}

#[cfg(all(feature = "server", feature = "probe"))]
inventory::submit!(sandpolis_instance::network::ResponderRegistration(
    &DesktopProbeResponderRegistration
));

// What a client must be granted to open this layer's streams.
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(DesktopStream), "desktop:session")
}
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(DesktopScreenshot), "desktop:screenshot")
}
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(VncStream), "desktop:vnc")
}
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(RdpStream), "desktop:rdp")
}
