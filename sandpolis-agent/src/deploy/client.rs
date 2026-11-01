//! Client side of the deploy stream.
//!
//! The requester does nothing but forward what the server reports into a
//! channel, because progress has to be rendered by the GUI and the GUI can only
//! read it from inside a system. See [`crate::client::gui::deploy`] for the
//! dialog that drains the receiver.

use super::{DeployStreamRequest, DeployStreamResponse};
use anyhow::Result;
use sandpolis_instance::network::StreamRequester;
use sandpolis_macros::Stream;
use tokio::sync::mpsc::{Sender, UnboundedReceiver, UnboundedSender, unbounded_channel};

#[derive(Stream)]
pub struct DeployStreamRequester {
    events: UnboundedSender<DeployStreamResponse>,
}

impl DeployStreamRequester {
    /// Construct a requester paired with the receiver the GUI drains.
    pub fn channel() -> (Self, UnboundedReceiver<DeployStreamResponse>) {
        let (events, rx) = unbounded_channel();
        (Self { events }, rx)
    }
}

impl StreamRequester for DeployStreamRequester {
    type In = DeployStreamResponse;
    type Out = DeployStreamRequest;

    async fn on_message(&self, response: Self::In, _tx: Sender<Self::Out>) -> Result<()> {
        // The receiver is gone once the dialog closes, which is not an error.
        let _ = self.events.send(response);
        Ok(())
    }
}
