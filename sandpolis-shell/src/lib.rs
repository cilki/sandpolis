use anyhow::Result;
use sandpolis_instance::network::{RegisterResponders, StreamRegistry};
use serde::{Deserialize, Serialize};

#[cfg(feature = "client")]
pub mod client;
#[cfg(all(feature = "client", not(target_os = "android")))]
pub mod cli;
pub mod execute;
pub mod session;
pub mod shell;
#[cfg(feature = "probe")]
pub mod ssh;

sandpolis_instance::register_layer!("Shell", "Remote shell access and command execution");

#[derive(Clone)]
pub struct ShellManager;

impl ShellManager {
    pub async fn new() -> Result<Self> {
        Ok(Self)
    }
}

/// Static handler for registering shell stream responders.
#[cfg(feature = "agent")]
pub struct ShellResponderRegistration;

#[cfg(feature = "agent")]
impl RegisterResponders for ShellResponderRegistration {
    fn register_responders(&self, registry: &StreamRegistry) {
        registry.register_responder(|| execute::ShellExecuteStreamResponder);
        registry.register_responder(session::ShellSessionStreamResponder::default);
    }
}

#[cfg(feature = "agent")]
inventory::submit!(sandpolis_instance::network::ResponderRegistration(
    &ShellResponderRegistration
));

/// Static handler for registering SSH probe responders.
///
/// Separate from [`ShellResponderRegistration`] because probes are reached only
/// from servers, whereas the PTY responder above only exists on agents.
#[cfg(all(feature = "server", feature = "probe"))]
pub struct ShellProbeResponderRegistration;

#[cfg(all(feature = "server", feature = "probe"))]
impl RegisterResponders for ShellProbeResponderRegistration {
    fn register_responders(&self, registry: &StreamRegistry) {
        registry.register_responder(ssh::SshSessionStreamResponder::default);
    }
}

#[cfg(all(feature = "server", feature = "probe"))]
inventory::submit!(sandpolis_instance::network::ResponderRegistration(
    &ShellProbeResponderRegistration
));

/// What a shell execute stream runs: each line is written to the shell's stdin
/// in turn.
#[derive(Serialize, Deserialize)]
pub struct ShellCommand(pub Vec<String>);

// What a client must be granted to open this layer's streams.
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(ShellExecuteStream), "shell:execute")
}
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(ShellSessionStream), "shell:session")
}
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(SshSessionStream), "shell:ssh")
}
