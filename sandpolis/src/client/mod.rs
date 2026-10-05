use crate::InstanceState;

#[cfg(feature = "client")]
pub mod gui;

#[cfg(all(feature = "client", not(target_os = "android")))]
pub mod tui;

/// Bring up everything a client needs and run `command`: the GUI in the
/// foreground, or a subcommand's focused TUI.
#[cfg(not(target_os = "android"))]
pub async fn start(command: crate::cli::Commands) -> anyhow::Result<std::process::ExitCode> {
    let args = command.client_args().cloned().unwrap_or_default();
    let options = args.options();

    // Nothing below can succeed without a window to draw in, so find that out
    // before loading certs or dialing a server — the GUI is the one command
    // whose whole purpose is the window.
    if let crate::cli::Commands::Client { .. } = command {
        sandpolis_client::gui::require_display()?;
    }

    // Clients read no config file, so a realm cert is the only way to point one
    // at a server without going through the GUI login dialog. Naming one
    // outright wins; otherwise every cert in the data directory attaches, which
    // is how a client installed by a package manager is configured. An
    // ephemeral client falls back to the default cert dir, which is where the
    // GUI realm-selection dialog saves an imported cert.
    let endpoint_certs = match args.realm.as_deref() {
        Some(path) => crate::load_realm_cert(Some(path))?.into_iter().collect(),
        None => match args.data.as_deref() {
            Some(dir) => crate::load_realm_certs_dir(dir)?,
            None => match default_realm_cert_dir() {
                Some(dir) => crate::load_realm_certs_dir(&dir)?,
                None => Vec::new(),
            },
        },
    };

    let state = crate::endpoint_state(&options, endpoint_certs.clone()).await?;

    for cert in &endpoint_certs {
        spawn_server_connection(state.clone(), cert.url()?);
    }

    // The GUI establishes the sync websocket itself; a subcommand needs it
    // before its view can show anything.
    if let crate::cli::Commands::Client { .. } = command {
        tracing::info!("Starting Sandpolis client");
        gui::main(options, state).await?;
        return Ok(std::process::ExitCode::SUCCESS);
    }

    // A subcommand against a realm with user accounts needs a login before it
    // can do anything, so the prompt comes up front rather than mid-command.
    ensure_authenticated(&state).await?;

    spawn_client_sync(state.clone());
    command.dispatch_client(&state).await
}

/// Make sure every connected server that requires a login holds a usable auth
/// token, opening an interactive login prompt when one is missing. Realms
/// without users and servers with a cached token pass straight through;
/// non-interactive runs fail with instructions instead of hanging on a prompt.
#[cfg(not(target_os = "android"))]
pub async fn ensure_authenticated(state: &InstanceState) -> anyhow::Result<()> {
    use sandpolis_client::tui::login::{LoginOutcome, LoginPromptWidget};
    use std::io::IsTerminal;

    // Connections come up in the background; give them a moment to appear so a
    // subcommand run right after startup still gets its login prompt.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while state.server.server_connections().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    for connection in state.server.server_connections() {
        if !connection.banner.users_configured {
            continue;
        }

        if let Some(token) = state.server.saved_token(&connection.url) {
            *connection.token.write().unwrap() = Some(token);
        }
        if connection
            .token
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|token| token.is_usable())
        {
            continue;
        }

        if !std::io::stdout().is_terminal() {
            anyhow::bail!(
                "authentication required for {}: run `sandpolis servers list` to log in",
                connection.url
            );
        }

        let saved_user = state.server.servers.iter().find_map(|server| {
            let server = server.read();
            (server.address == connection.url && !server.user.is_empty())
                .then(|| server.user.clone())
        });

        // The clone shares the connection's token slot, so a successful login
        // lands on the retained connection too.
        let widget = LoginPromptWidget::new((*connection).clone(), saved_user);
        let widget =
            sandpolis_client::tui::run_tui_until(widget, |widget| widget.finished()).await?;

        match widget.outcome() {
            Some(LoginOutcome::Success { username }) => {
                if let Some(token) = connection.token.read().unwrap().clone() {
                    state
                        .server
                        .update_server_token(&connection.url, username, token)?;
                }
            }
            _ => anyhow::bail!("login cancelled"),
        }
    }

    Ok(())
}

