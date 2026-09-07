use crate::{InstanceState, RuntimeOptions};
use anyhow::Result;
use axum::{
    Router,
    routing::{get, post},
};
use rand::RngExt;
use sandpolis_instance::ClusterId;
use sandpolis_instance::realm::RealmCert;
use std::path::PathBuf;
use tempfile::TempDir;
use tempfile::tempdir;
use tower_http::trace::TraceLayer;
use tracing::info;

pub mod config_watch;

/// Bring up everything a server needs and run it: the realms it serves, the
/// database that holds them, and the subsystems over both.
pub async fn start(args: crate::cli::ServerArgs) -> Result<std::process::ExitCode> {
    use sandpolis_instance::database::{DatabaseManager, ScopeTable, WriteAuthority};
    use sandpolis_instance::realm::RealmManager;
    use sandpolis_instance::realm::config::RealmBootstrap;

    let mut options = args.options();

    // The realm cert is the whole trust bootstrap for a server that attaches to
    // another one: it names the upstream, carries the realm CA, and holds this
    // server's own certificate.
    let endpoint = crate::load_realm_cert(args.realm.as_deref())?;
    let stratum = crate::stratum_of(endpoint.as_ref())?;

    // Every realm this server serves comes from a realm config in the data
    // directory. An ephemeral server has no such directory, so it gets an
    // implicit default realm whose CA lives only in the in-memory database —
    // which is what makes a development server work with no setup at all.
    if stratum.is_global()
        && let Some(dir) = options.database.storage.clone()
    {
        options.realms = crate::config::RealmConfig::load_dir(dir)?;
    }

    let implicit_default_realm = stratum.is_global() && options.realms.is_empty();
    if implicit_default_realm {
        info!(
            "Serving an implicit \"default\" realm. Pass `--data <dir>` to keep \
             its CA in a file that survives the database."
        );
    }

    // A local stratum server holds per-instance write authority: it owns the
    // data of the instances directly connected to it (as granted by the global
    // stratum server) and replicates everything else. The global stratum server
    // owns its database outright.
    let authority = if stratum.is_local() {
        WriteAuthority::Scoped(std::sync::Arc::new(ScopeTable::default()))
    } else {
        WriteAuthority::Full
    };

    let database = DatabaseManager::new(options.database.clone(), &crate::MODELS, authority)?;

    // Realms are only ever created from files, so this is where the set is
    // fixed for the life of the process.
    let mut bootstraps = Vec::new();
    if stratum.is_global() {
        if implicit_default_realm {
            bootstraps.push(RealmBootstrap::default());
        } else {
            for realm in &options.realms {
                bootstraps.push(realm.bootstrap()?);
            }
        }
    }

    let instance = sandpolis_instance::InstanceManager::new(
        database.clone(),
        sandpolis_instance::InstanceType::Server,
    )
    .await?;

    let mut endpoint_certs = Vec::new();
    if let Some(cert) = endpoint {
        endpoint_certs.push(cert);
    }

    let (realms, startup) = RealmManager::new(
        bootstraps,
        endpoint_certs,
        database.clone(),
        instance,
        stratum.is_global(),
        default_realm_url(&options)?,
    )
    .await?;

    // A realm CA the server had to generate goes back into the file it came
    // from, which is the durable copy. The implicit default realm has no file,
    // so its CA stays in the database as before.
    for ca in &startup.minted_cas {
        if let Some(realm) = options.realms.iter_mut().find(|r| r.name == ca.name) {
            realm.store_ca(ca.cert_pem.clone(), ca.key_pem.clone())?;
            info!(path = ?realm.path(), "Wrote the realm CA back to its file");
        }
    }

    write_realm_certs(&startup.endpoint_certs, options.database.storage.as_deref())?;

    install_persist_callbacks(&options, &stratum);

    let state = InstanceState::new(&options, database, realms, stratum.clone()).await?;

    info!(%stratum, "Starting Sandpolis server");
    main(options, state).await?;
    Ok(std::process::ExitCode::SUCCESS)
}

