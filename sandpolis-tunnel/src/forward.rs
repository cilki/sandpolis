//! The tunnel endpoint: the worker that runs on whichever instance is a
//! listener or a terminator, plus the responder that hosts it on a remote
//! endpoint.
//!
//! [`run_endpoint`] is the single implementation of both roles and both
//! transports. The server bridge drives it directly when an endpoint is the
//! local server, and [`TunnelStreamResponder`] drives it over a stream when the
//! endpoint is a remote agent, client, or server.
//!
//! Responder handlers run inline on the socket's receive path, so the `Open`
//! message spawns the worker and returns immediately; blocking here would stall
//! the whole connection's dispatch loop.

use crate::streams::{TunnelStreamRequest, TunnelStreamResponse};
use crate::{TunnelProtocol, TunnelRole};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// Read buffer size for copying socket bytes into the tunnel.
const IO_BUFFER: usize = 16 * 1024;
/// Largest UDP datagram the listener will relay.
const UDP_BUFFER: usize = 64 * 1024;

/// A live logical connection's write side and cancellation.
struct ConnHandle {
    /// Bytes to write toward the socket for this connection.
    to_socket: UnboundedSender<Vec<u8>>,
    /// Stops the connection's read/write tasks.
    cancel: CancellationToken,
}

/// Internal events a worker's helper tasks feed back to its main loop.
enum Internal {
    TcpAccepted {
        conn: u64,
        stream: TcpStream,
        peer: SocketAddr,
    },
    TcpConnected {
        conn: u64,
        stream: TcpStream,
    },
    UdpAccepted {
        conn: u64,
        peer: SocketAddr,
    },
    UdpConnected {
        conn: u64,
        socket: Arc<UdpSocket>,
    },
    ConnectFailed {
        conn: u64,
        message: String,
    },
    Ended {
        conn: u64,
    },
}

/// Run a tunnel endpoint until `cancel` fires or the stream ends.
///
/// `requests` carries every message after the initial `Open` (which the caller
/// consumes to pick the role); `responses` carries everything back toward the
/// server bridge.
pub async fn run_endpoint(
    role: TunnelRole,
    protocol: TunnelProtocol,
    requests: UnboundedReceiver<TunnelStreamRequest>,
    responses: UnboundedSender<TunnelStreamResponse>,
    cancel: CancellationToken,
) {
    match (role, protocol) {
        (TunnelRole::Listener { listen }, TunnelProtocol::Tcp) => {
            tcp_listener(listen, requests, responses, cancel).await
        }
        (TunnelRole::Terminator { target }, TunnelProtocol::Tcp) => {
            tcp_terminator(target, requests, responses, cancel).await
        }
        (TunnelRole::Listener { listen }, TunnelProtocol::Udp) => {
            udp_listener(listen, requests, responses, cancel).await
        }
        (TunnelRole::Terminator { target }, TunnelProtocol::Udp) => {
            udp_terminator(target, requests, responses, cancel).await
        }
    }
}

fn fail(responses: &UnboundedSender<TunnelStreamResponse>, message: String) {
    let _ = responses.send(TunnelStreamResponse::Error { message });
}

// --- TCP -------------------------------------------------------------------

async fn tcp_listener(
    listen: SocketAddr,
    mut requests: UnboundedReceiver<TunnelStreamRequest>,
    responses: UnboundedSender<TunnelStreamResponse>,
    cancel: CancellationToken,
) {
    let listener = match TcpListener::bind(listen).await {
        Ok(listener) => listener,
        Err(e) => return fail(&responses, format!("Failed to bind {listen}: {e}")),
    };
    let _ = responses.send(TunnelStreamResponse::Ready);

    let (internal_tx, mut internal_rx) = unbounded_channel::<Internal>();
    {
        let internal_tx = internal_tx.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut counter = 1u64;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, peer)) => {
                            let conn = counter;
                            counter += 1;
                            if internal_tx.send(Internal::TcpAccepted { conn, stream, peer }).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            debug!(error = %e, "Tunnel listener accept failed");
                            break;
                        }
                    }
                }
            }
        });
    }

    let mut conns: HashMap<u64, ConnHandle> = HashMap::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            request = requests.recv() => match request {
                Some(TunnelStreamRequest::Data { conn, bytes }) => {
                    if let Some(handle) = conns.get(&conn) {
                        let _ = handle.to_socket.send(bytes);
                    }
                }
                Some(TunnelStreamRequest::Close { conn }) => {
                    if let Some(handle) = conns.remove(&conn) {
                        handle.cancel.cancel();
                    }
                }
                Some(_) => {} // Open/Connect are not meaningful for a listener.
                None => break,
            },
            event = internal_rx.recv() => match event {
                Some(Internal::TcpAccepted { conn, stream, peer }) => {
                    let _ = responses.send(TunnelStreamResponse::Accepted { conn, peer: peer.to_string() });
                    conns.insert(conn, spawn_tcp_conn(conn, stream, responses.clone(), internal_tx.clone()));
                }
                Some(Internal::Ended { conn }) => {
                    if conns.remove(&conn).is_some() {
                        let _ = responses.send(TunnelStreamResponse::Closed { conn });
                    }
                }
                _ => {}
            },
        }
    }

    for (_, handle) in conns {
        handle.cancel.cancel();
    }
}

