//! Wire types for the boot stream, which a boot agent opens toward its server
//! right after connecting. The server answers with whether a boot hold is set;
//! a held agent stays on the stream until the server releases it.

use sandpolis_instance::InstanceId;
use sandpolis_macros::Stream;
use serde::{Deserialize, Serialize};

/// Agent -> server messages on the boot stream.
#[derive(Serialize, Deserialize)]
pub enum BootStreamRequest {
    /// Sent once when a boot agent connects
    Announce { agent: InstanceId },
}

/// Server -> agent messages on the boot stream.
#[derive(Serialize, Deserialize, Debug)]
pub enum BootStreamResponse {
    /// A hold is set; stay connected and await further instructions
    Hold,

    /// No hold; chainload normally
    Proceed,

    /// The hold is finished; reboot now
    Release,
}

/// Agent side of the boot stream: flips flags on the shared
/// [`BootAgentState`](super::BootAgentState) that the boot UI samples.
#[derive(Stream)]
pub struct BootStreamRequester {
    pub state: std::sync::Arc<super::BootAgentState>,
}

#[cfg(feature = "agent")]
impl sandpolis_instance::network::StreamRequester for BootStreamRequester {
    type In = BootStreamResponse;
    type Out = BootStreamRequest;

    async fn on_message(
        &self,
        response: Self::In,
        _: tokio::sync::mpsc::Sender<Self::Out>,
    ) -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;

        tracing::debug!(?response, "Boot stream instruction");
        match response {
            BootStreamResponse::Hold => self.state.hold.store(true, Ordering::Relaxed),
            BootStreamResponse::Proceed => self.state.hold.store(false, Ordering::Relaxed),
            BootStreamResponse::Release => self.state.release.store(true, Ordering::Relaxed),
        }
        Ok(())
    }
}
