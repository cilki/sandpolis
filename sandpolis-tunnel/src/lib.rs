//! Application-level tunnels: forward TCP/UDP traffic between any two instances
//! (except two clients).
//!
//! A tunnel is declared in a realm config with SSH-style local/reverse
//! semantics: one endpoint — the *listener* — binds a socket, and every
//! connection it accepts is carried to the other endpoint — the *terminator* —
//! which dials the real target and copies bytes back and forth.
//!
//! ## Indirect (default)
//!
//! The global stratum server reads the config and bridges the tunnel: it opens
//! a [`streams::TunnelStream`] toward each endpoint and copies bytes between
//! them, keyed by a logical connection id. This always works — it rides the
//! same multi-stratum relay every other stream uses, so an endpoint behind a
//! local stratum server is reached transparently.
//!
//! ## Direct (client <-> agent, best effort)
//!
//! A `Direct` tunnel would hole-punch a peer-to-peer connection and run the
//! data stream over it, bypassing the server. That transport does not exist
//! yet, so [`direct::attempt_direct`] always fails and the tunnel falls back to
//! the indirect bridge seamlessly; the observed choice is recorded as
//! `effective_mode`.

use anyhow::Result;
use native_db::ToKey;
use native_model::Model;
use sandpolis_instance::InstanceId;
use sandpolis_instance::InstanceManager;
use sandpolis_instance::database::DatabaseManager;
use sandpolis_macros::data;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

pub mod config;
pub mod direct;
pub mod streams;

#[cfg(any(feature = "agent", feature = "server", feature = "client"))]
pub mod forward;

#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "client")]
pub mod client;

#[cfg(all(feature = "client", not(target_os = "android")))]
pub mod cli;

sandpolis_instance::register_layer!("Tunnel", "Application-level tunnels");

/// Which transport a tunnel forwards.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TunnelProtocol {
    #[default]
    Tcp,
    Udp,
}

impl std::fmt::Display for TunnelProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TunnelProtocol::Tcp => "tcp",
            TunnelProtocol::Udp => "udp",
        })
    }
}

/// How a tunnel's data path is (or should be) carried.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TunnelMode {
    /// Bridged through one or more servers. Always works.
    #[default]
    Indirect,
    /// Hole-punched peer-to-peer, falling back to indirect if that fails. Only
    /// meaningful for a client <-> agent tunnel.
    Direct,
}

impl std::fmt::Display for TunnelMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TunnelMode::Indirect => "indirect",
            TunnelMode::Direct => "direct",
        })
    }
}

/// The lifecycle of a configured tunnel.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TunnelState {
    /// Declared, but at least one endpoint isn't reachable yet.
    #[default]
    Pending,
    /// Both endpoints are up and traffic can flow.
    Active,
    /// The tunnel could not be established.
    Failed,
}

impl TunnelState {
    /// Whether the tunnel is currently carrying (or ready to carry) traffic.
    pub fn active(&self) -> bool {
        matches!(self, TunnelState::Active)
    }
}

/// Which side of a tunnel an endpoint is, and what it binds or dials.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum TunnelRole {
    /// Bind `listen` and accept connections to forward.
    Listener { listen: SocketAddr },
    /// Dial `target` (a `host:port`) for each forwarded connection.
    Terminator { target: String },
}

/// A configured tunnel and its live state. Written by the global stratum server
/// that bridges it (global scope, so it replicates out to every client), and
/// rendered by the client GUI as a table row and a decorated world-view link.
#[data]
#[derive(Default)]
pub struct TunnelData {
    /// Human-readable name from the realm config.
    #[secondary_key]
    pub name: String,

    /// The endpoint that binds the listener, when the config named a valid one.
    pub listener_id: Option<InstanceId>,

    /// Where the listener binds, as a `host:port` string.
    pub listen_addr: String,

    /// The endpoint that dials the target, when the config named a valid one.
    pub terminator_id: Option<InstanceId>,

    /// The target the terminator dials, as a `host:port` string.
    pub target_addr: String,

    pub protocol: TunnelProtocol,

    /// The mode requested by the config.
    pub mode: TunnelMode,

    /// The mode actually in effect (may fall back from `Direct` to `Indirect`).
    pub effective_mode: TunnelMode,

    pub state: TunnelState,

    /// Logical connections currently open through the tunnel.
    pub active_connections: u32,

    /// Cumulative bytes flowing target -> listener (download).
    pub rx_bytes: u64,

    /// Cumulative bytes flowing listener -> target (upload).
    pub tx_bytes: u64,

    /// Why the tunnel failed, when it did.
    pub error: Option<String>,
}

inventory::submit! {
    sandpolis_instance::database::sync::SyncRegistration(|r| {
        r.register::<TunnelData>()
    })
}

#[derive(Clone)]
pub struct TunnelManager {
    #[allow(dead_code)]
    database: DatabaseManager,
    #[allow(dead_code)]
    pub instance_id: InstanceId,
}

impl TunnelManager {
    pub async fn new(database: DatabaseManager, instance: InstanceManager) -> Result<Self> {
        Ok(Self {
            instance_id: instance.instance_id,
            database,
        })
    }

    /// Give the server-side orchestrator its network and config context. Called
    /// once at server startup.
    #[cfg(feature = "server")]
    pub fn install_server(&self, context: server::TunnelServerContext) {
        server::install(context);
    }
}

/// Registers the endpoint responder, which serves both the listener and
/// terminator roles. Present on every instance type that can be an endpoint.
#[cfg(any(feature = "agent", feature = "server", feature = "client"))]
pub struct TunnelEndpointResponderRegistration;

#[cfg(any(feature = "agent", feature = "server", feature = "client"))]
impl sandpolis_instance::network::RegisterResponders for TunnelEndpointResponderRegistration {
    fn register_responders(&self, registry: &sandpolis_instance::network::StreamRegistry) {
        registry.register_responder(forward::TunnelStreamResponder::default);
    }
}

#[cfg(any(feature = "agent", feature = "server", feature = "client"))]
inventory::submit!(sandpolis_instance::network::ResponderRegistration(
    &TunnelEndpointResponderRegistration
));

// Reserved for the future direct client->agent data stream. The server-bridged
// streams are server-originated and therefore never pass through a client's
// permission gate, so this only matters once hole punching lands.
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(TunnelStream), "tunnel:open")
}
