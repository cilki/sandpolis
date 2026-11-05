//! Server side of the snapshot subsystem: it stores the qcow2 chains, answers
//! management requests from clients, and drives the block streams toward
//! agents.

pub mod qemu;

use crate::streams::*;
use crate::{
    HASHES_PER_MESSAGE, SNAPSHOT_BLOCK_SIZE, SnapshotData, SnapshotDirection,
    SnapshotOperationData, SnapshotOperationState, valid_uuid,
};
use anyhow::{Context, Result, bail};
use qemu::SnapshotStore;
use sandpolis_instance::InstanceId;
use sandpolis_instance::database::{Data, RealmDatabase, Resident, ResidentVec};
use sandpolis_instance::network::stream::StreamMessage;
use sandpolis_instance::network::{
    InstanceConnection, NetworkManager, StreamRequester, StreamResponder,
};
use sandpolis_macros::Stream;
use std::collections::HashSet;
use std::io::SeekFrom;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc::{Sender, UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::{debug, info, warn};

/// Decides whether this server currently owns an instance's data.
pub type OwnedFn = Arc<dyn Fn(InstanceId) -> bool + Send + Sync>;

/// zstd level for restored blocks sent to agents.
const WIRE_COMPRESSION_LEVEL: i32 = 3;

/// How long a block stream may go silent before the operation is abandoned.
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(300);

/// Minimum time between progress-row updates, so a fast transfer doesn't
/// replicate thousands of revisions.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

pub struct SnapshotServerContext {
    pub store: SnapshotStore,
    pub network: NetworkManager,
    pub owned: OwnedFn,
    /// Stored snapshot metadata, written here and replicated everywhere.
    pub snapshots: ResidentVec<SnapshotData>,
    /// Live operation rows the client GUI renders progress from.
    pub operations: ResidentVec<SnapshotOperationData>,
}

impl SnapshotServerContext {
    pub fn new(
        realm: RealmDatabase,
        store: SnapshotStore,
        network: NetworkManager,
        owned: OwnedFn,
    ) -> Result<Self> {
        Ok(Self {
            snapshots: realm.resident_vec(())?,
            operations: realm.resident_vec(())?,
            store,
            network,
            owned,
        })
    }
}

/// Held in a static so [`SnapshotMgmtStreamResponder`] can be constructed by
/// the stateless `inventory` factory — the deploy subsystem's arrangement.
static CONTEXT: OnceLock<SnapshotServerContext> = OnceLock::new();

/// One operation per (agent, partition) at a time.
static ACTIVE: LazyLock<Mutex<HashSet<(InstanceId, String)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Install the server context. Called once at startup.
pub fn install(context: SnapshotServerContext) {
    tokio::spawn(async {
        match SnapshotStore::check_qemu().await {
            Ok(_version) => info!("Snapshot store ready"),
            Err(e) => warn!(error = %e, "qemu-img is unavailable; snapshot operations will fail"),
        }
    });
    let _ = CONTEXT.set(context);
}

/// The agent's live inbound connection, if it is attached to this server.
fn agent_connection(
    context: &SnapshotServerContext,
    agent: InstanceId,
) -> Option<Arc<InstanceConnection>> {
    context
        .network
        .live_inbound()
        .into_iter()
        .find(|c| c.data.read().remote_instance == Some(agent))
}

/// Server side of the management stream.
#[derive(Stream, Default)]
pub struct SnapshotMgmtStreamResponder;

impl StreamResponder for SnapshotMgmtStreamResponder {
    type In = SnapshotMgmtRequest;
    type Out = SnapshotMgmtResponse;

    async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
        let Some(context) = CONTEXT.get() else {
            sender
                .send(SnapshotMgmtResponse::Failed(
                    "Snapshot storage is not initialized on this server".into(),
                ))
                .await?;
            return Ok(());
        };

        // Only a boot agent (`uki` build) has the block stream responders. A
        // regular agent refuses the stream by closing it, which the drivers
        // report (see `first_event`).
        let (agent, partition_uuid, direction) = match &request {
            SnapshotMgmtRequest::Create {
                agent,
                partition_uuid,
                ..
            } => (
                *agent,
                partition_uuid.clone(),
                Some(SnapshotDirection::Create),
            ),
            SnapshotMgmtRequest::Apply {
                agent,
                partition_uuid,
                snapshot_uuid,
            } => {
                if !valid_uuid(snapshot_uuid) {
                    sender
                        .send(SnapshotMgmtResponse::Failed("Invalid snapshot UUID".into()))
                        .await?;
                    return Ok(());
                }
                (
                    *agent,
                    partition_uuid.clone(),
                    Some(SnapshotDirection::Apply),
                )
            }
            SnapshotMgmtRequest::Delete {
                agent,
                partition_uuid,
                snapshot_uuid,
            } => {
                if !valid_uuid(snapshot_uuid) {
                    sender
                        .send(SnapshotMgmtResponse::Failed("Invalid snapshot UUID".into()))
                        .await?;
                    return Ok(());
                }
                (*agent, partition_uuid.clone(), None)
            }
        };

        if !valid_uuid(&partition_uuid) {
            sender
                .send(SnapshotMgmtResponse::Failed(
                    "Invalid partition UUID".into(),
                ))
                .await?;
            return Ok(());
        }

        // Deletion needs no agent participation.
        let Some(direction) = direction else {
            let SnapshotMgmtRequest::Delete { snapshot_uuid, .. } = request else {
                unreachable!()
            };
            let response = match run_delete(context, agent, &partition_uuid, &snapshot_uuid).await {
                Ok(()) => SnapshotMgmtResponse::Deleted,
                Err(e) => SnapshotMgmtResponse::Failed(e.to_string()),
            };
            sender.send(response).await?;
            return Ok(());
        };

        // TODO forward to the owning stratum server instead of refusing when
        // the agent is attached elsewhere.
        let Some(connection) = agent_connection(context, agent) else {
            sender
                .send(SnapshotMgmtResponse::Failed(
                    "The agent is not attached to this server".into(),
                ))
                .await?;
            return Ok(());
        };
        if !(context.owned)(agent) {
            sender
                .send(SnapshotMgmtResponse::Failed(
                    "This server does not own the agent's data".into(),
                ))
                .await?;
            return Ok(());
        }

        if !ACTIVE
            .lock()
            .unwrap()
            .insert((agent, partition_uuid.clone()))
        {
            sender
                .send(SnapshotMgmtResponse::Failed(
                    "An operation is already running on this partition".into(),
                ))
                .await?;
            return Ok(());
        }

        sender.send(SnapshotMgmtResponse::Started).await?;
        tokio::spawn(async move {
            let operation = new_operation(context, agent, &partition_uuid, direction);
            let result = match request {
                SnapshotMgmtRequest::Create { label, .. } => {
                    drive_create(
                        context,
                        &connection,
                        agent,
                        &partition_uuid,
                        label,
                        operation.as_ref(),
                    )
                    .await
                }
                SnapshotMgmtRequest::Apply { snapshot_uuid, .. } => {
                    drive_apply(
                        context,
                        &connection,
                        agent,
                        &partition_uuid,
                        &snapshot_uuid,
                        operation.as_ref(),
                    )
                    .await
                }
                SnapshotMgmtRequest::Delete { .. } => unreachable!(),
            };

            let response = match result {
                Ok((snapshot_uuid, bytes)) => {
                    finish_operation(operation, None);
                    SnapshotMgmtResponse::Finished {
                        snapshot_uuid,
                        bytes,
                    }
                }
                Err(e) => {
                    warn!(error = %e, %agent, partition = %partition_uuid, "Snapshot operation failed");
                    finish_operation(operation, Some(e.to_string()));
                    SnapshotMgmtResponse::Failed(e.to_string())
                }
            };
            ACTIVE.lock().unwrap().remove(&(agent, partition_uuid));
            let _ = sender.send(response).await;
        });
        Ok(())
    }
}

