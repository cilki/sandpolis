use crate::RuntimeOptions;
use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use colored::Colorize;
use std::path::PathBuf;
use std::process::ExitCode;
use tracing::info;

#[cfg(feature = "client")]
use sandpolis_client::cli::TargetArgs;

#[derive(Parser, Debug, Clone)]
// `about` with no value is the crate description, so the help text and the
// package metadata can't drift apart.
#[clap(author, version, about)]
pub struct CommandLine {
    #[clap(flatten)]
    pub instance: sandpolis_instance::cli::InstanceCommandLine,

    /// A process is exactly one instance, so which one it is has to be said.
    #[command(subcommand)]
    pub command: Commands,
}

/// Flags for the server daemon (`sandpolis server`).
#[cfg(feature = "server")]
#[derive(clap::Args, Debug, Clone)]
pub struct ServerArgs {
    /// Directory holding this server's database and the realm configs it
    /// serves ($S7S_DATA).
    ///
    /// Every `*.realm.ron` file in the directory declares one realm, named
    /// after the part of the filename before the suffix. A blank file means
    /// "generate a realm CA for me", which is then written back into the file —
    /// after which that file is the durable copy of the realm's trust root. A
    /// global stratum server that finds no realm config creates
    /// `default.realm.ron`.
    ///
    /// Without this flag the server is ephemeral: its databases are kept in
    /// memory and nothing survives the process.
    #[clap(long, value_name = "DIR", env = "S7S_DATA")]
    pub data: Option<PathBuf>,

    /// Path to a realm cert naming this server's upstream ($S7S_REALM).
    ///
    /// Having one is what puts this server in the local stratum: it carries the
    /// realm CA and this server's own certificate, whose common name is the
    /// upstream's address. A local stratum server serves no realms of its own.
    #[clap(long, value_name = "PATH", env = "S7S_REALM")]
    pub realm: Option<PathBuf>,

    /// Address:port for this server to listen on.
    #[clap(long, default_value = "0.0.0.0:8768")]
    pub listen: std::net::SocketAddr,

    /// Hostname clients and agents will use to reach this server
    /// ($S7S_SERVER_NAME).
    ///
    /// Certificates minted for realms that declare no `address` of their own
    /// carry this host in their common name, which is where instances loading
    /// those certificates dial. Defaults to the machine's hostname, falling
    /// back to `127.0.0.1` if it has none.
    #[clap(long, value_name = "HOST", env = "S7S_SERVER_NAME", value_parser = parse_server_name)]
    pub server_name: Option<String>,
}

/// Accept only a bare host for `--server-name`: the port comes from `--listen`
/// and the realm from each realm config, so anything else smuggled into the
/// value (`:port`, `/realm`) would be silently dropped or corrupt the
/// certificate's common name. Round-tripping through [`ServerUrl`] also
/// guarantees the host parses back on the instances that load the certificate.
#[cfg(feature = "server")]
fn parse_server_name(s: &str) -> Result<String, String> {
    use sandpolis_server::ServerUrl;

    let url: ServerUrl = s
        .parse()
        .map_err(|e| format!("not a valid hostname: {e}"))?;
    if url.host != s
        || url.port != ServerUrl::default_port()
        || url.realm != Default::default()
    {
        return Err("expected a bare hostname or IP address, without port or realm".into());
    }
    Ok(url.host)
}

/// Flags for the agent daemon (`sandpolis agent`).
#[cfg(feature = "agent")]
#[derive(clap::Args, Debug, Clone, Default)]
pub struct AgentArgs {
    /// Directory holding this agent's database ($S7S_DATA).
    ///
    /// Without this flag the agent is ephemeral: its database is kept in memory
    /// and nothing survives the process.
    #[clap(long, value_name = "DIR", env = "S7S_DATA")]
    pub data: Option<PathBuf>,

    /// Path to the realm cert naming the server this agent attaches to
    /// ($S7S_REALM).
    ///
    /// It carries the realm CA and this agent's own certificate, whose common
    /// name is the server's address.
    #[clap(long, value_name = "PATH", env = "S7S_REALM")]
    pub realm: Option<PathBuf>,

    /// Cron expression putting the agent in polling mode, e.g. `0 */5 * * * *`
    /// for a check-in every five minutes ($S7S_POLL).
    ///
    /// Without it the agent stays connected continuously.
    #[cfg(not(feature = "uki"))]
    #[clap(long, value_name = "CRON", env = "S7S_POLL")]
    pub poll: Option<String>,

    /// How long each check-in window stays open, in seconds ($S7S_POLL_TIMEOUT).
    ///
    /// Only meaningful alongside `--poll`.
    #[cfg(not(feature = "uki"))]
    #[clap(
        long,
        value_name = "SECONDS",
        env = "S7S_POLL_TIMEOUT",
        default_value_t = sandpolis_agent::PollConfig::default_timeout_secs(),
    )]
    pub poll_timeout: u64,
}

