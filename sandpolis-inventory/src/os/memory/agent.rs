use super::{MemoryData, MemoryDataKey};
use crate::HISTORY_RETENTION;
use anyhow::Result;
use sandpolis_agent::Collector;
use sandpolis_instance::InstanceId;
use sandpolis_instance::database::{DataCondition, DataExpiration, RealmDatabase, Resident};
use sysinfo::{MemoryRefreshKind, RefreshKind, System};
use tracing::trace;

pub struct MemoryMonitor {
    system: System,
    data: Resident<MemoryData>,
    instance_id: InstanceId,
}

impl MemoryMonitor {
    pub fn new(db: RealmDatabase, instance_id: InstanceId) -> Result<Self> {
        Ok(Self {
            system: System::new_with_specifics(
                RefreshKind::nothing().with_memory(MemoryRefreshKind::everything()),
            )?,
            data: db.resident_with(
                DataCondition::equal(MemoryDataKey::_instance_id, instance_id),
                || MemoryData::scoped(instance_id),
            )?,
            instance_id,
        })
    }
}

impl Collector for MemoryMonitor {
    async fn refresh(&mut self) -> Result<()> {
        self.system.refresh_memory();
        trace!(info = ?self.system, "Polled memory info");

        self.data.update(|data| {
            data._instance_id = self.instance_id;
            data.total = self.system.total_memory();
            data.free = self.system.free_memory();
            data.swap_total = self.system.total_swap();
            data.swap_free = self.system.free_swap();
            // Superseded readings become the usage history the client charts.
            // Restamping the expiration means even an identical reading writes
            // a revision, which is the point: history has no gaps while the
            // agent is up.
            data._expiration = DataExpiration::after(HISTORY_RETENTION);
            Ok(())
        })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandpolis_instance::database::{DataRevision, DatabaseManager};
    use sandpolis_instance::realm::RealmName;
    use sandpolis_instance::test_db;

    #[tokio::test]
    #[test_log::test]
    async fn test_memory_monitor() -> Result<()> {
        let database: DatabaseManager = test_db!(MemoryData);

        let instance_id = sandpolis_instance::ServerId::random().into();
        let mut monitor = MemoryMonitor::new(database.realm(RealmName::default())?, instance_id)?;
        monitor.refresh().await?;

        assert!(monitor.data.read().total > 0);
        assert_eq!(monitor.data.read()._instance_id, instance_id);
        Ok(())
    }

    /// Another instance's reading in the same table (a server built with the
    /// agent feature replicates its agents') is not this monitor's singleton.
    #[tokio::test]
    #[test_log::test]
    async fn test_memory_monitor_ignores_other_instances() -> Result<()> {
        let database: DatabaseManager = test_db!(MemoryData);
        let realm = database.realm(RealmName::default())?;

        let other = sandpolis_instance::AgentId::random().into();
        let rw = realm.local_write()?;
        rw.insert(MemoryData::scoped(other))?;
        rw.commit()?;

        let instance_id = sandpolis_instance::ServerId::random().into();
        let mut monitor = MemoryMonitor::new(realm.clone(), instance_id)?;
        monitor.refresh().await?;

        assert_eq!(monitor.data.read()._instance_id, instance_id);

        // Both instances still have a current reading of their own.
        let r = realm.r_transaction()?;
        let latest: Vec<InstanceId> = r
            .scan()
            .primary::<MemoryData>()?
            .all()?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|row| matches!(row._revision, DataRevision::Latest(_)))
            .map(|row| row._instance_id)
            .collect();
        assert!(latest.contains(&other), "another instance's reading was taken over");
        assert!(latest.contains(&instance_id));
        Ok(())
    }
}
