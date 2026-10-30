//! The direct (hole-punched) tunnel seam.
//!
//! A `Direct` tunnel between a client and an agent would establish a
//! peer-to-peer connection with UDP hole punching and run the data stream over
//! it, bypassing the server entirely. None of that transport exists yet — there
//! is no UDP transport, no DTLS `InstanceConnection`, and no STUN/TURN-style
//! rendezvous — so [`attempt_direct`] always fails and the caller falls back to
//! the indirect server bridge.
//!
//! When hole punching is implemented, `attempt_direct` returns a live
//! `InstanceConnection` over a DTLS transport (the third transport the
//! `InstanceConnection` doc comment anticipates), the rendezvous below is
//! exchanged through the server to synchronize the punch, and the tunnel's data
//! stream runs over the returned connection instead of the bridge.

use sandpolis_instance::InstanceId;
use serde::{Deserialize, Serialize};

/// What a peer needs from the server to attempt a direct connection: the other
/// peer's observed public address and a shared key, plus a clock so the server
/// can forecast a synchronized punch time.
///
/// Not yet exchanged.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Rendezvous {
    /// The peer to punch toward.
    pub peer: InstanceId,
    /// The peer's observed public `host:port`.
    pub peer_addr: String,
    /// Symmetric key both peers use to authenticate the punched connection.
    pub key: Vec<u8>,
    /// The initiating peer's clock, so the server can forecast the punch time.
    pub clock: u64,
}

/// The outcome of a direct-connection attempt.
pub enum DirectOutcome {
    /// Hole punching is not implemented; the caller must use the indirect bridge.
    Unsupported,
}

/// Attempt to establish a direct peer-to-peer connection to `peer`.
///
/// Always returns [`DirectOutcome::Unsupported`] today. This is the single seam
/// real NAT traversal drops into: on success it will yield an
/// `InstanceConnection` the tunnel's data stream runs over.
pub fn attempt_direct(_peer: InstanceId) -> DirectOutcome {
    DirectOutcome::Unsupported
}
