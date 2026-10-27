//! The WebTransport session that attaches an instance to a server.
//!
//! One QUIC connection per attachment. The dialer asks for a session on
//! [`CONNECT_PATH`] and says who it is in the request headers; the server
//! accepts, works out the realm from the certificate the handshake verified,
//! and then either closes the connection with one of the [`close`] codes or
//! opens the session's *control stream* — a bidirectional stream that carries
//! only its introduction and the keepalive — which is what tells the dialer
//! it is in.
//!
//! Every Sandpolis stream then gets a QUIC stream of its own: the first
//! [`StreamMessage`] sent for a [`StreamId`] opens one, and every message of
//! that stream, in both directions, travels on it. Loss or backlog on one
//! stream therefore never delays another, which is what the transport is for;
//! and finishing the QUIC stream is how a peer learns the stream is over. The
//! mapping is [`super::socket`]'s job; this module is the framing both kinds
//! of stream speak: `[kind: u8][payload]` behind a four byte length prefix,
//! since a QUIC stream is bytes rather than messages.
//!
//! [`StreamId`]: super::stream::StreamId
//! [`StreamMessage`]: super::stream::StreamMessage

use crate::InstanceId;
use crate::realm::RealmCert;
use crate::realm::url::ServerUrl;
use anyhow::{Result, anyhow, bail};
use futures_util::{Sink, Stream, StreamExt};
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio_util::bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use wtransport::endpoint::endpoint_side;
use wtransport::{ClientConfig, Connection, Endpoint, RecvStream, SendStream, VarInt};

/// The path a dialer requests its session on. Anything else is answered with
/// a 404, since the only thing to be had from a Sandpolis server over
/// WebTransport is an attachment.
pub const CONNECT_PATH: &str = "/connect";

/// Largest frame either side will accept. A frame is one [`StreamMessage`],
/// and the biggest of those is a snapshot block with its envelope; this is the
/// same ceiling the websocket transport had, so no stream has to change.
///
/// [`StreamMessage`]: super::stream::StreamMessage
const MAX_FRAME_LEN: usize = 64 << 20;

/// How many Sandpolis streams a peer may have open on one session at once,
/// which is how many QUIC streams it may open. QUIC's default (100) is sized
/// for request/response traffic; a client browsing an estate holds a sync
/// stream per subscription plus a stream per open panel, and a server's link
/// to a local stratum server carries every relayed stream behind it. Opening
/// past the limit doesn't fail, it waits for one to finish, so this only
/// needs to be out of the way.
const MAX_CONCURRENT_STREAMS: u32 = 4096;

/// The QUIC transport settings both ends agree on.
fn transport_config() -> wtransport::quinn::TransportConfig {
    let mut config = wtransport::quinn::TransportConfig::default();
    config
        // The socket driver's keepalive runs on the control stream, so this is
        // the backstop beneath it rather than the mechanism, and the two agree.
        .max_idle_timeout(Some(
            super::KEEPALIVE_DEADLINE
                .try_into()
                .expect("the keepalive deadline is a valid QUIC idle timeout"),
        ))
        // Unidirectional streams are left at the default: nothing of ours is
        // one, but the HTTP/3 session underneath runs on a few.
        .max_concurrent_bidi_streams(MAX_CONCURRENT_STREAMS.into())
        .datagram_send_buffer_size(DATAGRAM_SEND_BUFFER);
    config
}

/// How many bytes of datagrams may wait to go out. Unreliable messages are
/// only sent when they fit whole (see `super::socket`), so this is what
/// bounds how stale one can be by the time it leaves: room for a couple of
/// the largest ([`super::datagram::MAX_FRAGMENTS`] full datagrams each), not
/// quinn's default megabyte, which at a few Mbit/s is seconds of old frames.
const DATAGRAM_SEND_BUFFER: usize = 256 << 10;

/// The kind byte that leads every frame.
mod kind {
    pub const BINARY: u8 = 0;
    pub const PING: u8 = 1;
    pub const PONG: u8 = 2;
    pub const HELLO: u8 = 3;
}

