//! Streams provide ephemeral data transfer over time. Operations like file transfers,
//! remote desktop sessions, and shell prompt sessions all run over streams.
//!
//! A stream has two endpoints: a `StreamRequester` and a `StreamResponder`. The
//! `StreamRequester` is responsible for starting the stream and it sends "requests"
//! (one or more than one). The `StreamResponder` is created as a result of the
//! `StreamRequester`'s first request and sends "responses" (one or more than one).

use crate::InstanceId;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::mpsc::Sender;

/// Connections may have multiple streams running concurrently, so this identifier
/// allows each stream to remain separated.
///
/// The first half identifies the stream type and the second half identifies a particular
/// stream within that type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct StreamId(u64);

impl StreamId {
    /// Extract the type tag from a stream ID.
    pub fn tag(&self) -> u32 {
        (self.0 >> 32) as u32
    }
}

/// Encapsulates the stream's local messages.
#[derive(Serialize, Deserialize)]
pub struct StreamMessage {
    pub stream_id: StreamId,
    pub payload: Vec<u8>,

    /// Destination instance for a relayed stream. `None` means the message is
    /// handled by the receiving peer directly (e.g. sync, or responses on their
    /// way back to the origin). `Some(id)` means a server should forward this to
    /// the connection for that instance.
    #[serde(default)]
    pub dst: Option<InstanceId>,

    /// How many servers have already forwarded this message.
    ///
    /// A path may legitimately cross two servers (client → GS → LS → agent), but
    /// a stale or wrong reachability advertisement could otherwise send a message
    /// around a cycle forever. Incremented on each relay hop and dropped past
    /// [`MAX_HOPS`]. Construct messages with [`StreamMessage::local`] or
    /// [`StreamMessage::to`] so this is never forgotten.
    #[serde(default)]
    pub hops: u8,

    /// Whether this message may be lost on the way, in exchange for never
    /// waiting behind the stream's other messages: the transport sends it as
    /// datagrams rather than on the stream's QUIC stream (see
    /// [`super::datagram`]). Set from [`StreamResponder::unreliable`].
    ///
    /// Not part of the encoding, since how a message arrived says which it
    /// was; a relay forwards a message the same way it came in.
    #[serde(skip)]
    pub unreliable: bool,
}

/// The longest relay path the network is expected to need: client → global
/// stratum → local stratum → agent is two hops, so this leaves headroom without
/// letting a routing loop run away.
pub const MAX_HOPS: u8 = 4;

impl StreamMessage {
    /// A message handled directly by the receiving peer.
    pub fn local(stream_id: StreamId, payload: Vec<u8>) -> Self {
        Self::routed(stream_id, payload, None)
    }

    /// A message a server should forward toward `dst`.
    pub fn to(stream_id: StreamId, payload: Vec<u8>, dst: InstanceId) -> Self {
        Self::routed(stream_id, payload, Some(dst))
    }

    /// A message whose destination is already an `Option`, for callers that
    /// carry one around (a stream that may or may not be relayed).
    pub fn routed(stream_id: StreamId, payload: Vec<u8>, dst: Option<InstanceId>) -> Self {
        Self {
            stream_id,
            payload,
            dst,
            hops: 0,
            unreliable: false,
        }
    }

    /// The same message, marked as one that may be lost.
    pub fn with_unreliable(mut self, unreliable: bool) -> Self {
        self.unreliable = unreliable;
        self
    }
}

/// Implemented by stream types to generate unique IDs.
pub trait Stream {
    /// Use `#[derive(Stream)]` from `sandpolis_macros` to implement this.
    fn tag() -> u32
    where
        Self: Sized;
}

/// Initiates a stream and handles responses from the responder.
///
/// The requester is responsible for starting the stream by sending the initial
/// request message. It then receives responses from the `StreamResponder`.
pub trait StreamRequester: Stream + Send + Sync + Sized + 'static {
    /// Input message type (responses from the responder).
    type In: for<'de> Deserialize<'de> + Send;

    /// Output message type (requests to the responder).
    type Out: Serialize;

    /// Create a new requester and send the initial request.
    fn new(_: Self::Out, tx: Sender<Self::Out>) -> impl Future<Output = Result<Self>> + Send;

    /// Called when the stream receives a response from the responder.
    fn on_message(
        &self,
        _: Self::In,
        _tx: Sender<Self::Out>,
    ) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    /// Generate a new unique stream ID for this stream type.
    fn generate_id() -> StreamId
    where
        Self: Sized,
    {
        StreamId(((Self::tag() as u64) << 32) | (rand::random::<u32>() as u64))
    }
}

/// Handles incoming requests from a `StreamRequester` and sends responses.
///
/// A responder is created automatically when the first request arrives for a
/// stream type that has a registered factory.
pub trait StreamResponder: Stream + Send + Sync + 'static {
    /// Input message type (requests from the requester).
    type In: for<'de> Deserialize<'de> + Send;

    /// Output message type (responses to the requester).
    type Out: Serialize;

    /// Called when the stream receives a request from the requester.
    fn on_message(
        &self,
        _: Self::In,
        _: Sender<Self::Out>,
    ) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    /// Whether `response` may be lost on its way to the requester.
    ///
    /// For a response that is worthless once the next one exists, such as a
    /// desktop frame: it travels as datagrams, so it never waits behind the
    /// ones before it, and congestion or loss drops it instead of building a
    /// backlog. Such responses can arrive out of order with respect to the
    /// stream's reliable ones, and not at all. A response too big to cut into
    /// datagrams, or on a session that can't carry them, is sent reliably
    /// anyway.
    fn unreliable(_response: &Self::Out) -> bool
    where
        Self: Sized,
    {
        false
    }
}

/// One encoded message a stream handler sends, and whether it may be lost.
pub(crate) struct RawOut {
    payload: Vec<u8>,
    unreliable: bool,
}

/// Internal object-safe trait for stream handler storage.
/// Both `StreamRequesterWrapper` and `StreamResponderWrapper` implement this.
pub(crate) trait RawStreamHandler: Send + Sync + 'static {
    fn on_receive_raw(
        &self,
        payload: &[u8],
        raw_sender: Sender<RawOut>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
}