async fn tcp_terminator(
    target: String,
    mut requests: UnboundedReceiver<TunnelStreamRequest>,
    responses: UnboundedSender<TunnelStreamResponse>,
    cancel: CancellationToken,
) {
    let _ = responses.send(TunnelStreamResponse::Ready);

    let (internal_tx, mut internal_rx) = unbounded_channel::<Internal>();
    let mut conns: HashMap<u64, ConnHandle> = HashMap::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            request = requests.recv() => match request {
                Some(TunnelStreamRequest::Connect { conn }) => {
                    let target = target.clone();
                    let internal_tx = internal_tx.clone();
                    tokio::spawn(async move {
                        match TcpStream::connect(&target).await {
                            Ok(stream) => { let _ = internal_tx.send(Internal::TcpConnected { conn, stream }); }
                            Err(e) => { let _ = internal_tx.send(Internal::ConnectFailed { conn, message: e.to_string() }); }
                        }
                    });
                }
                Some(TunnelStreamRequest::Data { conn, bytes }) => {
                    if let Some(handle) = conns.get(&conn) {
                        let _ = handle.to_socket.send(bytes);
                    }
                }
                Some(TunnelStreamRequest::Close { conn }) => {
                    if let Some(handle) = conns.remove(&conn) {
                        handle.cancel.cancel();
                    }
                }
                Some(TunnelStreamRequest::Open { .. }) => {}
                None => break,
            },
            event = internal_rx.recv() => match event {
                Some(Internal::TcpConnected { conn, stream }) => {
                    let _ = responses.send(TunnelStreamResponse::Connected { conn });
                    conns.insert(conn, spawn_tcp_conn(conn, stream, responses.clone(), internal_tx.clone()));
                }
                Some(Internal::ConnectFailed { conn, message }) => {
                    let _ = responses.send(TunnelStreamResponse::ConnectFailed { conn, message });
                }
                Some(Internal::Ended { conn }) => {
                    if conns.remove(&conn).is_some() {
                        let _ = responses.send(TunnelStreamResponse::Closed { conn });
                    }
                }
                _ => {}
            },
        }
    }

    for (_, handle) in conns {
        handle.cancel.cancel();
    }
}

/// Wire up the read/write tasks for one accepted or dialed TCP connection.
fn spawn_tcp_conn(
    conn: u64,
    stream: TcpStream,
    responses: UnboundedSender<TunnelStreamResponse>,
    internal: UnboundedSender<Internal>,
) -> ConnHandle {
    let (read_half, write_half) = stream.into_split();
    let cancel = CancellationToken::new();
    let (to_socket, rx) = unbounded_channel::<Vec<u8>>();
    tokio::spawn(tcp_write_task(write_half, rx, cancel.clone()));
    tokio::spawn(tcp_read_task(read_half, conn, responses, internal, cancel.clone()));
    ConnHandle { to_socket, cancel }
}

async fn tcp_write_task(
    mut write_half: OwnedWriteHalf,
    mut rx: UnboundedReceiver<Vec<u8>>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                // A `Close` is handled only after every `Data` the peer sent
                // before it, so whatever is still queued here is the tail of
                // the stream. Without this, a close racing that tail (e.g. a
                // target that answers and hangs up) truncates the response.
                while let Ok(bytes) = rx.try_recv() {
                    if write_half.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                break;
            }
            bytes = rx.recv() => match bytes {
                Some(bytes) => {
                    if write_half.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                None => break,
            }
        }
    }
    let _ = write_half.shutdown().await;
}

