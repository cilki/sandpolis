//! The session driver that backs every [`InstanceConnection`].
//!
//! Both ends of an attachment run the same thing: one task owning the QUIC
//! connection and its control stream (keepalive out, keepalive in), and then
//! a QUIC stream per Sandpolis stream. The first outbound [`StreamMessage`]
//! for a [`StreamId`] opens one and hands it to a writer task; every QUIC
//! stream the peer opens gets a reader task, which learns the stream id from
//! the first message on it and keeps the send half so replies go back the
//! same way. A stream is over on the wire when its QUIC stream is finished,
//! which is what [`StreamRegistry::close`] does here and what a reader seeing
//! the peer's finish turns back into a `close`.
//!
//! Readers dispatch straight into the registry, so one stream's handler being
//! slow holds up that stream and nothing else. Outbound, the driver hands
//! each message to its stream's writer and only waits when that writer's
//! queue is full — a stream with [`WRITER_QUEUE`] frames backed up behind
//! QUIC flow control does stall the others until it drains, which is the one
//! place head-of-line blocking survives.
//!
//! A message marked [`unreliable`](StreamMessage::unreliable) skips all of
//! that: the driver cuts it into datagrams on the spot (see
//! [`super::datagram`]) and one more task puts the peer's back together and
//! dispatches them. It never opens a QUIC stream, so the stream it belongs to
//! must already have been opened by a reliable message.
//!
//! [`StreamMessage`]: super::stream::StreamMessage

use super::datagram::{self, Reassembler};
use super::stream::{StreamId, StreamMessage, StreamRegistry};
use super::webtransport::{Message, Reader, Session, Writer};
use super::{ConnectionData, InstanceConnection, RegisterResponders};
use crate::ClusterId;
use crate::database::Resident;
use crate::realm::RealmName;
use anyhow::{Context as _, Result};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::bytes::Bytes;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};
use wtransport::{Connection, RecvStream, SendStream};

/// How many frames one stream's writer may have queued before the driver
/// waits for it to drain.
const WRITER_QUEUE: usize = 32;

/// The writers for every QUIC stream this side sends on, by Sandpolis stream.
/// Dropping an entry closes its writer's queue, which finishes the stream.
type Writers = Arc<Mutex<HashMap<StreamId, mpsc::Sender<Bytes>>>>;

/// What the reader and writer tasks share with the driver.
#[derive(Clone)]
struct Shared {
    connection: Connection,
    streams: Arc<StreamRegistry>,
    writers: Writers,
    cancel: CancellationToken,
}

