use anyhow::Result;
use sandpolis_instance::LayerVersion;
use sandpolis_instance::database::DatabaseManager;
use sandpolis_instance::realm::RealmManager;
use std::collections::HashMap;

#[cfg(all(feature = "uki", any(feature = "server", feature = "client")))]
compile_error!(
    "the `uki` feature builds a boot agent; it cannot be combined with `server` or `client`"
);

#[cfg(feature = "agent")]
pub mod agent;
#[cfg(not(target_os = "android"))]
pub mod cli;
#[cfg(feature = "client")]
pub mod client;
pub mod config;
#[cfg(feature = "client")]
pub mod lsp;
#[cfg(feature = "server")]
pub mod server;

/// Re-exported so embedders (the mobile app) can name the stratum without
/// depending on `sandpolis-server` directly.
pub use sandpolis_server::ServerStratum;

/// Everything this process was told to do on the command line, plus what the
/// realm cert it was given contributes.
///
/// This is plumbing, not configuration: nothing here is serialized or read from
/// disk. Realm-scoped settings live in realm configs
/// ([`config::RealmConfig`]); what's here describes *this process*.
#[cfg_attr(feature = "client", derive(bevy::prelude::Resource))]
#[derive(Clone)]
pub struct RuntimeOptions {
    pub database: sandpolis_instance::database::config::DatabaseConfig,

    /// What this process was started as, which is what decides the type of its
    /// [`sandpolis_instance::InstanceId`]. One process is exactly one instance.
    pub instance_type: sandpolis_instance::InstanceType,

    /// Where this process's server binds.
    #[cfg(feature = "server")]
    pub listen: std::net::SocketAddr,

    /// Host to name in certificates minted for realms that declare no address
    /// of their own, from `--server-name`. `None` means detect the machine's
    /// hostname at server start.
    #[cfg(feature = "server")]
    pub server_name: Option<String>,

    /// Polling connection mode, from `--poll`. A boot agent has no polling
    /// mode: it stays continuously connected so the server can hold it.
    #[cfg(all(feature = "agent", not(feature = "uki")))]
    pub poll: Option<sandpolis_agent::PollConfig>,

    /// The server this agent attaches to, named by its realm cert.
    #[cfg(feature = "agent")]
    pub server: Option<sandpolis_server::ServerUrl>,

    /// Realms this server was told to serve, one per realm config.
    #[cfg(feature = "server")]
    pub realms: Vec<crate::config::RealmConfig>,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            database: Default::default(),
            instance_type: sandpolis_instance::InstanceType::Client,
            #[cfg(feature = "server")]
            listen: std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                sandpolis_server::ServerUrl::default_port(),
            ),
            #[cfg(feature = "server")]
            server_name: None,
            #[cfg(all(feature = "agent", not(feature = "uki")))]
            poll: None,
            #[cfg(feature = "agent")]
            server: None,
            #[cfg(feature = "server")]
            realms: Vec::new(),
        }
    }
}

impl RuntimeOptions {
    /// Defaults for a client that only connects out — the mobile app, an
    /// example — where no command line was parsed.
    pub fn embedded() -> Self {
        Self::default()
    }

    /// The account sections of every loaded realm, merged.
    ///
    /// Accounts are still seeded into the default realm, so a multi-realm server
    /// pools them for now. Per-realm account layering is a larger change than
    /// this file format.
    // TODO seed each realm's accounts into that realm's database
    #[cfg(all(feature = "server", feature = "account"))]
    pub fn merged_account_config(&self) -> sandpolis_account::config::AccountManagerConfig {
        let mut merged = sandpolis_account::config::AccountManagerConfig::default();
        for (index, realm) in self.realms.iter().enumerate() {
            if index == 0 {
                merged.scrape = realm.account.scrape.clone();
            }
            merged
                .accounts
                .extend(realm.account.accounts.iter().cloned());
        }
        merged
    }

    /// The inventory section of the first loaded realm. See
    /// [`merged_account_config`](Self::merged_account_config) for why config is
    /// pooled across realms for now.
    #[cfg(all(feature = "server", feature = "inventory"))]
    pub fn merged_inventory_config(&self) -> sandpolis_inventory::config::InventoryManagerConfig {
        self.realms
            .first()
            .map(|realm| realm.inventory.clone())
            .unwrap_or_default()
    }

