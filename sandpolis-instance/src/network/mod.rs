use crate::database::DatabaseManager;
use crate::database::Resident;
use crate::database::ResidentVec;
use crate::realm::RealmName;
use crate::{ClusterId, InstanceId};
use anyhow::Result;
use chrono::{DateTime, Utc};
use native_db::ToKey;
use native_model::Model;
use sandpolis_macros::data;
use serde::{Deserialize, Serialize};
use serde_with::chrono::serde::{ts_seconds, ts_seconds_option};
use std::sync::RwLock;
use std::{cmp::min, net::SocketAddr, sync::Arc, time::Duration};
use stream::{StreamId, StreamMessage};
pub use stream::{StreamRegistry, StreamRequester, StreamResponder};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace};

/// Trait for subsystems to register their stream responders on new connections.
pub trait RegisterResponders: Send + Sync + 'static {
    fn register_responders(&self, registry: &StreamRegistry);
}

/// Wrapper for collecting `RegisterResponders` implementations via inventory.
pub struct ResponderRegistration(pub &'static dyn RegisterResponders);

// SAFETY: The inner reference is 'static and the trait requires Send + Sync
unsafe impl Send for ResponderRegistration {}
unsafe impl Sync for ResponderRegistration {}

inventory::collect!(ResponderRegistration);

/// Returns an iterator over all registered responder handlers.
pub fn collected_responders() -> impl Iterator<Item = &'static dyn RegisterResponders> {
    inventory::iter::<ResponderRegistration>().map(|r| r.0)
}

#[cfg(any(feature = "agent", feature = "client", feature = "server"))]
pub mod client;
pub mod liveness;
pub mod messages;
pub mod ping;
pub mod reachability;
#[cfg(feature = "server")]
pub mod server;
pub mod stream;
pub mod sync;

#[data]
#[derive(Default)]
pub struct NetworkManagerData {}

#[derive(Clone)]
#[cfg_attr(feature = "client", derive(bevy::prelude::Resource))]
pub struct NetworkManager {
    data: Resident<NetworkManagerData>,

    /// Inbound connections
    pub inbound: Arc<RwLock<Vec<Arc<InstanceConnection>>>>,

    /// Server-side stream relay (forwards client streams to target agents).
    pub relay: Arc<stream::Relay>,

    /// All connections tracked in the database
    pub connections: ResidentVec<ConnectionData>,

    /// Who is reachable, as reported by the servers that can see them. Written
    /// by servers, read by anyone — a client's whole picture of which agents are
    /// up comes from here, since it holds no connection to any of them.
    pub liveness: ResidentVec<liveness::LivenessData>,

    pub database: DatabaseManager,
}

/// How often each side of a websocket sends a keepalive ping.
pub(crate) const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// How long a websocket may go without receiving anything before it's declared
/// dead. A peer that vanishes without closing its socket (a partition, a killed
/// machine) is otherwise indistinguishable from an idle one, and that is exactly
/// the case where going offline needs to be noticed.
///
/// Generous relative to the ping interval: a missed pong is not yet a fault.
pub(crate) const KEEPALIVE_DEADLINE: Duration = Duration::from_secs(90);

impl NetworkManager {
    /// The inbound connections that are still live, i.e. whose socket hasn't
    /// been cancelled.
    ///
    /// Callers want the set of instances attached to this server right now, but
    /// they each want a different slice of it, so this hands back the
    /// connections and lets them map. Snapshotted rather than borrowed so the
    /// lock isn't held across whatever the caller does next.
    pub fn live_inbound(&self) -> Vec<Arc<InstanceConnection>> {
        self.inbound
            .read()
            .unwrap()
            .iter()
            .filter(|c| !c.cancel.is_cancelled())
            .cloned()
            .collect()
    }

    /// Find a connection over which a stream to `instance` can be opened, plus
    /// the relay destination to stamp on its messages.
    ///
    /// Returns `(connection, None)` when the instance is directly attached here
    /// (open the stream directly), or `(connection, Some(instance))` when it is
    /// reachable through an advertised server peer (the message must be relayed).
    /// `None` when the instance is not reachable from this server.
    pub fn connection_to(
        &self,
        instance: InstanceId,
    ) -> Option<(Arc<InstanceConnection>, Option<InstanceId>)> {
        if let Some(direct) = self
            .live_inbound()
            .into_iter()
            .find(|c| c.data.read().remote_instance == Some(instance))
        {
            return Some((direct, None));
        }
        self.relay
            .reachable_via(instance)
            .map(|conn| (conn, Some(instance)))
    }

