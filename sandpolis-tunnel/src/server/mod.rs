//! Server side of the tunnel subsystem: the global stratum server reads the
//! realm config and *bridges* each tunnel.
//!
//! For every configured tunnel the server opens one [`streams::TunnelStream`]
//! toward the listener endpoint and one toward the terminator endpoint, then
//! copies bytes between them keyed by a logical connection id. An endpoint that
//! is the local server runs the worker in-process; a remote endpoint runs it
//! over the relay, so an instance behind a local stratum server is reached
//! transparently. This is the indirect data path — the default that always
//! works.
//!
//! [`crate::streams`]: crate::streams

use crate::config::{TunnelConfig, TunnelManagerConfig};
use crate::forward::run_endpoint;
use crate::streams::{TunnelStreamRequest, TunnelStreamResponse};
use crate::{TunnelData, TunnelMode, TunnelProtocol, TunnelRole, TunnelState, direct};
use anyhow::Result;
use sandpolis_instance::InstanceId;
use sandpolis_instance::database::{Data, RealmDatabase, Resident, ResidentVec};
use sandpolis_instance::network::stream::StreamMessage;
use sandpolis_instance::network::{NetworkManager, StreamRequester};
use sandpolis_macros::Stream;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::mpsc::{Sender, UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// How often the orchestrator re-checks whether an endpoint became reachable.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(3);
/// How long an endpoint has to report it's ready before the attempt is abandoned.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Minimum time between traffic-stat writes, so a busy tunnel doesn't replicate
/// thousands of revisions.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

pub struct TunnelServerContext {
    pub network: NetworkManager,
    pub self_id: InstanceId,
    pub tunnels: Vec<TunnelConfig>,
    /// The live tunnel rows, replicated out to every client.
    pub rows: ResidentVec<TunnelData>,
}

impl TunnelServerContext {
    pub fn new(
        realm: RealmDatabase,
        network: NetworkManager,
        self_id: InstanceId,
        config: TunnelManagerConfig,
    ) -> Result<Self> {
        Ok(Self {
            network,
            self_id,
            tunnels: config.tunnels,
            rows: realm.resident_vec(())?,
        })
    }
}

static CONTEXT: OnceLock<TunnelServerContext> = OnceLock::new();

/// Install the server context and start the orchestrator. Called once at
/// startup; a server with no configured tunnels simply idles.
pub fn install(context: TunnelServerContext) {
    if context.tunnels.is_empty() {
        // Still record the context so nothing else has to special-case it.
        let _ = CONTEXT.set(context);
        return;
    }
    let _ = CONTEXT.set(context);
    tokio::spawn(orchestrate());
}

/// One tunnel the orchestrator is managing.
struct Managed {
    config: TunnelConfig,
    /// `None` when the config couldn't be parsed; the row is left `Failed`.
    endpoints: Option<(InstanceId, InstanceId)>,
    row: Arc<Resident<TunnelData>>,
    cancel: Option<CancellationToken>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

async fn orchestrate() {
    let Some(ctx) = CONTEXT.get() else { return };

    // Drop any rows a previous run left behind, then declare each tunnel fresh.
    let stale: Vec<_> = ctx.rows.iter().map(|row| row.read().id()).collect();
    for id in stale {
        let _ = ctx.rows.remove(id);
    }

    let mut managed: Vec<Managed> = Vec::new();
    for config in &ctx.tunnels {
        let listener = config.listener.parse::<InstanceId>();
        let terminator = config.terminator.parse::<InstanceId>();
        let (endpoints, error) = match (listener, terminator) {
            (Ok(l), Ok(t)) => (Some((l, t)), None),
            _ => (
                None,
                Some("Invalid listener or terminator instance id".to_string()),
            ),
        };

        let data = TunnelData {
            name: config.name.clone(),
            listener_id: endpoints.map(|(l, _)| l),
            listen_addr: config.listen.to_string(),
            terminator_id: endpoints.map(|(_, t)| t),
            target_addr: config.target.clone(),
            protocol: config.protocol,
            mode: config.mode,
            effective_mode: TunnelMode::Indirect,
            state: if endpoints.is_some() {
                TunnelState::Pending
            } else {
                TunnelState::Failed
            },
            error,
            ..Default::default()
        };
        let row = match ctx.rows.push(data) {
            Ok(row) => Arc::new(row),
            Err(e) => {
                warn!(error = %e, tunnel = %config.name, "Failed to record tunnel");
                continue;
            }
        };
        managed.push(Managed {
            config: config.clone(),
            endpoints,
            row,
            cancel: None,
            handle: None,
        });
    }

    // Wake promptly whenever a connection comes or goes; otherwise poll.
    let notify = Arc::new(Notify::new());
    {
        let notify = notify.clone();
        ctx.network.connections.listen(move |_| notify.notify_one());
    }

    loop {
        for m in managed.iter_mut() {
            let Some((listener_id, terminator_id)) = m.endpoints else {
                continue;
            };
            // Clear a bridge that ended (an endpoint dropped), cancelling its
            // token so the surviving endpoint's forwarder/worker tasks and
            // streams are torn down rather than left running.
            if m.handle.as_ref().is_some_and(|h| h.is_finished()) {
                if let Some(cancel) = m.cancel.take() {
                    cancel.cancel();
                }
                m.handle = None;
            }
            if m.handle.is_some() {
                continue;
            }
            if !endpoint_reachable(ctx, listener_id) || !endpoint_reachable(ctx, terminator_id) {
                continue;
            }

            let cancel = CancellationToken::new();
            let listener = open_endpoint(
                ctx,
                listener_id,
                TunnelRole::Listener {
                    listen: m.config.listen,
                },
                m.config.protocol,
                cancel.clone(),
            )
            .await;
            let terminator = open_endpoint(
                ctx,
                terminator_id,
                TunnelRole::Terminator {
                    target: m.config.target.clone(),
                },
                m.config.protocol,
                cancel.clone(),
            )
            .await;

            let (listener, terminator) = match (listener, terminator) {
                (Ok(l), Ok(t)) => (l, t),
                (l, t) => {
                    if let Err(e) = l {
                        debug!(tunnel = %m.config.name, error = %e, "Listener endpoint unavailable");
                    }
                    if let Err(e) = t {
                        debug!(tunnel = %m.config.name, error = %e, "Terminator endpoint unavailable");
                    }
                    cancel.cancel();
                    continue;
                }
            };

            // The direct/hole-punch path is a stub, so a `Direct` client<->agent
            // tunnel falls back to the indirect bridge here.
            let effective = match m.config.mode {
                TunnelMode::Direct => match direct::attempt_direct(terminator_id) {
                    direct::DirectOutcome::Unsupported => TunnelMode::Indirect,
                },
                TunnelMode::Indirect => TunnelMode::Indirect,
            };
            let _ = m.row.update(|d| {
                d.state = TunnelState::Active;
                d.effective_mode = effective;
                d.error = None;
                Ok(())
            });

            let row = m.row.clone();
            let bridge_cancel = cancel.clone();
            m.handle = Some(tokio::spawn(async move {
                run_bridge(listener, terminator, row, bridge_cancel).await;
            }));
            m.cancel = Some(cancel);
        }

        tokio::select! {
            _ = notify.notified() => {}
            _ = tokio::time::sleep(RECONCILE_INTERVAL) => {}
        }
    }
}

/// Whether a tunnel endpoint can be reached right now (the local server always
/// can reach itself).
fn endpoint_reachable(ctx: &TunnelServerContext, id: InstanceId) -> bool {
    id == ctx.self_id || ctx.network.connection_to(id).is_some()
}

/// Server side of the tunnel stream: forwards the endpoint's responses.
#[derive(Stream)]
pub struct TunnelStreamRequester {
    responses: UnboundedSender<TunnelStreamResponse>,
}

impl TunnelStreamRequester {
    fn channel() -> (Self, UnboundedReceiver<TunnelStreamResponse>) {
        let (responses, rx) = unbounded_channel();
        (Self { responses }, rx)
    }
}

impl StreamRequester for TunnelStreamRequester {
    type In = TunnelStreamResponse;
    type Out = TunnelStreamRequest;

    async fn on_message(&self, response: Self::In, _tx: Sender<Self::Out>) -> Result<()> {
        let _ = self.responses.send(response);
        Ok(())
    }
}

/// One end of a bridged tunnel, whether local or reached over a stream.
struct Endpoint {
    /// Requests toward the endpoint (after the initial `Open`).
    to: UnboundedSender<TunnelStreamRequest>,
    /// Responses from the endpoint.
    from: UnboundedReceiver<TunnelStreamResponse>,
}

/// Open a tunnel endpoint, in-process if it's the local server, otherwise over
/// a (possibly relayed) stream.
async fn open_endpoint(
    ctx: &TunnelServerContext,
    id: InstanceId,
    role: TunnelRole,
    protocol: TunnelProtocol,
    cancel: CancellationToken,
) -> Result<Endpoint> {
    if id == ctx.self_id {
        let (to, req_rx) = unbounded_channel::<TunnelStreamRequest>();
        let (resp_tx, from) = unbounded_channel::<TunnelStreamResponse>();
        tokio::spawn(async move {
            run_endpoint(role, protocol, req_rx, resp_tx, cancel).await;
        });
        return Ok(Endpoint { to, from });
    }

    let (conn, dst) = ctx
        .network
        .connection_to(id)
        .ok_or_else(|| anyhow::anyhow!("endpoint {id} is not reachable"))?;
    let (requester, from) = TunnelStreamRequester::channel();
    let initial = TunnelStreamRequest::Open { role, protocol };
    let (stream_id, msg_tx) = match dst {
        Some(target) => conn.open_stream_to(target, requester, initial).await?,
        None => conn.open_stream(requester, initial).await?,
    };

    let (to, mut to_rx) = unbounded_channel::<TunnelStreamRequest>();
    let conn = conn.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                request = to_rx.recv() => match request {
                    Some(request) => match serde_cbor::to_vec(&request) {
                        Ok(payload) => {
                            if msg_tx.send(StreamMessage::routed(stream_id, payload, dst)).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => warn!(error = %e, "Failed to encode tunnel request"),
                    },
                    None => break,
                }
            }
        }
        conn.close_stream(stream_id);
    });

    Ok(Endpoint { to, from })
}