/// Why a server closed a session it had accepted.
///
/// The realm is only known once the session is up (the certificate chain is
/// on the connection, not the request), so a dialer that doesn't belong is
/// told so with an application close rather than an HTTP status. The dialer
/// sees the code on the stream it is waiting to be handed.
pub mod close {
    use wtransport::VarInt;

    /// The realm has users and the connection token was missing or invalid:
    /// the user has to log in, which the dialer must be able to tell apart
    /// from every other way of being refused.
    pub const AUTH_REQUIRED: VarInt = VarInt::from_u32(1);

    /// The certificate the handshake verified doesn't name a realm this
    /// server serves.
    pub const UNAUTHENTICATED: VarInt = VarInt::from_u32(2);

    /// Accepting the peer would break the network's shape: a second global
    /// stratum server, or a server dialing a local stratum server.
    pub const CONFLICT: VarInt = VarInt::from_u32(3);
}

/// One frame on a session's stream, as the socket driver sees it.
///
/// A stream's QUIC stream only ever carries [`Binary`](Self::Binary); the
/// control stream carries the rest.
#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    /// Application data: one cbor-encoded `StreamMessage`.
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    /// The server's introduction, sent once before any other frame.
    Hello(Bytes),
    /// A kind this build doesn't know. Skipped rather than fatal, so a newer
    /// peer can add frame kinds without cutting older ones off.
    Unknown,
}

impl Message {
    fn decode(mut frame: BytesMut) -> Self {
        if frame.is_empty() {
            return Self::Unknown;
        }
        let kind = frame.get_u8();
        let payload = frame.freeze();
        match kind {
            kind::BINARY => Self::Binary(payload),
            kind::PING => Self::Ping(payload),
            kind::PONG => Self::Pong(payload),
            kind::HELLO => Self::Hello(payload),
            _ => Self::Unknown,
        }
    }

    fn encode(&self) -> Bytes {
        let (kind, payload) = match self {
            Self::Binary(payload) => (kind::BINARY, payload),
            Self::Ping(payload) => (kind::PING, payload),
            Self::Pong(payload) => (kind::PONG, payload),
            Self::Hello(payload) => (kind::HELLO, payload),
            Self::Unknown => unreachable!("nothing sends a frame it doesn't understand"),
        };
        let mut frame = BytesMut::with_capacity(1 + payload.len());
        frame.put_u8(kind);
        frame.extend_from_slice(payload);
        frame.freeze()
    }
}

/// The frame codec every stream of a session speaks.
fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .length_field_length(4)
        .max_frame_length(MAX_FRAME_LEN)
        .new_codec()
}

/// The receiving half of a QUIC stream, framed into [`Message`]s.
pub struct Reader(FramedRead<RecvStream, LengthDelimitedCodec>);

impl Reader {
    pub fn new(rx: RecvStream) -> Self {
        Self(FramedRead::new(rx, codec()))
    }
}

impl Stream for Reader {
    type Item = Result<Message, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.0)
            .poll_next(cx)
            .map(|next| next.map(|frame| frame.map(Message::decode)))
    }
}

/// The sending half of a QUIC stream, framed into [`Message`]s. Closing it
/// finishes the QUIC stream, which is how the peer learns there is no more.
pub struct Writer(FramedWrite<SendStream, LengthDelimitedCodec>);

impl Writer {
    pub fn new(tx: SendStream) -> Self {
        Self(FramedWrite::new(tx, codec()))
    }
}

// `LengthDelimitedCodec` encodes both `Bytes` and `&[u8]`, so the write half
// is a `Sink` of either; the three item-less methods have to say which.
impl Sink<Message> for Writer {
    type Error = io::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Sink::<Bytes>::poll_ready(Pin::new(&mut self.0), cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        Pin::new(&mut self.0).start_send(item.encode())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Sink::<Bytes>::poll_flush(Pin::new(&mut self.0), cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Sink::<Bytes>::poll_close(Pin::new(&mut self.0), cx)
    }
}

/// A bidirectional QUIC stream, framed into [`Message`]s.
pub struct Socket {
    rx: Reader,
    tx: Writer,
}

impl Socket {
    pub fn new((tx, rx): (SendStream, RecvStream)) -> Self {
        Self {
            rx: Reader::new(rx),
            tx: Writer::new(tx),
        }
    }

