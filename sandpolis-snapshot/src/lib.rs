//! Cold snapshots: block-by-block partition images stored on the server.
//!
//! ## Create
//!
//! The server opens a [`streams::SnapshotCreateStream`] toward the agent. The
//! agent reads the partition sequentially, hashing every block and streaming
//! the hashes up in batches. The server compares each batch against the
//! reconstructed previous snapshot (or an all-zero image when this is the
//! first) and replies with the offsets whose blocks it needs; the agent uploads
//! those blocks zstd-compressed. When the whole partition has been scanned and
//! every requested block received, the server commits the staged image as a new
//! qcow2 layer.
//!
//! ## Apply
//!
//! The same exchange in reverse: the agent hashes and streams its current
//! blocks, and the server replies with the blocks that differ from the chosen
//! snapshot, which the agent writes back to the partition.
//!
//! Snapshot content never enters the realm database; the models here are
//! metadata only. The qcow2 layers live under the server's `--data` directory.

use anyhow::Result;
use native_db::ToKey;
use native_model::Model;
use sandpolis_instance::InstanceId;
use sandpolis_instance::InstanceManager;
use sandpolis_instance::database::DatabaseManager;
use sandpolis_macros::data;
use serde::{Deserialize, Serialize};

pub mod streams;

#[cfg(feature = "uki")]
pub mod agent;

#[cfg(feature = "uki")]
pub mod boot_snapshot;

#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "client")]
pub mod client;

#[cfg(all(feature = "client", not(target_os = "android")))]
pub mod cli;

sandpolis_instance::register_layer!("Snapshot", "Cold partition snapshots");

/// Size of the transfer unit in bytes. Recorded per snapshot so the value can
/// change without invalidating existing chains.
pub const SNAPSHOT_BLOCK_SIZE: u64 = 1 << 20;

/// How many block hashes travel in one stream message.
pub const HASHES_PER_MESSAGE: usize = 256;

