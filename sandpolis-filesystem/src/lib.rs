use anyhow::Result;

#[cfg(feature = "client")]
pub mod client;

pub mod session;

#[derive(Clone)]
pub struct FilesystemManager {}

impl FilesystemManager {
    pub async fn new() -> Result<Self> {
        Ok(Self {})
    }
}

// What a client must be granted to open this layer's streams.
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(FsSessionStream), "filesystem:session")
}

/// Static handler for registering filesystem stream responders.
#[cfg(feature = "agent")]
pub struct FilesystemResponderRegistration;

#[cfg(feature = "agent")]
impl sandpolis_instance::network::RegisterResponders for FilesystemResponderRegistration {
    fn register_responders(&self, registry: &sandpolis_instance::network::StreamRegistry) {
        registry.register_responder(session::FsSessionStreamResponder::default);
    }
}

#[cfg(feature = "agent")]
inventory::submit!(sandpolis_instance::network::ResponderRegistration(
    &FilesystemResponderRegistration
));
