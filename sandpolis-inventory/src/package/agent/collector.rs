use super::PackageManager as PackageManagerTrait;
use super::apt::Apt;
use super::nix::Nix;
use super::pacman::Pacman;
use crate::package::{PackageData, PackageDataKey, PackageManager as PM};
use anyhow::Result;
use sandpolis_agent::Collector;
use sandpolis_instance::InstanceId;
use sandpolis_instance::database::{Data, DataCondition, RealmDatabase, ResidentVec};
use tracing::{debug, trace, warn};

/// A concrete package manager detected on this host.
enum Manager {
    Apt(Apt),
    Nix(Nix),
    Pacman(Pacman),
}

impl Manager {
    /// Detect every available package manager on the system. A host can have
    /// more than one (e.g. nix profiles alongside apt).
    fn detect_all() -> Vec<Self> {
        let mut managers = Vec::new();
        if let Some(m) = Pacman::is_available()
            .then(Pacman::new)
            .and_then(|m| m.ok())
        {
            managers.push(Manager::Pacman(m));
        }
        if let Some(m) = Apt::is_available().then(Apt::new).and_then(|m| m.ok()) {
            managers.push(Manager::Apt(m));
        }
        if let Some(m) = Nix::is_available().then(Nix::new).and_then(|m| m.ok()) {
            managers.push(Manager::Nix(m));
        }
        managers
    }

    fn kind(&self) -> PM {
        match self {
            Manager::Apt(_) => PM::Apt,
            Manager::Nix(_) => PM::Nix,
            Manager::Pacman(_) => PM::Pacman,
        }
    }

    async fn get_installed(&self, instance: InstanceId) -> Result<Vec<PackageData>> {
        match self {
            Manager::Apt(m) => m.get_installed(instance).await,
            Manager::Nix(m) => m.get_installed(instance).await,
            Manager::Pacman(m) => m.get_installed(instance).await,
        }
    }

    async fn get_latest_available(
        &self,
        packages: &mut [PackageData],
        instance: InstanceId,
    ) -> Result<()> {
        match self {
            Manager::Apt(m) => m.get_latest_available(packages, instance).await,
            Manager::Nix(m) => m.get_latest_available(packages, instance).await,
            Manager::Pacman(m) => m.get_latest_available(packages, instance).await,
        }
    }
}

/// Collects installed packages from the host's package manager into the
/// database, scoped by instance. Mirrors the memory/user collectors.
pub struct PackageCollector {
    data: ResidentVec<PackageData>,
    managers: Vec<Manager>,
    instance_id: InstanceId,
}

impl PackageCollector {
    pub fn new(db: RealmDatabase, instance_id: InstanceId) -> Result<Self> {
        Ok(Self {
            data: db.resident_vec(DataCondition::equal(
                PackageDataKey::_instance_id,
                instance_id,
            ))?,
            managers: Manager::detect_all(),
            instance_id,
        })
    }
}

impl Collector for PackageCollector {
    async fn refresh(&mut self) -> Result<()> {
        if self.managers.is_empty() {
            trace!("No supported package manager detected");
        }

        // A failing manager must not block the others, and its packages must
        // not be pruned below on the strength of a listing that never arrived.
        let mut installed: Vec<PackageData> = Vec::new();
        let mut failed: Vec<PM> = Vec::new();
        for manager in &self.managers {
            match manager.get_installed(self.instance_id).await {
                Ok(mut packages) => {
                    debug!(
                        manager = ?manager.kind(),
                        count = packages.len(),
                        "Collected installed packages"
                    );
                    if let Err(error) = manager.get_latest_available(&mut packages, self.instance_id).await
                    {
                        warn!(
                            manager = ?manager.kind(),
                            %error,
                            "Failed to look up latest available package versions"
                        );
                    }
                    installed.extend(packages);
                }
                Err(error) => {
                    warn!(manager = ?manager.kind(), %error, "Failed to list installed packages");
                    failed.push(manager.kind());
                }
            }
        }

        // Update existing packages or add newly-installed ones.
        'next: for mut pkg in installed.iter().cloned() {
            pkg._instance_id = self.instance_id;
            for resident in self.data.iter() {
                let existing = resident.read();
                if existing.name == pkg.name && existing.manager == pkg.manager {
                    drop(existing);
                    resident.update(|p| {
                        p.version = pkg.version.clone();
                        p.latest_available = pkg.latest_available.clone();
                        p.repository = pkg.repository.clone();
                        Ok(())
                    })?;
                    continue 'next;
                }
            }
            self.data.push(pkg)?;
        }

        // Remove packages that are no longer installed. A manager that isn't
        // detected anymore counts as an empty listing; one whose listing
        // failed keeps its packages untouched.
        let live: Vec<(String, PM)> = installed
            .iter()
            .map(|p| (p.name.clone(), p.manager.clone()))
            .collect();
        let stale: Vec<_> = self
            .data
            .iter()
            .filter(|r| {
                let pkg = r.read();
                !failed.contains(&pkg.manager)
                    && !live.contains(&(pkg.name.clone(), pkg.manager.clone()))
            })
            .map(|r| r.read().id())
            .collect();
        for id in stale {
            self.data.remove(id)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandpolis_instance::database::DatabaseManager;
    use sandpolis_instance::realm::RealmName;
    use sandpolis_instance::test_db;

    #[tokio::test]
    #[test_log::test]
    #[ignore = "runs the host's real package manager"]
    async fn test_package_collector() -> Result<()> {
        let database: DatabaseManager = test_db!(PackageData);

        let instance_id = sandpolis_instance::ServerId::random().into();
        let mut collector =
            PackageCollector::new(database.realm(RealmName::default())?, instance_id)?;
        collector.refresh().await?;

        for pkg in collector.data.iter() {
            assert_eq!(pkg.read()._instance_id, instance_id);
        }
        Ok(())
    }
}