/// Flags for the client, which every client subcommand shares.
#[cfg(feature = "client")]
#[derive(clap::Args, Debug, Clone, Default)]
pub struct ClientArgs {
    /// Path to a realm cert naming the server this client connects to
    /// ($S7S_REALM).
    ///
    /// It carries the realm CA and this client's own certificate, whose common
    /// name is the server's address. Without it, realm certs are read from
    /// `--data` (or the default state directory), and if none are found there
    /// either, the GUI asks for one to be picked interactively.
    #[clap(long, value_name = "PATH", env = "S7S_REALM")]
    pub realm: Option<PathBuf>,

    /// Directory holding this client's database and its realm certs
    /// ($S7S_DATA).
    ///
    /// Every `*.realm.pem` file in the directory attaches this client to the
    /// server that cert names, which is how a client is configured without
    /// naming a file on the command line. Without this flag the client is
    /// ephemeral: its database is kept in memory and nothing survives the
    /// process.
    #[clap(long, value_name = "DIR", env = "S7S_DATA")]
    pub data: Option<PathBuf>,
}

#[cfg(feature = "server")]
impl ServerArgs {
    /// The process-wide options these flags describe. The realms and stratum
    /// are filled in by the caller, which is what reads the `--data` directory
    /// and the realm cert.
    pub fn options(&self) -> RuntimeOptions {
        RuntimeOptions {
            database: sandpolis_instance::database::config::DatabaseConfig {
                storage: self.data.clone(),
                ..Default::default()
            },
            instance_type: sandpolis_instance::InstanceType::Server,
            listen: self.listen,
            server_name: self.server_name.clone(),
            ..Default::default()
        }
    }
}

#[cfg(feature = "agent")]
impl AgentArgs {
    /// The process-wide options these flags describe. What the realm cert
    /// contributes is filled in by the caller, which is what loads it.
    pub fn options(&self) -> RuntimeOptions {
        RuntimeOptions {
            database: sandpolis_instance::database::config::DatabaseConfig {
                storage: self.data.clone(),
                ..Default::default()
            },
            instance_type: sandpolis_instance::InstanceType::Agent,
            #[cfg(not(feature = "uki"))]
            poll: self
                .poll
                .clone()
                .map(|schedule| sandpolis_agent::PollConfig {
                    schedule,
                    timeout_secs: self.poll_timeout,
                }),
            ..Default::default()
        }
    }
}

#[cfg(feature = "client")]
impl ClientArgs {
    /// The process-wide options these flags describe.
    pub fn options(&self) -> RuntimeOptions {
        RuntimeOptions {
            database: sandpolis_instance::database::config::DatabaseConfig {
                storage: self.data.clone(),
                ..Default::default()
            },
            instance_type: sandpolis_instance::InstanceType::Client,
            ..Default::default()
        }
    }
}

/// Subcommands for `sandpolis agents`, which manages agent instances.
#[cfg(feature = "client")]
#[derive(Subcommand, Debug, Clone)]
pub enum AgentsCommand {
    /// List all connected agents
    List {
        /// Emit machine-readable JSON instead of opening a TUI
        #[clap(long)]
        json: bool,

        #[clap(flatten)]
        client: ClientArgs,
    },
    /// Restart (reboot) the target agent's device
    Restart {
        /// Target instance
        #[clap(long)]
        instance: sandpolis_instance::InstanceId,

        #[clap(flatten)]
        client: ClientArgs,
    },
    /// Install an agent on a host over SSH, or reconfigure one already there
    Deploy {
        /// Target host: `[user@]host` or a `~/.ssh/config` alias
        host: String,

        /// SSH username (default: the user@ prefix, then ~/.ssh/config, then
        /// $USER)
        #[clap(long)]
        user: Option<String>,

        /// SSH port (default: ~/.ssh/config, then 22)
        #[clap(long)]
        port: Option<u16>,

        /// Private key file (default: ~/.ssh/config, then ~/.ssh/id_*).
        /// Without one, a password is prompted for on the terminal.
        #[clap(long, value_name = "PATH")]
        key: Option<PathBuf>,

        /// Expected SHA256 host key fingerprint; without one the host key is
        /// trusted on first use
        #[clap(long)]
        fingerprint: Option<String>,

        /// Cron expression putting the deployed agent in polling mode
        #[clap(long, value_name = "CRON")]
        poll: Option<String>,

        /// Check-in window length for --poll, in seconds
        #[clap(long, value_name = "SECONDS",
               default_value_t = sandpolis_agent::PollConfig::default_timeout_secs())]
        poll_timeout: u64,

        /// Report what the deployment would do without changing the target
        #[clap(long)]
        dryrun: bool,

        /// Emit a machine-readable JSON result instead of progress lines
        #[clap(long)]
        json: bool,

        #[clap(flatten)]
        client: ClientArgs,
    },
}

