use super::{MountpointData, MountpointDataKey};
use anyhow::Result;
use sandpolis_agent::Collector;
use sandpolis_instance::InstanceId;
use sandpolis_instance::database::{Data, DataCondition, RealmDatabase, ResidentVec};
use sysinfo::Disks;
use tracing::trace;

/// Block size the collected capacities are expressed in.
///
/// `sysinfo` reports bytes, but the model is block-based (it predates this
/// collector and matches what `statvfs` hands back), so a block is a byte here
/// and the counts carry through unscaled.
const BLOCK_SIZE: u64 = 1;

/// Polls mounted filesystems and their capacity.
pub struct MountpointCollector {
    disks: Disks,
    data: ResidentVec<MountpointData>,
    instance_id: InstanceId,
}

impl MountpointCollector {
    pub fn new(db: RealmDatabase, instance_id: InstanceId) -> Result<Self> {
        Ok(Self {
            disks: Disks::new()?,
            data: db.resident_vec(DataCondition::equal(
                MountpointDataKey::_instance_id,
                instance_id,
            ))?,
            instance_id,
        })
    }
}

impl Collector for MountpointCollector {
    async fn refresh(&mut self) -> Result<()> {
        self.disks.refresh(true);
        trace!(mounts = self.disks.len(), "Polled mountpoints");

        let mut live: Vec<String> = Vec::new();

        'next_disk: for disk in &self.disks {
            let path = disk.mount_point().to_string_lossy().to_string();
            let device = disk.name().to_string_lossy().to_string();
            let kind = disk.file_system().to_string_lossy().to_string();
            let total = disk.total_space();
            let available = disk.available_space();
            live.push(path.clone());

            for resident in self.data.iter() {
                if resident.read().path == path {
                    resident.update(|mount| {
                        mount.mounted = true;
                        mount.device = device.clone();
                        mount.r#type = kind.clone();
                        mount.blocks_size = BLOCK_SIZE;
                        mount.blocks = total;
                        mount.blocks_free = available;
                        mount.blocks_available = available;
                        Ok(())
                    })?;
                    continue 'next_disk;
                }
            }

            self.data.push(MountpointData {
                mounted: true,
                device,
                device_alias: String::new(),
                path,
                r#type: kind,
                blocks_size: BLOCK_SIZE,
                blocks: total,
                blocks_free: available,
                blocks_available: available,
                ..MountpointData::scoped(self.instance_id)
            })?;
        }

        // A filesystem that was unmounted between polls stops being a row rather
        // than lingering as a stale capacity nobody can act on.
        let stale: Vec<_> = self
            .data
            .iter()
            .filter(|mount| !live.contains(&mount.read().path))
            .map(|mount| mount.read().id())
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
    async fn test_mountpoint_collector() -> Result<()> {
        let database: DatabaseManager = test_db!(MountpointData);

        let instance_id = sandpolis_instance::ServerId::random().into();
        let mut collector =
            MountpointCollector::new(database.realm(RealmName::default())?, instance_id)?;
        collector.refresh().await?;

        for mount in collector.data.iter() {
            let mount = mount.read();
            assert_eq!(mount._instance_id, instance_id);
            assert!(!mount.path.is_empty());
            // Used can never exceed total, which is the invariant the storage
            // gauge relies on to stay inside its track.
            assert!(mount.used_bytes() <= mount.total_bytes());
        }
        Ok(())
    }
}