    /// The probe sections of every loaded realm, merged. See
    /// [`merged_account_config`](Self::merged_account_config) for why.
    #[cfg(all(feature = "server", feature = "probe"))]
    pub fn merged_probe_config(&self) -> sandpolis_probe::config::ProbeManagerConfig {
        sandpolis_probe::config::ProbeManagerConfig {
            devices: self
                .realms
                .iter()
                .flat_map(|realm| realm.probe.devices.iter().cloned())
                .collect(),
            // Scanning is a per-server switch, not a merged list; take the first
            // realm's, like other single-value sections.
            scan: self
                .realms
                .first()
                .map(|realm| realm.probe.scan.clone())
                .unwrap_or_default(),
        }
    }

    /// The tunnel sections of every loaded realm, merged. See
    /// [`merged_account_config`](Self::merged_account_config) for why.
    #[cfg(all(feature = "server", feature = "tunnel"))]
    pub fn merged_tunnel_config(&self) -> sandpolis_tunnel::config::TunnelManagerConfig {
        sandpolis_tunnel::config::TunnelManagerConfig {
            tunnels: self
                .realms
                .iter()
                .flat_map(|realm| realm.tunnel.tunnels.iter().cloned())
                .collect(),
        }
    }
}

/// Load the realm cert given on the command line, if any.
///
/// Returns the certificate it holds, which names exactly one server and realm.
#[cfg(not(target_os = "android"))]
pub fn load_realm_cert(
    path: Option<&std::path::Path>,
) -> Result<Option<sandpolis_instance::realm::RealmCert>> {
    let Some(path) = path else {
        return Ok(None);
    };
    Ok(Some(sandpolis_instance::realm::RealmCert::read_pem(path)?))
}

/// Every realm cert in `dir`, which is how an instance is attached without
/// naming a file on the command line.
///
/// A directory that holds none is not an error — a client with nowhere to
/// connect starts at its login dialog.
#[cfg(not(target_os = "android"))]
pub fn load_realm_certs_dir(
    dir: &std::path::Path,
) -> Result<Vec<sandpolis_instance::realm::RealmCert>> {
    sandpolis_instance::realm::load_realm_certs_dir(dir)
}

/// Bring up the state an endpoint instance — an agent or a client — runs on.
///
/// The two differ only in how they came by their certificate. Past that they
/// agree: each owns its database outright (the server it attaches to replicates
/// *from* it), each knows exactly the realms its certificate names, and neither
/// runs a server of its own, so the stratum it reports is inert.
#[cfg(not(target_os = "android"))]
pub async fn endpoint_state(
    options: &RuntimeOptions,
    endpoint_certs: Vec<sandpolis_instance::realm::RealmCert>,
) -> Result<InstanceState> {
    use sandpolis_instance::database::WriteAuthority;

    let database = DatabaseManager::new(options.database.clone(), &MODELS, WriteAuthority::Full)?;
    let realms = RealmManager::for_endpoint(endpoint_certs, database.clone())?;

    InstanceState::new(options, database, realms, ServerStratum::Global).await
}

/// Which stratum a server runs in.
///
/// A realm cert means this server attaches to the one it names, which puts it
/// in the local stratum. Without one, this is the network's single global
/// stratum server.
#[cfg(not(target_os = "android"))]
pub fn stratum(realm_cert: Option<&std::path::Path>) -> Result<ServerStratum> {
    stratum_of(load_realm_cert(realm_cert)?.as_ref())
}

/// [`stratum`], for a caller that already loaded the realm cert. The server
/// itself needs the certificate for more than this, so it reads the file once
/// and asks here rather than going back to disk.
#[cfg(not(target_os = "android"))]
pub fn stratum_of(
    endpoint: Option<&sandpolis_instance::realm::RealmCert>,
) -> Result<ServerStratum> {
    Ok(match endpoint {
        Some(cert) => ServerStratum::Local {
            global: cert.url()?,
        },
        None => ServerStratum::Global,
    })
}

