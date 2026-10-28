use super::{PartitionData, PartitionDataKey};
use crate::sysfs;
use anyhow::Result;
use sandpolis_agent::Collector;
use sandpolis_instance::InstanceId;
use sandpolis_instance::database::{Data, DataCondition, RealmDatabase, ResidentVec};
use tracing::trace;

/// Polls the host's disk partitions out of sysfs, with partition UUIDs from the
/// udev symlinks under `/dev/disk/by-partuuid` and mountpoints from
/// `/proc/mounts`.
pub struct PartitionCollector {
    data: ResidentVec<PartitionData>,
    instance_id: InstanceId,
}

impl PartitionCollector {
    pub fn new(db: RealmDatabase, instance_id: InstanceId) -> Result<Self> {
        Ok(Self {
            data: db.resident_vec(DataCondition::equal(
                PartitionDataKey::_instance_id,
                instance_id,
            ))?,
            instance_id,
        })
    }

    /// Enumerate partitions: every subdirectory of `/sys/block/<disk>` that
    /// carries a `partition` attribute is one.
    fn scan(instance: InstanceId) -> Vec<PartitionData> {
        let uuids = sysfs::partuuid_map();
        let mounts = sysfs::mount_map();
        let mut partitions = Vec::new();

        let Ok(disks) = std::fs::read_dir("/sys/block") else {
            return partitions;
        };
        for disk in disks.flatten() {
            let disk_name = disk.file_name().to_string_lossy().to_string();
            let Ok(children) = std::fs::read_dir(disk.path()) else {
                continue;
            };
            for child in children.flatten() {
                if !child.path().join("partition").is_file() {
                    continue;
                }
                let name = child.file_name().to_string_lossy().to_string();
                let device = format!("/dev/{name}");
                let (major, minor) = sysfs::read_trimmed(child.path().join("dev"))
                    .and_then(|dev| {
                        let (major, minor) = dev.split_once(':')?;
                        Some((major.parse().ok()?, minor.parse().ok()?))
                    })
                    .unwrap_or_default();

                partitions.push(PartitionData {
                    identification: device.clone(),
                    description: disk_name.clone(),
                    uuid: uuids.get(&name).cloned().unwrap_or_default(),
                    // The sysfs size attribute counts 512-byte sectors
                    // regardless of the device's logical block size.
                    size: sysfs::read_u64(child.path().join("size")).unwrap_or(0) * 512,
                    major,
                    minor,
                    mount: mounts.get(&device).cloned().unwrap_or_default(),
                    name,
                    ..PartitionData::scoped(instance)
                });
            }
        }
        partitions
    }
}

impl Collector for PartitionCollector {
    async fn refresh(&mut self) -> Result<()> {
        let partitions = Self::scan(self.instance_id);
        trace!(partitions = partitions.len(), "Polled disk partitions");

        let mut live: Vec<String> = Vec::new();
        'next_partition: for partition in partitions {
            live.push(partition.name.clone());

            for resident in self.data.iter() {
                if resident.read().name == partition.name {
                    resident.update(|row| {
                        row.identification = partition.identification.clone();
                        row.description = partition.description.clone();
                        row.uuid = partition.uuid.clone();
                        row.size = partition.size;
                        row.major = partition.major;
                        row.minor = partition.minor;
                        row.mount = partition.mount.clone();
                        Ok(())
                    })?;
                    continue 'next_partition;
                }
            }

            self.data.push(PartitionData {
                _instance_id: self.instance_id,
                ..partition
            })?;
        }

        // A partition that disappeared (repartitioned, device unplugged) stops
        // being a row rather than lingering as a stale snapshot target.
        let stale: Vec<_> = self
            .data
            .iter()
            .filter(|row| !live.contains(&row.read().name))
            .map(|row| row.read().id())
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
    async fn test_partition_collector() -> Result<()> {
        let database: DatabaseManager = test_db!(PartitionData);

        let instance_id = sandpolis_instance::ServerId::random().into();
        let mut collector =
            PartitionCollector::new(database.realm(RealmName::default())?, instance_id)?;
        collector.refresh().await?;

        // The sandbox may expose no partitions at all; assert invariants on
        // whatever was found.
        for row in collector.data.iter() {
            let row = row.read();
            assert_eq!(row._instance_id, instance_id);
            assert!(!row.name.is_empty());
            assert!(row.identification.starts_with("/dev/"));
        }
        Ok(())
    }
}