/// Replace any stale operation rows for this partition with a fresh one.
/// Best-effort: right after an agent attaches, the ownership grant may not have
/// landed yet, and a missing progress row shouldn't abort the operation itself.
fn new_operation(
    context: &SnapshotServerContext,
    agent: InstanceId,
    partition_uuid: &str,
    direction: SnapshotDirection,
) -> Option<Resident<SnapshotOperationData>> {
    let stale: Vec<_> = context
        .operations
        .iter()
        .filter(|op| {
            let op = op.read();
            op._instance_id == agent && op.partition_uuid == partition_uuid
        })
        .map(|op| op.read().id())
        .collect();
    for id in stale {
        let _ = context.operations.remove(id);
    }

    match context.operations.push(SnapshotOperationData {
        partition_uuid: partition_uuid.to_string(),
        direction,
        state: SnapshotOperationState::Preparing,
        ..SnapshotOperationData::scoped(agent)
    }) {
        Ok(op) => Some(op),
        Err(e) => {
            warn!(error = %e, "Failed to record the snapshot operation; continuing without progress reporting");
            None
        }
    }
}

fn update_operation<F>(operation: Option<&Resident<SnapshotOperationData>>, mutator: F)
where
    F: Fn(&mut SnapshotOperationData) -> Result<()>,
{
    if let Some(operation) = operation
        && let Err(e) = operation.update(mutator)
    {
        warn!(error = %e, "Failed to update the snapshot operation row");
    }
}