/// Establish the websocket to the first available server and install it for DB
/// sync. Runs in the background until a server connection exists (the user logs
/// in), then opens the websocket once and hands it to `sandpolis_client::sync`.
pub fn spawn_client_sync(state: InstanceState) {
    let server = state.server.clone();
    let network = state.network.clone();
    let database = state.network.database.clone();
    let instance = state.instance.clone();

    // Start surfacing notifications before any of them can arrive. This is the
    // one path both the GUI and the subcommand TUIs take, so it covers every
    // way the client runs; without a GUI to toast into, delivery falls through
    // to the operating system.
    if let Err(e) = sandpolis_client::notification::watch(&database) {
        tracing::warn!(error = %e, "Notifications will not be surfaced");
    }

    tokio::spawn(async move {
        loop {
            let conns = server.server_connections();
            tracing::debug!(
                count = conns.len(),
                "spawn_client_sync: checking server connections"
            );
            for connection in conns {
                let has_ws = connection.inner.read().unwrap().is_some();
                tracing::debug!(has_ws, "spawn_client_sync: connection slot");
                if has_ws {
                    continue;
                }
                tracing::info!("spawn_client_sync: opening websocket");
                match connection.open_websocket(&network, &instance).await {
                    Ok(ic) => {
                        tracing::info!("Established sync websocket to server");
                        // Register every server for routing; the first also becomes
                        // the primary connection backing `sync::connection()`.
                        sandpolis_client::sync::register_connection(
                            connection.url.clone(),
                            ic.clone(),
                        );
                        sandpolis_client::sync::init(ic, database.clone());

                        // Domains group nodes in the world view, so they're
                        // needed for as long as the GUI runs. Subscribing is a
                        // no-op before `init`, hence here rather than at
                        // startup.
                        sandpolis_client::sync::subscribe(
                            sandpolis_instance::domain::domain_model_id(),
                            None,
                        );

                        // Notifications are wanted for as long as the client
                        // runs, not only while some view is open, so this
                        // subscription is standing rather than opened by a
                        // panel.
                        sandpolis_client::notification::subscribe();

                        // Which instances exist, and which of them are up. The
                        // client holds no connection to an agent, so both are
                        // things it can only learn by replication — and it needs
                        // them for as long as there's a world view to draw.
                        sandpolis_client::sync::subscribe(
                            sandpolis_instance::instance_manager_model_id(),
                            None,
                        );
                        sandpolis_client::sync::subscribe(
                            sandpolis_instance::network::liveness::liveness_model_id(),
                            None,
                        );
                    }
                    Err(e) if e.is::<sandpolis_server::AuthRequired>() => {
                        // The user hasn't logged in yet; the login dialog is
                        // (or will be) up, and this loop retries after it.
                        tracing::debug!("Sync websocket awaiting login");
                    }
                    Err(e) => {
                        tracing::info!(error = %e, "Failed to open sync websocket");
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    });
}

/// Where a client without `--data` keeps its realm certs, so a cert imported
/// through the GUI survives ephemeral runs.
#[cfg(not(target_os = "android"))]
pub fn default_realm_cert_dir() -> Option<std::path::PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_dir)
        .map(|dir| dir.join("sandpolis"))
}

/// Open (and retain) a connection to `url`, retrying until it succeeds.
///
/// Clients read no config file, so a realm cert is how a standalone client is
/// pointed at a server without going through the GUI login dialog. Either
/// stratum works — the client addresses agents by id and the servers route to
/// them.
pub fn spawn_server_connection(state: InstanceState, url: sandpolis_server::ServerUrl) {
    let server = state.server.clone();

    // Surface the server in the (database-backed) saved server list so it
    // appears in the TUI, deduplicating so it isn't re-added every run. On a
    // read-only replica this list can't be written, which is harmless — the
    // connection itself still comes up.
    let already_saved = server.servers.iter().any(|s| s.read().address == url);
    if !already_saved {
        use sandpolis_instance::database::{DataCreation, DataIdentifier, DataRevision};
        if let Err(e) = server.save_server(sandpolis_server::client::SavedServerData {
            address: url.clone(),
            token: sandpolis_server::user::ClientAuthToken(String::new()),
            user: sandpolis_server::user::UserName::default(),
            _id: DataIdentifier::default(),
            _revision: DataRevision::Latest(0),
            _creation: DataCreation::default(),
        }) {
            tracing::debug!(error = %e, "Failed to save local server entry");
        }
    }

    tokio::spawn(async move {
        loop {
            tracing::debug!(%url, "Attempting server connection");
            match server.connect(url.clone()).await {
                Ok(connection) => {
                    // A token from an earlier login lets this connection skip
                    // the login dialog entirely.
                    if let Some(token) = server.saved_token(&url) {
                        *connection.token.write().unwrap() = Some(token);
                    }

                    server
                        .outbound
                        .write()
                        .unwrap()
                        .push(std::sync::Arc::new(connection));
                    tracing::info!(%url, "Connected to server");
                    return;
                }
                Err(e) => {
                    tracing::debug!(error = %e, %url, "Server not ready yet, retrying in 2s");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
    });
}