#[cfg_attr(feature = "client", derive(bevy::prelude::Resource))]
#[cfg_attr(feature = "server", derive(axum_macros::FromRef))]
#[derive(Clone)]
pub struct InstanceState {
    #[cfg(feature = "account")]
    pub account: sandpolis_account::AccountManager,
    pub agent: sandpolis_agent::AgentManager,
    #[cfg(feature = "audit")]
    pub audit: sandpolis_audit::AuditManager,
    #[cfg(feature = "desktop")]
    pub desktop: sandpolis_desktop::DesktopManager,
    #[cfg(feature = "filesystem")]
    pub filesystem: sandpolis_filesystem::FilesystemManager,
    #[cfg(feature = "health")]
    pub health: sandpolis_health::HealthManager,
    pub realms: RealmManager,
    pub instance: sandpolis_instance::InstanceManager,
    pub network: sandpolis_instance::network::NetworkManager,
    #[cfg(feature = "inventory")]
    pub inventory: sandpolis_inventory::InventoryManager,
    pub server: sandpolis_server::ServerManager,
    #[cfg(feature = "shell")]
    pub shell: sandpolis_shell::ShellManager,
    #[cfg(feature = "snapshot")]
    pub snapshot: sandpolis_snapshot::SnapshotManager,
    #[cfg(feature = "tunnel")]
    pub tunnel: sandpolis_tunnel::TunnelManager,
    #[cfg(feature = "probe")]
    pub probe: sandpolis_probe::ProbeManager,
    pub user: sandpolis_server::user::UserManager,
}

impl InstanceState {
    /// `stratum` describes the server this process runs (if any). On builds
    /// without the server feature it is inert, since nothing consults it.
    ///
    /// `realms` is built by the caller, which is the only place that knows the
    /// realm configs and realm certs this process was given.
    pub async fn new(
        options: &RuntimeOptions,
        database: DatabaseManager,
        realms: RealmManager,
        stratum: sandpolis_server::ServerStratum,
    ) -> Result<Self> {
        // Create all the configured subsystems, starting with the most foundational

        let instance =
            sandpolis_instance::InstanceManager::new(database.clone(), options.instance_type)
                .await?;

        let network = sandpolis_instance::network::NetworkManager::new(database.clone()).await?;

        let server = sandpolis_server::ServerManager::new(
            database.clone(),
            network.clone(),
            realms.clone(),
            stratum,
            options.instance_type,
        )
        .await?;

        let user = sandpolis_server::user::UserManager::new(
            instance.clone(),
            database.clone(),
            network.clone(),
            #[cfg(feature = "server")]
            realms.iter().map(|realm| realm.name.clone()).collect(),
            #[cfg(feature = "server")]
            options
                .realms
                .iter()
                .map(|realm| (realm.name.clone(), realm.user.clone()))
                .collect(),
            #[cfg(feature = "server")]
            server.stratum.clone(),
            #[cfg(feature = "server")]
            server.ownership.clone(),
        )
        .await?;

        let agent = sandpolis_agent::AgentManager::new(database.clone()).await?;

        // Deployment mints an agent certificate for whichever realm the new
        // agent will connect to. Its responder is built by the stateless
        // `inventory` factory and holds no state of its own, so this is how the
        // realm handles reach it.
        #[cfg(feature = "server")]
        sandpolis_agent::deploy::server::install_realms(realms.clone());

        #[cfg(feature = "inventory")]
        let inventory =
            sandpolis_inventory::InventoryManager::new(database.clone(), instance.clone()).await?;

        #[cfg(feature = "health")]
        let health =
            sandpolis_health::HealthManager::new(database.clone(), instance.clone()).await?;

        #[cfg(feature = "audit")]
        let audit = sandpolis_audit::AuditManager::new(database.clone(), instance.clone()).await?;

        #[cfg(feature = "shell")]
        let shell = sandpolis_shell::ShellManager::new(database.clone()).await?;

        #[cfg(feature = "filesystem")]
        let filesystem = sandpolis_filesystem::FilesystemManager::new().await?;

        #[cfg(feature = "desktop")]
        let desktop =
            sandpolis_desktop::DesktopManager::new(database.clone(), instance.clone()).await?;

        #[cfg(feature = "account")]
        let account = sandpolis_account::AccountManager::new(database.clone()).await?;

        #[cfg(feature = "snapshot")]
        let snapshot =
            sandpolis_snapshot::SnapshotManager::new(database.clone(), instance.clone()).await?;

        #[cfg(feature = "tunnel")]
        let tunnel =
            sandpolis_tunnel::TunnelManager::new(database.clone(), instance.clone()).await?;

        #[cfg(feature = "probe")]
        let probe = sandpolis_probe::ProbeManager::new(
            {
                #[cfg(feature = "server")]
                {
                    options.merged_probe_config()
                }
                #[cfg(not(feature = "server"))]
                {
                    Default::default()
                }
            },
            instance.instance_id,
        );

        Ok(Self {
            #[cfg(feature = "inventory")]
            inventory,
            #[cfg(feature = "health")]
            health,
            #[cfg(feature = "audit")]
            audit,
            #[cfg(feature = "shell")]
            shell,
            #[cfg(feature = "filesystem")]
            filesystem,
            #[cfg(feature = "desktop")]
            desktop,
            #[cfg(feature = "snapshot")]
            snapshot,
            #[cfg(feature = "tunnel")]
            tunnel,
            #[cfg(feature = "probe")]
            probe,
            #[cfg(feature = "account")]
            account,
            user,
            agent,
            server,
            network,
            realms,
            instance,
        })
    }
}