async fn tcp_read_task(
    mut read_half: OwnedReadHalf,
    conn: u64,
    responses: UnboundedSender<TunnelStreamResponse>,
    internal: UnboundedSender<Internal>,
    cancel: CancellationToken,
) {
    let mut buf = vec![0u8; IO_BUFFER];
    loop {
        tokio::select! {
            // Closed by request (peer teardown); the server already knows.
            _ = cancel.cancelled() => return,
            read = read_half.read(&mut buf) => match read {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if responses.send(TunnelStreamResponse::Data { conn, bytes: buf[..n].to_vec() }).is_err() {
                        return;
                    }
                }
            }
        }
    }
    let _ = internal.send(Internal::Ended { conn });
}

// --- UDP -------------------------------------------------------------------
//
// A UDP "connection" is a session keyed by source address. Sessions currently
// live for the tunnel's lifetime rather than expiring on idle; churny UDP
// therefore accumulates session ids until the tunnel stops.

async fn udp_listener(
    listen: SocketAddr,
    mut requests: UnboundedReceiver<TunnelStreamRequest>,
    responses: UnboundedSender<TunnelStreamResponse>,
    cancel: CancellationToken,
) {
    let socket = match UdpSocket::bind(listen).await {
        Ok(socket) => Arc::new(socket),
        Err(e) => return fail(&responses, format!("Failed to bind {listen}: {e}")),
    };
    let _ = responses.send(TunnelStreamResponse::Ready);

    let (internal_tx, mut internal_rx) = unbounded_channel::<Internal>();
    {
        let socket = socket.clone();
        let responses = responses.clone();
        let internal_tx = internal_tx.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut peers: HashMap<SocketAddr, u64> = HashMap::new();
            let mut counter = 1u64;
            let mut buf = vec![0u8; UDP_BUFFER];
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    received = socket.recv_from(&mut buf) => match received {
                        Ok((n, peer)) => {
                            let conn = *peers.entry(peer).or_insert_with(|| {
                                let conn = counter;
                                counter += 1;
                                let _ = responses.send(TunnelStreamResponse::Accepted { conn, peer: peer.to_string() });
                                let _ = internal_tx.send(Internal::UdpAccepted { conn, peer });
                                conn
                            });
                            let _ = responses.send(TunnelStreamResponse::Data { conn, bytes: buf[..n].to_vec() });
                        }
                        Err(e) => {
                            debug!(error = %e, "Tunnel UDP listener recv failed");
                            break;
                        }
                    }
                }
            }
        });
    }

    let mut peer_of: HashMap<u64, SocketAddr> = HashMap::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            request = requests.recv() => match request {
                Some(TunnelStreamRequest::Data { conn, bytes }) => {
                    if let Some(peer) = peer_of.get(&conn) {
                        let _ = socket.send_to(&bytes, peer).await;
                    }
                }
                Some(TunnelStreamRequest::Close { conn }) => {
                    peer_of.remove(&conn);
                }
                Some(_) => {}
                None => break,
            },
            event = internal_rx.recv() => {
                if let Some(Internal::UdpAccepted { conn, peer }) = event {
                    peer_of.insert(conn, peer);
                }
            }
        }
    }
}

async fn udp_terminator(
    target: String,
    mut requests: UnboundedReceiver<TunnelStreamRequest>,
    responses: UnboundedSender<TunnelStreamResponse>,
    cancel: CancellationToken,
) {
    let _ = responses.send(TunnelStreamResponse::Ready);

    let (internal_tx, mut internal_rx) = unbounded_channel::<Internal>();
    let mut conns: HashMap<u64, ConnHandle> = HashMap::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            request = requests.recv() => match request {
                Some(TunnelStreamRequest::Connect { conn }) => {
                    let target = target.clone();
                    let internal_tx = internal_tx.clone();
                    tokio::spawn(async move {
                        match udp_connect(&target).await {
                            Ok(socket) => { let _ = internal_tx.send(Internal::UdpConnected { conn, socket: Arc::new(socket) }); }
                            Err(e) => { let _ = internal_tx.send(Internal::ConnectFailed { conn, message: e.to_string() }); }
                        }
                    });
                }
                Some(TunnelStreamRequest::Data { conn, bytes }) => {
                    if let Some(handle) = conns.get(&conn) {
                        let _ = handle.to_socket.send(bytes);
                    }
                }
                Some(TunnelStreamRequest::Close { conn }) => {
                    if let Some(handle) = conns.remove(&conn) {
                        handle.cancel.cancel();
                    }
                }
                Some(TunnelStreamRequest::Open { .. }) => {}
                None => break,
            },
            event = internal_rx.recv() => match event {
                Some(Internal::UdpConnected { conn, socket }) => {
                    let _ = responses.send(TunnelStreamResponse::Connected { conn });
                    conns.insert(conn, spawn_udp_conn(conn, socket, responses.clone()));
                }
                Some(Internal::ConnectFailed { conn, message }) => {
                    let _ = responses.send(TunnelStreamResponse::ConnectFailed { conn, message });
                }
                _ => {}
            },
        }
    }

    for (_, handle) in conns {
        handle.cancel.cancel();
    }
}

