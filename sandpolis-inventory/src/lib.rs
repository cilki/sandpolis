use anyhow::Result;
use native_db::ToKey;
use native_model::Model;
use sandpolis_instance::InstanceManager;
use sandpolis_instance::database::{DatabaseManager, Resident};
use sandpolis_instance::realm::RealmName;
use sandpolis_macros::data;
#[cfg(feature = "agent")]
use std::sync::Arc;

#[cfg(feature = "client")]
pub mod client;

pub mod applications;
pub mod config;
pub mod cve;
pub mod hardware;
pub mod os;
pub mod package;

sandpolis_instance::register_layer!("Inventory", "Hardware and software inventory");

#[cfg(feature = "agent")]
pub(crate) mod sysfs;
// Version comparison serves the package collector, the server's CVE matching,
// and the client's package history — none of which exist in a uki build.
#[cfg(any(
    feature = "server",
    feature = "client",
    all(feature = "agent", not(feature = "uki"))
))]
pub(crate) mod version;

#[data]
#[derive(Default)]
pub struct InventoryManagerData {}

/// How long superseded revisions of fast-changing readings (CPU usage, memory)
/// are kept, which bounds how far back the client's history charts can reach.
pub const HISTORY_RETENTION: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// How often agents poll CPU utilization. The client also buckets replicated
/// core readings by this interval to reassemble per-pass averages.
pub const CPU_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

#[cfg(feature = "agent")]
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct InventoryManager {
    #[allow(dead_code)]
    data: Resident<InventoryManagerData>,
    #[cfg(feature = "server")]
    realm: sandpolis_instance::database::RealmDatabase,
    #[cfg(feature = "agent")]
    pub memory: Arc<Mutex<os::memory::agent::MemoryMonitor>>,
    #[cfg(feature = "agent")]
    pub cpu: Arc<Mutex<hardware::cpu::agent::CpuCollector>>,
    #[cfg(all(feature = "agent", not(feature = "uki")))]
    pub mountpoints: Arc<Mutex<os::mountpoint::agent::MountpointCollector>>,
    #[cfg(all(feature = "agent", not(feature = "uki")))]
    pub users: Arc<Mutex<os::user::agent::UserCollector>>,
    #[cfg(all(feature = "agent", not(feature = "uki")))]
    pub packages: Arc<Mutex<package::agent::PackageCollector>>,
    #[cfg(feature = "agent")]
    pub partitions: Arc<Mutex<hardware::disk::partition::agent::PartitionCollector>>,
    #[cfg(feature = "agent")]
    pub block_devices: Arc<Mutex<os::block_device::agent::BlockDeviceCollector>>,
}

impl InventoryManager {
    #[cfg_attr(not(feature = "agent"), allow(unused_variables))]
    pub async fn new(database: DatabaseManager, instance: InstanceManager) -> Result<Self> {
        Ok(Self {
            #[cfg(feature = "agent")]
            memory: Arc::new(Mutex::new(os::memory::agent::MemoryMonitor::new(
                database.realm(RealmName::default())?,
                instance.instance_id,
            )?)),
            #[cfg(feature = "agent")]
            cpu: Arc::new(Mutex::new(hardware::cpu::agent::CpuCollector::new(
                database.realm(RealmName::default())?,
                instance.instance_id,
            )?)),
            #[cfg(all(feature = "agent", not(feature = "uki")))]
            mountpoints: Arc::new(Mutex::new(os::mountpoint::agent::MountpointCollector::new(
                database.realm(RealmName::default())?,
                instance.instance_id,
            )?)),
            #[cfg(all(feature = "agent", not(feature = "uki")))]
            users: Arc::new(Mutex::new(os::user::agent::UserCollector::new(
                database.realm(RealmName::default())?,
                instance.instance_id,
            )?)),
            #[cfg(all(feature = "agent", not(feature = "uki")))]
            packages: Arc::new(Mutex::new(package::agent::PackageCollector::new(
                database.realm(RealmName::default())?,
                instance.instance_id,
            )?)),
            #[cfg(feature = "agent")]
            partitions: Arc::new(Mutex::new(
                hardware::disk::partition::agent::PartitionCollector::new(
                    database.realm(RealmName::default())?,
                    instance.instance_id,
                )?,
            )),
            #[cfg(feature = "agent")]
            block_devices: Arc::new(Mutex::new(
                os::block_device::agent::BlockDeviceCollector::new(
                    database.realm(RealmName::default())?,
                    instance.instance_id,
                )?,
            )),
            #[cfg(feature = "server")]
            realm: database.realm(RealmName::default())?,
            data: database.realm(RealmName::default())?.resident(())?,
        })
    }

    /// Add the subsystem's background services to the server's runner.
    ///
    /// `owned` decides which instances' data this server currently owns, so a
    /// local stratum server only matches (and writes findings for) its own
    /// agents. The config flag is a coarse startup switch: a disabled service
    /// is never registered and doesn't appear in the client at all.
    #[cfg(feature = "server")]
    pub fn register_server_services(
        &self,
        config: &config::InventoryManagerConfig,
        data_dir: std::path::PathBuf,
        owned: cve::server::OwnedFn,
        runner: &mut sandpolis_instance::service::ServiceRunner,
    ) -> Result<()> {
        if !config.cve.enabled {
            tracing::info!("CVE matching is disabled");
            return Ok(());
        }

        runner.register(cve::CveService::new(
            self.realm.clone(),
            data_dir,
            &config.cve,
            owned,
        )?);
        Ok(())
    }

    /// Add the subsystem's background services to the agent's runner.
    ///
    /// Packages get their own longer interval because enumerating them is
    /// expensive and they change rarely, unlike memory and users. CPU is the
    /// other way around: utilization averaged over half a minute says nothing,
    /// so it polls fast enough to still be a live reading.
    #[cfg(feature = "agent")]
    pub fn register_services(&self, runner: &mut sandpolis_instance::service::ServiceRunner) {
        use sandpolis_agent::CollectorService;
        use std::time::Duration;

        runner.register(CollectorService::new(
            self.memory.clone(),
            "Inventory",
            "memory",
            "Collects the host's memory usage",
            Duration::from_secs(30),
        ));
        runner.register(CollectorService::new(
            self.cpu.clone(),
            "Inventory",
            "cpu",
            "Collects the host's per-core CPU utilization",
            CPU_POLL_INTERVAL,
        ));
        // The OS-level collectors only exist on regular agents: the UKI boot
        // environment has no mounts, users, or packages worth reporting.
        #[cfg(not(feature = "uki"))]
        {
            runner.register(CollectorService::new(
                self.mountpoints.clone(),
                "Inventory",
                "mountpoints",
                "Collects the host's mounted filesystems and their capacity",
                Duration::from_secs(60),
            ));
            runner.register(CollectorService::new(
                self.users.clone(),
                "Inventory",
                "users",
                "Collects the host's user accounts",
                Duration::from_secs(30),
            ));
            runner.register(CollectorService::new(
                self.packages.clone(),
                "Inventory",
                "packages",
                "Collects the host's installed packages",
                Duration::from_secs(300),
            ));
        }
        runner.register(CollectorService::new(
            self.partitions.clone(),
            "Inventory",
            "partitions",
            "Collects the host's disk partitions",
            Duration::from_secs(60),
        ));
        runner.register(CollectorService::new(
            self.block_devices.clone(),
            "Inventory",
            "block-devices",
            "Collects the host's block devices",
            Duration::from_secs(60),
        ));
    }
}