/// Object-safe factory trait for creating responder instances.
pub(crate) trait RawResponderFactory: Send + Sync + 'static {
    fn create(&self) -> Arc<dyn RawStreamHandler>;
}

/// Typed wrapper for responder factories.
struct ResponderFactory<R, F>
where
    R: StreamResponder + 'static,
    R::Out: Send + 'static,
    F: Fn() -> R + Send + Sync + 'static,
{
    factory: F,
    _marker: std::marker::PhantomData<R>,
}

impl<R, F> RawResponderFactory for ResponderFactory<R, F>
where
    R: StreamResponder + 'static,
    R::Out: Send + 'static,
    F: Fn() -> R + Send + Sync + 'static,
{
    fn create(&self) -> Arc<dyn RawStreamHandler> {
        Arc::new(StreamResponderWrapper::new((self.factory)()))
    }
}

/// Cumulative byte counts for one stream, as seen by a single connection. The
/// sizes are of the encoded `StreamMessage`, i.e. what actually crossed the
/// socket rather than the decoded payload.
#[derive(Default)]
pub struct StreamCounters {
    /// Bytes received for this stream.
    pub rx: AtomicU64,
    /// Bytes sent for this stream.
    pub tx: AtomicU64,
}

/// Registry for managing active streams on a connection.
///
/// This is transport-agnostic and works with `StreamMessage` payloads.
pub struct StreamRegistry {
    streams: RwLock<HashMap<StreamId, (Arc<dyn RawStreamHandler>, Sender<RawOut>)>>,
    /// Factories for creating responders, keyed by type tag.
    responder_factories: RwLock<HashMap<u32, Box<dyn RawResponderFactory>>>,
    /// Sender for outgoing stream messages.
    tx: Sender<StreamMessage>,
    /// Server-side relay for forwarding streams to other connections. Held as a
    /// `Weak` to avoid a reference cycle (relay -> connections -> registry).
    relay: RwLock<Option<std::sync::Weak<Relay>>>,
    /// Per-stream byte counts. Deliberately scoped to this registry rather than
    /// kept globally, since a stream id is only unique within one connection.
    traffic: RwLock<HashMap<StreamId, Arc<StreamCounters>>>,

    /// Authorization gate for streams the peer initiates, by type tag. `None`
    /// means everything is allowed, which is right for every connection except
    /// a server's inbound client connections — the server installs a gate built
    /// from the logged-in user's permissions.
    gate: RwLock<Option<Arc<dyn Fn(u32) -> bool + Send + Sync>>>,

    /// Told about every [`close`](Self::close), so the transport can finish the
    /// stream on the wire and the peer learns it is over. Unbounded because
    /// `close` is synchronous and a close must never be lost.
    on_close: RwLock<Option<tokio::sync::mpsc::UnboundedSender<StreamId>>>,
}

impl StreamRegistry {
    pub fn new(tx: Sender<StreamMessage>) -> Self {
        Self {
            streams: RwLock::new(HashMap::new()),
            responder_factories: RwLock::new(HashMap::new()),
            tx,
            relay: RwLock::new(None),
            traffic: RwLock::new(HashMap::new()),
            gate: RwLock::new(None),
            on_close: RwLock::new(None),
        }
    }

    /// Install an authorization gate for peer-initiated streams. Messages for a
    /// stream this connection doesn't already know are dropped unless the gate
    /// approves their type tag.
    pub fn set_gate(&self, gate: Arc<dyn Fn(u32) -> bool + Send + Sync>) {
        *self.gate.write().unwrap() = Some(gate);
    }

    /// Have every [`close`](Self::close) reported on `hook`. The transport
    /// installs this so a closed stream is finished on the wire too.
    pub fn set_close_hook(&self, hook: tokio::sync::mpsc::UnboundedSender<StreamId>) {
        *self.on_close.write().unwrap() = Some(hook);
    }