    pub async fn new(database: DatabaseManager) -> Result<Self> {
        debug!("Initializing network manager");

        let realm = database.realm(RealmName::default())?;
        let inbound = Arc::new(RwLock::new(Vec::new()));
        let network = Self {
            relay: Arc::new(stream::Relay::new(inbound.clone())),
            inbound,
            data: realm.resident(())?,
            connections: realm.resident_vec(())?,
            liveness: realm.resident_vec(())?,
            database,
        };

        Ok(network)
    }

    /// Track an accepted connection and clean up after it.
    ///
    /// When the socket task ends — the peer closed, the transport errored, or
    /// the keepalive deadline passed — the connection leaves `inbound` and its
    /// [`ConnectionData`] row leaves the database. The removal is what fires
    /// `connections.listen`, so everything watching for a disconnect (ownership
    /// reconcilers, liveness) wakes without polling.
    pub fn track_inbound(&self, connection: Arc<InstanceConnection>) {
        let cancel = connection.cancel.clone();
        let (row, instance) = {
            let data = connection.data.read();
            (data._id, data.remote_instance)
        };
        let realm = connection.realm.clone();
        let weak = Arc::downgrade(&connection);

        // Same classification the connect handler logs: a peer that identified
        // itself as neither a server nor an agent was served as a client.
        let kind = match instance {
            Some(id) if id.is_server() => "server",
            Some(id) if id.is_agent() => "agent",
            _ => "client",
        };

        self.inbound.write().unwrap().push(connection);

        let inbound = self.inbound.clone();
        let connections = self.connections.clone();
        tokio::spawn(async move {
            cancel.cancelled().await;

            // Dropping the last `Arc` is itself one of the ways we get here, so
            // a failed upgrade just means `inbound` let go first.
            if let Some(connection) = weak.upgrade() {
                inbound
                    .write()
                    .unwrap()
                    .retain(|c| !Arc::ptr_eq(c, &connection));
            }

            if let Err(e) = connections.remove_local(row) {
                debug!(error = %e, "Failed to remove a closed connection");
            }

            info!(kind, instance = ?instance, realm = %realm, "Instance disconnected");
        });
    }
}

/// Convenience type to be used as return of request handler.
#[cfg(feature = "server")]
pub type RequestResult<T> = Result<axum::Json<T>, axum::Json<T>>;

#[data(temporal, defaults)]
pub struct ConnectionData {
    #[secondary_key]
    pub _instance_id: InstanceId,

    /// The peer's id, once it has identified itself. `None` for a peer that
    /// never sent one, e.g. a browser hitting the web listener.
    pub remote_instance: Option<InstanceId>,

    /// Application-level bytes read since the connection was established
    pub read_bytes: u64,

    /// Total number of bytes written since the connection was established
    pub write_bytes: u64,

    /// "Recent" read throughput in bytes/second
    pub read_throughput: u64,

    /// "Recent" write throughput in bytes/second
    pub write_throughput: u64,

    /// Round trip time to the peer in microseconds, measured by timing the
    /// pong that answers the keepalive ping. `None` until the first pong
    /// arrives, so a connection younger than [`KEEPALIVE_INTERVAL`] has no
    /// measurement yet.
    pub latency: Option<u32>,

    pub local_socket: Option<SocketAddr>,
    pub remote_socket: Option<SocketAddr>,

    #[serde(with = "ts_seconds")]
    pub established: DateTime<Utc>,

    #[serde(with = "ts_seconds_option")]
    pub disconnected: Option<DateTime<Utc>>,
}

/// Times the keepalive ping/pong exchange so a connection can report how far
/// away its peer is.
///
/// The socket already pings every [`KEEPALIVE_INTERVAL`] to prove the peer is
/// there, so the round trip costs nothing extra to measure. Only one ping is
/// ever outstanding, but the pong still has to carry the nonce back: a peer is
/// allowed to send an unsolicited pong as a heartbeat, and timing one of those
/// against our own ping would report a round trip that never happened.
pub(crate) struct LatencyProbe {
    data: Resident<ConnectionData>,
    nonce: u64,
    outstanding: Option<(u64, tokio::time::Instant)>,
}

impl LatencyProbe {
    pub(crate) fn new(data: Resident<ConnectionData>) -> Self {
        Self {
            data,
            nonce: 0,
            outstanding: None,
        }
    }

    /// Payload for the next keepalive ping, which starts the clock. A ping that
    /// was never answered is simply forgotten here.
    pub(crate) fn ping(&mut self) -> Vec<u8> {
        self.nonce = self.nonce.wrapping_add(1);
        self.outstanding = Some((self.nonce, tokio::time::Instant::now()));
        self.nonce.to_le_bytes().to_vec()
    }

