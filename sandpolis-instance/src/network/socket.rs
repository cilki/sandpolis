//! The websocket driver that backs every [`InstanceConnection`].
//!
//! Both ends of a connection run the same loop: keepalive out, keepalive in,
//! outbound [`StreamMessage`]s encoded as binary frames, inbound binary frames
//! decoded and dispatched. The only difference between the accept side
//! ([`super::server`], on `axum`) and the dial side ([`super::client`], on
//! `reqwest_websocket`) is the message type their socket speaks, which is what
//! [`SocketMessage`] abstracts.

use super::{
    ConnectionData, InstanceConnection, RegisterResponders,
    stream::{StreamMessage, StreamRegistry},
};
use crate::ClusterId;
use crate::database::Resident;
use crate::realm::RealmName;
use futures_util::{SinkExt, StreamExt, sink::Sink, stream::Stream};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// One incoming frame, as the socket driver sees it.
pub enum Frame<'a> {
    /// Application data: one cbor-encoded [`StreamMessage`].
    Binary(&'a [u8]),
    Ping(&'a [u8]),
    Pong(&'a [u8]),
    /// Text, close, or anything else the driver takes no action on.
    Other,
}

/// What the socket driver needs of a websocket message type.
///
/// Implemented for `axum::extract::ws::Message` on the accept side and
/// `reqwest_websocket::Message` on the dial side. The two enums are unrelated
/// types describing the same frames, and this is the whole of the difference
/// between driving one and driving the other.
pub trait SocketMessage: Send + 'static {
    fn binary(payload: Vec<u8>) -> Self;
    fn ping(payload: Vec<u8>) -> Self;
    fn pong(payload: Vec<u8>) -> Self;

    /// Classify an incoming message, borrowing its payload.
    fn frame(&self) -> Frame<'_>;
}

impl InstanceConnection {
    /// Wrap a websocket with an `InstanceConnection`.
    ///
    /// The `handlers` slice contains subsystems that will register their stream
    /// responders with the connection's stream registry.
    ///
    /// Works the same on either side of a connection: whoever accepted the
    /// socket and whoever dialed it run this identical loop, so streams are
    /// symmetric across the connection.
    pub fn socket<M, S, E>(
        socket: S,
        data: Resident<ConnectionData>,
        realm: RealmName,
        cluster_id: ClusterId,
        handlers: &[&dyn RegisterResponders],
    ) -> Arc<Self>
    where
        M: SocketMessage,
        E: Send + 'static,
        S: Stream<Item = Result<M, E>> + Sink<M> + Send + 'static,
    {
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // Channel for the StreamRegistry to send outgoing StreamMessages
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel::<StreamMessage>(32);
        let streams = Arc::new(StreamRegistry::new(stream_tx));
        let streams_clone = streams.clone();

        // Register responders from all handlers
        for handler in handlers {
            handler.register_responders(&streams);
        }

        // The socket task times its own keepalive, so it writes the connection
        // row the same as the `InstanceConnection` it belongs to.
        let mut latency = super::LatencyProbe::new(data.clone());

        // Spawn task that owns the actual socket
        tokio::spawn(async move {
            let (mut ws_tx, mut ws_rx) = socket.split();

            let mut keepalive = tokio::time::interval(super::KEEPALIVE_INTERVAL);
            keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last_frame = tokio::time::Instant::now();

            loop {
                tokio::select! {
                    // Prove the peer is still there. Tungstenite answers a ping
                    // itself, so any frame arriving is enough to keep this from
                    // firing — no cooperation needed from the other end.
                    _ = keepalive.tick() => {
                        if last_frame.elapsed() > super::KEEPALIVE_DEADLINE {
                            break;
                        }
                        if ws_tx.send(M::ping(latency.ping())).await.is_err() {
                            break;
                        }
                    }
                    // Handle outgoing stream messages
                    Some(msg) = stream_rx.recv() => {
                        let data = serde_cbor::to_vec(&msg).unwrap();
                        streams_clone.record_tx(msg.stream_id, data.len() as u64);
                        if ws_tx.send(M::binary(data)).await.is_err() {
                            break;
                        }
                    }
                    // Handle incoming messages from the socket
                    msg = ws_rx.next() => {
                        // Any frame at all, including the pong answering our
                        // ping, says the peer is alive.
                        last_frame = tokio::time::Instant::now();
                        match msg {
                            Some(Ok(msg)) => match msg.frame() {
                                Frame::Binary(data) => {
                                    if let Ok(message) = serde_cbor::from_slice::<StreamMessage>(data) {
                                        streams_clone.record_rx(message.stream_id, data.len() as u64);
                                        streams_clone.dispatch(message).await;
                                    }
                                }
                                // Tungstenite queues a pong itself, but only
                                // flushes it on the next write; answering here
                                // guarantees one, which is what the peer's
                                // deadline is waiting for.
                                Frame::Ping(payload) => {
                                    let pong = M::pong(payload.to_vec());
                                    if ws_tx.send(pong).await.is_err() {
                                        break;
                                    }
                                }
                                Frame::Pong(payload) => latency.pong(payload),
                                Frame::Other => {}
                            },
                            Some(Err(_)) | None => break,
                        }
                    }
                    // Handle cancellation
                    _ = cancel_clone.cancelled() => {
                        break;
                    }
                }
            }

            // However the socket ended, the connection is over. Cancelling here
            // is what tells the rest of the process: every `is_cancelled` check
            // and every janitor waiting on this token keys off it.
            cancel_clone.cancel();
        });

        Arc::new(Self {
            data,
            realm,
            cluster_id,
            cancel,
            streams,
            poll: Default::default(),
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::network::stream::{StreamId, StreamRequester};
    use crate::test_db;
    use anyhow::Result;
    use futures::channel::mpsc;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::time::{Duration, timeout};

    /// Stand-in for the two real websocket message types.
    #[derive(Debug, PartialEq, Eq)]
    enum TestMessage {
        Binary(Vec<u8>),
        Ping(Vec<u8>),
        Pong(Vec<u8>),
        #[allow(dead_code)]
        Text(String),
    }

    impl SocketMessage for TestMessage {
        fn binary(payload: Vec<u8>) -> Self {
            Self::Binary(payload)
        }
        fn ping(payload: Vec<u8>) -> Self {
            Self::Ping(payload)
        }
        fn pong(payload: Vec<u8>) -> Self {
            Self::Pong(payload)
        }
        fn frame(&self) -> Frame<'_> {
            match self {
                Self::Binary(data) => Frame::Binary(data),
                Self::Ping(data) => Frame::Ping(data),
                Self::Pong(data) => Frame::Pong(data),
                Self::Text(_) => Frame::Other,
            }
        }
    }

    /// The transport's error type, which the driver only ever matches on.
    #[derive(Debug)]
    struct TestError;

    /// A socket whose two directions are channels the test holds the far end of.
    struct TestSocket {
        incoming: mpsc::Receiver<Result<TestMessage, TestError>>,
        outgoing: mpsc::Sender<TestMessage>,
    }

    impl Stream for TestSocket {
        type Item = Result<TestMessage, TestError>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Pin::new(&mut self.incoming).poll_next(cx)
        }
    }

    impl Sink<TestMessage> for TestSocket {
        type Error = mpsc::SendError;

        fn poll_ready(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Pin::new(&mut self.outgoing).poll_ready(cx)
        }
        fn start_send(mut self: Pin<&mut Self>, item: TestMessage) -> Result<(), Self::Error> {
            Pin::new(&mut self.outgoing).start_send(item)
        }
        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Pin::new(&mut self.outgoing).poll_flush(cx)
        }
        fn poll_close(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Pin::new(&mut self.outgoing).poll_close(cx)
        }
    }

    /// Only a stream type can mint a [`StreamId`], so the tests bring one.
    struct TestStream;

    impl crate::network::stream::Stream for TestStream {
        fn tag() -> u32 {
            0x7e57_0001
        }
    }

    impl StreamRequester for TestStream {
        type In = ();
        type Out = ();

        fn new(
            _: Self::Out,
            _: tokio::sync::mpsc::Sender<Self::Out>,
        ) -> impl Future<Output = Result<Self>> + Send {
            async { Ok(Self) }
        }
    }

    /// The driver, plus the far end of its socket.
    struct Harness {
        connection: Arc<InstanceConnection>,
        /// What the test sends to the driver.
        peer_tx: mpsc::Sender<Result<TestMessage, TestError>>,
        /// What the driver sends to the test.
        peer_rx: mpsc::Receiver<TestMessage>,
        _database: crate::database::DatabaseManager,
        _connections: crate::database::ResidentVec<ConnectionData>,
    }

    fn harness() -> Result<Harness> {
        let database = test_db!(ConnectionData);
        let realm = database.realm(RealmName::default())?;
        let connections: crate::database::ResidentVec<ConnectionData> = realm.resident_vec(())?;
        let data = connections.push(ConnectionData::scoped(crate::ServerId::random().into()))?;

        let (peer_tx, incoming) = mpsc::channel(32);
        let (outgoing, peer_rx) = mpsc::channel(32);

        let connection = InstanceConnection::socket(
            TestSocket { incoming, outgoing },
            data,
            RealmName::default(),
            ClusterId::default(),
            &[],
        );

        Ok(Harness {
            connection,
            peer_tx,
            peer_rx,
            _database: database,
            _connections: connections,
        })
    }

    /// Let the socket task run until `check` holds, without moving the clock
    /// (these tests measure virtual time, so advancing it would change what
    /// they are asserting).
    async fn settle(mut check: impl FnMut() -> bool) {
        for _ in 0..1000 {
            if check() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("the socket task never got there");
    }

    /// One `StreamMessage` the way a peer sends it: cbor on a binary frame.
    fn binary(stream_id: StreamId) -> (TestMessage, u64) {
        let encoded =
            serde_cbor::to_vec(&StreamMessage::local(stream_id, b"hello".to_vec())).unwrap();
        let len = encoded.len() as u64;
        (TestMessage::Binary(encoded), len)
    }

    /// The keepalive ping goes out as soon as the socket comes up, and the pong
    /// answering it is what measures the round trip.
    #[tokio::test(start_paused = true)]
    async fn pings_immediately_and_times_the_pong() -> Result<()> {
        let mut h = harness()?;

        let payload = match timeout(Duration::from_secs(5), h.peer_rx.next())
            .await?
            .expect("socket closed")
        {
            TestMessage::Ping(payload) => payload,
            other => panic!("expected a ping, got {other:?}"),
        };

        // No measurement until a pong answers it.
        assert_eq!(h.connection.data.read().latency, None);

        // Time passes between the ping and its answer, so the measurement is
        // the round trip rather than a constant.
        tokio::time::advance(Duration::from_millis(250)).await;
        h.peer_tx.try_send(Ok(TestMessage::Pong(payload))).unwrap();

        settle(|| h.connection.data.read().latency.is_some()).await;
        assert_eq!(h.connection.data.read().latency, Some(250_000));
        Ok(())
    }

    /// A peer may send an unsolicited pong as a heartbeat; timing one of those
    /// against our own ping would report a round trip that never happened.
    #[tokio::test(start_paused = true)]
    async fn ignores_an_unsolicited_pong() -> Result<()> {
        let mut h = harness()?;

        timeout(Duration::from_secs(5), h.peer_rx.next()).await?;
        tokio::time::advance(Duration::from_millis(250)).await;
        h.peer_tx
            .try_send(Ok(TestMessage::Pong(0xdead_beefu64.to_le_bytes().to_vec())))
            .unwrap();

        // The socket is ordered, so the answer to this ping is proof that the
        // pong ahead of it has already been handled.
        h.peer_tx.try_send(Ok(TestMessage::Ping(vec![1]))).unwrap();
        assert_eq!(
            timeout(Duration::from_secs(5), h.peer_rx.next())
                .await?
                .expect("socket closed"),
            TestMessage::Pong(vec![1])
        );

        assert_eq!(h.connection.data.read().latency, None);
        Ok(())
    }

    /// A peer's ping is answered immediately rather than queued behind the next
    /// write, because the peer's keepalive deadline is waiting on it.
    #[tokio::test(start_paused = true)]
    async fn answers_a_peer_ping_with_a_pong() -> Result<()> {
        let mut h = harness()?;

        // Discard our own opening keepalive ping.
        timeout(Duration::from_secs(5), h.peer_rx.next()).await?;

        h.peer_tx
            .try_send(Ok(TestMessage::Ping(vec![7, 7, 7])))
            .unwrap();

        let answer = timeout(Duration::from_secs(5), h.peer_rx.next())
            .await?
            .expect("socket closed");
        assert_eq!(answer, TestMessage::Pong(vec![7, 7, 7]));
        Ok(())
    }

    /// A binary frame decodes to a `StreamMessage` and is accounted against its
    /// stream; an outbound one is encoded and accounted the same way.
    #[tokio::test(start_paused = true)]
    async fn frames_stream_messages_both_ways() -> Result<()> {
        let mut h = harness()?;
        timeout(Duration::from_secs(5), h.peer_rx.next()).await?;

        let stream_id = TestStream::generate_id();
        let (frame, len) = binary(stream_id);
        h.peer_tx.try_send(Ok(frame)).unwrap();

        settle(|| h.connection.streams.traffic(stream_id).is_some()).await;
        assert_eq!(h.connection.streams.traffic(stream_id), Some((len, 0)));

        // Outbound: what the registry sends crosses the socket as one binary
        // frame, accounted against the same stream.
        h.connection
            .streams
            .send_raw(StreamMessage::local(stream_id, b"hello".to_vec()))
            .await;

        let sent = timeout(Duration::from_secs(5), h.peer_rx.next())
            .await?
            .expect("socket closed");
        assert_eq!(sent, binary(stream_id).0);
        assert_eq!(h.connection.streams.traffic(stream_id), Some((len, len)));
        Ok(())
    }

    /// A peer that stops answering is declared dead, which is what makes a
    /// partition distinguishable from an idle connection.
    #[tokio::test(start_paused = true)]
    async fn keepalive_deadline_cancels_the_connection() -> Result<()> {
        let h = harness()?;

        // Nothing is ever read from `peer_rx`, so the driver's keepalive pings
        // pile up in the channel (whose capacity the deadline is reached well
        // inside of) rather than failing the send. The clock auto-advances to
        // each keepalive tick, so this is the deadline firing, not a timeout.
        timeout(
            super::super::KEEPALIVE_DEADLINE * 10,
            h.connection.cancel.cancelled(),
        )
        .await?;
        Ok(())
    }

    /// However the socket ended, the connection is over: the janitors waiting
    /// on the token run on a transport error too.
    #[tokio::test(start_paused = true)]
    async fn transport_error_cancels_the_connection() -> Result<()> {
        let mut h = harness()?;
        timeout(Duration::from_secs(5), h.peer_rx.next()).await?;

        h.peer_tx.try_send(Err(TestError)).unwrap();

        timeout(Duration::from_secs(5), h.connection.cancel.cancelled()).await?;
        Ok(())
    }
}