fn finish_operation(operation: Option<Resident<SnapshotOperationData>>, error: Option<String>) {
    update_operation(operation.as_ref(), |op| {
        op.state = if error.is_some() {
            SnapshotOperationState::Failed
        } else {
            op.progress = 1.0;
            SnapshotOperationState::Complete
        };
        op.error = error.clone();
        Ok(())
    });
}

/// The chain's leaf: the snapshot no other snapshot is backed by.
fn latest_snapshot(
    context: &SnapshotServerContext,
    agent: InstanceId,
    partition_uuid: &str,
) -> Option<SnapshotData> {
    let rows: Vec<SnapshotData> = context
        .snapshots
        .iter()
        .map(|row| row.read().clone())
        .filter(|row| row._instance_id == agent && row.partition_uuid == partition_uuid)
        .collect();
    rows.iter()
        .find(|row| {
            !rows
                .iter()
                .any(|other| other.parent.as_deref() == Some(&row.uuid))
        })
        .cloned()
}

/// Server side of the create block stream: forwards everything into the
/// driver. Unbounded so the connection's dispatch loop never blocks on a busy
/// driver — the driver applies its own backpressure by pacing `Need` replies.
#[derive(Stream)]
pub struct SnapshotCreateStreamRequester {
    to_driver: UnboundedSender<SnapshotCreateResponse>,
}

impl StreamRequester for SnapshotCreateStreamRequester {
    type In = SnapshotCreateResponse;
    type Out = SnapshotCreateRequest;

    async fn on_message(&self, response: Self::In, _tx: Sender<Self::Out>) -> Result<()> {
        let _ = self.to_driver.send(response);
        Ok(())
    }
}

/// Server side of the apply block stream.
#[derive(Stream)]
pub struct SnapshotApplyStreamRequester {
    to_driver: UnboundedSender<SnapshotApplyResponse>,
}

impl StreamRequester for SnapshotApplyStreamRequester {
    type In = SnapshotApplyResponse;
    type Out = SnapshotApplyRequest;

    async fn on_message(&self, response: Self::In, _tx: Sender<Self::Out>) -> Result<()> {
        let _ = self.to_driver.send(response);
        Ok(())
    }
}

/// Wait for the next stream event, giving up after [`INACTIVITY_TIMEOUT`].
async fn next_event<T>(events: &mut UnboundedReceiver<T>) -> Result<T> {
    match tokio::time::timeout(INACTIVITY_TIMEOUT, events.recv()).await {
        Ok(Some(event)) => Ok(event),
        Ok(None) => bail!("The stream closed unexpectedly"),
        Err(_) => bail!("The stream went silent"),
    }
}

/// Wait for the agent's first reply on a block stream. An agent that closes the
/// stream before saying anything has no responder for it: only an agent booted
/// into the UKI answers the block streams.
async fn first_event<T>(events: &mut UnboundedReceiver<T>) -> Result<T> {
    match tokio::time::timeout(INACTIVITY_TIMEOUT, events.recv()).await {
        Ok(Some(event)) => Ok(event),
        Ok(None) => bail!("The agent does not take snapshots; only a boot agent does"),
        Err(_) => bail!("The stream went silent"),
    }
}