/// The address certificates name when a realm's config declares none:
/// `--server-name` if given, else the machine's hostname, else loopback.
fn default_realm_url(options: &RuntimeOptions) -> anyhow::Result<sandpolis_server::ServerUrl> {
    let host = match options.server_name.clone() {
        Some(host) => host,
        None => match gethostname::gethostname().into_string() {
            Ok(hostname) if !hostname.is_empty() => hostname,
            _ => {
                tracing::warn!(
                    "The machine has no usable hostname; certificates will name loopback \
                     and only same-host instances will be able to connect"
                );
                "127.0.0.1".to_string()
            }
        },
    };

    let url: sandpolis_server::ServerUrl =
        format!("{host}:{}", options.listen.port()).parse()?;

    // A host that doesn't resolve still makes a valid certificate, but the
    // instances loading it won't be able to dial the address it names.
    if url.resolve().is_err() {
        tracing::warn!(
            %host,
            "The server's hostname doesn't resolve; instances on other hosts may not \
             be able to connect. Pass --server-name or set the realm's address"
        );
    }

    Ok(url)
}

/// Write out the realm cert for each realm this server brought up.
///
/// This is how clients and agents are given a certificate: the server mints one
/// per realm on every start and drops it next to the realm configs, so attaching
/// an instance is a matter of copying a file rather than running a command.
/// A server with no data directory keeps its realms in memory, so its files go
/// to `/tmp` — enough to point instances started on the same host at it.
fn write_realm_certs(
    endpoint_certs: &[sandpolis_instance::realm::RealmCert],
    data_dir: Option<&std::path::Path>,
) -> Result<()> {
    use sandpolis_instance::realm::config::REALM_CERT_SUFFIX;

    for cert in endpoint_certs {
        let path = data_dir
            .unwrap_or_else(|| std::path::Path::new("/tmp"))
            .join(format!("{}{REALM_CERT_SUFFIX}", cert.name));

        cert.write_pem(&path)?;
        info!(realm = %cert.name, path = %path.display(), "Wrote the realm's endpoint certificate");
    }
    Ok(())
}

/// Realm configs live on the global stratum server only, so it is also the only
/// instance that writes changes back to them.
// TODO do this somewhere else
#[allow(unused_variables)]
fn install_persist_callbacks(options: &RuntimeOptions, stratum: &crate::ServerStratum) {
    if !stratum.is_global() {
        return;
    }

    #[cfg(feature = "account")]
    {
        let realms_config = options.realms.clone();
        sandpolis_account::set_account_persist(move |accounts| {
            // Accounts aren't per-realm yet, so they go back to the single
            // loaded realm. With several realms there's no way to tell which one
            // owns an account, so leave every file alone rather than guess.
            let mut realms_config = realms_config.clone();
            let accounts = accounts.to_vec();
            match realms_config.len() {
                1 => realms_config[0].modify(|c| {
                    // Only the account list is replaced; `account.scrape` and
                    // every other section keep whatever is on disk.
                    c.account.accounts = accounts.clone();
                    Ok(())
                }),
                0 => Ok(()),
                _ => {
                    tracing::warn!(
                        "Not persisting accounts: several realm configs are loaded and \
                         accounts are not yet realm-scoped"
                    );
                    Ok(())
                }
            }
        });
    }

    // Only the global stratum server keeps the authoritative probe config; local
    // stratum servers don't persist a probe list of their own.
    #[cfg(feature = "probe")]
    {
        let realms_config = options.realms.clone();
        sandpolis_probe::set_device_persist(move |devices| {
            let mut realms_config = realms_config.clone();
            let probe = sandpolis_probe::devices_to_config(devices);

            // A device records the server that reaches it, so route each one to
            // that server's realm. A device with no server can only be placed
            // when there's exactly one realm to place it in.
            let only_realm = realms_config.len() == 1;
            for realm in realms_config.iter_mut() {
                let name = realm.name.clone();
                let devices: Vec<_> = probe
                    .devices
                    .iter()
                    .filter(|device| match device.server.as_ref() {
                        Some(server) => server.realm == name,
                        None => only_realm,
                    })
                    .cloned()
                    .collect();
                if let Err(e) = realm.modify(|c| {
                    c.probe.devices = devices.clone();
                    Ok(())
                }) {
                    tracing::warn!(
                        realm = %name,
                        error = %e,
                        "Not persisting probe devices to this realm"
                    );
                }
            }

            if !only_realm && probe.devices.iter().any(|device| device.server.is_none()) {
                tracing::warn!(
                    "Some probe devices name no server and several realm configs are \
                     loaded, so they were not persisted to any of them"
                );
            }
            Ok(())
        });
    }
}