/// Bind an ephemeral UDP socket connected to `target` (resolving a hostname).
async fn udp_connect(target: &str) -> anyhow::Result<UdpSocket> {
    let addr = tokio::net::lookup_host(target)
        .await?
        .next()
        .ok_or_else(|| anyhow::anyhow!("Could not resolve {target}"))?;
    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let socket = UdpSocket::bind(bind).await?;
    socket.connect(addr).await?;
    Ok(socket)
}

/// Wire up the read/write tasks for one dialed UDP session.
fn spawn_udp_conn(
    conn: u64,
    socket: Arc<UdpSocket>,
    responses: UnboundedSender<TunnelStreamResponse>,
) -> ConnHandle {
    let cancel = CancellationToken::new();
    let (to_socket, mut rx) = unbounded_channel::<Vec<u8>>();

    // Write side: datagrams from the server toward the target.
    {
        let socket = socket.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    bytes = rx.recv() => match bytes {
                        Some(bytes) => { let _ = socket.send(&bytes).await; }
                        None => break,
                    }
                }
            }
        });
    }

    // Read side: datagrams from the target back toward the server.
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; UDP_BUFFER];
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    received = socket.recv(&mut buf) => match received {
                        Ok(n) => {
                            if responses.send(TunnelStreamResponse::Data { conn, bytes: buf[..n].to_vec() }).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        });
    }

    ConnHandle { to_socket, cancel }
}

// --- Responder -------------------------------------------------------------

use anyhow::Result;
use sandpolis_instance::network::StreamResponder;
use sandpolis_macros::Stream;
use std::sync::Mutex;
use tokio::sync::mpsc::Sender;

/// Hosts a tunnel endpoint on a remote instance. Both roles run through the
/// same responder; the initial [`TunnelStreamRequest::Open`] selects which.
#[derive(Stream, Default)]
pub struct TunnelStreamResponder {
    /// Feeds requests after `Open` into the running worker.
    worker: Mutex<Option<UnboundedSender<TunnelStreamRequest>>>,
    /// Stops the worker (and all its connections) on drop.
    cancel: CancellationToken,
}

impl StreamResponder for TunnelStreamResponder {
    type In = TunnelStreamRequest;
    type Out = TunnelStreamResponse;

    async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
        match request {
            TunnelStreamRequest::Open { role, protocol } => {
                let (req_tx, req_rx) = unbounded_channel::<TunnelStreamRequest>();
                let (resp_tx, mut resp_rx) = unbounded_channel::<TunnelStreamResponse>();
                *self.worker.lock().unwrap() = Some(req_tx);

                // Forward the worker's unbounded output onto the stream's
                // bounded sender.
                tokio::spawn(async move {
                    while let Some(response) = resp_rx.recv().await {
                        if sender.send(response).await.is_err() {
                            break;
                        }
                    }
                });

                let cancel = self.cancel.clone();
                tokio::spawn(async move {
                    run_endpoint(role, protocol, req_rx, resp_tx, cancel).await;
                });
            }
            other => {
                if let Some(worker) = self.worker.lock().unwrap().as_ref() {
                    let _ = worker.send(other);
                }
            }
        }
        Ok(())
    }
}

impl Drop for TunnelStreamResponder {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    async fn recv(
        rx: &mut UnboundedReceiver<TunnelStreamResponse>,
    ) -> TunnelStreamResponse {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("endpoint response timed out")
            .expect("endpoint stream closed")
    }