/// Read the block at `offset` of the staging file.
async fn read_staging_block(
    file: &mut tokio::fs::File,
    offset: u64,
    block_size: u64,
    size: u64,
) -> Result<Vec<u8>> {
    let len = block_size.min(size - offset) as usize;
    let mut block = vec![0u8; len];
    file.seek(SeekFrom::Start(offset)).await?;
    file.read_exact(&mut block).await?;
    Ok(block)
}

/// Drive one capture to completion, returning the new snapshot's uuid and the
/// block bytes transferred.
async fn drive_create(
    context: &SnapshotServerContext,
    connection: &InstanceConnection,
    agent: InstanceId,
    partition_uuid: &str,
    label: Option<String>,
    operation: Option<&Resident<SnapshotOperationData>>,
) -> Result<(Option<String>, u64)> {
    let previous = latest_snapshot(context, agent, partition_uuid);

    // The comparison image: the previous snapshot's content, or all zeros for a
    // base capture (which is what makes unwritten regions free to skip).
    let staging = match &previous {
        Some(prev) => {
            if prev.block_size != SNAPSHOT_BLOCK_SIZE {
                bail!(
                    "The existing chain uses block size {}; delete it to re-base",
                    prev.block_size
                );
            }
            context
                .store
                .reconstruct(agent, partition_uuid, &prev.uuid)
                .await?
        }
        None => context.store.new_staging(agent, partition_uuid).await?,
    };

    let result = drive_create_inner(
        context,
        connection,
        agent,
        partition_uuid,
        label,
        operation,
        previous,
        &staging,
    )
    .await;
    // The commit deletes the staging file itself; this covers the error paths.
    let _ = tokio::fs::remove_file(&staging).await;
    result
}

