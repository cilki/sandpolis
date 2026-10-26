use anyhow::Result;
use native_db::*;
use native_model::Model;
use regex::Regex;
use sandpolis_instance::InstanceId;
use sandpolis_instance::database::DatabaseManager;
use sandpolis_instance::network::{RegisterResponders, StreamRegistry};
use sandpolis_macros::data;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::trace;


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
pub struct ShellManager {
    database: DatabaseManager,
    #[cfg(feature = "agent")]
    sessions: Arc<Mutex<Vec<crate::session::ShellSessionStreamResponder>>>,
}

impl ShellManager {
    pub async fn new(database: DatabaseManager) -> Result<Self> {
        Ok(Self {
            database,
            #[cfg(feature = "agent")]
            sessions: Arc::new(Mutex::new(Vec::new())),
        })
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

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub enum ShellType {
    /// Busybox shell
    Ash,

    /// Bourne Again shell
    Bash,

    /// Microsoft CMD.EXE
    CmdExe,
    /// C shell
    Csh,
    Dash,
    /// Fish shell (https://fishshell.com)
    Fish,
    /// Korn shell (http://kornshell.com)
    Ksh,
    Powershell,
    /// Generic POSIX shell
    Sh,

    /// We couldn't figure out the shell type
    #[default]
    Unknown,

    /// Z shell
    Zsh,
}

#[data]
pub struct ShellSessionData {
    #[secondary_key]
    pub _instance_id: InstanceId,

    pub shell_type: ShellType,

    /// Path to the shell's executable
    pub shell: Option<PathBuf>,

    pub started: Option<u64>,

    pub ended: Option<u64>,

    /// Total CPU time used by the process and all children
    pub cpu_time: f64,

    /// Peak memory used by the process and all children
    pub max_memory: u64,
}

/// A reusable shell script.
#[derive(Serialize, Deserialize)]
pub struct ShellSnippet {
    pub shell_type: ShellType,
    pub content: String,
}

#[derive(Serialize, Deserialize)]
pub enum ShellCommand {
    /// Inline command
    Command(Vec<String>),
    /// Snippet ID
    Snippet(String),
}

#[derive(Serialize, Deserialize)]
pub struct DiscoveredShell {
    /// Closest type of discovered shell
    pub shell_type: ShellType,

    /// Location of the shell executable
    pub location: PathBuf,

    // Version number if available
    pub version: Option<String>,
}

impl DiscoveredShell {
    pub async fn scan() -> Result<Vec<DiscoveredShell>> {
        let mut shells = Vec::new();

        // Search for bash
        match Command::new("bash").arg("--version").output().await {
            Ok(output) => {
                if let Ok(stdout) = String::from_utf8(output.stdout) {
                    let version = Regex::new(r"version ([1-9]+\.[0-9]+\.[0-9]+\S*)")?
                        .captures(&stdout)
                        .map(|m| m[1].to_string());

                    let location =
                        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                            .map(|p| p.join("bash"))
                            .find(|p| p.is_file())
                            .unwrap_or_else(|| PathBuf::from("bash"));

                    shells.push(DiscoveredShell {
                        shell_type: ShellType::Bash,
                        location,
                        version,
                    });
                }
            }
            Err(_) => trace!("Bash shell not found"),
        };

        Ok(shells)
    }
}

#[cfg(test)]
mod test_discovered_shell {
    #[tokio::test]
    pub async fn test_scan() {
        // Assume at least one shell is available
        assert!(super::DiscoveredShell::scan().await.unwrap().len() > 0);
    }
}

/// Supported shell information.
#[derive(Serialize, Deserialize)]
pub struct ShellListResponse {
    pub shells: Vec<DiscoveredShell>,
}

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