    /// Account `bytes` sent for `stream_id`.
    pub fn record_tx(&self, stream_id: StreamId, bytes: u64) {
        self.counters(stream_id)
            .tx
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// Account `bytes` received for `stream_id`.
    pub fn record_rx(&self, stream_id: StreamId, bytes: u64) {
        self.counters(stream_id)
            .rx
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// Cumulative `(received, sent)` bytes for a stream, if any have been seen.
    pub fn traffic(&self, stream_id: StreamId) -> Option<(u64, u64)> {
        self.traffic
            .read()
            .unwrap()
            .get(&stream_id)
            .map(|c| (c.rx.load(Ordering::Relaxed), c.tx.load(Ordering::Relaxed)))
    }

    fn counters(&self, stream_id: StreamId) -> Arc<StreamCounters> {
        if let Some(counters) = self.traffic.read().unwrap().get(&stream_id) {
            return counters.clone();
        }
        self.traffic
            .write()
            .unwrap()
            .entry(stream_id)
            .or_default()
            .clone()
    }

    /// Attach a relay so unknown streams can be forwarded to other connections.
    /// Only used on the server.
    pub fn set_relay(&self, relay: std::sync::Weak<Relay>) {
        *self.relay.write().unwrap() = Some(relay);
    }

    /// Send a raw message directly to this connection's peer.
    pub async fn send_raw(&self, message: StreamMessage) {
        let _ = self.tx.send(message).await;
    }

    /// Whether `tx` is this registry's own outbound channel, i.e. messages on it
    /// originate from this connection. Used by the relay to avoid routing a
    /// message back to its sender.
    pub fn is_origin(&self, tx: &Sender<StreamMessage>) -> bool {
        self.tx.same_channel(tx)
    }

    /// Register a responder factory for a given stream type.
    /// When an incoming message arrives for an unknown stream ID,
    /// the factory will be used to create a new responder instance.
    pub fn register_responder<R, F>(&self, factory: F)
    where
        R: StreamResponder + 'static,
        R::Out: Send + 'static,
        F: Fn() -> R + Send + Sync + 'static,
    {
        let tag = R::tag();
        let boxed_factory: Box<dyn RawResponderFactory> = Box::new(ResponderFactory {
            factory,
            _marker: std::marker::PhantomData,
        });
        self.responder_factories
            .write()
            .unwrap()
            .insert(tag, boxed_factory);
    }

    /// Handle an incoming `StreamMessage` by dispatching to the appropriate handler.
    /// If no handler exists for the stream ID, attempt to create one using a
    /// registered responder factory.
    pub async fn dispatch(&self, message: StreamMessage) {
        let handler_opt = {
            let streams = self.streams.read().unwrap();
            streams
                .get(&message.stream_id)
                .map(|(handler, response_tx)| (handler.clone(), response_tx.clone()))
        };

        if let Some((handler, response_tx)) = handler_opt {
            handler.on_receive_raw(&message.payload, response_tx).await;
            return;
        }

        // Everything below acts on a stream the peer initiated (a stream this
        // instance opened is already registered above), so it is where the
        // authorization gate applies.
        if let Some(gate) = self.gate.read().unwrap().clone()
            && !gate(message.stream_id.tag())
        {
            tracing::warn!(
                tag = message.stream_id.tag(),
                "Dropping unauthorized stream message"
            );
            return;
        }

        // No local handler. On a server, try to relay to another connection.
        let relay = self.relay.read().unwrap().clone();
        if let Some(relay) = relay.and_then(|r| r.upgrade())
            && relay.route(&message, &self.tx).await
        {
            return;
        }

        // A stream is only ever opened reliably, so an unreliable message for
        // a stream nobody here knows is a straggler: one that arrived after
        // its stream was closed, or overtook the message that opens it.
        if message.unreliable {
            tracing::trace!(
                stream_id = ?message.stream_id,
                "Dropping an unreliable message for an unknown stream"
            );
            return;
        }

        // Otherwise create a responder from a registered factory.
        let type_tag = message.stream_id.tag();
        let factory_opt = {
            let factories = self.responder_factories.read().unwrap();
            factories.get(&type_tag).map(|f| f.create())
        };

        if let Some(handler) = factory_opt {
            // Create channel for response messages from the handler
            let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<RawOut>(32);

            // Register the new responder
            self.streams
                .write()
                .unwrap()
                .insert(message.stream_id, (handler.clone(), response_tx.clone()));

            // Spawn task to forward response bytes as StreamMessages. Responses
            // carry no `dst`: they travel back to whoever opened the stream
            // (either the direct peer, or via the relay's routing table).
            let tx = self.tx.clone();
            let stream_id = message.stream_id;
            tokio::spawn(async move {
                while let Some(out) = response_rx.recv().await {
                    let msg = StreamMessage::local(stream_id, out.payload)
                        .with_unreliable(out.unreliable);
                    if tx.send(msg).await.is_err() {
                        break;
                    }
                }
            });

            // Dispatch the message to the newly created handler
            handler.on_receive_raw(&message.payload, response_tx).await;
        }
    }

    /// Register a stream handler and return a sender for outbound messages.
    pub fn register<S: StreamRequester>(&self, handler: S) -> (StreamId, Sender<StreamMessage>)
    where
        S::Out: Send + 'static,
    {
        self.register_to(handler, None)
    }

    /// Like [`register`](Self::register) but stamps `dst` on every outbound
    /// message for the stream, so a server relays it to the target instance.
    pub fn register_to<S: StreamRequester>(
        &self,
        handler: S,
        dst: Option<InstanceId>,
    ) -> (StreamId, Sender<StreamMessage>)
    where
        S::Out: Send + 'static,
    {
        let id = S::generate_id();

        // Create channel for response messages from the handler
        let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<RawOut>(32);

        // Wrap the typed handler in the object-safe wrapper
        let wrapped: Arc<dyn RawStreamHandler> = Arc::new(StreamRequesterWrapper::new(handler));
        self.streams
            .write()
            .unwrap()
            .insert(id, (wrapped, response_tx));

        // Spawn task to forward the handler's outgoing bytes as StreamMessages.
        let tx = self.tx.clone();
        let stream_id = id;
        tokio::spawn(async move {
            while let Some(out) = response_rx.recv().await {
                let msg = StreamMessage::routed(stream_id, out.payload, dst)
                    .with_unreliable(out.unreliable);
                if tx.send(msg).await.is_err() {
                    break;
                }
            }
        });

        // Create channel for outgoing request messages from the caller. These
        // are pre-built `StreamMessage`s (the caller sets `dst`).
        let tx2 = self.tx.clone();
        let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel::<StreamMessage>(32);
        tokio::spawn(async move {
            while let Some(msg) = msg_rx.recv().await {
                if tx2.send(msg).await.is_err() {
                    break;
                }
            }
        });

        (id, msg_tx)
    }

    /// Remove a stream from the registry, finish it on the wire, and drop any
    /// relay route for it.
    ///
    /// Closing travels the way the stream was opened: on a server, a stream
    /// that was relayed onward is closed on the connection it was relayed to,
    /// so the responder at the far end is dropped when the requester lets go
    /// rather than living until that connection does. Nothing travels the
    /// other way — a responder finishing its side is just that side.
    pub fn close(&self, stream_id: StreamId) {
        self.streams.write().unwrap().remove(&stream_id);
        self.traffic.write().unwrap().remove(&stream_id);

        if let Some(hook) = self.on_close.read().unwrap().as_ref() {
            let _ = hook.send(stream_id);
        }

        let Some(relay) = self.relay.read().unwrap().clone().and_then(|r| r.upgrade()) else {
            return;
        };
        relay.routes.lock().unwrap().remove(&stream_id);

        // Taken out of the table before the next hop is closed, so the close
        // arriving back here from that connection's own cleanup finds nothing
        // to forward and stops.
        let next = relay
            .forward
            .lock()
            .unwrap()
            .remove(&stream_id)
            .and_then(|next| next.upgrade());
        if let Some(next) = next
            && !std::ptr::eq(Arc::as_ptr(&next.streams), self)
        {
            next.streams.close(stream_id);
        }
    }
}

/// Server-side stream router. Forwards messages between a client connection and a
/// target agent connection, keyed by stream id.
pub struct Relay {
    /// All connections the server holds (shared with `NetworkManager::inbound`).
    connections: Arc<RwLock<Vec<Arc<super::InstanceConnection>>>>,
    /// stream id -> the origin connection's outbound sender (for responses).
    routes: Mutex<HashMap<StreamId, Sender<StreamMessage>>>,
    /// stream id -> the connection it was last forwarded to, so closing the
    /// stream can follow it there. Weak, like `reachable`: a connection that
    /// has gone took its streams with it.
    forward: Mutex<HashMap<StreamId, std::sync::Weak<super::InstanceConnection>>>,

    /// Instances reachable *through* a peer rather than directly, learned from
    /// reachability advertisements. Held weakly so a dropped connection stops
    /// being a candidate without needing to be cleaned up first.
    reachable: RwLock<HashMap<InstanceId, std::sync::Weak<super::InstanceConnection>>>,

    /// Default route for targets that are neither directly connected nor
    /// advertised. Set on a local stratum server to point at its global stratum
    /// server; `None` on the global stratum server, which is the end of the line.
    upstream: RwLock<Option<std::sync::Weak<super::InstanceConnection>>>,
}

impl Relay {
    pub fn new(connections: Arc<RwLock<Vec<Arc<super::InstanceConnection>>>>) -> Self {
        Self {
            connections,
            routes: Mutex::new(HashMap::new()),
            forward: Mutex::new(HashMap::new()),
            reachable: RwLock::new(HashMap::new()),
            upstream: RwLock::new(None),
        }
    }

    /// Record which instances are reachable through `via`, replacing whatever
    /// that peer advertised before.
    ///
    /// Only server peers advertise: a local stratum server tells its global
    /// stratum server which agents and clients are connected to it, so the GS can
    /// route to them. Advertising for an instance that is directly connected here
    /// does not displace the direct route — [`next_hop`](Self::next_hop) prefers
    /// direct connections.
    pub fn advertise(&self, via: &Arc<super::InstanceConnection>, instances: &[InstanceId]) {
        let via_id = via.data.read().remote_instance;
        let handle = Arc::downgrade(via);

        let mut reachable = self.reachable.write().unwrap();

        // Drop this peer's previous advertisement so instances that went away
        // stop being routed to it.
        reachable.retain(|_, entry| match entry.upgrade() {
            Some(conn) => conn.data.read().remote_instance != via_id,
            None => false,
        });

        for instance in instances {
            reachable.insert(*instance, handle.clone());
        }

        tracing::debug!(
            via = ?via_id,
            count = instances.len(),
            "Recorded reachability advertisement"
        );
    }

    /// The connection a message for `target` would be relayed through, if the
    /// target is advertised as reachable via a server peer (rather than being
    /// directly connected here). Used to originate a server-opened stream toward
    /// an instance behind another stratum.
    pub fn reachable_via(&self, target: InstanceId) -> Option<Arc<super::InstanceConnection>> {
        self.reachable
            .read()
            .unwrap()
            .get(&target)
            .and_then(|entry| entry.upgrade())
    }

    /// Forget every route advertised through `via` (its connection dropped).
    pub fn withdraw(&self, via: InstanceId) {
        self.reachable
            .write()
            .unwrap()
            .retain(|_, entry| match entry.upgrade() {
                Some(conn) => conn.data.read().remote_instance != Some(via),
                None => false,
            });
    }

    /// Set the default route: anything not directly connected or advertised is
    /// forwarded here.
    ///
    /// A local stratum server points this at its global stratum server, which is
    /// how it reaches instances attached to the GS or behind a sibling LS. The GS
    /// has no upstream, so an unresolvable target there is genuinely unknown.
    pub fn set_upstream(&self, upstream: std::sync::Weak<super::InstanceConnection>) {
        *self.upstream.write().unwrap() = Some(upstream);
    }

    /// Pick the connection to forward a message for `target` through, skipping
    /// the origin connection.
    ///
    /// An instance can hold more than one connection here (a reconnect that
    /// overlaps the old socket), so several may match `target`; excluding the
    /// origin (the sender) routes to another one, never back to the sender.
    fn next_hop(
        &self,
        target: InstanceId,
        origin_tx: &Sender<StreamMessage>,
    ) -> Option<Arc<super::InstanceConnection>> {
        // 1. Directly connected here.
        let direct = self
            .connections
            .read()
            .unwrap()
            .iter()
            .find(|c| {
                c.data.read().remote_instance == Some(target) && !c.streams.is_origin(origin_tx)
            })
            .cloned();
        if direct.is_some() {
            return direct;
        }

        // 2. Advertised as reachable through a server peer (GS -> LS -> agent).
        let advertised = self
            .reachable
            .read()
            .unwrap()
            .get(&target)
            .and_then(|entry| entry.upgrade())
            .filter(|c| !c.streams.is_origin(origin_tx));
        if advertised.is_some() {
            return advertised;
        }

        // 3. Default route upstream (LS -> GS, which resolves it from there).
        self.upstream
            .read()
            .unwrap()
            .as_ref()
            .and_then(|entry| entry.upgrade())
            .filter(|c| !c.streams.is_origin(origin_tx))
    }

    /// Attempt to route an unhandled message. Returns `true` if it was forwarded
    /// (and should not be handled locally).
    async fn route(&self, message: &StreamMessage, origin_tx: &Sender<StreamMessage>) -> bool {
        // Client -> agent: an addressed message. Remember the origin so responses
        // can return, then forward to the target connection.
        if let Some(target) = message.dst {
            if message.hops >= MAX_HOPS {
                tracing::warn!(
                    target = %target,
                    stream_id = ?message.stream_id,
                    hops = message.hops,
                    "Stream message exceeded the hop limit; dropping (routing loop?)"
                );
                return true;
            }

            let Some(conn) = self.next_hop(target, origin_tx) else {
                // Unknown target: swallow it rather than mis-handling locally.
                tracing::warn!(
                    target = %target,
                    stream_id = ?message.stream_id,
                    "No connection for relay target; dropping stream message"
                );
                return true;
            };
            self.routes
                .lock()
                .unwrap()
                .insert(message.stream_id, origin_tx.clone());
            self.forward
                .lock()
                .unwrap()
                .insert(message.stream_id, Arc::downgrade(&conn));
            conn.streams
                .send_raw(StreamMessage {
                    stream_id: message.stream_id,
                    payload: message.payload.clone(),
                    dst: Some(target),
                    hops: message.hops + 1,
                    unreliable: message.unreliable,
                })
                .await;
            return true;
        }

        // Agent -> client: a response on a relayed stream goes back to its origin.
        let origin = self.routes.lock().unwrap().get(&message.stream_id).cloned();
        if let Some(origin) = origin {
            let _ = origin
                .send(
                    StreamMessage::local(message.stream_id, message.payload.clone())
                        .with_unreliable(message.unreliable),
                )
                .await;
            return true;
        }

        false
    }
}

/// Wrapper that adapts a typed `StreamRequester` to the object-safe `RawStreamHandler`.
pub(crate) struct StreamRequesterWrapper<T: StreamRequester> {
    handler: T,
}

impl<T: StreamRequester> StreamRequesterWrapper<T> {
    pub fn new(handler: T) -> Self {
        Self { handler }
    }
}

impl<T: StreamRequester> RawStreamHandler for StreamRequesterWrapper<T>
where
    T::In: Send,
    T::Out: Send + 'static,
{
    fn on_receive_raw(
        &self,
        payload: &[u8],
        raw_sender: Sender<RawOut>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let msg = match serde_cbor::from_slice::<T::In>(payload) {
            Ok(msg) => msg,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to decode stream response");
                return Box::pin(async {});
            }
        };

        // Create a typed sender that serializes to raw bytes
        let (typed_tx, mut typed_rx) = tokio::sync::mpsc::channel::<T::Out>(32);

        // Spawn task to forward typed messages to raw sender. Requests are
        // always reliable: they're what opens the stream and drives it.
        tokio::spawn(async move {
            while let Some(typed_msg) = typed_rx.recv().await {
                if let Ok(payload) = serde_cbor::to_vec(&typed_msg)
                    && raw_sender
                        .send(RawOut {
                            payload,
                            unreliable: false,
                        })
                        .await
                        .is_err()
                {
                    break;
                }
            }
        });

        Box::pin(async move {
            if let Err(e) = self.handler.on_message(msg, typed_tx).await {
                tracing::warn!(error = %e, "Stream requester failed to handle a response");
            }
        })
    }
}

/// Wrapper that adapts a typed `StreamResponder` to the object-safe `RawStreamHandler`.
pub(crate) struct StreamResponderWrapper<T: StreamResponder> {
    handler: T,
}

impl<T: StreamResponder> StreamResponderWrapper<T> {
    pub fn new(handler: T) -> Self {
        Self { handler }
    }
}

impl<T: StreamResponder> RawStreamHandler for StreamResponderWrapper<T>
where
    T::In: Send,
    T::Out: Send + 'static,
{
    fn on_receive_raw(
        &self,
        payload: &[u8],
        raw_sender: Sender<RawOut>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let msg = match serde_cbor::from_slice::<T::In>(payload) {
            Ok(msg) => msg,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to decode stream request");
                return Box::pin(async {});
            }
        };

        // Create a typed sender that serializes to raw bytes
        let (typed_tx, mut typed_rx) = tokio::sync::mpsc::channel::<T::Out>(32);

        // Spawn task to forward typed messages to raw sender
        tokio::spawn(async move {
            while let Some(typed_msg) = typed_rx.recv().await {
                let unreliable = T::unreliable(&typed_msg);
                if let Ok(payload) = serde_cbor::to_vec(&typed_msg)
                    && raw_sender
                        .send(RawOut {
                            payload,
                            unreliable,
                        })
                        .await
                        .is_err()
                {
                    break;
                }
            }
        });

        Box::pin(async move {
            if let Err(e) = self.handler.on_message(msg, typed_tx).await {
                tracing::warn!(error = %e, "Stream responder failed to handle a request");
            }
        })
    }
}

