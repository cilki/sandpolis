//! Client-side access to synced inventory data.
//!
//! Mirrors the health subsystem: a view subscribes to the relevant models when it
//! opens, and reads the records the sync module has replicated into the client's
//! local database.

use crate::cve::VulnerabilityData;
use crate::hardware::cpu::CpuCoreData;
use crate::hardware::disk::partition::PartitionData;
use crate::os::memory::MemoryData;
use crate::os::mountpoint::MountpointData;
use crate::os::user::UserData;
use crate::package::PackageData;
use native_model::Model;
use sandpolis_instance::InstanceId;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(feature = "client")]
pub mod gui;

/// Subscribe to live inventory updates for an instance (call when a view opens).
pub fn subscribe(instance: InstanceId) {
    sandpolis_client::sync::subscribe_all(inventory_model_ids(), Some(instance));
}

/// Unsubscribe from inventory updates for an instance (call when a view closes).
pub fn unsubscribe(instance: InstanceId) {
    sandpolis_client::sync::unsubscribe_all(inventory_model_ids(), Some(instance));
}

fn inventory_model_ids() -> [u32; 7] {
    [
        <MemoryData as Model>::native_model_id(),
        <CpuCoreData as Model>::native_model_id(),
        <MountpointData as Model>::native_model_id(),
        <UserData as Model>::native_model_id(),
        <PackageData as Model>::native_model_id(),
        <VulnerabilityData as Model>::native_model_id(),
        <PartitionData as Model>::native_model_id(),
    ]
}

/// Query the live memory usage for an instance.
pub fn query_memory(id: InstanceId) -> anyhow::Result<Option<MemoryData>> {
    Ok(sandpolis_client::sync::scan_latest::<MemoryData>()?
        .into_iter()
        .find(|m| m._instance_id == id))
}

/// Query the live per-core CPU utilization for an instance, ordered by core.
pub fn query_cpu_cores(id: InstanceId) -> anyhow::Result<Vec<CpuCoreData>> {
    let mut cores: Vec<CpuCoreData> = sandpolis_client::sync::scan_latest::<CpuCoreData>()?
        .into_iter()
        .filter(|core| core._instance_id == id)
        .collect();
    cores.sort_by_key(|core| core.index);
    Ok(cores)
}

/// Query the mean CPU utilization over time for an instance, oldest first.
///
/// The replicated rows are the revision history of every core. Readings from
/// the same collection pass are bucketed by the agent's poll interval and
/// averaged across cores, one point per pass.
pub fn query_cpu_usage_history(id: InstanceId) -> anyhow::Result<Vec<(SystemTime, f32)>> {
    let interval = crate::CPU_POLL_INTERVAL.as_millis() as i64;
    let mut buckets: BTreeMap<i64, (f64, u32)> = BTreeMap::new();
    for core in sandpolis_client::sync::scan_all::<CpuCoreData>()? {
        if core._instance_id != id {
            continue;
        }
        let bucket = core
            ._creation
            .timestamp()
            .timestamp_millis()
            .div_euclid(interval);
        let entry = buckets.entry(bucket).or_insert((0.0, 0));
        entry.0 += core.usage;
        entry.1 += 1;
    }
    Ok(buckets
        .into_iter()
        .map(|(bucket, (sum, count))| {
            let millis = (bucket * interval + interval / 2).max(0) as u64;
            (
                UNIX_EPOCH + Duration::from_millis(millis),
                (sum / count as f64) as f32,
            )
        })
        .collect())
}

/// Query the memory usage fraction over time for an instance, oldest first.
pub fn query_memory_history(id: InstanceId) -> anyhow::Result<Vec<(SystemTime, f32)>> {
    let mut rows: Vec<MemoryData> = sandpolis_client::sync::scan_all::<MemoryData>()?
        .into_iter()
        .filter(|m| m._instance_id == id && m.total > 0)
        .collect();
    rows.sort_by_key(|m| m._creation.timestamp());
    Ok(rows
        .into_iter()
        .map(|m| {
            let used = m.total.saturating_sub(m.free);
            (
                SystemTime::from(m._creation.timestamp()),
                used as f32 / m.total as f32,
            )
        })
        .collect())
}

/// Query the mounted filesystems known for an instance, largest first.
pub fn query_mountpoints(id: InstanceId) -> anyhow::Result<Vec<MountpointData>> {
    let mut mounts: Vec<MountpointData> = sandpolis_client::sync::scan_latest::<MountpointData>()?
        .into_iter()
        .filter(|mount| mount._instance_id == id && mount.mounted && mount.total_bytes() > 0)
        .collect();
    mounts.sort_by_key(|mount| std::cmp::Reverse(mount.total_bytes()));
    Ok(mounts)
}

/// Query the disk partitions known for an instance, ordered by name.
pub fn query_partitions(id: InstanceId) -> anyhow::Result<Vec<PartitionData>> {
    let mut partitions: Vec<PartitionData> =
        sandpolis_client::sync::scan_latest::<PartitionData>()?
            .into_iter()
            .filter(|p| p._instance_id == id)
            .collect();
    partitions.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(partitions)
}

/// Model id of [PartitionData], so other subsystems (snapshot) can subscribe
/// to partition updates without redeclaring the model.
pub fn partition_model_id() -> u32 {
    <PartitionData as Model>::native_model_id()
}

/// Query the user accounts known for an instance.
pub fn query_users(id: InstanceId) -> anyhow::Result<Vec<UserData>> {
    Ok(sandpolis_client::sync::scan_latest::<UserData>()?
        .into_iter()
        .filter(|u| u._instance_id == id)
        .collect())
}

/// Query the installed packages known for an instance.
pub fn query_packages(id: InstanceId) -> anyhow::Result<Vec<PackageData>> {
    Ok(sandpolis_client::sync::scan_latest::<PackageData>()?
        .into_iter()
        .filter(|p| p._instance_id == id)
        .collect())
}

/// Query the known vulnerabilities for an instance, worst first.
pub fn query_vulnerabilities(id: InstanceId) -> anyhow::Result<Vec<VulnerabilityData>> {
    let mut vulnerabilities: Vec<VulnerabilityData> =
        sandpolis_client::sync::scan_latest::<VulnerabilityData>()?
            .into_iter()
            .filter(|v| v._instance_id == id)
            .collect();
    vulnerabilities.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| b.cve_id.cmp(&a.cve_id))
    });
    Ok(vulnerabilities)
}