impl InstanceConnection {
    /// Drive a session as an `InstanceConnection`.
    ///
    /// The `handlers` slice contains subsystems that will register their stream
    /// responders with the connection's stream registry.
    ///
    /// Works the same on either side of a connection: whoever accepted the
    /// session and whoever dialed it run this identical loop, so streams are
    /// symmetric across the connection.
    pub fn session(
        session: Session,
        data: Resident<ConnectionData>,
        realm: RealmName,
        cluster_id: ClusterId,
        handlers: &[&dyn RegisterResponders],
    ) -> Arc<Self> {
        let cancel = CancellationToken::new();

        // Channel for the StreamRegistry to send outgoing StreamMessages
        let (stream_tx, mut stream_rx) = mpsc::channel::<StreamMessage>(32);
        let streams = Arc::new(StreamRegistry::new(stream_tx));

        // And to say which streams it has closed, so they can be finished.
        let (close_tx, mut close_rx) = mpsc::unbounded_channel::<StreamId>();
        streams.set_close_hook(close_tx);

        // Register responders from all handlers
        for handler in handlers {
            handler.register_responders(&streams);
        }

        let shared = Shared {
            connection: session.connection,
            streams: streams.clone(),
            writers: Default::default(),
            cancel: cancel.clone(),
        };

        // The socket task times its own keepalive, so it writes the connection
        // row the same as the `InstanceConnection` it belongs to.
        let mut latency = super::LatencyProbe::new(data.clone());

        // Every stream the peer opens gets a reader.
        tokio::spawn({
            let shared = shared.clone();
            async move {
                loop {
                    let accepted = tokio::select! {
                        _ = shared.cancel.cancelled() => break,
                        accepted = shared.connection.accept_bi() => accepted,
                    };
                    match accepted {
                        Ok((send, recv)) => spawn_reader(&shared, recv, Some(send), None),
                        // The connection is over; the driver notices the same
                        // on the control stream and winds everything up.
                        Err(_) => break,
                    }
                }
            }
        });

        // And the peer's datagrams are put back together into messages.
        tokio::spawn({
            let shared = shared.clone();
            async move {
                let mut reassembler = Reassembler::default();
                loop {
                    let received = tokio::select! {
                        _ = shared.cancel.cancelled() => break,
                        received = shared.connection.receive_datagram() => received,
                    };
                    let Ok(datagram) = received else {
                        break;
                    };
                    let Some(payload) = reassembler.push(datagram.payload()) else {
                        continue;
                    };
                    let Ok(mut message) = serde_cbor::from_slice::<StreamMessage>(&payload) else {
                        continue;
                    };
                    message.unreliable = true;
                    shared
                        .streams
                        .record_rx(message.stream_id, payload.len() as u64);
                    // Inline rather than spawned, so they're handed on in the
                    // order they arrived; a handler slower than the sender
                    // just means QUIC drops the oldest of what is waiting.
                    shared.streams.dispatch(message).await;
                }
            }
        });

        // Spawn the driver, which owns the control stream and the connection.
        tokio::spawn(async move {
            let (mut control_tx, mut control_rx) = session.control.split();
            let mut next_datagram: u32 = 0;

            let mut keepalive = tokio::time::interval(super::KEEPALIVE_INTERVAL);
            keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last_frame = tokio::time::Instant::now();

            loop {
                tokio::select! {
                    // Prove the peer is still there. The pong answering our
                    // ping is what keeps this from firing, so a peer whose
                    // control stream stops answering is dead however busy its
                    // data streams are.
                    _ = keepalive.tick() => {
                        if last_frame.elapsed() > super::KEEPALIVE_DEADLINE {
                            break;
                        }
                        if control_tx.send(Message::Ping(latency.ping().into())).await.is_err() {
                            break;
                        }
                    }
                    // Handle outgoing stream messages: each goes to its own
                    // stream's writer, opened on first use.
                    Some(msg) = stream_rx.recv() => {
                        let data = serde_cbor::to_vec(&msg).unwrap();

                        if msg.unreliable {
                            match send_unreliable(&shared.connection, &mut next_datagram, &data) {
                                Unreliable::Sent => {
                                    shared.streams.record_tx(msg.stream_id, data.len() as u64);
                                    continue;
                                }
                                Unreliable::Dropped => {
                                    trace!(stream_id = ?msg.stream_id, "Dropped an unreliable message");
                                    continue;
                                }
                                // Goes on the stream like any other.
                                Unreliable::Reliable => {}
                            }
                        }
                        shared.streams.record_tx(msg.stream_id, data.len() as u64);

                        let writer = shared
                            .writers
                            .lock()
                            .unwrap()
                            .entry(msg.stream_id)
                            .or_insert_with(|| spawn_writer(&shared, msg.stream_id, None))
                            .clone();
                        if writer.send(data.into()).await.is_err() {
                            // The writer died with its stream (the peer stopped
                            // it, or the connection is going); the next message
                            // for this stream opens a fresh one.
                            let mut writers = shared.writers.lock().unwrap();
                            if writers.get(&msg.stream_id).is_some_and(|w| w.same_channel(&writer)) {
                                writers.remove(&msg.stream_id);
                            }
                        }
                    }
                    // A stream closed here is finished on the wire: dropping
                    // its writer's queue ends the writer, which finishes the
                    // QUIC stream once what's queued has gone out.
                    Some(stream_id) = close_rx.recv() => {
                        shared.writers.lock().unwrap().remove(&stream_id);
                    }
                    // Handle the control stream
                    msg = control_rx.next() => {
                        last_frame = tokio::time::Instant::now();
                        match msg {
                            // The peer's deadline is waiting on this, so it
                            // goes out ahead of any queued data.
                            Some(Ok(Message::Ping(payload))) => {
                                if control_tx.send(Message::Pong(payload)).await.is_err() {
                                    break;
                                }
                            }
                            Some(Ok(Message::Pong(payload))) => latency.pong(&payload),
                            Some(Ok(_)) => {}
                            Some(Err(_)) | None => break,
                        }
                    }
                    // Handle cancellation
                    _ = shared.cancel.cancelled() => {
                        break;
                    }
                }
            }

            // However the session ended, the connection is over. Cancelling
            // here is what tells the rest of the process: every `is_cancelled`
            // check and every janitor waiting on this token keys off it. The
            // readers and writers key off it too, and the connection closing
            // under them.
            shared.cancel.cancel();
            shared.writers.lock().unwrap().clear();
            shared
                .connection
                .close(wtransport::VarInt::from_u32(0), b"");
        });

        Arc::new(Self {
            data,
            realm,
            cluster_id,
            cancel,
            streams,
        })
    }
}