/// Declares what a client must be granted to open streams of a given type,
/// collected with `inventory` from every layer crate. A stream type with no
/// declaration is denied to clients outright, so anything new fails closed.
///
/// The tag comes from `sandpolis_macros::stream_tag!`, invoked in the crate
/// that derives `Stream` for the type, so the declaration doesn't depend on
/// the stream type itself being compiled into this build.
pub struct StreamPermission {
    pub tag: u32,

    /// The permission required (e.g. `"shell:session"`), or `None` for
    /// infrastructure every authenticated client may use (sync, ping).
    pub permission: Option<&'static str>,
}

impl StreamPermission {
    pub const fn require(tag: u32, permission: &'static str) -> Self {
        Self {
            tag,
            permission: Some(permission),
        }
    }

    pub const fn open(tag: u32) -> Self {
        Self { tag, permission: None }
    }
}

inventory::collect!(StreamPermission);

// Sync subscriptions and pings are how a client is a client at all, so they are
// open to every authenticated connection.
inventory::submit! {
    StreamPermission::open(sandpolis_macros::stream_tag!(Sync))
}
inventory::submit! {
    StreamPermission::open(sandpolis_macros::stream_tag!(PingStream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Serialize, Deserialize)]
    struct TestMessage {
        value: usize,
    }

    #[derive(Serialize, Deserialize)]
    struct TestResponse;

    #[derive(Serialize, Deserialize)]
    struct TestRequest;

    struct TestStreamResponder {
        received_count: Arc<AtomicUsize>,
    }

    impl Stream for TestStreamResponder {
        fn tag() -> u32 {
            0x12345678
        }
    }

    impl StreamResponder for TestStreamResponder {
        type In = TestMessage;
        type Out = TestResponse;

        async fn on_message(&self, message: Self::In, _sender: Sender<Self::Out>) -> Result<()> {
            self.received_count
                .fetch_add(message.value, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_stream_responder_receives_messages() {
        let received_count = Arc::new(AtomicUsize::new(0));
        let stream = TestStreamResponder {
            received_count: received_count.clone(),
        };
        let wrapper = StreamResponderWrapper::new(stream);

        let (tx, _rx) = tokio::sync::mpsc::channel::<RawOut>(32);
        let payload = serde_cbor::to_vec(&TestMessage { value: 42 }).unwrap();
        wrapper.on_receive_raw(&payload, tx.clone()).await;
        wrapper.on_receive_raw(&payload, tx).await;

        // Give time for async processing
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        assert_eq!(received_count.load(Ordering::SeqCst), 84);
    }

    /// An unreliable message never opens a stream: one for a stream nobody
    /// knows is a straggler from a closed stream, and must not bring a
    /// responder back to life. The same message sent reliably does open one.
    #[tokio::test]
    async fn unreliable_messages_never_open_streams() {
        let received_count = Arc::new(AtomicUsize::new(0));
        let (tx, _rx) = tokio::sync::mpsc::channel(32);
        let registry = StreamRegistry::new(tx);
        registry.register_responder({
            let received_count = received_count.clone();
            move || TestStreamResponder {
                received_count: received_count.clone(),
            }
        });

        let id = StreamId((TestStreamResponder::tag() as u64) << 32 | 1);
        let payload = serde_cbor::to_vec(&TestMessage { value: 1 }).unwrap();

        registry
            .dispatch(StreamMessage::local(id, payload.clone()).with_unreliable(true))
            .await;
        assert_eq!(received_count.load(Ordering::SeqCst), 0);
        assert!(registry.streams.read().unwrap().is_empty());

        registry.dispatch(StreamMessage::local(id, payload)).await;
        assert_eq!(received_count.load(Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod relay_tests {
    use super::*;
    use crate::id::{AgentId, ClientId, ServerId};
    use crate::network::{ConnectionData, InstanceConnection};
    use crate::realm::RealmName;
    use crate::{ClusterId, InstanceId, test_db};
    use sandpolis_macros::Stream;
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};
    use tokio_util::sync::CancellationToken;

    #[derive(Serialize, Deserialize)]
    struct RelayPing(u64);
    #[derive(Serialize, Deserialize)]
    struct RelayPong(u64);

    #[derive(Stream, Default)]
    struct RelayEchoResponder;
    impl StreamResponder for RelayEchoResponder {
        type In = RelayPing;
        type Out = RelayPong;
        async fn on_message(&self, ping: Self::In, sender: Sender<Self::Out>) -> Result<()> {
            sender.send(RelayPong(ping.0 * 2)).await?;
            Ok(())
        }
    }

    #[derive(Stream)]
    struct RelayEchoRequester {
        result: mpsc::Sender<u64>,
    }
    impl StreamRequester for RelayEchoRequester {
        type In = RelayPong;
        type Out = RelayPing;
        async fn new(_: Self::Out, _: Sender<Self::Out>) -> Result<Self> {
            anyhow::bail!("constructed directly")
        }
        async fn on_message(&self, pong: Self::In, _: Sender<Self::Out>) -> Result<()> {
            let _ = self.result.send(pong.0).await;
            Ok(())
        }
    }

    fn pump(mut rx: mpsc::Receiver<StreamMessage>, dst: Arc<StreamRegistry>) {
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                dst.dispatch(msg).await;
            }
        });
    }

    /// Wire a client and an agent to a relaying server and run one echo
    /// round-trip. The ids may be equal, which is what two connections to the
    /// same instance look like.
    async fn relay_echo_roundtrip(
        agent_id: InstanceId,
        client_id: InstanceId,
    ) -> anyhow::Result<()> {
        let db = test_db!(ConnectionData);
        let realm = db.realm(RealmName::default())?;
        let conns = realm.resident_vec::<ConnectionData>(())?;

        // Channels named by the dispatch they feed.
        let (c2s_tx, c2s_rx) = mpsc::channel(32); // client -> server (client-facing)
        let (s2c_tx, s2c_rx) = mpsc::channel(32); // server -> client
        let (s2a_tx, s2a_rx) = mpsc::channel(32); // server -> agent
        let (a2s_tx, a2s_rx) = mpsc::channel(32); // agent -> server (agent-facing)

        let client_reg = Arc::new(StreamRegistry::new(c2s_tx));
        let server_client_reg = Arc::new(StreamRegistry::new(s2c_tx));
        let server_agent_reg = Arc::new(StreamRegistry::new(s2a_tx));
        let agent_reg = Arc::new(StreamRegistry::new(a2s_tx));

        agent_reg.register_responder(RelayEchoResponder::default);

        // Server connections (so the relay can find the agent by instance id).
        let mut agent_data = ConnectionData::scoped(InstanceId::from(ServerId::random()));
        agent_data.remote_instance = Some(agent_id);
        let mut client_data = ConnectionData::scoped(InstanceId::from(ServerId::random()));
        client_data.remote_instance = Some(client_id);
        let agent_conn = Arc::new(InstanceConnection {
            data: conns.push(agent_data)?,
            realm: RealmName::default(),
            cluster_id: ClusterId::default(),
            cancel: CancellationToken::new(),
            streams: server_agent_reg.clone(),
        });
        let client_conn = Arc::new(InstanceConnection {
            data: conns.push(client_data)?,
            realm: RealmName::default(),
            cluster_id: ClusterId::default(),
            cancel: CancellationToken::new(),
            streams: server_client_reg.clone(),
        });

        let connections = Arc::new(RwLock::new(vec![agent_conn, client_conn]));
        let relay = Arc::new(Relay::new(connections));
        server_client_reg.set_relay(Arc::downgrade(&relay));
        server_agent_reg.set_relay(Arc::downgrade(&relay));

        pump(c2s_rx, server_client_reg.clone());
        pump(s2c_rx, client_reg.clone());
        pump(s2a_rx, agent_reg.clone());
        pump(a2s_rx, server_agent_reg.clone());

        // Client opens a stream addressed to the agent.
        let (result_tx, mut result_rx) = mpsc::channel(8);
        let (id, tx) =
            client_reg.register_to(RelayEchoRequester { result: result_tx }, Some(agent_id));
        tx.send(StreamMessage::to(
            id,
            serde_cbor::to_vec(&RelayPing(21))?,
            agent_id,
        ))
        .await?;

        let got = timeout(Duration::from_secs(2), result_rx.recv())
            .await?
            .expect("relayed response");
        assert_eq!(got, 42);

        Ok(())
    }

    /// A client opens a stream addressed to an agent; the server relays the
    /// request to the agent and the agent's response back to the client.
    #[tokio::test]
    async fn relays_client_to_agent_and_back() -> anyhow::Result<()> {
        relay_echo_roundtrip(
            InstanceId::from(AgentId::random()),
            InstanceId::from(ClientId::random()),
        )
        .await
    }

    /// Same round-trip when both connections carry the same peer id, which
    /// happens whenever an instance holds two sockets at once (a reconnect that
    /// overlaps the old one). The relay must exclude the origin connection and
    /// route to the other one.
    #[tokio::test]
    async fn relays_with_duplicate_instance_id() -> anyhow::Result<()> {
        let shared = InstanceId::from(AgentId::random());
        relay_echo_roundtrip(shared, shared).await
    }

    /// Build a server-side connection wrapping `reg`, whose peer is `peer`.
    fn conn(
        conns: &crate::database::ResidentVec<ConnectionData>,
        reg: &Arc<StreamRegistry>,
        peer: InstanceId,
    ) -> anyhow::Result<Arc<InstanceConnection>> {
        let mut data = ConnectionData::scoped(InstanceId::from(ServerId::random()));
        data.remote_instance = Some(peer);
        Ok(Arc::new(InstanceConnection {
            data: conns.push(data)?,
            realm: RealmName::default(),
            cluster_id: ClusterId::default(),
            cancel: CancellationToken::new(),
            streams: reg.clone(),
        }))
    }

    /// A client attached to the global stratum server reaches an agent attached
    /// to a local stratum server: two relay hops out, two back.
    ///
    /// The client addresses the agent by id alone — it never learns that the
    /// agent is a hop away.
    #[tokio::test]
    async fn relays_across_two_strata() -> anyhow::Result<()> {
        let db = test_db!(ConnectionData);
        let realm = db.realm(RealmName::default())?;
        let conns = realm.resident_vec::<ConnectionData>(())?;

        let client_id = InstanceId::from(ClientId::random());
        let ls_id = InstanceId::from(ServerId::random());
        let agent_id = InstanceId::from(AgentId::random());

        let (client_out, client_out_rx) = mpsc::channel(32);
        let (gs_to_client, gs_to_client_rx) = mpsc::channel(32);
        let (gs_to_ls, gs_to_ls_rx) = mpsc::channel(32);
        let (ls_to_gs, ls_to_gs_rx) = mpsc::channel(32);
        let (ls_to_agent, ls_to_agent_rx) = mpsc::channel(32);
        let (agent_out, agent_out_rx) = mpsc::channel(32);

        let client_reg = Arc::new(StreamRegistry::new(client_out));
        let gs_client_reg = Arc::new(StreamRegistry::new(gs_to_client));
        let gs_ls_reg = Arc::new(StreamRegistry::new(gs_to_ls));
        let ls_gs_reg = Arc::new(StreamRegistry::new(ls_to_gs));
        let ls_agent_reg = Arc::new(StreamRegistry::new(ls_to_agent));
        let agent_reg = Arc::new(StreamRegistry::new(agent_out));

        agent_reg.register_responder(RelayEchoResponder::default);

        // Global stratum: the client and the local stratum server attach here.
        let gs_client_conn = conn(&conns, &gs_client_reg, client_id)?;
        let gs_ls_conn = conn(&conns, &gs_ls_reg, ls_id)?;
        let gs_relay = Arc::new(Relay::new(Arc::new(RwLock::new(vec![
            gs_client_conn,
            gs_ls_conn.clone(),
        ]))));
        gs_client_reg.set_relay(Arc::downgrade(&gs_relay));
        gs_ls_reg.set_relay(Arc::downgrade(&gs_relay));

        // Local stratum: only the agent attaches here; the upstream link is the
        // default route rather than a member of `connections`.
        let ls_agent_conn = conn(&conns, &ls_agent_reg, agent_id)?;
        let ls_upstream_conn = conn(&conns, &ls_gs_reg, ls_id)?;
        let ls_relay = Arc::new(Relay::new(Arc::new(RwLock::new(vec![ls_agent_conn]))));
        ls_relay.set_upstream(Arc::downgrade(&ls_upstream_conn));
        ls_agent_reg.set_relay(Arc::downgrade(&ls_relay));
        ls_gs_reg.set_relay(Arc::downgrade(&ls_relay));

        // The local stratum server tells the global one which instances it can
        // reach. Without this the GS has no route to the agent.
        gs_relay.advertise(&gs_ls_conn, &[agent_id]);

        pump(client_out_rx, gs_client_reg.clone());
        pump(gs_to_client_rx, client_reg.clone());
        pump(gs_to_ls_rx, ls_gs_reg.clone());
        pump(ls_to_gs_rx, gs_ls_reg.clone());
        pump(ls_to_agent_rx, agent_reg.clone());
        pump(agent_out_rx, ls_agent_reg.clone());

        let (result_tx, mut result_rx) = mpsc::channel(8);
        let (id, tx) =
            client_reg.register_to(RelayEchoRequester { result: result_tx }, Some(agent_id));
        tx.send(StreamMessage::to(
            id,
            serde_cbor::to_vec(&RelayPing(21))?,
            agent_id,
        ))
        .await?;

        let got = timeout(Duration::from_secs(2), result_rx.recv())
            .await?
            .expect("response relayed back across both strata");
        assert_eq!(got, 42);

        Ok(())
    }

    /// An instance that is neither attached here nor advertised goes to the
    /// default route, which is how a local stratum server reaches the rest of
    /// the estate without knowing anything about it.
    #[tokio::test]
    async fn unknown_target_goes_upstream() -> anyhow::Result<()> {
        let db = test_db!(ConnectionData);
        let realm = db.realm(RealmName::default())?;
        let conns = realm.resident_vec::<ConnectionData>(())?;

        let (peer_out, _peer_rx) = mpsc::channel(32);
        let (upstream_out, mut upstream_rx) = mpsc::channel(32);
        let peer_reg = Arc::new(StreamRegistry::new(peer_out));
        let upstream_reg = Arc::new(StreamRegistry::new(upstream_out));

        let peer_conn = conn(&conns, &peer_reg, InstanceId::from(AgentId::random()))?;
        let upstream_conn = conn(
            &conns,
            &upstream_reg,
            InstanceId::from(ServerId::random()),
        )?;

        let relay = Arc::new(Relay::new(Arc::new(RwLock::new(vec![peer_conn]))));
        relay.set_upstream(Arc::downgrade(&upstream_conn));

        // A registry for some third connection that receives the message.
        let (origin_out, _origin_rx) = mpsc::channel(32);
        let origin_reg = Arc::new(StreamRegistry::new(origin_out));
        origin_reg.set_relay(Arc::downgrade(&relay));

        let stranger = InstanceId::from(AgentId::random());
        origin_reg
            .dispatch(StreamMessage::to(
                RelayEchoRequester::generate_id(),
                vec![1, 2, 3],
                stranger,
            ))
            .await;

        let forwarded = timeout(Duration::from_secs(2), upstream_rx.recv())
            .await?
            .expect("forwarded to the default route");
        assert_eq!(forwarded.dst, Some(stranger));
        assert_eq!(forwarded.hops, 1, "the hop count must advance");

        Ok(())
    }

    /// A directly attached instance is preferred over an advertised route, so a
    /// stale advertisement can't capture traffic for a local peer.
    #[tokio::test]
    async fn direct_route_beats_advertised() -> anyhow::Result<()> {
        let db = test_db!(ConnectionData);
        let realm = db.realm(RealmName::default())?;
        let conns = realm.resident_vec::<ConnectionData>(())?;

        let agent_id = InstanceId::from(AgentId::random());

        let (direct_out, mut direct_rx) = mpsc::channel(32);
        let (advertised_out, mut advertised_rx) = mpsc::channel(32);
        let direct_reg = Arc::new(StreamRegistry::new(direct_out));
        let advertised_reg = Arc::new(StreamRegistry::new(advertised_out));

        let direct_conn = conn(&conns, &direct_reg, agent_id)?;
        let peer_conn = conn(
            &conns,
            &advertised_reg,
            InstanceId::from(ServerId::random()),
        )?;

        let relay = Arc::new(Relay::new(Arc::new(RwLock::new(vec![direct_conn]))));
        relay.advertise(&peer_conn, &[agent_id]);

        let (origin_out, _origin_rx) = mpsc::channel(32);
        let origin_reg = Arc::new(StreamRegistry::new(origin_out));
        origin_reg.set_relay(Arc::downgrade(&relay));

        origin_reg
            .dispatch(StreamMessage::to(
                RelayEchoRequester::generate_id(),
                vec![7],
                agent_id,
            ))
            .await;

        timeout(Duration::from_secs(2), direct_rx.recv())
            .await?
            .expect("delivered over the direct connection");
        assert!(
            advertised_rx.try_recv().is_err(),
            "must not also go via the advertised route"
        );

        Ok(())
    }

    /// A fresh advertisement replaces the previous one, so an instance that left
    /// a local stratum server stops being routed there.
    #[tokio::test]
    async fn readvertising_drops_departed_instances() -> anyhow::Result<()> {
        let db = test_db!(ConnectionData);
        let realm = db.realm(RealmName::default())?;
        let conns = realm.resident_vec::<ConnectionData>(())?;

        let departed = InstanceId::from(AgentId::random());
        let stayed = InstanceId::from(AgentId::random());

        let (peer_out, mut peer_rx) = mpsc::channel(32);
        let peer_reg = Arc::new(StreamRegistry::new(peer_out));
        let peer_conn = conn(&conns, &peer_reg, InstanceId::from(ServerId::random()))?;

        let relay = Arc::new(Relay::new(Arc::new(RwLock::new(vec![]))));
        relay.advertise(&peer_conn, &[departed, stayed]);
        relay.advertise(&peer_conn, &[stayed]);

        let (origin_out, _origin_rx) = mpsc::channel(32);
        let origin_reg = Arc::new(StreamRegistry::new(origin_out));
        origin_reg.set_relay(Arc::downgrade(&relay));

        // No route and no default route: the message is dropped, not forwarded.
        origin_reg
            .dispatch(StreamMessage::to(
                RelayEchoRequester::generate_id(),
                vec![1],
                departed,
            ))
            .await;
        assert!(
            peer_rx.try_recv().is_err(),
            "the departed instance must no longer be routed to this peer"
        );

        origin_reg
            .dispatch(StreamMessage::to(
                RelayEchoRequester::generate_id(),
                vec![1],
                stayed,
            ))
            .await;
        timeout(Duration::from_secs(2), peer_rx.recv())
            .await?
            .expect("the still-attached instance is routed");

        Ok(())
    }

    /// A message that has already crossed the maximum number of servers is
    /// dropped rather than forwarded again.
    #[tokio::test]
    async fn hop_limit_breaks_routing_loops() -> anyhow::Result<()> {
        let db = test_db!(ConnectionData);
        let realm = db.realm(RealmName::default())?;
        let conns = realm.resident_vec::<ConnectionData>(())?;

        let target = InstanceId::from(AgentId::random());
        let (next_out, mut next_rx) = mpsc::channel(32);
        let next_reg = Arc::new(StreamRegistry::new(next_out));
        let next_conn = conn(&conns, &next_reg, target)?;

        let relay = Arc::new(Relay::new(Arc::new(RwLock::new(vec![next_conn]))));
        let (origin_out, _origin_rx) = mpsc::channel(32);
        let origin_reg = Arc::new(StreamRegistry::new(origin_out));
        origin_reg.set_relay(Arc::downgrade(&relay));

        let mut message = StreamMessage::to(RelayEchoRequester::generate_id(), vec![1], target);
        message.hops = MAX_HOPS;
        origin_reg.dispatch(message).await;

        assert!(
            next_rx.try_recv().is_err(),
            "a message at the hop limit must be dropped, not forwarded"
        );

        Ok(())
    }
}