pub async fn main(options: RuntimeOptions, state: InstanceState) -> Result<()> {
    #[cfg(feature = "account")]
    let account_config = options.merged_account_config();

    #[cfg(feature = "account")]
    // Before the services start, so the first favicon sweep sees every
    // configured domain.
    state.account.seed_accounts(&account_config)?;

    // Every subsystem's server-side background work goes on one runner, which owns
    // the schedules and lets a client enable or disable individual services.
    let mut services = sandpolis_instance::service::ServiceRunner::new(
        state.instance.realm().clone(),
        state.instance.instance_id,
    );

    // Scraping third-party data (favicons, etc) is server-only: agents have no
    // reason to reach out on the estate's behalf, and every client doing it
    // independently would just multiply the traffic.
    #[cfg(feature = "account")]
    state
        .account
        .register_services(&account_config, &mut services)?;

    // CVE matching runs where the agents' package data lives. Only the owner of
    // an instance's data writes findings for it, so a local stratum server
    // covers exactly its own agents and the global stratum server the rest.
    #[cfg(feature = "inventory")]
    {
        let cve_dir = options
            .database
            .get_storage_dir()?
            .unwrap_or_else(std::env::temp_dir)
            .join("cve");
        let ownership = state.server.ownership.clone();
        let self_id = state.instance.instance_id;
        state.inventory.register_server_services(
            &options.merged_inventory_config(),
            cve_dir,
            std::sync::Arc::new(move |id| {
                ownership
                    .owned_by(self_id)
                    .iter()
                    .any(|scope| scope.instance == id)
            }),
            &mut services,
        )?;
    }

    // Probe "scanning mode": sweep the local network for devices and register
    // them credential-less. Off unless the realm config enables it.
    #[cfg(feature = "probe")]
    {
        let scan = options.merged_probe_config().scan;
        // Stamp discovered devices with this server's URL so the persist hook
        // routes them to the primary realm.
        let server_url = default_realm_url(&options).ok().map(|mut url| {
            if let Some(realm) = options.realms.first() {
                url.realm = realm.name.clone();
            }
            url
        });
        sandpolis_probe::register_server_services(&scan, server_url, &mut services);
    }

    // Snapshot storage lives on the server's filesystem, next to the realm
    // database but never inside it. The ownership gate mirrors CVE matching:
    // only the owner of an agent's data records snapshots for it.
    #[cfg(feature = "snapshot")]
    {
        let snapshot_dir = options
            .database
            .get_storage_dir()?
            .unwrap_or_else(std::env::temp_dir)
            .join("snapshots");
        let ownership = state.server.ownership.clone();
        let self_id = state.instance.instance_id;
        state
            .snapshot
            .install_server(sandpolis_snapshot::server::SnapshotServerContext::new(
                state.instance.realm().clone(),
                sandpolis_snapshot::server::qemu::SnapshotStore::new(snapshot_dir),
                state.network.clone(),
                std::sync::Arc::new(move |id| {
                    ownership
                        .owned_by(self_id)
                        .iter()
                        .any(|scope| scope.instance == id)
                }),
            )?);
    }

    // Boot agents announce themselves on connect; the responder answers from
    // the realm's per-agent boot hold rows.
    state
        .agent
        .install_server(sandpolis_agent::bootagent::server::BootServerContext::new(
            state.instance.realm().clone(),
        )?);

    // Tunnels are declared in realm config (only the global stratum server reads
    // it) and bridged by this server; an empty config simply idles.
    #[cfg(feature = "tunnel")]
    {
        state
            .tunnel
            .install_server(sandpolis_tunnel::server::TunnelServerContext::new(
                state.instance.realm().clone(),
                state.network.clone(),
                state.instance.instance_id,
                options.merged_tunnel_config(),
            )?);
    }

    services.start()?;

    // A local stratum server can't serve TLS until the global stratum server has
    // issued its certificate, so this blocks (with retries) before binding.
    sandpolis_server::stratum::enroll(&state.server, &state.instance).await?;

    // Whether the instances attached here are up is this server's to record and
    // replicate, on either stratum: a client has no connection to an agent, so
    // this is the only way it can know. Runs regardless of ownership, since an
    // edge server watching the agent in front of it shouldn't have to wait on a
    // grant to say what it plainly sees.
    {
        let liveness = std::sync::Arc::new(sandpolis_server::liveness::Liveness::new(
            &state.network,
            state.instance.instance_id,
        ));
        tokio::spawn(sandpolis_server::liveness::maintain_liveness(
            state.network.clone(),
            liveness,
        ));
    }

    // The global stratum server claims its own attached instances against the
    // grant table; this is what revokes a local stratum server when an agent
    // moves here.
    if state.server.stratum.is_global() {
        tokio::spawn(sandpolis_server::ownership::maintain_local_claims(
            state.server.ownership.clone(),
            state.network.clone(),
            state.instance.instance_id,
        ));
    }

    // Realm configs are hand-editable while the server runs, but the realms
    // themselves are frozen at startup: watch the files so a manual edit warns
    // that a restart is needed and stops the server from overwriting it.
    if state.server.stratum.is_global()
        && let Some(dir) = options.database.storage.clone()
    {
        tokio::spawn(async move {
            if let Err(e) = config_watch::watch_realm_configs(dir).await {
                tracing::error!(error = %e, "Realm config watcher stopped");
            }
        });
    }

    // A local stratum server holds a link to its global stratum server for as
    // long as it runs: estate data replicates down it, ownership is claimed up
    // it, and it is the default route for anything not attached here.
    if state.server.stratum.is_local() {
        let server = state.server.clone();
        let network = state.network.clone();
        let instance = state.instance.clone();
        tokio::spawn(async move {
            if let Err(e) =
                sandpolis_server::stratum::maintain_upstream(server, network, instance).await
            {
                tracing::error!(error = %e, "Upstream stratum link stopped");
            }
        });

        // Pull each owned, attached agent's records into the local database.
        // Independent of the upstream link, so agents keep syncing while the
        // global stratum server is unreachable.
        if let Some(table) = state.server.database.authority().scope_table() {
            tokio::spawn(sandpolis_server::ownership::maintain_agent_sync(
                state.network.clone(),
                state.instance.realm().clone(),
                table.clone(),
                state.server.ownership.clone(),
            ));
        }
    }

    let app: Router<InstanceState> = Router::new();

    // Server subsystem
    let app: Router<InstanceState> =
        app.route("/server/banner", get(sandpolis_server::banner::get_banner));

    // User subsystem
    let app: Router<InstanceState> = app.route(
        "/user/login",
        post(sandpolis_server::login::server::post_login),
    );

    // Websocket connection endpoint (clients + agents) for streams / sync
    let app: Router<InstanceState> =
        app.route("/connect", get(sandpolis_server::user::server::connect));

    // Realm manager: the global stratum server issues server certificates to
    // local stratum servers so the whole network shares one trust root.
    let app: Router<InstanceState> = app.route(
        "/realm/server-cert",
        post(sandpolis_server::stratum::issue_server_cert),
    );

    // Certificate authentication runs before the realm is known, so its
    // failure notification stays on unless every served realm disables it. A
    // server with no realm configs (a local stratum or ephemeral server)
    // notifies by default.
    let notify_cert_failures = options
        .realms
        .iter()
        .all(|realm| realm.server.notify_cert_failures);

    let app = app.route_layer(axum::middleware::from_fn_with_state(
        notify_cert_failures,
        sandpolis_instance::realm::server::auth_middleware,
    ));

    // Tracing support for Axum
    let app = app.layer(TraceLayer::new_for_http());

    info!(listener = ?options.listen, "Starting server listener");
    axum_server::bind(options.listen)
        .acceptor(
            sandpolis_instance::realm::server::RealmAcceptor::new(
                state.instance.clone(),
                state.realms.clone(),
                notify_cert_failures,
            )
            .await?,
        )
        .serve(
            app.clone()
                .with_state(state.clone())
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod test_default_realm_url {
    use super::*;

    /// `--server-name` beats hostname detection, and the port always comes
    /// from `--listen`.
    #[test]
    fn server_name_wins() -> anyhow::Result<()> {
        let mut options = RuntimeOptions::embedded();
        options.listen = "0.0.0.0:9999".parse()?;
        options.server_name = Some("gs.example.com".to_string());

        let url = default_realm_url(&options)?;
        assert_eq!(url.host, "gs.example.com");
        assert_eq!(url.port, 9999);
        Ok(())
    }

    /// Without `--server-name`, certificates name the machine's hostname.
    #[test]
    fn falls_back_to_the_machine_hostname() -> anyhow::Result<()> {
        let options = RuntimeOptions::embedded();

        let url = default_realm_url(&options)?;
        match gethostname::gethostname().into_string() {
            Ok(hostname) if !hostname.is_empty() => assert_eq!(url.host, hostname),
            _ => assert_eq!(url.host, "127.0.0.1"),
        }
        assert_eq!(url.port, options.listen.port());
        Ok(())
    }
}

/// Holds randomized parameters for a test server.
pub struct TestServer {
    pub port: u16,
    /// A realm cert a client can be pointed at to reach this server.
    pub endpoint_cert: PathBuf,
    certs: TempDir,
}

/// Run a standalone server instance for testing.
pub async fn test_server() -> Result<TestServer> {
    // Get ready to do some cryptography
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("crypto provider is available");

    // Temporary listening port
    let port: u16 = rand::rng().random_range(9000..9999);
    let url: sandpolis_server::ServerUrl = format!("127.0.0.1:{port}/test").parse()?;

    let mut options = RuntimeOptions::embedded();
    options.instance_type = sandpolis_instance::InstanceType::Server;
    options.database.storage = None;
    options.listen = format!("127.0.0.1:{port}").parse()?;

    // Create temporary database
    let database = sandpolis_instance::database::DatabaseManager::new(
        options.database.clone(),
        &crate::MODELS,
        sandpolis_instance::database::WriteAuthority::Full,
    )?;

    // The realm's CA is generated here and handed to the server as a bootstrap,
    // exactly as a realm config would.
    let ca_cert = RealmCert::new_cluster(ClusterId::default(), url.realm.clone())?;

    // The client's half goes into a realm cert for the caller to use.
    let certs = tempdir()?;
    let endpoint_cert = certs.path().join("test.realm.pem");
    ca_cert.endpoint_cert(&url)?.write_pem(&endpoint_cert)?;

    let instance = sandpolis_instance::InstanceManager::new(
        database.clone(),
        sandpolis_instance::InstanceType::Server,
    )
    .await?;

    let (realms, _) = sandpolis_instance::realm::RealmManager::new(
        vec![sandpolis_instance::realm::config::RealmBootstrap {
            name: url.realm.clone(),
            ca: Some((
                ca_cert.cert.clone(),
                ca_cert.key.clone().expect("a fresh CA carries its key"),
            )),
            address: Some(url.clone()),
        }],
        Vec::new(),
        database.clone(),
        instance,
        true,
        url,
    )
    .await?;

    let state = InstanceState::new(
        &options,
        database,
        realms,
        sandpolis_server::ServerStratum::Global,
    )
    .await?;

    // Spawn the server
    tokio::spawn(async move { main(options, state).await });

    Ok(TestServer {
        port,
        endpoint_cert,
        certs,
    })
}