/// Subcommands for `sandpolis servers`, which manages configured servers.
#[cfg(feature = "client")]
#[derive(Subcommand, Debug, Clone)]
pub enum ServersCommand {
    /// List all configured servers
    List {
        /// Emit machine-readable JSON instead of opening a TUI
        #[clap(long)]
        json: bool,

        #[clap(flatten)]
        client: ClientArgs,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
    /// Show the layers compiled into this build, and their versions
    About,

    /// Run the configuration LSP
    #[cfg(feature = "client")]
    Lsp {
        #[clap(flatten)]
        args: crate::lsp::LspArgs,
    },

    /// Run the agent daemon
    #[cfg(feature = "agent")]
    Agent {
        #[clap(flatten)]
        args: AgentArgs,
    },

    /// Run the server daemon
    #[cfg(feature = "server")]
    Server {
        #[clap(flatten)]
        args: ServerArgs,
    },

    /// Manage agent instances
    #[cfg(feature = "client")]
    Agents {
        #[command(subcommand)]
        action: AgentsCommand,
    },

    /// Manage configured servers
    #[cfg(feature = "client")]
    Servers {
        #[command(subcommand)]
        action: ServersCommand,
    },

    /// Run the client in the foreground
    #[cfg(feature = "client")]
    Client {
        #[clap(flatten)]
        client: ClientArgs,
    },

    /// Manage probes
    #[cfg(all(feature = "client", feature = "probe"))]
    Probe {
        #[command(subcommand)]
        action: Option<sandpolis_probe::cli::ProbeCommand>,

        #[clap(flatten)]
        target: TargetArgs,

        #[clap(flatten)]
        client: ClientArgs,
    },

    /// Connect to remote desktop sessions
    #[cfg(all(feature = "client", feature = "desktop"))]
    Desktop {
        #[command(subcommand)]
        action: Option<sandpolis_desktop::cli::DesktopCommand>,

        #[clap(flatten)]
        target: TargetArgs,

        #[clap(flatten)]
        client: ClientArgs,
    },

    /// Connect to remote shell sessions
    #[cfg(all(feature = "client", feature = "shell"))]
    Shell {
        #[clap(flatten)]
        target: TargetArgs,

        #[clap(flatten)]
        client: ClientArgs,
    },

    /// Inspect agent health
    #[cfg(all(feature = "client", feature = "health"))]
    Health {
        #[clap(flatten)]
        target: TargetArgs,

        #[clap(flatten)]
        client: ClientArgs,
    },

    /// Inspect agent inventory
    #[cfg(all(feature = "client", feature = "inventory"))]
    Inventory {
        #[clap(flatten)]
        target: TargetArgs,

        #[clap(flatten)]
        client: ClientArgs,
    },

    /// Browse agent filesystems
    #[cfg(all(feature = "client", feature = "filesystem"))]
    Filesystem {
        #[clap(flatten)]
        target: TargetArgs,

        #[clap(flatten)]
        client: ClientArgs,
    },

    /// Manage accounts
    #[cfg(all(feature = "client", feature = "account"))]
    Account {
        #[clap(flatten)]
        target: TargetArgs,

        #[clap(flatten)]
        client: ClientArgs,
    },

    /// Manage cold snapshots
    #[cfg(all(feature = "client", feature = "snapshot"))]
    Snapshot {
        #[command(subcommand)]
        action: Option<sandpolis_snapshot::cli::SnapshotCommand>,

        #[clap(flatten)]
        target: TargetArgs,

        #[clap(flatten)]
        client: ClientArgs,
    },

    /// Wake / power control (interactive TUI)
    #[cfg(feature = "client")]
    Wake {
        #[clap(flatten)]
        target: TargetArgs,

        #[clap(flatten)]
        client: ClientArgs,
    },

    /// Inspect audit events (interactive TUI)
    #[cfg(all(feature = "client", feature = "audit"))]
    Audit {
        #[clap(flatten)]
        target: TargetArgs,

        #[clap(flatten)]
        client: ClientArgs,
    },

    /// Manage tunnels
    #[cfg(all(feature = "client", feature = "tunnel"))]
    Tunnel {
        #[command(subcommand)]
        action: Option<sandpolis_tunnel::cli::TunnelCommand>,

        #[clap(flatten)]
        target: TargetArgs,

        #[clap(flatten)]
        client: ClientArgs,
    },
}

impl Commands {
    /// Commands that run on their own without starting any instances or
    /// establishing a client connection.
    pub fn standalone(&self) -> bool {
        match self {
            Commands::About => true,
            #[cfg(feature = "client")]
            Commands::Lsp { .. } => true,
            #[allow(unreachable_patterns)]
            _ => false,
        }
    }

