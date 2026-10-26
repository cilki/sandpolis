use anyhow::Result;
use native_db::ToKey;
use native_model::Model;
use sandpolis_instance::InstanceId;
use sandpolis_instance::InstanceManager;
use sandpolis_instance::database::{DatabaseManager, Resident};
use sandpolis_instance::realm::RealmName;
use sandpolis_macros::data;

#[cfg(feature = "agent")]
use std::sync::Arc;

#[cfg(feature = "client")]
pub mod client;

pub mod systemd;

sandpolis_instance::register_layer!("Health", "Service and host health");

#[data]
#[derive(Default)]
pub struct HealthManagerData {}

/// The health subsystem tracks the operational status of services and the host's
/// overall well-being (currently: systemd units).
#[derive(Clone)]
pub struct HealthManager {
    #[allow(dead_code)]
    data: Resident<HealthManagerData>,
    #[allow(dead_code)]
    pub instance_id: InstanceId,

    /// Agent-side systemd collector.
    #[cfg(feature = "agent")]
    pub systemd: Arc<tokio::sync::Mutex<systemd::agent::SystemdCollector>>,
}

impl HealthManager {
    pub async fn new(database: DatabaseManager, instance: InstanceManager) -> Result<Self> {
        Ok(Self {
            #[cfg(feature = "agent")]
            systemd: Arc::new(tokio::sync::Mutex::new(
                systemd::agent::SystemdCollector::new(
                    database.realm(RealmName::default())?,
                    instance.instance_id,
                )?,
            )),
            instance_id: instance.instance_id,
            data: database.realm(RealmName::default())?.resident(())?,
        })
    }

    /// Add the subsystem's background services to the agent's runner.
    #[cfg(feature = "agent")]
    pub fn register_services(&self, runner: &mut sandpolis_instance::service::ServiceRunner) {
        runner.register(sandpolis_agent::CollectorService::new(
            self.systemd.clone(),
            "Health",
            "systemd",
            "Collects the state of the host's systemd units",
            std::time::Duration::from_secs(30),
        ));
    }
}
