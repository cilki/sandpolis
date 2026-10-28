// doc_comment! {
//     include_str!("../README.md")
// }

use anyhow::Result;
use sandpolis_instance::database::DatabaseManager;

pub mod bootagent;
#[cfg(feature = "client")]
pub mod client;
pub mod deploy;
pub mod uefi;
pub mod wake;

sandpolis_instance::register_layer!("Agent", "Managed instances running the agent");

#[derive(Clone)]
pub struct AgentManager {
    database: DatabaseManager,
}

impl AgentManager {
    pub async fn new(database: DatabaseManager) -> Result<Self> {
        Ok(Self { database })
    }

    /// Give the server-side boot responder its database context. Called once
    /// at server startup.
    #[cfg(feature = "server")]
    pub fn install_server(&self, context: bootagent::server::BootServerContext) {
        bootagent::server::install(context);
    }
}

/// Polls data periodically.
///
/// A collector doesn't own its schedule. The agent subsystem wraps each one in a
/// [`CollectorService`] and hands it to the service runner, which decides when —
/// and whether — `refresh` is called.
pub trait Collector: Send + 'static {
    /// Written as `-> impl Future` rather than `async fn` so the `Send` bound is
    /// part of the contract; [`CollectorService`] is generic over collectors and
    /// can't otherwise prove its own futures are `Send`.
    fn refresh(&mut self) -> impl Future<Output = Result<()>> + Send;
}

/// Runs a [`Collector`] on an interval as a supervised service.
#[cfg(feature = "agent")]
pub struct CollectorService<C> {
    collector: std::sync::Arc<tokio::sync::Mutex<C>>,
    layer: sandpolis_instance::LayerName,
    name: &'static str,
    description: &'static str,
    interval: std::time::Duration,
}

#[cfg(feature = "agent")]
impl<C: Collector> CollectorService<C> {
    /// Wrap `collector`, which the subsystem keeps a handle to, in a service
    /// belonging to `layer`.
    pub fn new(
        collector: std::sync::Arc<tokio::sync::Mutex<C>>,
        layer: impl Into<sandpolis_instance::LayerName>,
        name: &'static str,
        description: &'static str,
        interval: std::time::Duration,
    ) -> Self {
        Self {
            collector,
            layer: layer.into(),
            name,
            description,
            interval,
        }
    }
}

#[cfg(feature = "agent")]
impl<C: Collector> sandpolis_instance::service::Service for CollectorService<C> {
    fn name(&self) -> &'static str {
        self.name
    }

    fn layer(&self) -> sandpolis_instance::LayerName {
        self.layer.clone()
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn schedule(&self) -> sandpolis_instance::service::ServiceSchedule {
        sandpolis_instance::service::ServiceSchedule::every(self.interval)
    }

    async fn run(
        &self,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<sandpolis_instance::service::ServiceReport> {
        self.collector.lock().await.refresh().await?;
        // A collector writes whatever it found straight to the database without
        // saying how much, so there's nothing honest to report but the pass
        // itself. Failures still surface through the returned error.
        Ok(sandpolis_instance::service::ServiceReport::default())
    }
}

// What a client must be granted to open this layer's streams.
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(DeployStream), "agent:deploy")
}
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(WakeStream), "agent:power")
}