#[allow(clippy::too_many_arguments)]
async fn drive_create_inner(
    context: &SnapshotServerContext,
    connection: &InstanceConnection,
    agent: InstanceId,
    partition_uuid: &str,
    label: Option<String>,
    operation: Option<&Resident<SnapshotOperationData>>,
    previous: Option<SnapshotData>,
    staging: &std::path::Path,
) -> Result<(Option<String>, u64)> {
    let (to_driver, mut events) = unbounded_channel();
    let (stream_id, messages) = connection
        .open_stream(
            SnapshotCreateStreamRequester { to_driver },
            SnapshotCreateRequest::Start {
                partition_uuid: partition_uuid.to_string(),
                block_size: SNAPSHOT_BLOCK_SIZE,
            },
        )
        .await?;
    let result = async {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(staging)
            .await?;

        // The agent measures the partition before anything else flows.
        let size = match first_event(&mut events).await? {
            SnapshotCreateResponse::Meta { size } => size,
            SnapshotCreateResponse::Failed(e) => bail!("{e}"),
            _ => bail!("The agent broke protocol"),
        };
        if size == 0 {
            bail!("The partition is empty");
        }
        match &previous {
            Some(prev) if prev.size != size => bail!(
                "Partition size changed since the last snapshot ({} != {}); delete the chain to re-base",
                size,
                prev.size
            ),
            Some(_) => {}
            None => file.set_len(size).await?,
        }

        update_operation(operation, |op| {
            op.state = SnapshotOperationState::Transferring;
            op.total_bytes = size;
            Ok(())
        });

        let mut bytes = 0u64;
        let mut last_progress = Instant::now();
        loop {
            match next_event(&mut events).await? {
                SnapshotCreateResponse::Meta { .. } => bail!("The agent broke protocol"),
                SnapshotCreateResponse::Hashes { offset, hashes } => {
                    if hashes.len() > HASHES_PER_MESSAGE {
                        bail!("Oversized hash batch");
                    }
                    let mut needed = Vec::new();
                    for (i, hash) in hashes.iter().enumerate() {
                        let block_offset = offset + i as u64 * SNAPSHOT_BLOCK_SIZE;
                        if block_offset >= size {
                            bail!("Hash offset out of range");
                        }
                        let block =
                            read_staging_block(&mut file, block_offset, SNAPSHOT_BLOCK_SIZE, size)
                                .await?;
                        if <[u8; 32]>::from(blake3::hash(&block)) != *hash {
                            needed.push(block_offset);
                        }
                    }
                    if !needed.is_empty() {
                        let payload =
                            serde_cbor::to_vec(&SnapshotCreateRequest::Need { offsets: needed })?;
                        messages
                            .send(StreamMessage::local(stream_id, payload))
                            .await?;
                    }

                    let scanned =
                        (offset + hashes.len() as u64 * SNAPSHOT_BLOCK_SIZE).min(size);
                    if last_progress.elapsed() >= PROGRESS_INTERVAL {
                        last_progress = Instant::now();
                        update_operation(operation, |op| {
                            op.progress = scanned as f32 / size as f32;
                            op.bytes_transferred = bytes;
                            Ok(())
                        });
                    }
                }
                SnapshotCreateResponse::HashesDone => {
                    let payload = serde_cbor::to_vec(&SnapshotCreateRequest::NeedDone)?;
                    messages
                        .send(StreamMessage::local(stream_id, payload))
                        .await?;
                }
                SnapshotCreateResponse::Block { offset, data } => {
                    let block = zstd::bulk::decompress(&data, SNAPSHOT_BLOCK_SIZE as usize)?;
                    if offset >= size || offset + block.len() as u64 > size {
                        bail!("Uploaded block out of range");
                    }
                    bytes += block.len() as u64;
                    file.seek(SeekFrom::Start(offset)).await?;
                    file.write_all(&block).await?;
                }
                SnapshotCreateResponse::Done => break,
                SnapshotCreateResponse::Failed(e) => bail!("{e}"),
            }
        }
        file.sync_all().await?;
        drop(file);

        update_operation(operation, |op| {
            op.state = SnapshotOperationState::Committing;
            op.bytes_transferred = bytes;
            Ok(())
        });

        let uuid = uuid::Uuid::now_v7().to_string();
        let stored_size = context
            .store
            .commit(
                agent,
                partition_uuid,
                staging,
                &uuid,
                previous.as_ref().map(|p| p.uuid.as_str()),
            )
            .await?;

        // Unlike the progress rows, losing this write loses the snapshot, so
        // ride out a transient ownership hiccup before giving up.
        let record = SnapshotData {
            partition_uuid: partition_uuid.to_string(),
            uuid: uuid.clone(),
            parent: previous.map(|p| p.uuid),
            size,
            block_size: SNAPSHOT_BLOCK_SIZE,
            stored_size,
            label,
            ..SnapshotData::scoped(agent)
        };
        for attempt in 0.. {
            match context.snapshots.push(record.clone()) {
                Ok(_) => break,
                Err(e) if attempt < 3 => {
                    warn!(error = %e, "Failed to record the snapshot; retrying");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(e) => {
                    let _ = context.store.delete_layer(agent, partition_uuid, &uuid).await;
                    return Err(e).context("Failed to record the snapshot");
                }
            }
        }

        debug!(%agent, partition = %partition_uuid, snapshot = %uuid, bytes, "Snapshot captured");
        Ok((Some(uuid), bytes))
    }
    .await;
    connection.close_stream(stream_id);
    result
}

/// Drive one restore to completion, returning the applied snapshot's uuid and
/// the block bytes transferred.
async fn drive_apply(
    context: &SnapshotServerContext,
    connection: &InstanceConnection,
    agent: InstanceId,
    partition_uuid: &str,
    snapshot_uuid: &str,
    operation: Option<&Resident<SnapshotOperationData>>,
) -> Result<(Option<String>, u64)> {
    let snapshot = context
        .snapshots
        .iter()
        .map(|row| row.read().clone())
        .find(|row| {
            row._instance_id == agent
                && row.partition_uuid == partition_uuid
                && row.uuid == snapshot_uuid
        })
        .context("No such snapshot")?;

    let staging = context
        .store
        .reconstruct(agent, partition_uuid, &snapshot.uuid)
        .await?;
    let result =
        drive_apply_inner(connection, partition_uuid, &snapshot, operation, &staging).await;
    let _ = tokio::fs::remove_file(&staging).await;
    result
}

async fn drive_apply_inner(
    connection: &InstanceConnection,
    partition_uuid: &str,
    snapshot: &SnapshotData,
    operation: Option<&Resident<SnapshotOperationData>>,
    staging: &std::path::Path,
) -> Result<(Option<String>, u64)> {
    let (to_driver, mut events) = unbounded_channel();
    let (stream_id, messages) = connection
        .open_stream(
            SnapshotApplyStreamRequester { to_driver },
            SnapshotApplyRequest::Start {
                partition_uuid: partition_uuid.to_string(),
                block_size: snapshot.block_size,
                size: snapshot.size,
            },
        )
        .await?;
    let result = async {
        let mut file = OpenOptions::new().read(true).open(staging).await?;
        let size = snapshot.size;
        let block_size = snapshot.block_size;

        update_operation(operation, |op| {
            op.state = SnapshotOperationState::Transferring;
            op.total_bytes = size;
            Ok(())
        });

        let mut bytes = 0u64;
        let mut last_progress = Instant::now();
        let mut event = first_event(&mut events).await?;
        loop {
            match event {
                SnapshotApplyResponse::Hashes { offset, hashes } => {
                    if hashes.len() > HASHES_PER_MESSAGE {
                        bail!("Oversized hash batch");
                    }
                    for (i, hash) in hashes.iter().enumerate() {
                        let block_offset = offset + i as u64 * block_size;
                        if block_offset >= size {
                            bail!("Hash offset out of range");
                        }
                        let block =
                            read_staging_block(&mut file, block_offset, block_size, size).await?;
                        if <[u8; 32]>::from(blake3::hash(&block)) == *hash {
                            continue;
                        }
                        bytes += block.len() as u64;
                        let payload = serde_cbor::to_vec(&SnapshotApplyRequest::Block {
                            offset: block_offset,
                            data: zstd::bulk::compress(&block, WIRE_COMPRESSION_LEVEL)?,
                        })?;
                        messages
                            .send(StreamMessage::local(stream_id, payload))
                            .await?;
                    }

                    let scanned = (offset + hashes.len() as u64 * block_size).min(size);
                    if last_progress.elapsed() >= PROGRESS_INTERVAL {
                        last_progress = Instant::now();
                        update_operation(operation, |op| {
                            op.progress = scanned as f32 / size as f32;
                            op.bytes_transferred = bytes;
                            Ok(())
                        });
                    }
                }
                SnapshotApplyResponse::HashesDone => {
                    let payload = serde_cbor::to_vec(&SnapshotApplyRequest::Done)?;
                    messages
                        .send(StreamMessage::local(stream_id, payload))
                        .await?;
                }
                SnapshotApplyResponse::Applied => break,
                SnapshotApplyResponse::Failed(e) => bail!("{e}"),
            }
            event = next_event(&mut events).await?;
        }

        debug!(partition = %partition_uuid, snapshot = %snapshot.uuid, bytes, "Snapshot applied");
        Ok((Some(snapshot.uuid.clone()), bytes))
    }
    .await;
    connection.close_stream(stream_id);
    result
}

/// Delete a leaf snapshot: its layer file and its metadata row.
async fn run_delete(
    context: &SnapshotServerContext,
    agent: InstanceId,
    partition_uuid: &str,
    snapshot_uuid: &str,
) -> Result<()> {
    if ACTIVE
        .lock()
        .unwrap()
        .contains(&(agent, partition_uuid.to_string()))
    {
        bail!("An operation is running on this partition");
    }

    let rows: Vec<SnapshotData> = context
        .snapshots
        .iter()
        .map(|row| row.read().clone())
        .filter(|row| row._instance_id == agent && row.partition_uuid == partition_uuid)
        .collect();
    let target = rows
        .iter()
        .find(|row| row.uuid == snapshot_uuid)
        .context("No such snapshot")?;
    if rows
        .iter()
        .any(|row| row.parent.as_deref() == Some(snapshot_uuid))
    {
        bail!("Not a leaf snapshot; later snapshots are backed by it");
    }

    context
        .store
        .delete_layer(agent, partition_uuid, snapshot_uuid)
        .await?;
    context.snapshots.remove(target.id())?;
    debug!(%agent, partition = %partition_uuid, snapshot = %snapshot_uuid, "Snapshot deleted");
    Ok(())
}
