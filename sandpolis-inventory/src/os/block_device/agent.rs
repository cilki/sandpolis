use super::{BlockDeviceData, BlockDeviceDataKey};
use crate::sysfs;
use anyhow::Result;
use sandpolis_agent::Collector;
use sandpolis_instance::InstanceId;
use sandpolis_instance::database::{Data, DataCondition, RealmDatabase, ResidentVec};
use tracing::trace;

/// Polls the host's whole block devices (disks) out of sysfs.
pub struct BlockDeviceCollector {
    data: ResidentVec<BlockDeviceData>,
    instance_id: InstanceId,
}

impl BlockDeviceCollector {
    pub fn new(db: RealmDatabase, instance_id: InstanceId) -> Result<Self> {
        Ok(Self {
            data: db.resident_vec(DataCondition::equal(
                BlockDeviceDataKey::_instance_id,
                instance_id,
            ))?,
            instance_id,
        })
    }

    fn scan(instance: InstanceId) -> Vec<BlockDeviceData> {
        let mut devices = Vec::new();
        let Ok(disks) = std::fs::read_dir("/sys/block") else {
            return devices;
        };
        for disk in disks.flatten() {
            let name = disk.file_name().to_string_lossy().to_string();
            // The sysfs size attribute counts 512-byte sectors; the model wants
            // logical blocks.
            let sectors = sysfs::read_u64(disk.path().join("size")).unwrap_or(0);
            if sectors == 0 {
                // Present but empty devices (unpopulated loop/ram slots) are
                // noise, not inventory.
                continue;
            }
            let block_size =
                sysfs::read_u64(disk.path().join("queue/logical_block_size")).unwrap_or(512);

            devices.push(BlockDeviceData {
                vendor: sysfs::read_trimmed(disk.path().join("device/vendor")),
                model: sysfs::read_trimmed(disk.path().join("device/model")).unwrap_or_default(),
                size: (sectors * 512) / block_size.max(1),
                block_size,
                r#type: "disk".to_string(),
                name,
                ..BlockDeviceData::scoped(instance)
            });
        }
        devices
    }
}

impl Collector for BlockDeviceCollector {
    async fn refresh(&mut self) -> Result<()> {
        let devices = Self::scan(self.instance_id);
        trace!(devices = devices.len(), "Polled block devices");

        let mut live: Vec<String> = Vec::new();
        'next_device: for device in devices {
            live.push(device.name.clone());

            for resident in self.data.iter() {
                if resident.read().name == device.name {
                    resident.update(|row| {
                        row.vendor = device.vendor.clone();
                        row.model = device.model.clone();
                        row.size = device.size;
                        row.block_size = device.block_size;
                        Ok(())
                    })?;
                    continue 'next_device;
                }
            }

            self.data.push(BlockDeviceData {
                _instance_id: self.instance_id,
                ..device
            })?;
        }

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
    async fn test_block_device_collector() -> Result<()> {
        let database: DatabaseManager = test_db!(BlockDeviceData);

        let instance_id = sandpolis_instance::ServerId::random().into();
        let mut collector =
            BlockDeviceCollector::new(database.realm(RealmName::default())?, instance_id)?;
        collector.refresh().await?;

        for row in collector.data.iter() {
            let row = row.read();
            assert_eq!(row._instance_id, instance_id);
            assert!(!row.name.is_empty());
            assert!(row.block_size >= 512);
        }
        Ok(())
    }
}
