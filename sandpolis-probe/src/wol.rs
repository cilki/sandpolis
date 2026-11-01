use macaddr::MacAddr6;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

/// Build a Wake-on-LAN magic packet (102 bytes).
///
/// The magic packet consists of:
/// - 6 bytes of 0xFF (synchronization stream)
/// - 16 repetitions of the target MAC address (96 bytes)
pub fn magic_packet(mac: &MacAddr6) -> [u8; 102] {
    let mut packet = [0xFFu8; 102];
    let bytes = mac.as_ref();

    // Fill bytes 6-101 with 16 repetitions of the MAC address
    for i in 0..16 {
        let offset = 6 + i * 6;
        packet[offset..offset + 6].copy_from_slice(bytes);
    }

    packet
}

/// Request to send a Wake-on-LAN magic packet.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WolPacketRequest {
    /// The target device's MAC address.
    pub mac_address: MacAddr6,

    /// Optional broadcast address to send the packet to.
    /// Defaults to 255.255.255.255 if not specified.
    pub broadcast_address: Option<String>,

    /// Optional port to send the packet to.
    /// Defaults to 9 (discard protocol) if not specified.
    pub port: Option<u16>,
}

/// Response from a Wake-on-LAN packet send operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WolPacketResponse {
    /// Packet was sent successfully.
    Ok,
    /// The broadcast address was invalid.
    InvalidBroadcastAddress(String),
    /// Failed to send the packet.
    SendFailed(String),
}

/// Send a Wake-on-LAN magic packet to wake a device.
pub fn send_wol_packet(request: &WolPacketRequest) -> WolPacketResponse {
    let broadcast_addr: IpAddr = match &request.broadcast_address {
        Some(addr) => match addr.parse() {
            Ok(ip) => ip,
            Err(_) => return WolPacketResponse::InvalidBroadcastAddress(addr.clone()),
        },
        None => IpAddr::V4(Ipv4Addr::BROADCAST),
    };

    let port = request.port.unwrap_or(9);
    let dest = SocketAddr::new(broadcast_addr, port);

    // Bind to any available port on all interfaces
    let bind_addr = match broadcast_addr {
        IpAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), 0),
    };

    let socket = match UdpSocket::bind(bind_addr) {
        Ok(s) => s,
        Err(e) => return WolPacketResponse::SendFailed(format!("failed to bind socket: {}", e)),
    };

    // Enable broadcast
    if let Err(e) = socket.set_broadcast(true) {
        return WolPacketResponse::SendFailed(format!("failed to enable broadcast: {}", e));
    }

    let packet = magic_packet(&request.mac_address);

    match socket.send_to(&packet, dest) {
        Ok(_) => WolPacketResponse::Ok,
        Err(e) => WolPacketResponse::SendFailed(format!("failed to send packet: {}", e)),
    }
}

/// Server side: sends the magic packet on behalf of a client. Probes are accessed
/// only from servers, so Wake-on-LAN runs here rather than on the client.
#[cfg(feature = "server")]
mod server {
    use super::*;
    use anyhow::Result;
    use sandpolis_instance::network::{
        RegisterResponders, ResponderRegistration, StreamRegistry, StreamResponder,
    };
    use sandpolis_macros::Stream;
    use tokio::sync::mpsc::Sender;

    #[derive(Stream, Default)]
    pub struct WolStreamResponder;

    impl StreamResponder for WolStreamResponder {
        type In = WolPacketRequest;
        type Out = WolPacketResponse;

        async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
            let _ = sender.send(send_wol_packet(&request)).await;
            Ok(())
        }
    }

    /// Registers [`WolStreamResponder`] on each connection.
    pub struct WolResponderRegistration;

    impl RegisterResponders for WolResponderRegistration {
        fn register_responders(&self, registry: &StreamRegistry) {
            registry.register_responder(WolStreamResponder::default);
        }
    }

    inventory::submit!(ResponderRegistration(&WolResponderRegistration));
}

/// Client side: asks the connected server to send the magic packet and logs the
/// outcome it reports back.
#[cfg(feature = "client")]
mod client {
    use super::*;
    use anyhow::Result;
    use sandpolis_instance::network::InstanceConnection;
    use sandpolis_instance::network::StreamRequester;
    use sandpolis_instance::network::stream::StreamMessage;
    use sandpolis_macros::Stream;
    use std::sync::Arc;
    use tokio::sync::mpsc::Sender;

    #[derive(Stream, Default)]
    pub struct WolStreamRequester;

    impl StreamRequester for WolStreamRequester {
        type In = WolPacketResponse;
        type Out = WolPacketRequest;

        async fn on_message(&self, response: Self::In, _: Sender<Self::Out>) -> Result<()> {
            match response {
                WolPacketResponse::Ok => tracing::info!("Wake-on-LAN magic packet sent"),
                WolPacketResponse::InvalidBroadcastAddress(addr) => {
                    tracing::warn!("Invalid broadcast address: {}", addr)
                }
                WolPacketResponse::SendFailed(e) => {
                    tracing::warn!("Wake-on-LAN send failed: {}", e)
                }
            }
            Ok(())
        }
    }

    /// Ask the server to send a Wake-on-LAN magic packet.
    pub fn send_wake(conn: Arc<InstanceConnection>, request: WolPacketRequest) {
        sandpolis_client::sync::spawn(async move {
            let (id, tx) = conn.register_stream(WolStreamRequester);
            let payload = match serde_cbor::to_vec(&request) {
                Ok(p) => p,
                Err(_) => return,
            };
            let _ = tx
                .send(StreamMessage::local(id, payload))
                .await;
            // Keep the stream registered long enough to receive and log the
            // server's response, then release it.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            conn.close_stream(id);
        });
    }
}

#[cfg(feature = "client")]
pub use client::send_wake;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_magic_packet() {
        let mac: MacAddr6 = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let packet = magic_packet(&mac);

        // Check header (6 bytes of 0xFF)
        assert_eq!(&packet[0..6], &[0xFF; 6]);

        // Check that MAC address is repeated 16 times
        let mac_bytes = mac.as_ref();
        for i in 0..16 {
            let offset = 6 + i * 6;
            assert_eq!(&packet[offset..offset + 6], mac_bytes);
        }

        // Check total length
        assert_eq!(packet.len(), 102);
    }
}