// TODO inventory crate
pub fn layers() -> HashMap<sandpolis_instance::LayerName, LayerVersion> {
    HashMap::from([])
}

/// Every model linked into this build, collected from the `#[data]` macro's
/// registrations. Re-exported here because this is where callers expect it.
pub use sandpolis_instance::database::MODELS;

#[cfg(all(test, feature = "server", not(target_os = "android")))]
mod test_realm_certs_dir {
    use anyhow::Result;
    use sandpolis_instance::realm::RealmCert;

    /// Mint a realm cert for `realm` into `dir`, the way a server writes one out
    /// at startup.
    fn write_cert(dir: &std::path::Path, realm: &str) -> Result<()> {
        let url: sandpolis_instance::realm::url::ServerUrl =
            format!("gs.example.com:8768/{realm}").parse()?;
        let ca = RealmCert::new_cluster(Default::default(), url.realm.clone())?;
        ca.endpoint_cert(&url)?
            .write_pem(dir.join(format!("{realm}.realm.pem")))
    }

    /// Every realm cert in the directory attaches the client, in a stable order,
    /// and nothing else there is mistaken for one — the database and the realm
    /// configs live alongside them.
    #[test]
    fn every_realm_cert_is_loaded_in_order() -> Result<()> {
        let dir = tempfile::tempdir()?;
        write_cert(dir.path(), "prod")?;
        write_cert(dir.path(), "beta")?;
        std::fs::write(dir.path().join("prod.realm.ron"), "")?;
        std::fs::write(dir.path().join("default.db"), "")?;

        let certs = super::load_realm_certs_dir(dir.path())?;
        let names: Vec<_> = certs.iter().map(|cert| cert.name.to_string()).collect();
        assert_eq!(names, vec!["beta", "prod"]);
        Ok(())
    }

    /// A client with nowhere to connect starts at its login dialog, so neither
    /// an empty directory nor a missing one is an error.
    #[test]
    fn no_certs_is_not_an_error() -> Result<()> {
        let dir = tempfile::tempdir()?;
        assert!(super::load_realm_certs_dir(dir.path())?.is_empty());
        assert!(super::load_realm_certs_dir(&dir.path().join("absent"))?.is_empty());
        Ok(())
    }

    /// A file named like a realm cert but holding something else is a
    /// misconfiguration, not something to skip past — the client would
    /// otherwise start with no connection and no explanation.
    #[test]
    fn a_malformed_cert_is_an_error() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("prod.realm.pem"), "not pem at all")?;

        assert!(super::load_realm_certs_dir(dir.path()).is_err());
        Ok(())
    }
}

#[cfg(test)]
mod test_models {
    /// Building the registry is what proves the estate's models agree: every
    /// `#[data]` type registers itself, and `define` rejects a duplicate
    /// native_model id, so a collision between two subsystems fails here rather
    /// than when an instance starts.
    ///
    /// This binary links every subsystem, which is what makes the check meaningful.
    #[test]
    fn models_have_no_id_collisions() {
        let _ = &*super::MODELS;
    }
}
