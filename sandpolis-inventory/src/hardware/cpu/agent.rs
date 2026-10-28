use super::{CpuCoreData, CpuCoreDataKey, CpuData, CpuDataKey};
use crate::HISTORY_RETENTION;
use anyhow::Result;
use sandpolis_agent::Collector;
use sandpolis_instance::InstanceId;
use sandpolis_instance::database::{
    Data, DataCondition, DataExpiration, RealmDatabase, Resident, ResidentVec,
};
use sysinfo::{CpuRefreshKind, RefreshKind, System};
use tracing::trace;

/// Polls per-core CPU utilization and frequency, plus the model/vendor strings
/// that don't change but have to come from somewhere.
pub struct CpuCollector {
    system: System,
    cpu: Resident<CpuData>,
    cores: ResidentVec<CpuCoreData>,
    instance_id: InstanceId,
}

impl CpuCollector {
    pub fn new(db: RealmDatabase, instance_id: InstanceId) -> Result<Self> {
        Ok(Self {
            system: System::new_with_specifics(
                RefreshKind::nothing().with_cpu(CpuRefreshKind::everything()),
            ),
            cpu: db.resident_with(
                DataCondition::equal(CpuDataKey::_instance_id, instance_id),
                || CpuData::scoped(instance_id),
            )?,
            cores: db.resident_vec(DataCondition::equal(
                CpuCoreDataKey::_instance_id,
                instance_id,
            ))?,
            instance_id,
        })
    }
}

impl Collector for CpuCollector {
    async fn refresh(&mut self) -> Result<()> {
        self.system.refresh_cpu_all();
        trace!(cores = self.system.cpus().len(), "Polled CPU info");

        // sysinfo reports the model on every core; the first one is the whole
        // package as far as this subsystem is concerned.
        if let Some(first) = self.system.cpus().first() {
            let model = first.brand().trim().to_string();
            let vendor = first.vendor_id().trim().to_string();
            self.cpu.update(|data| {
                data._instance_id = self.instance_id;
                data.model = (!model.is_empty()).then(|| model.clone());
                data.vendor = (!vendor.is_empty()).then(|| vendor.clone());
                Ok(())
            })?;
        }

        'next_core: for (index, cpu) in self.system.cpus().iter().enumerate() {
            let index = index as u32;
            // `cpu_usage` is a percentage; the model stores a 0.0..=1.0 fraction
            // so a gauge can use it without knowing where it came from.
            let usage = (cpu.cpu_usage() as f64 / 100.0).clamp(0.0, 1.0);
            // sysinfo reports MHz.
            let frequency = cpu.frequency().saturating_mul(1_000_000);

            for resident in self.cores.iter() {
                if resident.read().index == index {
                    resident.update(|core| {
                        core.usage = usage;
                        core.frequency = frequency;
                        // Superseded readings become the usage history the
                        // client charts. Restamping the expiration means even
                        // an identical reading writes a revision, which is the
                        // point: history has no gaps while the agent is up.
                        core._expiration = DataExpiration::after(HISTORY_RETENTION);
                        Ok(())
                    })?;
                    continue 'next_core;
                }
            }

            self.cores.push(CpuCoreData {
                index,
                usage,
                frequency,
                ..CpuCoreData::scoped(self.instance_id)
            })?;
        }

        // Cores don't come and go on most hosts, but they do on a VM that was
        // resized under us.
        let live = self.system.cpus().len() as u32;
        let stale: Vec<_> = self
            .cores
            .iter()
            .filter(|core| core.read().index >= live)
            .map(|core| core.read().id())
            .collect();
        for id in stale {
            self.cores.remove(id)?;
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
    async fn test_cpu_collector() -> Result<()> {
        let database: DatabaseManager = test_db!(CpuData, CpuCoreData);

        let instance_id = sandpolis_instance::ServerId::random().into();
        let mut collector = CpuCollector::new(database.realm(RealmName::default())?, instance_id)?;
        collector.refresh().await?;

        assert!(collector.cores.iter().count() > 0, "no cores reported");
        for core in collector.cores.iter() {
            let core = core.read();
            assert_eq!(core._instance_id, instance_id);
            // A usage outside this range means the percentage/fraction
            // conversion drifted, which a gauge would render as a full bar.
            assert!(
                (0.0..=1.0).contains(&core.usage),
                "usage out of range: {}",
                core.usage
            );
        }
        Ok(())
    }

    /// A process can hold other instances' readings too (a server built with
    /// the agent feature replicates its agents'). The collector must neither
    /// trip over them nor treat them as its own.
    #[tokio::test]
    #[test_log::test]
    async fn test_cpu_collector_ignores_other_instances() -> Result<()> {
        let database: DatabaseManager = test_db!(CpuData, CpuCoreData);
        let realm = database.realm(RealmName::default())?;

        let other = sandpolis_instance::AgentId::random().into();
        let foreign_core = CpuCoreData {
            // Beyond any real core count, so it would look stale if it were ours.
            index: u32::MAX,
            ..CpuCoreData::scoped(other)
        };
        let rw = realm.local_write()?;
        rw.insert(CpuData::scoped(other))?;
        rw.insert(foreign_core.clone())?;
        rw.commit()?;

        let instance_id = sandpolis_instance::ServerId::random().into();
        let mut collector = CpuCollector::new(realm.clone(), instance_id)?;
        collector.refresh().await?;

        assert_eq!(collector.cpu.read()._instance_id, instance_id);
        assert!(
            collector
                .cores
                .iter()
                .all(|core| core.read()._instance_id == instance_id)
        );

        let r = realm.r_transaction()?;
        let stored: Option<CpuCoreData> = r.get().primary(foreign_core.id())?;
        assert!(stored.is_some(), "another instance's core was removed");
        Ok(())
    }
}