    pub fn split(self) -> (Writer, Reader) {
        (self.tx, self.rx)
    }
}

/// An attachment that is up: the QUIC connection and its control stream.
///
/// The control stream is the one the server opened to say yes. The keepalive
/// runs on it, and nothing else does; it is at the highest priority so a
/// backlog on a data stream can't starve the pings the peer's deadline is
/// waiting on.
pub struct Session {
    pub connection: Connection,
    pub control: Socket,
}

impl Session {
    fn new(connection: Connection, (tx, rx): (SendStream, RecvStream)) -> Self {
        tx.set_priority(i32::MAX);
        Self {
            connection,
            control: Socket::new((tx, rx)),
        }
    }
}

/// What the server says first on the stream it opens: its own instance id,
/// so the dialer records the real peer instead of a freshly-generated default
/// (which would surface as a phantom graph node, growing on every reconnect).
/// The dialer's side of the introduction travels in the session request's
/// headers, which the server reads before it accepts.
#[derive(serde::Serialize, serde::Deserialize)]
struct Hello {
    instance: InstanceId,
}

/// Accept-side half of bringing a session up, once the server has decided the
/// peer may attach: open the control stream and introduce ourselves on it.
#[cfg(feature = "server")]
pub async fn open(connection: Connection, instance: InstanceId) -> Result<Session> {
    use anyhow::Context as _;
    use futures_util::SinkExt;

    let stream = connection
        .open_bi()
        .await
        .context("Opening the control stream")?
        .await
        .context("Opening the control stream")?;

    let mut session = Session::new(connection, stream);
    let hello = serde_cbor::to_vec(&Hello { instance })?;
    session
        .control
        .tx
        .send(Message::Hello(hello.into()))
        .await
        .context("Introducing ourselves on the control stream")?;
    Ok(session)
}

/// Dial-side half: wait for the server to open the control stream, which it
/// only does after it has decided we may attach. Resolves to the server's
/// instance id and the session; a server that decided otherwise closed the
/// connection instead, which surfaces here as [`Refused`].
///
/// The control stream is the first the server opens, and QUIC hands a peer's
/// streams over in the order they were opened, so the first accepted is it.
pub async fn accept(connection: Connection) -> Result<(InstanceId, Session)> {
    let stream = match connection.accept_bi().await {
        Ok(stream) => stream,
        Err(wtransport::error::ConnectionError::ApplicationClosed(close)) => {
            return Err(Refused {
                code: close.code(),
                reason: String::from_utf8_lossy(close.reason()).into_owned(),
            }
            .into());
        }
        Err(e) => return Err(anyhow!(e).context("Waiting for the control stream")),
    };

    let mut session = Session::new(connection, stream);
    let hello = match session.control.rx.next().await {
        Some(Ok(Message::Hello(payload))) => serde_cbor::from_slice::<Hello>(&payload)?,
        Some(Ok(other)) => bail!("Expected the server's hello, got {other:?}"),
        Some(Err(e)) => return Err(anyhow!(e).context("Reading the server's hello")),
        None => bail!("Control stream closed before the server's hello"),
    };

    Ok((hello.instance, session))
}

/// The server accepted the session and then closed it, with one of the
/// [`close`] codes saying why.
#[derive(Debug)]
pub struct Refused {
    pub code: VarInt,
    pub reason: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "server refused the session: {}", self.reason)
    }
}

impl std::error::Error for Refused {}

/// The endpoint a server accepts sessions on: the same address as its HTTP
/// listener, over UDP.
///
/// `tls` is the realm acceptor's configuration, so the handshake verifies
/// peers against the same realm CAs and presents the same per-realm server
/// certificates as the HTTP listener does. Only the ALPN differs.
#[cfg(feature = "server")]
pub fn server_endpoint(
    address: SocketAddr,
    mut tls: rustls::ServerConfig,
) -> Result<Endpoint<endpoint_side::Server>> {
    tls.alpn_protocols = vec![wtransport::proto::WEBTRANSPORT_ALPN.to_vec()];

    let config = wtransport::ServerConfig::builder()
        .with_bind_address(address)
        .with_custom_tls_and_transport(tls, transport_config())
        .build();

    Ok(Endpoint::server(config)?)
}