    /// The flags for the server daemon, when that's what this command is.
    #[cfg(feature = "server")]
    pub fn server_daemon(&self) -> Option<&ServerArgs> {
        match self {
            Commands::Server { args } => Some(args),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The flags for the agent daemon, when that's what this command is.
    #[cfg(feature = "agent")]
    pub fn agent_daemon(&self) -> Option<&AgentArgs> {
        match self {
            Commands::Agent { args } => Some(args),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The client flags this command carries, which is also how a command says
    /// it runs as a client at all. Daemons and standalone commands have none.
    #[cfg(feature = "client")]
    pub fn client_args(&self) -> Option<&ClientArgs> {
        match self {
            Commands::Client { client } => Some(client),
            Commands::Agents { action } => match action {
                AgentsCommand::List { client, .. } => Some(client),
                AgentsCommand::Restart { client, .. } => Some(client),
                AgentsCommand::Deploy { client, .. } => Some(client),
            },
            Commands::Servers { action } => match action {
                ServersCommand::List { client, .. } => Some(client),
            },
            #[cfg(feature = "probe")]
            Commands::Probe { client, .. } => Some(client),
            #[cfg(feature = "desktop")]
            Commands::Desktop { client, .. } => Some(client),
            #[cfg(feature = "shell")]
            Commands::Shell { client, .. } => Some(client),
            #[cfg(feature = "health")]
            Commands::Health { client, .. } => Some(client),
            #[cfg(feature = "inventory")]
            Commands::Inventory { client, .. } => Some(client),
            #[cfg(feature = "filesystem")]
            Commands::Filesystem { client, .. } => Some(client),
            #[cfg(feature = "account")]
            Commands::Account { client, .. } => Some(client),
            #[cfg(feature = "snapshot")]
            Commands::Snapshot { client, .. } => Some(client),
            Commands::Wake { client, .. } => Some(client),
            #[cfg(feature = "audit")]
            Commands::Audit { client, .. } => Some(client),
            #[cfg(feature = "tunnel")]
            Commands::Tunnel { client, .. } => Some(client),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// Whether this command draws in the terminal it was started from, which is
    /// every client subcommand except the GUI. Those get a log file instead of
    /// stderr so the logs don't corrupt the view.
    pub fn owns_terminal(&self) -> bool {
        #[cfg(feature = "client")]
        {
            !matches!(self, Commands::Client { .. }) && self.client_args().is_some()
        }
        #[cfg(not(feature = "client"))]
        {
            false
        }
    }

    /// Dispatch a [`standalone`](Self::standalone) command. These run without
    /// starting any instances or opening a client connection. Panics if called
    /// with a client subcommand (those go through `dispatch_client`).
    #[allow(unused_variables)]
    pub async fn dispatch_standalone(self) -> Result<ExitCode> {
        match self {
            #[cfg(feature = "client")]
            Commands::Lsp { args } => {
                crate::lsp::run(args).await?;
            }
            Commands::About => {
                // The emblem as the terminal can take it, decided entirely by
                // cilki: nothing when redirected or when color is turned off.
                if let Some(emblem) = cilki::terminal::banner(&cilki::emblem::SANDPOLIS, 56, 7) {
                    print!("{emblem}");
                }
                println!(
                    "{} {}\n",
                    env!("CARGO_PKG_NAME").bold(),
                    env!("CARGO_PKG_VERSION")
                );

                let layers = crate::layers();
                let width = layers
                    .iter()
                    .map(|layer| layer.name.len())
                    .chain(std::iter::once("Layer".len()))
                    .max()
                    .unwrap_or_default();

                // Padded before it's bolded: the escape codes count toward a
                // format width, so bolding each cell first would skew them.
                println!(
                    "{}",
                    format!("{:width$}  {:7}  {}", "Layer", "Version", "Description").bold()
                );
                for layer in layers {
                    // An unparseable version is the registration's problem, not
                    // the user's; show what it says rather than failing the
                    // whole command.
                    let version = match layer.version() {
                        Ok(version) => version.to_string(),
                        Err(_) => layer.version.to_string(),
                    };
                    println!("{:width$}  {version:7}  {}", layer.name, layer.description);
                }
            }
            #[allow(unreachable_patterns)]
            _ => unreachable!("client subcommands are dispatched by dispatch_client"),
        }
        Ok(ExitCode::SUCCESS)
    }

    /// Dispatch a client subcommand: opens a focused TUI, or runs
    /// noninteractively (`--json`). Requires the live [`InstanceState`].
    #[cfg(feature = "client")]
    pub async fn dispatch_client(self, state: &crate::InstanceState) -> Result<ExitCode> {
        match self {
            Commands::Agents { action } => client::agents(action).await,
            Commands::Servers { action } => client::servers(action, &state.server).await,
            #[cfg(feature = "probe")]
            Commands::Probe { action, target, .. } => {
                sandpolis_probe::cli::dispatch(action, target, &state.probe).await
            }
            #[cfg(feature = "desktop")]
            Commands::Desktop { action, target, .. } => {
                sandpolis_desktop::cli::dispatch(action, target, &state.desktop).await
            }
            #[cfg(feature = "shell")]
            Commands::Shell { target, .. } => {
                sandpolis_shell::cli::dispatch(target, state.shell.clone()).await
            }
            #[cfg(feature = "health")]
            Commands::Health { target, .. } => client::stub("health", target).await,
            #[cfg(feature = "inventory")]
            Commands::Inventory { target, .. } => client::stub("inventory", target).await,
            #[cfg(feature = "filesystem")]
            Commands::Filesystem { target, .. } => client::stub("filesystem", target).await,
            #[cfg(feature = "account")]
            Commands::Account { target, .. } => client::stub("account", target).await,
            #[cfg(feature = "snapshot")]
            Commands::Snapshot { action, target, .. } => {
                sandpolis_snapshot::cli::dispatch(action, target).await
            }
            Commands::Wake { target, .. } => client::stub("wake", target).await,
            #[cfg(feature = "audit")]
            Commands::Audit { target, .. } => client::stub("audit", target).await,
            #[cfg(feature = "tunnel")]
            Commands::Tunnel { action, target, .. } => {
                sandpolis_tunnel::cli::dispatch(action, target).await
            }
            #[allow(unreachable_patterns)]
            _ => unreachable!("standalone commands are dispatched by dispatch_standalone"),
        }
    }
}

#[cfg(all(test, feature = "server"))]
mod test_stratum_args {
    use super::*;
    use sandpolis_instance::realm::RealmCert;
    use sandpolis_server::ServerStratum;

    /// Write a realm cert whose certificate names `url`, the way a server
    /// writes one out for its realm at startup.
    fn upstream_realm_cert(dir: &std::path::Path, url: &str) -> Result<PathBuf> {
        let url: sandpolis_server::ServerUrl = url.parse()?;
        let ca = RealmCert::new_cluster(Default::default(), url.realm.clone())?;
        let path = dir.join("upstream.realm.pem");
        ca.endpoint_cert(&url)?.write_pem(&path)?;
        Ok(path)
    }

    /// The flags `sandpolis server` was given.
    fn server_args(argv: &[&str]) -> Result<ServerArgs, clap::Error> {
        match CommandLine::try_parse_from(argv)?.command {
            Commands::Server { args, .. } => Ok(args),
            other => panic!("expected the server subcommand, got {other:?}"),
        }
    }

    /// A process is one instance, so it has to say which one.
    #[test]
    fn an_instance_is_required() {
        assert!(CommandLine::try_parse_from(["sandpolis"]).is_err());
    }

    /// `--server-name` is a bare host: the port belongs to `--listen` and the
    /// realm to each realm config, so values carrying either are rejected.
    #[test]
    fn server_name_is_a_bare_host() {
        let args = server_args(&["sandpolis", "server", "--server-name", "gs.example.com"])
            .expect("a bare hostname parses");
        assert_eq!(args.server_name.as_deref(), Some("gs.example.com"));

        assert!(server_args(&["sandpolis", "server", "--server-name", "host:9000"]).is_err());
        assert!(server_args(&["sandpolis", "server", "--server-name", "host/realm"]).is_err());
    }

    /// Absent `--realm`, this is the network's single global stratum server.
    #[test]
    fn no_realm_cert_means_global_stratum() -> Result<()> {
        let args = server_args(&["sandpolis", "server"]).expect("`server` parses");
        assert_eq!(
            crate::stratum(args.realm.as_deref())?,
            ServerStratum::Global
        );
        Ok(())
    }

    /// The realm cert names the upstream and selects the local stratum; the
    /// address comes out of the certificate rather than a separate flag.
    #[test]
    fn realm_cert_means_local_stratum() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = upstream_realm_cert(dir.path(), "gs.example.com:8768/default")?;

        let args = server_args(&["sandpolis", "server", "--realm", path.to_str().unwrap()])
            .expect("--realm parses");

        let ServerStratum::Local { global } = crate::stratum(args.realm.as_deref())? else {
            panic!("--realm must select the local stratum");
        };
        assert_eq!(global.host, "gs.example.com");
        assert_eq!(global.port, 8768);
        Ok(())
    }

    /// A local stratum server keeps a database of its own, so the two flags
    /// describe different things and go together.
    #[test]
    fn data_and_realm_go_together() -> Result<()> {
        let args = server_args(&[
            "sandpolis",
            "server",
            "--data",
            "/var/lib/sandpolis",
            "--realm",
            "./upstream.realm.pem",
        ])
        .expect("--data and --realm parse together");

        assert_eq!(
            args.data.as_deref(),
            Some(std::path::Path::new("/var/lib/sandpolis"))
        );
        Ok(())
    }

    /// A realm cert that isn't there is a misconfiguration, not something to
    /// silently fall back from.
    #[test]
    fn missing_realm_cert_is_an_error() {
        let args = server_args(&[
            "sandpolis",
            "server",
            "--realm",
            "/nonexistent/upstream.realm.pem",
        ])
        .expect("--realm parses");
        assert!(crate::stratum(args.realm.as_deref()).is_err());
    }
}

#[cfg(all(test, feature = "agent"))]
mod test_agent_args {
    use super::*;

    /// The agent daemon's flags belong to the agent daemon, not to the process.
    #[test]
    fn agent_takes_its_own_flags() {
        let command = CommandLine::try_parse_from([
            "sandpolis",
            "agent",
            "--realm",
            "./fleet.realm.pem",
            "--data",
            "/var/lib/sandpolis",
        ])
        .expect("`agent --realm --data` parses")
        .command;

        let Commands::Agent { args, .. } = command else {
            panic!("expected the agent subcommand, got {command:?}");
        };
        assert_eq!(
            args.realm.as_deref(),
            Some(std::path::Path::new("./fleet.realm.pem"))
        );
        assert_eq!(
            args.data.as_deref(),
            Some(std::path::Path::new("/var/lib/sandpolis"))
        );
    }

    /// Polling is a property of how this agent runs rather than of the
    /// certificate it holds, so it comes from the command line.
    #[cfg(not(feature = "uki"))]
    #[test]
    fn poll_flags_build_the_poll_config() {
        let command = CommandLine::try_parse_from([
            "sandpolis",
            "agent",
            "--poll",
            "0 */5 * * * *",
            "--poll-timeout",
            "45",
        ])
        .expect("`agent --poll --poll-timeout` parses")
        .command;

        let Commands::Agent { args, .. } = command else {
            panic!("expected the agent subcommand, got {command:?}");
        };
        let poll = args.options().poll.expect("--poll selects polling mode");
        assert_eq!(poll.schedule, "0 */5 * * * *");
        assert_eq!(poll.timeout_secs, 45);
    }

    /// Without `--poll` the agent stays continuously connected, whatever
    /// `--poll-timeout` says.
    #[cfg(not(feature = "uki"))]
    #[test]
    fn no_poll_flag_means_continuous() {
        let command = CommandLine::try_parse_from(["sandpolis", "agent", "--poll-timeout", "45"])
            .expect("`agent --poll-timeout` parses")
            .command;

        let Commands::Agent { args, .. } = command else {
            panic!("expected the agent subcommand, got {command:?}");
        };
        assert!(args.options().poll.is_none());
    }
}

#[cfg(all(test, feature = "client"))]
mod test_client_commands {
    use super::*;

    /// Managing agent instances is `agents list`, distinct from `agent` which
    /// runs the daemon.
    #[test]
    fn agents_list_parses() {
        let command = CommandLine::try_parse_from(["sandpolis", "agents", "list", "--json"])
            .expect("`agents list --json` parses")
            .command;

        let Commands::Agents {
            action: AgentsCommand::List { json, .. },
        } = command
        else {
            panic!("expected the agents list subcommand, got {command:?}");
        };
        assert!(json);
    }

    /// Managing configured servers is `servers list`, distinct from `server`
    /// which runs the daemon.
    #[test]
    fn servers_list_parses() {
        let command = CommandLine::try_parse_from(["sandpolis", "servers", "list", "--json"])
            .expect("`servers list --json` parses")
            .command;

        let Commands::Servers {
            action: ServersCommand::List { json, .. },
        } = command
        else {
            panic!("expected the servers list subcommand, got {command:?}");
        };
        assert!(json);
    }

    /// The plural commands manage instances, so they need an action to say
    /// which management operation to run.
    #[test]
    fn agents_requires_a_subcommand() {
        assert!(CommandLine::try_parse_from(["sandpolis", "agents"]).is_err());
    }

    /// `agents deploy` takes the target as `[user@]host` plus flags.
    #[test]
    fn agents_deploy_parses() {
        let command = CommandLine::try_parse_from([
            "sandpolis",
            "agents",
            "deploy",
            "root@example.com",
            "--port",
            "2222",
            "--dryrun",
            "--json",
        ])
        .expect("`agents deploy` parses")
        .command;

        let Commands::Agents {
            action:
                AgentsCommand::Deploy {
                    host,
                    user,
                    port,
                    dryrun,
                    json,
                    ..
                },
        } = command
        else {
            panic!("expected the agents deploy subcommand, got {command:?}");
        };
        // The user@ prefix is split off at dispatch, not by clap.
        assert_eq!(host, "root@example.com");
        assert_eq!(user, None);
        assert_eq!(port, Some(2222));
        assert!(dryrun);
        assert!(json);
    }

    /// A deployment with no target host is meaningless.
    #[test]
    fn agents_deploy_requires_a_host() {
        assert!(CommandLine::try_parse_from(["sandpolis", "agents", "deploy"]).is_err());
    }
}

#[cfg(all(test, feature = "client"))]
mod test_lsp_args {
    use super::*;

    fn lsp_args(argv: &[&str]) -> Result<crate::lsp::LspArgs, clap::Error> {
        match CommandLine::try_parse_from(argv)?.command {
            Commands::Lsp { args } => Ok(args),
            other => panic!("expected the lsp subcommand, got {other:?}"),
        }
    }

    /// Naming no format leaves the root type undecided, which would serve
    /// completions for whichever one happened to be the default.
    #[test]
    fn a_root_type_is_required() {
        assert!(lsp_args(&["sandpolis", "lsp"]).is_err());
    }

    /// The flag selects the format it names.
    #[test]
    fn the_flag_selects_its_format() -> Result<()> {
        assert_eq!(
            lsp_args(&["sandpolis", "lsp", "--realm"])?.root_type(),
            "crate::config::RealmConfig"
        );
        Ok(())
    }
}

#[cfg(feature = "client")]
mod client {
    use super::*;
    use anyhow::{Context, bail};

    /// A not-yet-implemented client subcommand: opens a placeholder TUI, or
    /// prints an unimplemented JSON result for noninteractive callers.
    pub(super) async fn stub(name: &str, target: TargetArgs) -> Result<ExitCode> {
        if target.json {
            println!(
                "{}",
                serde_json::json!({"status": "unimplemented", "command": name})
            );
            return Ok(ExitCode::FAILURE);
        }
        sandpolis_client::tui::run_tui(sandpolis_client::tui::PlaceholderPanel::new(name)).await?;
        Ok(ExitCode::SUCCESS)
    }

    pub(super) async fn agents(action: AgentsCommand) -> Result<ExitCode> {
        match action {
            AgentsCommand::List { json, .. } => {
                if json {
                    return list_agents_json().await;
                }
                let widget = crate::client::tui::agent_list::AgentListWidget::new()?;
                sandpolis_client::tui::run_tui(widget).await?;
                Ok(ExitCode::SUCCESS)
            }
            AgentsCommand::Restart { instance, .. } => {
                // The agent reboot stream is not yet wired end-to-end; report
                // honestly rather than pretend success.
                println!(
                    "{}",
                    serde_json::json!({
                        "instance": instance.to_string(),
                        "status": "unimplemented",
                        "detail": "agent reboot stream is not yet wired",
                    })
                );
                Ok(ExitCode::FAILURE)
            }
            AgentsCommand::Deploy {
                host,
                user,
                port,
                key,
                fingerprint,
                poll,
                poll_timeout,
                dryrun,
                json,
                ..
            } => {
                deploy(
                    host,
                    user,
                    port,
                    key,
                    fingerprint,
                    poll,
                    poll_timeout,
                    dryrun,
                    json,
                )
                .await
            }
        }
    }

    /// How long a deployment may run end to end (a fresh install moves a
    /// binary).
    const DEPLOY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1800);

    /// Drive a deployment (or a dry run) through the server, printing its
    /// progress as it goes.
    #[allow(clippy::too_many_arguments)]
    async fn deploy(
        host: String,
        user: Option<String>,
        port: Option<u16>,
        key: Option<PathBuf>,
        fingerprint: Option<String>,
        poll: Option<String>,
        poll_timeout: u64,
        dryrun: bool,
        json: bool,
    ) -> Result<ExitCode> {
        use sandpolis_agent::client::deploy_form::{self, DeployDefaults, DeployForm};
        use sandpolis_agent::deploy::client::DeployStreamRequester;
        use sandpolis_agent::deploy::{DeployStreamRequest, DeployStreamResponse, DeployTarget};
        use std::io::IsTerminal;
        use std::time::Duration;

        // A user@ prefix wins over --user, the way OpenSSH treats it.
        let (username, host) = match host.split_once('@') {
            Some((user, host)) => (user.to_string(), host.to_string()),
            None => (user.unwrap_or_default(), host),
        };

        let resolved = deploy_form::resolve_with_ssh_config(
            &DeployForm {
                host,
                username,
                port,
                key_path: key
                    .map(|key| key.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                have_password: false,
                fingerprint,
            },
            &DeployDefaults::detect(),
        )?;

        // The password is the account password when no key resolved, or the
        // key's passphrase when the key at rest says it needs one. Prompted
        // rather than taken as a flag so it stays out of shell history.
        let password = if resolved.key_path.is_none() {
            if !std::io::stdin().is_terminal() {
                bail!(
                    "no private key resolved for {}@{} and a password prompt needs \
                     a terminal; pass --key or configure one in ~/.ssh/config",
                    resolved.username,
                    resolved.host
                );
            }
            rpassword::prompt_password(format!(
                "{}@{}'s password: ",
                resolved.username, resolved.host
            ))?
        } else if let Some(key_path) = resolved
            .key_path
            .as_deref()
            .filter(|path| {
                std::fs::read_to_string(path)
                    .is_ok_and(|pem| deploy_form::key_looks_encrypted(&pem))
            })
        {
            if !std::io::stdin().is_terminal() {
                bail!("the key at {key_path} is encrypted and a passphrase prompt needs a terminal");
            }
            rpassword::prompt_password(format!("Enter passphrase for {key_path}: "))?
        } else {
            String::new()
        };
        let auth = deploy_form::read_auth(resolved.key_path.as_deref(), &password)?;

        let connection = sandpolis_client::sync::wait_for_connection(Duration::from_secs(30))
            .await
            .context("no server connection")?;
        let server = sandpolis_client::sync::primary_server_url()
            .context("the server connection has no URL yet")?;

        let target_host = resolved.host.clone();
        let (requester, mut events) = DeployStreamRequester::channel();
        let request = DeployStreamRequest::Start {
            target: DeployTarget {
                host: resolved.host,
                port: resolved.port,
                username: resolved.username,
                auth,
                fingerprint: resolved.fingerprint,
            },
            server,
            poll: poll.map(|schedule| sandpolis_agent::PollConfig {
                schedule,
                timeout_secs: poll_timeout,
            }),
            dry_run: dryrun,
        };

        // The sender stays alive for the drain: dropping it closes the stream,
        // which is also how a timeout calls the deployment off server-side.
        let (id, _tx) = connection.open_stream(requester, request).await?;
        let deadline = tokio::time::Instant::now() + DEPLOY_TIMEOUT;

        let code = loop {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Some(DeployStreamResponse::Step { step, message })) => {
                    if !json {
                        println!("[{}] {message}", step.label());
                    }
                }
                Ok(Some(DeployStreamResponse::Done { .. })) => {}
                Ok(Some(DeployStreamResponse::Finished { reconfigured })) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({"status": "deployed", "reconfigured": reconfigured})
                        );
                    } else if reconfigured {
                        println!("{target_host} already had an agent; its realm cert was rewritten.");
                    } else {
                        println!("The agent is installed and running on {target_host}.");
                    }
                    break ExitCode::SUCCESS;
                }
                Ok(Some(DeployStreamResponse::Planned {
                    os,
                    arch,
                    installed,
                    actions,
                    blocker,
                })) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "status": "planned",
                                "os": os,
                                "arch": arch,
                                "installed": installed,
                                "actions": actions,
                                "blocker": blocker,
                            })
                        );
                    } else {
                        println!(
                            "Dry run against {target_host} ({os}/{arch}, agent {}):",
                            if installed {
                                "already installed"
                            } else {
                                "not installed"
                            }
                        );
                        for action in actions {
                            println!("  would {action}");
                        }
                        if let Some(blocker) = &blocker {
                            println!("  blocked: {blocker}");
                        }
                    }
                    break if blocker.is_none() {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    };
                }
                Ok(Some(DeployStreamResponse::Failed { step, message })) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "status": "failed",
                                "step": step.label(),
                                "error": message,
                            })
                        );
                    } else {
                        eprintln!("{}: {message}", step.label());
                    }
                    break ExitCode::FAILURE;
                }
                Ok(None) => {
                    eprintln!("The connection to the server was lost.");
                    break ExitCode::FAILURE;
                }
                Err(_) => {
                    eprintln!("The deployment timed out; calling it off.");
                    break ExitCode::FAILURE;
                }
            }
        };

        connection.close_stream(id);
        Ok(code)
    }

    pub(super) async fn servers(
        action: ServersCommand,
        server_manager: &sandpolis_server::ServerManager,
    ) -> Result<ExitCode> {
        match action {
            ServersCommand::List { json, .. } => {
                if json {
                    return list_servers_json(server_manager);
                }
                let widget =
                    crate::client::tui::server_list::ServerListWidget::new(server_manager.clone())?;
                sandpolis_client::tui::run_tui(widget).await?;
                Ok(ExitCode::SUCCESS)
            }
        }
    }

    fn list_servers_json(server_manager: &sandpolis_server::ServerManager) -> Result<ExitCode> {
        let servers: Vec<_> = server_manager
            .servers
            .iter()
            .map(|resident| {
                let data = resident.read();
                serde_json::json!({
                    "address": data.address.to_string(),
                    "user": data.user.to_string(),
                })
            })
            .collect();

        println!("{}", serde_json::to_string_pretty(&servers)?);
        Ok(ExitCode::SUCCESS)
    }

    /// Print every known agent instance as JSON. Waits for the sync session
    /// to be established (the connection to the server comes up
    /// asynchronously), then subscribes to the instance model and reads from
    /// the client database.
    async fn list_agents_json() -> Result<ExitCode> {
        use sandpolis_instance::InstanceManagerData;
        use sandpolis_instance::realm::RealmName;
        use std::time::Duration;

        info!("list_agents_json: waiting for server connection (up to 30s)");
        if sandpolis_client::sync::wait_for_connection(Duration::from_secs(30))
            .await
            .is_none()
        {
            bail!("no server connection");
        }
        info!("list_agents_json: connection established, subscribing");

        // Subscribe only after the connection is up so the call isn't a no-op.
        sandpolis_client::sync::subscribe(sandpolis_instance::instance_manager_model_id(), None);
        sandpolis_client::sync::subscribe(
            sandpolis_instance::network::liveness::liveness_model_id(),
            None,
        );

        // Give the subscription a moment to deliver records.
        info!("list_agents_json: waiting 500ms for sync delivery");
        tokio::time::sleep(Duration::from_millis(500)).await;
        info!("list_agents_json: reading client database");

        let db =
            sandpolis_client::sync::client_database().context("client database unavailable")?;
        let realm = db.realm(RealmName::default())?;
        let r = realm.r_transaction()?;
        let all: Vec<InstanceManagerData> =
            r.scan().primary()?.all()?.collect::<Result<Vec<_>, _>>()?;

        // Which of them are up is a separate, server-written model, resolved the
        // same way the GUI resolves it: an observer's word counts only while the
        // observer itself is reachable.
        let rows: Vec<sandpolis_instance::network::liveness::LivenessData> =
            r.scan().primary()?.all()?.collect::<Result<Vec<_>, _>>()?;
        let online = sandpolis_instance::network::liveness::reachable(
            rows,
            sandpolis_client::sync::connected_instances(),
        );

        let agents: Vec<_> = all
            .into_iter()
            .filter(|i| i._instance_id.is_agent())
            .map(|i| {
                serde_json::json!({
                    "instance_id": i._instance_id.to_string(),
                    "cluster_id": i.cluster_id.to_string(),
                    "os": i.os_info.to_string(),
                    "online": online.contains(&i._instance_id),
                })
            })
            .collect();

        println!("{}", serde_json::to_string_pretty(&agents)?);
        Ok(ExitCode::SUCCESS)
    }
}