/// What became of a message offered to [`send_unreliable`].
enum Unreliable {
    /// Every fragment is queued on the connection.
    Sent,
    /// Nothing was sent: QUIC couldn't take the whole message right now.
    Dropped,
    /// The session can't carry this message as datagrams (the peer doesn't
    /// take them, or it's too big), so it has to go on its stream after all.
    Reliable,
}

/// Send an encoded message as datagrams, whole or not at all.
///
/// QUIC makes room for a new datagram by dropping the oldest still waiting to
/// go out, which would leave the peer with pieces of messages it can never
/// finish. So a message is only sent when there is room for all of it, and
/// dropped otherwise: the stream marked it as one that may be lost, and under
/// congestion the next one will be newer anyway.
fn send_unreliable(connection: &Connection, next: &mut u32, data: &[u8]) -> Unreliable {
    let Some(max) = connection.max_datagram_size() else {
        return Unreliable::Reliable;
    };
    let Some(fragments) = datagram::fragment(*next, data, max) else {
        return Unreliable::Reliable;
    };
    *next = next.wrapping_add(1);

    // WebTransport puts the session id in front of each one, which the
    // buffer counts; a varint, so a few bytes at most.
    let needed: usize = fragments.iter().map(|f| f.len() + 8).sum();
    if connection.quic_connection().datagram_send_buffer_space() < needed {
        return Unreliable::Dropped;
    }
    for fragment in fragments {
        // Only if the path MTU shrank since `max` was read, or the connection
        // is going.
        if connection.send_datagram(fragment).is_err() {
            return Unreliable::Dropped;
        }
    }
    Unreliable::Sent
}

/// Start the writer for `stream_id` and return its queue.
///
/// `send` is the send half of a stream the peer opened; without one, the
/// writer opens a new QUIC stream first (and starts a reader on the receive
/// half, since the peer's replies come back on it). The writer runs until its
/// queue closes — the stream was closed here, or the driver ended — and then
/// finishes the stream.
fn spawn_writer(
    shared: &Shared,
    stream_id: StreamId,
    send: Option<SendStream>,
) -> mpsc::Sender<Bytes> {
    let (tx, mut rx) = mpsc::channel::<Bytes>(WRITER_QUEUE);
    let shared = shared.clone();
    tokio::spawn(async move {
        let send = match send {
            Some(send) => send,
            None => match open(&shared.connection).await {
                Ok((send, recv)) => {
                    spawn_reader(&shared, recv, None, Some(stream_id));
                    send
                }
                Err(e) => {
                    debug!(?stream_id, error = %e, "Failed to open a stream");
                    return;
                }
            },
        };

        let mut writer = Writer::new(send);
        while let Some(frame) = rx.recv().await {
            if let Err(e) = writer.send(Message::Binary(frame)).await {
                trace!(?stream_id, error = %e, "Stream write failed");
                return;
            }
        }
        let _ = writer.close().await;
    });
    tx
}