/// Per-connection bridge state.
struct ConnState {
    /// Whether the terminator has connected to the target yet.
    connected: bool,
    /// Listener bytes buffered until the terminator connects.
    buffer: Vec<Vec<u8>>,
}

/// Copy bytes between the two endpoints until one goes away or `cancel` fires.
async fn run_bridge(
    mut listener: Endpoint,
    mut terminator: Endpoint,
    row: Arc<Resident<TunnelData>>,
    cancel: CancellationToken,
) {
    if let Err(e) = wait_ready(&mut listener.from, &cancel).await {
        return finish(&row, Some(format!("listener: {e}")));
    }
    if let Err(e) = wait_ready(&mut terminator.from, &cancel).await {
        return finish(&row, Some(format!("terminator: {e}")));
    }

    let mut conns: HashMap<u64, ConnState> = HashMap::new();
    let (mut rx_bytes, mut tx_bytes) = {
        let data = row.read();
        (data.rx_bytes, data.tx_bytes)
    };
    let mut dirty = false;
    let mut flush = tokio::time::interval(PROGRESS_INTERVAL);
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,

            // Listener -> server: accepted connections and upload bytes.
            message = listener.from.recv() => match message {
                None => return finish(&row, None),
                Some(TunnelStreamResponse::Accepted { conn, .. }) => {
                    conns.insert(conn, ConnState { connected: false, buffer: Vec::new() });
                    let _ = terminator.to.send(TunnelStreamRequest::Connect { conn });
                    dirty = true;
                }
                Some(TunnelStreamResponse::Data { conn, bytes }) => {
                    tx_bytes += bytes.len() as u64;
                    dirty = true;
                    match conns.get_mut(&conn) {
                        Some(state) if state.connected => {
                            let _ = terminator.to.send(TunnelStreamRequest::Data { conn, bytes });
                        }
                        Some(state) => state.buffer.push(bytes),
                        None => {}
                    }
                }
                Some(TunnelStreamResponse::Closed { conn }) => {
                    if conns.remove(&conn).is_some() {
                        let _ = terminator.to.send(TunnelStreamRequest::Close { conn });
                        dirty = true;
                    }
                }
                Some(TunnelStreamResponse::Error { message }) => {
                    return finish(&row, Some(format!("listener: {message}")));
                }
                Some(_) => {}
            },

            // Terminator -> server: connect results and download bytes.
            message = terminator.from.recv() => match message {
                None => return finish(&row, None),
                Some(TunnelStreamResponse::Connected { conn }) => {
                    if let Some(state) = conns.get_mut(&conn) {
                        state.connected = true;
                        for bytes in state.buffer.drain(..) {
                            let _ = terminator.to.send(TunnelStreamRequest::Data { conn, bytes });
                        }
                    }
                }
                Some(TunnelStreamResponse::ConnectFailed { conn, .. }) => {
                    if conns.remove(&conn).is_some() {
                        let _ = listener.to.send(TunnelStreamRequest::Close { conn });
                        dirty = true;
                    }
                }
                Some(TunnelStreamResponse::Data { conn, bytes }) => {
                    rx_bytes += bytes.len() as u64;
                    dirty = true;
                    let _ = listener.to.send(TunnelStreamRequest::Data { conn, bytes });
                }
                Some(TunnelStreamResponse::Closed { conn }) => {
                    if conns.remove(&conn).is_some() {
                        let _ = listener.to.send(TunnelStreamRequest::Close { conn });
                        dirty = true;
                    }
                }
                Some(TunnelStreamResponse::Error { message }) => {
                    return finish(&row, Some(format!("terminator: {message}")));
                }
                Some(_) => {}
            },

            _ = flush.tick() => {
                if dirty {
                    let connections = conns.len() as u32;
                    let _ = row.update(|d| {
                        d.rx_bytes = rx_bytes;
                        d.tx_bytes = tx_bytes;
                        d.active_connections = connections;
                        Ok(())
                    });
                    dirty = false;
                }
            }
        }
    }
}

/// Record a terminal state on a tunnel row: `Failed` with the message, or
/// `Pending` (endpoint lost) so the orchestrator retries.
fn finish(row: &Resident<TunnelData>, error: Option<String>) {
    let _ = row.update(|d| {
        d.state = if error.is_some() {
            TunnelState::Failed
        } else {
            TunnelState::Pending
        };
        d.active_connections = 0;
        d.error = error.clone();
        Ok(())
    });
}

/// Wait for an endpoint's `Ready` (or a fatal `Error`).
async fn wait_ready(
    from: &mut UnboundedReceiver<TunnelStreamResponse>,
    cancel: &CancellationToken,
) -> std::result::Result<(), String> {
    let deadline = tokio::time::sleep(READY_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Err("cancelled".into()),
            _ = &mut deadline => return Err("did not become ready".into()),
            message = from.recv() => match message {
                Some(TunnelStreamResponse::Ready) => return Ok(()),
                Some(TunnelStreamResponse::Error { message }) => return Err(message),
                Some(_) => continue,
                None => return Err("disconnected".into()),
            }
        }
    }
}