    /// A background TCP echo server; returns its address.
    async fn echo_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    loop {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if socket.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });
        addr
    }

    /// The terminator dials the target and copies bytes both ways.
    #[tokio::test]
    async fn terminator_forwards_tcp() {
        let target = echo_server().await;
        let (to_tx, to_rx) = unbounded_channel();
        let (resp_tx, mut resp_rx) = unbounded_channel();
        let cancel = CancellationToken::new();
        tokio::spawn(run_endpoint(
            TunnelRole::Terminator {
                target: target.to_string(),
            },
            TunnelProtocol::Tcp,
            to_rx,
            resp_tx,
            cancel.clone(),
        ));

        assert!(matches!(recv(&mut resp_rx).await, TunnelStreamResponse::Ready));
        to_tx.send(TunnelStreamRequest::Connect { conn: 1 }).unwrap();
        assert!(matches!(
            recv(&mut resp_rx).await,
            TunnelStreamResponse::Connected { conn: 1 }
        ));

        to_tx
            .send(TunnelStreamRequest::Data {
                conn: 1,
                bytes: b"hello".to_vec(),
            })
            .unwrap();
        match recv(&mut resp_rx).await {
            TunnelStreamResponse::Data { conn: 1, bytes } => assert_eq!(bytes, b"hello"),
            other => panic!("unexpected {other:?}"),
        }
        cancel.cancel();
    }

    /// The listener accepts a connection and copies bytes both ways.
    #[tokio::test]
    async fn listener_forwards_tcp() {
        // Grab a free port, then hand it to the listener.
        let addr = {
            let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
            probe.local_addr().unwrap()
        };
        let (to_tx, to_rx) = unbounded_channel();
        let (resp_tx, mut resp_rx) = unbounded_channel();
        let cancel = CancellationToken::new();
        tokio::spawn(run_endpoint(
            TunnelRole::Listener { listen: addr },
            TunnelProtocol::Tcp,
            to_rx,
            resp_tx,
            cancel.clone(),
        ));

        // Ready is sent after bind, so the connect below always lands.
        assert!(matches!(recv(&mut resp_rx).await, TunnelStreamResponse::Ready));
        let mut client = TcpStream::connect(addr).await.unwrap();

        let conn = match recv(&mut resp_rx).await {
            TunnelStreamResponse::Accepted { conn, .. } => conn,
            other => panic!("unexpected {other:?}"),
        };

        client.write_all(b"ping").await.unwrap();
        match recv(&mut resp_rx).await {
            TunnelStreamResponse::Data { conn: c, bytes } => {
                assert_eq!(c, conn);
                assert_eq!(bytes, b"ping");
            }
            other => panic!("unexpected {other:?}"),
        }

        to_tx
            .send(TunnelStreamRequest::Data {
                conn,
                bytes: b"pong".to_vec(),
            })
            .unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
        cancel.cancel();
    }

    /// A `Close` that arrives right behind the last `Data` (a target that
    /// answers and hangs up) still delivers that data before the socket is
    /// shut down. Whether the close beats the queued bytes is a coin flip per
    /// connection, so try enough of them that a regression can't hide.
    #[tokio::test]
    async fn listener_delivers_data_queued_before_close() {
        let addr = {
            let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
            probe.local_addr().unwrap()
        };
        let (to_tx, to_rx) = unbounded_channel();
        let (resp_tx, mut resp_rx) = unbounded_channel();
        let cancel = CancellationToken::new();
        tokio::spawn(run_endpoint(
            TunnelRole::Listener { listen: addr },
            TunnelProtocol::Tcp,
            to_rx,
            resp_tx,
            cancel.clone(),
        ));
        assert!(matches!(recv(&mut resp_rx).await, TunnelStreamResponse::Ready));

        let payload: Vec<u8> = (0..64 * 1024).map(|i| i as u8).collect();
        for _ in 0..32 {
            let mut client = TcpStream::connect(addr).await.unwrap();
            let conn = match recv(&mut resp_rx).await {
                TunnelStreamResponse::Accepted { conn, .. } => conn,
                other => panic!("unexpected {other:?}"),
            };

            to_tx
                .send(TunnelStreamRequest::Data {
                    conn,
                    bytes: payload.clone(),
                })
                .unwrap();
            to_tx.send(TunnelStreamRequest::Close { conn }).unwrap();

            let mut received = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut received))
                .await
                .expect("listener never shut the connection down")
                .unwrap();
            assert_eq!(received.len(), payload.len(), "connection {conn} lost its tail");
            assert_eq!(received, payload);
        }
        cancel.cancel();
    }
}