    /// Record the round trip if this pong answers the outstanding ping.
    pub(crate) fn pong(&mut self, payload: &[u8]) {
        let Some((nonce, sent)) = self.outstanding else {
            return;
        };
        if payload != nonce.to_le_bytes() {
            return;
        }
        self.outstanding = None;

        let latency = sent.elapsed().as_micros().min(u32::MAX as u128) as u32;

        // Connection rows are local bookkeeping that never replicate, so this
        // is writable even on a read-only replica.
        if let Err(e) = self.data.update_local(|data| {
            data.latency = Some(latency);
            Ok(())
        }) {
            trace!(error = %e, "Failed to record connection latency");
        }
    }
}

/// Connection to another instance that's suitable for running streams. The transport
/// will either be a Websocket (when a server is one of the peers) or DTLS (when
/// neither peer is a server).
///
/// For the DTLS case, reliable/orderly delivery is not guaranteed which fits
/// the use case of direct connections.
pub struct InstanceConnection {
    pub data: Resident<ConnectionData>,
    pub realm: RealmName,
    pub cluster_id: ClusterId,
    pub cancel: CancellationToken,
    pub streams: Arc<StreamRegistry>,

    /// The check-in schedule a polling peer announced when it connected, set by
    /// whichever side accepted the socket. In memory only: it describes this
    /// socket, and a peer that reconnects announces it again.
    pub poll: std::sync::OnceLock<liveness::PollAnnouncement>,
}

impl Drop for InstanceConnection {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl InstanceConnection {
    /// Register a stream handler and return a sender for outbound messages.
    pub fn register_stream<S: stream::StreamRequester>(
        &self,
        handler: S,
    ) -> (StreamId, tokio::sync::mpsc::Sender<StreamMessage>)
    where
        S::Out: Send + 'static,
    {
        self.streams.register(handler)
    }

    /// Remove a stream (Drop handles cleanup on the handler).
    pub fn close_stream(&self, stream_id: StreamId) {
        self.streams.close(stream_id);
    }

    /// Open a stream handled directly by the connected server (no relay target).
    /// Sends the `initial` request and returns the stream id plus the outbound
    /// sender.
    pub async fn open_stream<S: StreamRequester>(
        &self,
        handler: S,
        initial: S::Out,
    ) -> anyhow::Result<(StreamId, tokio::sync::mpsc::Sender<StreamMessage>)>
    where
        S::Out: Send + 'static,
    {
        let (id, tx) = self.streams.register(handler);
        let payload = serde_cbor::to_vec(&initial)?;
        tx.send(StreamMessage::local(id, payload)).await?;
        Ok((id, tx))
    }

    /// Open a stream addressed to `target`, so a server relays it there. Sends
    /// the `initial` request and returns the stream id plus the outbound sender.
    pub async fn open_stream_to<S: StreamRequester>(
        &self,
        target: InstanceId,
        handler: S,
        initial: S::Out,
    ) -> anyhow::Result<(StreamId, tokio::sync::mpsc::Sender<StreamMessage>)>
    where
        S::Out: Send + 'static,
    {
        let (id, tx) = self.streams.register_to(handler, Some(target));
        let payload = serde_cbor::to_vec(&initial)?;
        tx.send(StreamMessage::to(id, payload, target)).await?;
        Ok((id, tx))
    }
}

/// How long to wait to retry after an unsuccessful connection attempt.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub enum RetryWait {
    /// An exponentially increasing wait
    Exponential {
        /// Initial wait value
        initial: Duration,

        /// Number of retries required for the total wait to
        /// increase by a factor of the initial value.
        constant: f64,

        /// Maximum wait value
        limit: Option<Duration>,

        /// Number of times waited
        iteration: u32,
    },

    /// A wait period that never changes
    Constant {
        /// Initial wait value
        initial: Duration,

        /// Number of times waited
        iteration: u32,
    },
}

impl Iterator for RetryWait {
    type Item = Duration;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            RetryWait::Exponential {
                initial,
                constant,
                limit,
                iteration,
            } => {
                let value = Duration::from_millis(
                    ((initial.as_millis() as f64)
                        * (initial.as_millis() as f64).powf(*iteration as f64 / *constant))
                        as u64,
                );

                *iteration += 1;

                Some(match limit {
                    // Apply maximum limit
                    Some(l) => min(value, *l),
                    None => value,
                })
            }
            RetryWait::Constant { initial, iteration } => {
                *iteration += 1;
                Some(*initial)
            }
        }
    }
}

impl Default for RetryWait {
    fn default() -> Self {
        Self::Constant {
            initial: Duration::from_millis(4000),
            iteration: 0,
        }
    }
}