/// Answers the fake domain a realm's server certificate is issued for.
///
/// A server certificate's SAN is `{cluster_id}.{realm}`, which exists in no
/// DNS; the realm cert says where that server actually is. Mapping the one to
/// the other here is what lets the handshake verify the certificate's name
/// without the name having to resolve.
#[derive(Debug)]
struct RealmResolver {
    /// `host:port`, as the endpoint asks for it.
    name: String,
    address: SocketAddr,
}

impl wtransport::config::DnsResolver for RealmResolver {
    fn resolve(&self, host: &str) -> Pin<Box<dyn wtransport::config::DnsLookupFuture>> {
        let address = (host == self.name).then_some(self.address);
        Box::pin(async move { Ok(address) })
    }
}

/// Everything a dialer needs to open sessions with one server: the endpoint
/// configured with its realm cert, and the URL the sessions are requested on.
pub struct Dialer {
    endpoint: Endpoint<endpoint_side::Client>,
    url: String,
}

impl Dialer {
    /// Prepare to dial the server `url` names, authenticating with `cert`.
    ///
    /// Only the realm CA may vouch for the server, so the platform trust
    /// store is left out entirely.
    pub fn new(cert: &RealmCert, url: &ServerUrl) -> Result<Arc<Self>> {
        let address = url
            .resolve()?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("{} does not resolve", url.host))?;

        let server_name = format!("{}.{}", cert.cluster_id()?, cert.name);

        let mut tls = cert.client_tls_config()?;
        tls.alpn_protocols = vec![wtransport::proto::WEBTRANSPORT_ALPN.to_vec()];

        // Bind in the server's address family, so a v4-only host doesn't fail
        // on the dual-stack default.
        let bind: SocketAddr = match address {
            SocketAddr::V4(_) => "0.0.0.0:0".parse()?,
            SocketAddr::V6(_) => "[::]:0".parse()?,
        };

        let config = ClientConfig::builder()
            .with_bind_address(bind)
            .with_custom_tls_and_transport(tls, transport_config())
            .dns_resolver(RealmResolver {
                name: format!("{server_name}:{}", url.port),
                address,
            })
            .build();

        Ok(Arc::new(Self {
            endpoint: Endpoint::client(config)?,
            url: format!("https://{server_name}:{}{CONNECT_PATH}", url.port),
        }))
    }

    /// Request a session, carrying `headers` for the server to read before it
    /// decides. The attachment isn't usable until [`accept`] has run on the
    /// returned connection.
    pub async fn connect(
        &self,
        headers: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Connection> {
        let mut options = wtransport::endpoint::ConnectOptions::builder(&self.url);
        for (key, value) in headers {
            options = options.add_header(key, value);
        }
        Ok(self.endpoint.connect(options.build()).await?)
    }

    /// Close every session this dialer opened and stop it opening more.
    ///
    /// The server is told with a close frame, so it sees the attachment end
    /// straight away instead of waiting out the keepalive deadline. Sending the
    /// frame is the endpoint's background work, so this waits (for at most
    /// `grace`) until it has gone out before it returns: a process that exits
    /// right after this call still leaves cleanly.
    pub async fn close(&self, grace: std::time::Duration) {
        self.endpoint.close(VarInt::from_u32(0), b"");
        let _ = tokio::time::timeout(grace, self.endpoint.wait_idle()).await;
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// A frame is its kind byte followed by the payload, and a frame of a kind
    /// this build doesn't know decodes to something the driver ignores rather
    /// than an error that would end the connection.
    #[test]
    fn frames_round_trip() {
        for message in [
            Message::Binary(Bytes::from_static(b"hello")),
            Message::Ping(Bytes::from_static(&[1, 2, 3])),
            Message::Pong(Bytes::from_static(&[])),
            Message::Hello(Bytes::from_static(b"id")),
        ] {
            let encoded = message.encode();
            assert_eq!(Message::decode(BytesMut::from(&encoded[..])), message);
        }

        assert_eq!(
            Message::decode(BytesMut::from(&[0xff, 1, 2][..])),
            Message::Unknown
        );
        assert_eq!(Message::decode(BytesMut::new()), Message::Unknown);
    }
}