async fn open(connection: &Connection) -> Result<(SendStream, RecvStream)> {
    Ok(connection
        .open_bi()
        .await
        .context("Opening a stream")?
        .await
        .context("Opening a stream")?)
}

/// Read `recv` until the peer finishes it, dispatching each message.
///
/// For a stream the peer opened, `send` is its other half, registered as the
/// writer for whatever stream id the first message carries so replies go back
/// on the same QUIC stream. For a stream this side opened, `known` is the id
/// (the writer already exists), and the peer's messages are expected to match
/// it. Either way, the peer finishing the stream closes it here.
fn spawn_reader(
    shared: &Shared,
    recv: RecvStream,
    mut send: Option<SendStream>,
    known: Option<StreamId>,
) {
    let shared = shared.clone();
    tokio::spawn(async move {
        let mut reader = Reader::new(recv);
        let mut stream_id = known;

        loop {
            let frame = tokio::select! {
                _ = shared.cancel.cancelled() => break,
                frame = reader.next() => frame,
            };
            let payload = match frame {
                Some(Ok(Message::Binary(payload))) => payload,
                // Only data travels on a stream's QUIC stream.
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => break,
            };
            let Ok(message) = serde_cbor::from_slice::<StreamMessage>(&payload) else {
                continue;
            };

            // A QUIC stream carries exactly one Sandpolis stream: the first
            // message names it, and anything else is a confused peer.
            match stream_id {
                None => stream_id = Some(message.stream_id),
                Some(id) if id != message.stream_id => {
                    warn!(
                        expected = ?id,
                        got = ?message.stream_id,
                        "Dropping a message for another stream on this stream"
                    );
                    continue;
                }
                Some(_) => {}
            }

            if let Some(send) = send.take() {
                match shared.writers.lock().unwrap().entry(message.stream_id) {
                    Entry::Vacant(entry) => {
                        entry.insert(spawn_writer(&shared, message.stream_id, Some(send)));
                    }
                    // The peer opened a second stream for an id this side is
                    // already writing on, which only a confused peer does.
                    // Ours stays; this half is finished unused.
                    Entry::Occupied(_) => drop(send),
                }
            }

            shared
                .streams
                .record_rx(message.stream_id, payload.len() as u64);
            shared.streams.dispatch(message).await;
        }

        // The peer finished (or the stream failed, or the connection is
        // going): whatever this side still had for the stream is over too.
        if let Some(stream_id) = stream_id
            && !shared.cancel.is_cancelled()
        {
            shared.writers.lock().unwrap().remove(&stream_id);
            shared.streams.close(stream_id);
        }
    });
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::network::stream::{Relay, Stream, StreamRequester, StreamResponder};
    use crate::network::webtransport::{self, Dialer};
    use crate::realm::{RealmCert, url::ServerUrl};
    use crate::{AgentId, ClientId, InstanceId, ServerId, test_db};
    use anyhow::{Result, anyhow, bail};
    use rustls::server::{ResolvesServerCertUsingSni, WebPkiClientVerifier};
    use serde::{Deserialize, Serialize};
    use std::sync::RwLock;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc::Sender;
    use tokio::time::{Duration, timeout};

    /// A server endpoint on the loopback interface serving one realm the test
    /// minted, and a dialer holding an endpoint certificate from it: exactly
    /// what a server and the instances that attach to it hold.
    struct Harness {
        server_id: ServerId,
        endpoint: wtransport::Endpoint<wtransport::endpoint::endpoint_side::Server>,
        dialer: Arc<Dialer>,
        connections: crate::database::ResidentVec<ConnectionData>,
        _database: crate::database::DatabaseManager,
    }

    impl Harness {
        fn new() -> Result<Self> {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

            let database = test_db!(ConnectionData);
            let connections = database
                .realm(RealmName::default())?
                .resident_vec::<ConnectionData>(())?;

            let server_id = ServerId::random();
            let ca = RealmCert::new_cluster(ClusterId::default(), RealmName::default())?;
            let server_cert = ca.server_cert(server_id)?;

            // What `realm::server::realm_tls_config` builds for a server: the
            // realm CA verifies peers, the server certificate is served by SNI.
            let mut roots = rustls::RootCertStore::empty();
            roots.add(ca.cert.clone().try_into()?)?;
            let builder = rustls::ServerConfig::builder();
            let key = builder.crypto_provider().key_provider.load_private_key(
                server_cert
                    .key
                    .clone()
                    .ok_or_else(|| anyhow!("No server key"))?
                    .try_into()
                    .map_err(|_| anyhow!("Failed to parse key"))?,
            )?;
            let mut sni = ResolvesServerCertUsingSni::new();
            sni.add(
                &server_cert.subject_name()?,
                rustls::sign::CertifiedKey::new(vec![server_cert.cert.clone().try_into()?], key),
            )?;
            let tls = builder
                .with_client_cert_verifier(WebPkiClientVerifier::builder(Arc::new(roots)).build()?)
                .with_cert_resolver(Arc::new(sni));

            let endpoint = webtransport::server_endpoint("127.0.0.1:0".parse()?, tls)?;
            let url: ServerUrl =
                format!("127.0.0.1:{}/default", endpoint.local_addr()?.port()).parse()?;
            let dialer = Dialer::new(&ca.endpoint_cert(&url)?, &url)?;

            Ok(Self {
                server_id,
                endpoint,
                dialer,
                connections,
                _database: database,
            })
        }

        /// Attach `instance` to the server: the handshake both sides run, and
        /// then the driver on each. Returns the server's connection and the
        /// instance's, in that order.
        async fn attach(
            &self,
            instance: InstanceId,
            server_handlers: &[&dyn RegisterResponders],
            instance_handlers: &[&dyn RegisterResponders],
        ) -> Result<(Arc<InstanceConnection>, Arc<InstanceConnection>)> {
            let (server, attached, _) = self
                .attach_quic(instance, server_handlers, instance_handlers)
                .await?;
            Ok((server, attached))
        }

        /// [`attach`](Self::attach), also handing back the instance's QUIC
        /// connection, so a test can see what actually crossed the wire.
        async fn attach_quic(
            &self,
            instance: InstanceId,
            server_handlers: &[&dyn RegisterResponders],
            instance_handlers: &[&dyn RegisterResponders],
        ) -> Result<(Arc<InstanceConnection>, Arc<InstanceConnection>, Connection)> {
            let server_id = self.server_id;
            let accept = async {
                let request = self.endpoint.accept().await.await?;
                let connection = request.accept().await?;
                webtransport::open(connection, server_id.into()).await
            };
            let dial = async {
                let connection = self.dialer.connect([]).await?;
                webtransport::accept(connection).await
            };
            let (server_session, (remote, instance_session)) =
                timeout(Duration::from_secs(10), async {
                    tokio::try_join!(accept, dial)
                })
                .await??;
            assert_eq!(remote, InstanceId::from(server_id));

            let mut row = ConnectionData::scoped(server_id.into());
            row.remote_instance = Some(instance);
            let server = InstanceConnection::session(
                server_session,
                self.connections.push(row)?,
                RealmName::default(),
                ClusterId::default(),
                server_handlers,
            );

            let quic = instance_session.connection.clone();
            let mut row = ConnectionData::scoped(instance);
            row.remote_instance = Some(remote);
            let attached = InstanceConnection::session(
                instance_session,
                self.connections.push(row)?,
                RealmName::default(),
                ClusterId::default(),
                instance_handlers,
            );

            Ok((server, attached, quic))
        }
    }

    /// Wait for `check` to hold, on real time since real sockets are involved.
    async fn until(mut check: impl FnMut() -> bool) {
        for _ in 0..500 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("it never happened");
    }

    #[derive(Serialize, Deserialize)]
    struct Word(String);

    /// Echoes each request back. Counts itself in `live` while it exists, so a
    /// test can tell when the registry let go of it.
    struct EchoResponder {
        live: Arc<AtomicUsize>,
    }

    impl Stream for EchoResponder {
        fn tag() -> u32 {
            0x7e57_0002
        }
    }

    impl StreamResponder for EchoResponder {
        type In = Word;
        type Out = Word;

        async fn on_message(&self, word: Word, sender: Sender<Word>) -> Result<()> {
            sender.send(word).await?;
            Ok(())
        }
    }

    impl Drop for EchoResponder {
        fn drop(&mut self) {
            self.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Registers [`EchoResponder`]s, and knows how many are alive.
    #[derive(Default)]
    struct Echo {
        live: Arc<AtomicUsize>,
    }

    impl Echo {
        fn live(&self) -> usize {
            self.live.load(Ordering::SeqCst)
        }
    }

    impl RegisterResponders for Echo {
        fn register_responders(&self, registry: &StreamRegistry) {
            let live = self.live.clone();
            registry.register_responder(move || {
                live.fetch_add(1, Ordering::SeqCst);
                EchoResponder { live: live.clone() }
            });
        }
    }

    /// Collects what comes back.
    struct EchoRequester {
        replies: mpsc::UnboundedSender<String>,
    }

    impl Stream for EchoRequester {
        fn tag() -> u32 {
            EchoResponder::tag()
        }
    }

    impl StreamRequester for EchoRequester {
        type In = Word;
        type Out = Word;

        async fn new(_: Word, _: Sender<Word>) -> Result<Self> {
            bail!("the tests build this directly")
        }

        async fn on_message(&self, word: Word, _: Sender<Word>) -> Result<()> {
            self.replies.send(word.0)?;
            Ok(())
        }
    }

    /// Open an echo stream on `from`, addressed to `to` if it is to be relayed,
    /// and prove it works with one round trip.
    async fn echo_stream(
        from: &InstanceConnection,
        to: Option<InstanceId>,
        word: &str,
    ) -> Result<(
        StreamId,
        Sender<StreamMessage>,
        mpsc::UnboundedReceiver<String>,
    )> {
        let (replies, mut received) = mpsc::unbounded_channel();
        let requester = EchoRequester { replies };
        let initial = Word(word.to_string());
        let (id, tx) = match to {
            Some(to) => from.open_stream_to(to, requester, initial).await?,
            None => from.open_stream(requester, initial).await?,
        };
        assert_eq!(
            timeout(Duration::from_secs(5), received.recv()).await?,
            Some(word.to_string())
        );
        Ok((id, tx, received))
    }

    /// Either side can open a stream to a responder on the other, and the
    /// bytes are accounted against the stream at both ends.
    #[tokio::test]
    async fn streams_cross_the_session_both_ways() -> Result<()> {
        let harness = Harness::new()?;
        let (server_echo, client_echo) = (Echo::default(), Echo::default());
        let (server, client) = harness
            .attach(ClientId::random().into(), &[&server_echo], &[&client_echo])
            .await?;

        let (id, tx, mut replies) = echo_stream(&client, None, "hello").await?;
        tx.send(StreamMessage::local(
            id,
            serde_cbor::to_vec(&Word("again".into()))?,
        ))
        .await?;
        assert_eq!(
            timeout(Duration::from_secs(5), replies.recv()).await?,
            Some("again".to_string())
        );
        let (rx, sent) = client.streams.traffic(id).expect("accounted on the client");
        assert!(rx > 0 && sent > 0);
        let (rx, sent) = server.streams.traffic(id).expect("accounted on the server");
        assert!(rx > 0 && sent > 0);

        // The other way round, which is what sync does.
        echo_stream(&server, None, "hello").await?;
        assert_eq!(server_echo.live(), 1);
        assert_eq!(client_echo.live(), 1);
        Ok(())
    }

    /// Closing a stream finishes its QUIC stream, which is how the peer learns
    /// to drop the responder; and since every stream has its own, the others
    /// aren't affected.
    #[tokio::test]
    async fn closing_a_stream_finishes_it_on_the_peer() -> Result<()> {
        let harness = Harness::new()?;
        let echo = Echo::default();
        let (_server, client) = harness
            .attach(AgentId::random().into(), &[&echo], &[])
            .await?;

        let (a, _, _) = echo_stream(&client, None, "a").await?;
        let (b, b_tx, mut b_replies) = echo_stream(&client, None, "b").await?;
        assert_eq!(echo.live(), 2);

        client.close_stream(a);
        until(|| echo.live() == 1).await;

        b_tx.send(StreamMessage::local(
            b,
            serde_cbor::to_vec(&Word("still".into()))?,
        ))
        .await?;
        assert_eq!(
            timeout(Duration::from_secs(5), b_replies.recv()).await?,
            Some("still".to_string())
        );

        client.close_stream(b);
        until(|| echo.live() == 0).await;
        Ok(())
    }

    /// A relayed stream is closed on the connection it was relayed to, so the
    /// responder at the far end goes when the requester does.
    #[tokio::test]
    async fn closing_a_relayed_stream_follows_it_to_the_target() -> Result<()> {
        let harness = Harness::new()?;
        let agent_id: InstanceId = AgentId::random().into();
        let echo = Echo::default();

        let (server_client, client) = harness.attach(ClientId::random().into(), &[], &[]).await?;
        let (server_agent, _agent) = harness.attach(agent_id, &[], &[&echo]).await?;

        // What the server does with its inbound connections.
        let inbound = Arc::new(RwLock::new(vec![
            server_client.clone(),
            server_agent.clone(),
        ]));
        let relay = Arc::new(Relay::new(inbound));
        server_client.streams.set_relay(Arc::downgrade(&relay));
        server_agent.streams.set_relay(Arc::downgrade(&relay));

        let (id, _, _) = echo_stream(&client, Some(agent_id), "relayed").await?;
        assert_eq!(echo.live(), 1);

        client.close_stream(id);
        until(|| echo.live() == 0).await;
        Ok(())
    }

    #[derive(Serialize, Deserialize)]
    struct Size(usize);

    #[derive(Serialize, Deserialize)]
    struct Blob(Vec<u8>);

    fn blob(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Answers a [`Size`] with a blob that long, marked as one that may be lost.
    struct BlobResponder;

    impl Stream for BlobResponder {
        fn tag() -> u32 {
            0x7e57_0003
        }
    }

    impl StreamResponder for BlobResponder {
        type In = Size;
        type Out = Blob;

        async fn on_message(&self, size: Size, sender: Sender<Blob>) -> Result<()> {
            sender.send(Blob(blob(size.0))).await?;
            Ok(())
        }

        fn unreliable(_: &Blob) -> bool {
            true
        }
    }

    struct Blobs;

    impl RegisterResponders for Blobs {
        fn register_responders(&self, registry: &StreamRegistry) {
            registry.register_responder(|| BlobResponder);
        }
    }

    struct BlobRequester {
        blobs: mpsc::UnboundedSender<Vec<u8>>,
    }

    impl Stream for BlobRequester {
        fn tag() -> u32 {
            BlobResponder::tag()
        }
    }

    impl StreamRequester for BlobRequester {
        type In = Blob;
        type Out = Size;

        async fn new(_: Size, _: Sender<Size>) -> Result<Self> {
            bail!("the tests build this directly")
        }

        async fn on_message(&self, blob: Blob, _: Sender<Size>) -> Result<()> {
            self.blobs.send(blob.0)?;
            Ok(())
        }
    }

    /// Ask `from` for a blob of `len` bytes, addressed to `to` if it is to be
    /// relayed, and return what arrived.
    async fn fetch_blob(
        from: &InstanceConnection,
        to: Option<InstanceId>,
        len: usize,
    ) -> Result<Vec<u8>> {
        let (blobs, mut received) = mpsc::unbounded_channel();
        let requester = BlobRequester { blobs };
        match to {
            Some(to) => from.open_stream_to(to, requester, Size(len)).await?,
            None => from.open_stream(requester, Size(len)).await?,
        };
        timeout(Duration::from_secs(10), received.recv())
            .await?
            .ok_or_else(|| anyhow!("the stream ended without a blob"))
    }

    fn datagrams_received(connection: &Connection) -> u64 {
        connection.quic_connection().stats().frame_rx.datagram
    }

    /// A response the responder marks unreliable crosses the session as
    /// datagrams — however many it takes — and arrives whole.
    #[tokio::test]
    async fn unreliable_responses_travel_as_datagrams() -> Result<()> {
        let harness = Harness::new()?;
        let (_server, client, quic) = harness
            .attach_quic(ClientId::random().into(), &[&Blobs], &[])
            .await?;

        assert_eq!(fetch_blob(&client, None, 10).await?, blob(10));
        let single = datagrams_received(&quic);
        assert!(single >= 1, "the small blob came as a datagram");

        // Many times the size of a datagram.
        assert_eq!(fetch_blob(&client, None, 20_000).await?, blob(20_000));
        assert!(
            datagrams_received(&quic) - single >= 20_000 / 1500,
            "the large blob came in fragments"
        );
        Ok(())
    }

    /// A relaying server forwards an unreliable response the way it came in,
    /// so it is datagrams on both legs.
    #[tokio::test]
    async fn relayed_unreliable_responses_stay_datagrams() -> Result<()> {
        let harness = Harness::new()?;
        let agent_id: InstanceId = AgentId::random().into();

        let (server_client, client, quic) = harness
            .attach_quic(ClientId::random().into(), &[], &[])
            .await?;
        let (server_agent, _agent) = harness.attach(agent_id, &[], &[&Blobs]).await?;

        let inbound = Arc::new(RwLock::new(vec![
            server_client.clone(),
            server_agent.clone(),
        ]));
        let relay = Arc::new(Relay::new(inbound));
        server_client.streams.set_relay(Arc::downgrade(&relay));
        server_agent.streams.set_relay(Arc::downgrade(&relay));

        assert_eq!(
            fetch_blob(&client, Some(agent_id), 20_000).await?,
            blob(20_000)
        );
        assert!(datagrams_received(&quic) >= 20_000 / 1500);
        Ok(())
    }

    /// A message too big to be worth cutting into datagrams goes on its
    /// stream instead, so marking a response unreliable never makes it
    /// undeliverable.
    #[tokio::test]
    async fn oversized_unreliable_responses_go_reliably() -> Result<()> {
        let harness = Harness::new()?;
        let (_server, client, quic) = harness
            .attach_quic(ClientId::random().into(), &[&Blobs], &[])
            .await?;

        let len = datagram::MAX_FRAGMENTS * 1500;
        assert_eq!(fetch_blob(&client, None, len).await?, blob(len));
        assert_eq!(datagrams_received(&quic), 0);
        Ok(())
    }

    /// The keepalive ping goes out as soon as the session comes up, and the
    /// pong answering it is what measures the round trip, on both sides.
    #[tokio::test]
    async fn pings_on_connect_and_times_the_pong() -> Result<()> {
        let harness = Harness::new()?;
        let (server, client) = harness.attach(AgentId::random().into(), &[], &[]).await?;

        until(|| server.data.read().latency.is_some() && client.data.read().latency.is_some())
            .await;
        Ok(())
    }

    /// Cancelling a connection closes the QUIC connection under the peer, so
    /// the peer's janitors run too rather than waiting out the deadline.
    #[tokio::test]
    async fn cancelling_ends_the_peer_too() -> Result<()> {
        let harness = Harness::new()?;
        let (server, client) = harness.attach(AgentId::random().into(), &[], &[]).await?;

        client.cancel.cancel();
        timeout(Duration::from_secs(5), server.cancel.cancelled()).await?;
        Ok(())
    }
}