/// Whether a UUID received over the wire is safe to use as a path component
/// (device links on the agent, storage directories on the server).
pub fn valid_uuid(uuid: &str) -> bool {
    !uuid.is_empty() && uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Wipe free space on the given filesystem by filling it with zeros, which can
/// significantly reduce the size of subsequent snapshots.
///
/// This belongs to the *regular* agent: it must run while the filesystem is
/// still mounted, before the machine reboots into a cold snapshot. Don't use it
/// with software-based encryption schemes — the zeros are encrypted into
/// incompressible noise, making snapshots larger rather than smaller.
///
/// Not yet wired into any stream or service.
#[cfg(all(feature = "agent", not(feature = "uki")))]
#[allow(dead_code)]
pub async fn wipe_free<P>(path: P) -> Result<()>
where
    P: AsRef<std::path::Path>,
{
    use tokio::io::AsyncWriteExt;

    let path = path.as_ref().join(".blank");
    let mut file = tokio::fs::File::create(&path).await?;
    let zeros = vec![0u8; SNAPSHOT_BLOCK_SIZE as usize];

    // Fill until the filesystem refuses more, then release it all.
    while file.write_all(&zeros).await.is_ok() {}

    file.sync_all().await?;
    drop(file);
    tokio::fs::remove_file(&path).await?;
    Ok(())
}

/// One stored snapshot of one partition. Written by the server that holds the
/// qcow2 layer, scoped to the agent the partition belongs to.
#[data(instance, defaults)]
pub struct SnapshotData {
    /// UUID of the partition this snapshot captured
    #[secondary_key]
    pub partition_uuid: String,

    /// The snapshot's own UUID, which is also the qcow2 file stem on the server
    #[secondary_key]
    pub uuid: String,

    /// UUID of the snapshot this layer is backed by; `None` for the base image
    pub parent: Option<String>,

    /// Partition size in bytes at capture time
    pub size: u64,

    /// Transfer block size the capture used
    pub block_size: u64,

    /// Bytes the compressed qcow2 layer occupies on the server
    pub stored_size: u64,

    /// Optional human-readable label
    pub label: Option<String>,
}

/// Which way the blocks flow.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SnapshotDirection {
    /// Partition -> server
    #[default]
    Create,
    /// Server -> partition
    Apply,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SnapshotOperationState {
    /// The server is reconstructing the comparison image
    #[default]
    Preparing,
    /// Blocks are being hashed and transferred
    Transferring,
    /// The server is committing the new qcow2 layer
    Committing,
    Complete,
    Failed,
}

impl SnapshotOperationState {
    /// Whether the operation is still running.
    pub fn active(&self) -> bool {
        !matches!(self, Self::Complete | Self::Failed)
    }
}

/// A snapshot operation in progress (or recently finished), written by the
/// server driving it. The client GUI renders these as progress bars and link
/// activity.
#[data(instance, defaults)]
pub struct SnapshotOperationData {
    /// UUID of the partition being captured or restored
    #[secondary_key]
    pub partition_uuid: String,

    pub direction: SnapshotDirection,

    pub state: SnapshotOperationState,

    /// Fraction of the partition scanned so far (0.0..=1.0)
    pub progress: f32,

    /// Block bytes actually transferred (before compression)
    pub bytes_transferred: u64,

    /// Total partition bytes
    pub total_bytes: u64,

    /// Why the operation failed, when it did
    pub error: Option<String>,
}

inventory::submit! {
    sandpolis_instance::database::sync::SyncRegistration(|r| {
        r.register_scoped::<SnapshotData>(|d| d._instance_id)
    })
}

inventory::submit! {
    sandpolis_instance::database::sync::SyncRegistration(|r| {
        r.register_scoped::<SnapshotOperationData>(|d| d._instance_id)
    })
}

#[derive(Clone)]
pub struct SnapshotManager {
    #[allow(dead_code)]
    database: DatabaseManager,
    #[allow(dead_code)]
    pub instance_id: InstanceId,
}

impl SnapshotManager {
    pub async fn new(database: DatabaseManager, instance: InstanceManager) -> Result<Self> {
        Ok(Self {
            instance_id: instance.instance_id,
            database,
        })
    }

    /// Give the server-side responder its storage and network context. Called
    /// once at server startup.
    #[cfg(feature = "server")]
    pub fn install_server(&self, context: server::SnapshotServerContext) {
        server::install(context);
    }
}

/// Static handler for registering the boot agent's block stream responders.
/// Cold snapshots only run in the UKI boot environment, so regular agents
/// don't answer these streams at all.
#[cfg(feature = "uki")]
pub struct SnapshotAgentResponderRegistration;

#[cfg(feature = "uki")]
impl sandpolis_instance::network::RegisterResponders for SnapshotAgentResponderRegistration {
    fn register_responders(&self, registry: &sandpolis_instance::network::StreamRegistry) {
        registry.register_responder(agent::SnapshotCreateStreamResponder::default);
        registry.register_responder(agent::SnapshotApplyStreamResponder::default);
    }
}

#[cfg(feature = "uki")]
inventory::submit!(sandpolis_instance::network::ResponderRegistration(
    &SnapshotAgentResponderRegistration
));

/// Static handler for registering the server's management responder.
#[cfg(feature = "server")]
pub struct SnapshotServerResponderRegistration;

#[cfg(feature = "server")]
impl sandpolis_instance::network::RegisterResponders for SnapshotServerResponderRegistration {
    fn register_responders(&self, registry: &sandpolis_instance::network::StreamRegistry) {
        registry.register_responder(server::SnapshotMgmtStreamResponder::default);
    }
}

#[cfg(feature = "server")]
inventory::submit!(sandpolis_instance::network::ResponderRegistration(
    &SnapshotServerResponderRegistration
));

// What a client must be granted to manage snapshots. The block streams have no
// permission declaration on purpose: undeclared tags fail closed, so only
// servers (whose connections are not gated) can open them toward agents.
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(SnapshotMgmtStream), "snapshot:manage")
}
