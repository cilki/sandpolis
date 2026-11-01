//! Client-side access to synced snapshot data and the management stream.
//!
//! Mirrors the health subsystem: a view subscribes to the relevant models when
//! it opens and reads what the sync module replicated into the client's local
//! database.

use crate::streams::{SnapshotMgmtRequest, SnapshotMgmtResponse};
use crate::{SnapshotData, SnapshotOperationData};
use anyhow::Result;
use native_model::Model;
use sandpolis_instance::InstanceId;
use sandpolis_instance::network::StreamRequester;
use sandpolis_macros::Stream;
use tokio::sync::mpsc::{Sender, UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::warn;

#[cfg(feature = "client")]
pub mod gui;

fn snapshot_model_ids() -> [u32; 2] {
    [
        <SnapshotData as Model>::native_model_id(),
        <SnapshotOperationData as Model>::native_model_id(),
    ]
}

/// Model id of [SnapshotOperationData], for subscriptions scoped to it alone.
pub fn operation_model_id() -> u32 {
    <SnapshotOperationData as Model>::native_model_id()
}

/// Subscribe to live snapshot updates for an instance (call when a view opens).
pub fn subscribe(instance: InstanceId) {
    sandpolis_client::sync::subscribe_all(snapshot_model_ids(), Some(instance));
}

/// Drop the subscriptions created by [`subscribe`].
pub fn unsubscribe(instance: InstanceId) {
    sandpolis_client::sync::unsubscribe_all(snapshot_model_ids(), Some(instance));
}

/// Subscribe to snapshot updates for every instance (the CLI list view).
pub fn subscribe_everything() {
    sandpolis_client::sync::subscribe_all(snapshot_model_ids(), None);
}

/// Query the stored snapshots for an instance, oldest first (chain order).
pub fn query_snapshots(id: InstanceId) -> Result<Vec<SnapshotData>> {
    let mut snapshots: Vec<SnapshotData> = sandpolis_client::sync::scan_latest::<SnapshotData>()?
        .into_iter()
        .filter(|s| s._instance_id == id)
        .collect();
    snapshots.sort_by_key(|s| s._creation.timestamp());
    Ok(snapshots)
}

/// Query the snapshot operations known for an instance.
pub fn query_operations(id: InstanceId) -> Result<Vec<SnapshotOperationData>> {
    Ok(
        sandpolis_client::sync::scan_latest::<SnapshotOperationData>()?
            .into_iter()
            .filter(|op| op._instance_id == id)
            .collect(),
    )
}

/// Every operation currently running, across all instances. Drives the link
/// decoration in the GUI.
pub fn active_operations() -> Result<Vec<SnapshotOperationData>> {
    Ok(
        sandpolis_client::sync::scan_latest::<SnapshotOperationData>()?
            .into_iter()
            .filter(|op| op.state.active())
            .collect(),
    )
}

/// Client side of the management stream: forwards the server's responses.
#[derive(Stream)]
pub struct SnapshotMgmtStreamRequester {
    result: UnboundedSender<SnapshotMgmtResponse>,
}

impl SnapshotMgmtStreamRequester {
    /// Construct a requester paired with the receiver the caller drains.
    pub fn channel() -> (Self, UnboundedReceiver<SnapshotMgmtResponse>) {
        let (result, rx) = unbounded_channel();
        (Self { result }, rx)
    }
}

impl StreamRequester for SnapshotMgmtStreamRequester {
    type In = SnapshotMgmtResponse;
    type Out = SnapshotMgmtRequest;

    async fn on_message(&self, response: Self::In, _tx: Sender<Self::Out>) -> Result<()> {
        let _ = self.result.send(response);
        Ok(())
    }
}

/// Send one management request to the connected server, yielding its responses
/// (`Started`, then a terminal outcome). The stream is closed after the
/// terminal response, so callers may drop the receiver without leaking it.
pub fn request(initial: SnapshotMgmtRequest) -> UnboundedReceiver<SnapshotMgmtResponse> {
    let (out_tx, out_rx) = unbounded_channel();
    let spawned = sandpolis_client::sync::spawn(async move {
        let Some(connection) = sandpolis_client::sync::connection() else {
            let _ = out_tx.send(SnapshotMgmtResponse::Failed(
                "Not connected to a server".into(),
            ));
            return;
        };
        let (requester, mut rx) = SnapshotMgmtStreamRequester::channel();
        let (id, _tx) = match connection.open_stream(requester, initial).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "Failed to open the snapshot management stream");
                let _ = out_tx.send(SnapshotMgmtResponse::Failed(e.to_string()));
                return;
            }
        };
        while let Some(response) = rx.recv().await {
            let terminal = !matches!(response, SnapshotMgmtResponse::Started);
            let _ = out_tx.send(response);
            if terminal {
                break;
            }
        }
        connection.close_stream(id);
    });
    if !spawned {
        // out_tx was moved into the (never-spawned) future and dropped with it,
        // so the receiver just yields None: callers report it as no response.
        warn!("Client sync is not initialized");
    }
    out_rx
}
