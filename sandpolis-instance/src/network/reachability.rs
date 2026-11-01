//! Reachability advertisements: how a server learns to route to instances that
//! aren't connected to it directly.
//!
//! A local stratum (LS) server tells its global stratum (GS) server which
//! instances are attached to it. The GS records those in its [`Relay`] so a
//! client attached to the GS can open a stream to an agent behind the LS without
//! knowing anything about the topology — it addresses the agent by `InstanceId`
//! and the servers work out the path.
//!
//! The reverse direction needs no protocol: an LS forwards anything it can't
//! resolve to its GS by default route (see [`Relay::set_upstream`]).
//!
//! Only server peers may advertise. The GS registers the responder exclusively
//! on connections whose peer is a server, so an agent or client cannot claim to
//! carry traffic for someone else.
//!
//! [`Relay`]: super::stream::Relay
//! [`Relay::set_upstream`]: super::stream::Relay::set_upstream

use super::InstanceConnection;
use super::stream::{Relay, StreamRequester, StreamResponder};
use crate::InstanceId;
use anyhow::Result;
use sandpolis_macros::Stream;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Weak};
use tokio::sync::mpsc::Sender;

/// Sent by a local stratum server up to its global stratum server.
#[derive(Serialize, Deserialize, Debug)]
pub enum ReachabilityRequest {
    /// The complete set of instances directly connected to the sender, replacing
    /// any previous advertisement. Sent on connect and again whenever the set
    /// changes.
    Advertise { instances: Vec<InstanceId> },
}

impl ReachabilityRequest {
    pub fn advertise(instances: &[InstanceId]) -> Self {
        Self::Advertise {
            instances: instances.to_vec(),
        }
    }
}

/// Acknowledgement, so the advertiser can tell the route was installed.
#[derive(Serialize, Deserialize, Debug)]
pub struct ReachabilityAck {
    pub accepted: usize,
}

/// Sends this server's directly-connected instances upstream.
#[derive(Stream)]
pub struct ReachabilityRequester;

impl StreamRequester for ReachabilityRequester {
    type In = ReachabilityAck;
    type Out = ReachabilityRequest;

    async fn on_message(&self, ack: Self::In, _: Sender<Self::Out>) -> Result<()> {
        tracing::debug!(
            accepted = ack.accepted,
            "Reachability advertisement accepted"
        );
        Ok(())
    }
}

/// Installs the advertised routes into this server's relay.
#[derive(Stream)]
pub struct ReachabilityResponder {
    relay: Arc<Relay>,
    /// The advertising connection, held weakly: it owns the stream registry that
    /// owns this responder, so a strong reference would be a cycle.
    via: Weak<InstanceConnection>,
}

impl StreamResponder for ReachabilityResponder {
    type In = ReachabilityRequest;
    type Out = ReachabilityAck;

    async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
        let ReachabilityRequest::Advertise { instances } = request;

        let Some(via) = self.via.upgrade() else {
            // The advertising connection is already gone; its routes are dead.
            return Ok(());
        };

        self.relay.advertise(&via, &instances);
        sender
            .send(ReachabilityAck {
                accepted: instances.len(),
            })
            .await?;
        Ok(())
    }
}

impl InstanceConnection {
    /// Register a [`ReachabilityRequester`] stream for advertising upstream.
    ///
    /// Returns the stream id and outbound sender; the caller encodes and sends
    /// the [`ReachabilityRequest`] itself, and may reuse neither after the
    /// connection drops.
    pub fn open_reachability(
        &self,
    ) -> (
        super::stream::StreamId,
        Sender<super::stream::StreamMessage>,
    ) {
        self.streams.register(ReachabilityRequester)
    }
}

/// Accept reachability advertisements from `connection` and install them in
/// `relay`, withdrawing them again when the connection is cancelled.
///
/// **Call this only for connections whose peer is a server.** It is what lets a
/// peer claim to carry traffic on another instance's behalf, so an agent or
/// client must never be given it.
///
/// Registered after the connection exists (rather than through the handler list
/// passed at construction) because the responder needs a handle to the very
/// connection it is being attached to.
pub fn accept_advertisements(connection: &Arc<InstanceConnection>, relay: Arc<Relay>) {
    let via = Arc::downgrade(connection);

    {
        let relay = relay.clone();
        let via = via.clone();
        connection
            .streams
            .register_responder(move || ReachabilityResponder {
                relay: relay.clone(),
                via: via.clone(),
            });
    }

    // Routes through a dead peer are worse than no route: `next_hop` would pick
    // it and the message would be dropped rather than falling through to the
    // default route.
    // An unidentified peer never advertised, so there is nothing to withdraw.
    let Some(peer) = connection.data.read().remote_instance else {
        return;
    };
    let cancel = connection.cancel.clone();
    tokio::spawn(async move {
        cancel.cancelled().await;
        relay.withdraw(peer);
        tracing::debug!(via = %peer, "Withdrew routes for a closed server connection");
    });
}
