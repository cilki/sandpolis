use crate::{InstanceState, RuntimeOptions};
use anyhow::Result;
use chrono::Utc;
#[cfg(feature = "uki")]
use sandpolis_agent::bootagent::BootAgentState;
#[cfg(feature = "uki")]
use sandpolis_agent::bootagent::streams::{BootStreamRequest, BootStreamRequester};
use sandpolis_instance::network::RetryWait;
use sandpolis_server::ServerConnectStrategy;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info, warn};

#[cfg(feature = "uki")]
use tracing::error;

#[cfg(feature = "uki")]
pub mod boot;

/// How long a server connection must stay up before it counts as healthy and
/// the backoff resets. Without it, a server that accepts the websocket and then
/// immediately drops it would reset the backoff on every attempt, and the agent
/// would reconnect in a tight loop.
const STABLE_AFTER: Duration = Duration::from_secs(30);

/// Backoff for reaching the server. Starts fast, because a restarting server is
/// usually back within seconds, and caps out so a long outage doesn't turn into
/// a busy loop against a server that is likely already struggling.
fn server_retry() -> RetryWait {
    RetryWait::Exponential {
        initial: Duration::from_secs(1),
        constant: 20.0,
        limit: Some(Duration::from_secs(60)),
        iteration: 0,
    }
}

/// Bring up everything an agent needs and run it: the realm named by its realm
/// cert, the database its collectors write into, and the subsystems over both.
#[cfg(not(target_os = "android"))]
pub async fn start(args: crate::cli::AgentArgs) -> Result<std::process::ExitCode> {
    let mut options = args.options();

    // The realm cert names the server, carries the realm CA, and holds this
    // agent's own certificate. How the agent connects to that server — straight
    // through or on a schedule — comes from the command line.
    let mut endpoint_certs = Vec::new();
    if let Some(cert) = crate::load_realm_cert(args.realm.as_deref())? {
        options.server = Some(cert.url()?);
        endpoint_certs.push(cert);
    }

    let state = crate::endpoint_state(&options, endpoint_certs).await?;

    // A UKI build is always a boot agent: the fullscreen chainloader UI takes
    // over the main thread (`#[tokio::main]`'s `block_on` runs `start` there),
    // while the regular agent continues on the runtime's workers.
    #[cfg(feature = "uki")]
    {
        let boot_state = Arc::new(BootAgentState::default());
        {
            let boot_state = boot_state.clone();
            tokio::spawn(async move {
                if let Err(e) = main(options, state, boot_state).await {
                    error!(error = %e, "Agent failed");
                }
            });
        }

        let handle = tokio::runtime::Handle::current();
        boot::run_boot_ui(boot_state, boot::chainload_entries(), move |entry| {
            boot::chainload(&handle, entry)
        })?;
        Ok(std::process::ExitCode::SUCCESS)
    }

    #[cfg(not(feature = "uki"))]
    {
        main(options, state).await?;
        Ok(std::process::ExitCode::SUCCESS)
    }
}

pub async fn main(
    options: RuntimeOptions,
    state: InstanceState,
    #[cfg(feature = "uki")] boot: Arc<BootAgentState>,
) -> Result<()> {
    let Some(url) = options.server.clone() else {
        warn!("Waiting for server configuration");
        std::future::pending::<()>().await;
        return Ok(());
    };

    info!("Starting Sandpolis agent");

    // Every subsystem's collectors go on one runner, which owns their schedules and
    // lets a client enable or disable them individually. Their updates land in
    // the local database, which the SyncResponder streams to the server on
    // demand.
    let mut services = sandpolis_instance::service::ServiceRunner::new(
        state.instance.realm().clone(),
        state.instance.instance_id,
    );

    #[cfg(feature = "health")]
    state.health.register_services(&mut services);

    #[cfg(feature = "audit")]
    state.audit.register_services(&mut services);

    #[cfg(feature = "inventory")]
    state.inventory.register_services(&mut services);

    #[cfg(feature = "desktop")]
    state.desktop.register_services(&mut services);

    services.start()?;

    // A boot agent stays continuously connected so the server can hold it;
    // `--poll` doesn't exist in UKI builds.
    #[cfg(feature = "uki")]
    let strategy = ServerConnectStrategy::Continuous;

    // Pick the connection strategy from `--poll`: a schedule selects polling
    // mode (periodic check-ins), otherwise the agent stays continuously
    // connected.
    #[cfg(not(feature = "uki"))]
    let strategy = match &options.poll {
        Some(poll) => {
            match ServerConnectStrategy::polling(
                &poll.schedule,
                Duration::from_secs(poll.timeout_secs),
            ) {
                Ok(strategy) => strategy,
                Err(e) => {
                    warn!(error = %e, "Invalid poll schedule; using continuous mode");
                    ServerConnectStrategy::Continuous
                }
            }
        }
        None => ServerConnectStrategy::Continuous,
    };

    let server = state.server.clone();
    let network = state.network.clone();
    let instance = state.instance.clone();

    match &strategy {
        // Hold a single connection open for the agent's lifetime,
        // reconnecting whenever it drops.
        ServerConnectStrategy::Continuous => {
            let mut retry = server_retry();
            loop {
                // Every path through this loop comes back round, because an
                // agent that stops trying is indistinguishable from one that
                // died — and nobody is watching its logs to find out which.
                let wait = match attempt_connection(
                    &server,
                    &network,
                    &instance,
                    &url,
                    #[cfg(feature = "uki")]
                    &boot,
                )
                .await
                {
                    // The connection was up long enough to count as healthy, so
                    // treat this outage as a fresh one and reconnect promptly.
                    Ok(uptime) if uptime >= STABLE_AFTER => {
                        warn!(url = %url, uptime = ?uptime, "Server connection cancelled, reconnecting");
                        retry = server_retry();
                        retry.next().unwrap()
                    }
                    // Established but immediately dropped: keep backing off,
                    // otherwise a server that accepts and then drops us
                    // produces a hot loop.
                    Ok(uptime) => {
                        let wait = retry.next().unwrap();
                        warn!(url = %url, uptime = ?uptime, waiting = ?wait, "Server connection dropped quickly; backing off");
                        wait
                    }
                    Err(e) => {
                        let wait = retry.next().unwrap();
                        debug!(error = %e, url = %url, waiting = ?wait, "Connection attempt failed");
                        wait
                    }
                };

                sleep(wait).await;
            }
        }

        // Stay disconnected between check-ins. On each scheduled tick,
        // open the websocket so the server pulls our accumulated data
        // and delivers any pending work, hold it briefly, then close.
        ServerConnectStrategy::Polling { schedule, timeout } => {
            // Build the connection (http client + banner) once and reuse
            // it; only the websocket opens and closes per window.
            let mut retry = RetryWait::default();
            let entry = loop {
                match server
                    .connect_with_strategy(url.clone(), strategy.clone())
                    .await
                {
                    Ok(connection) => break Arc::new(connection),
                    Err(e) => {
                        let wait = retry.next().unwrap();
                        debug!(error = %e, url = %url, waiting = ?wait, "Connection attempt failed");
                        sleep(wait).await;
                    }
                }
            };
            server.outbound.write().unwrap().push(entry.clone());
            info!(url = %url, schedule = %schedule, "Agent connected in polling mode");

            loop {
                // Sleep until the next scheduled check-in.
                let wait = schedule
                    .upcoming(Utc)
                    .next()
                    .and_then(|t| (t - Utc::now()).to_std().ok())
                    .unwrap_or_else(|| Duration::from_secs(1));
                debug!(url = %url, waiting = ?wait, "Waiting for next poll window");
                sleep(wait).await;

                match entry.open_websocket(&network, &instance).await {
                    Ok(_) => {
                        info!(url = %url, timeout = ?timeout, "Poll window open");
                        sleep(*timeout).await;
                        entry.close_websocket();
                        debug!(url = %url, "Poll window closed");
                    }
                    Err(e) => {
                        warn!(error = %e, url = %url, "Poll check-in failed");
                    }
                }
            }
        }
    }
}

/// One attempt at connecting to the server and holding the connection open.
///
/// Returns how long the websocket stayed up, or an error if it never came up.
/// Nothing is waited out in here: the caller owns the backoff, so that every way
/// an attempt can fail is retried the same way.
async fn attempt_connection(
    server: &sandpolis_server::ServerManager,
    network: &sandpolis_instance::network::NetworkManager,
    instance: &sandpolis_instance::InstanceManager,
    url: &sandpolis_server::ServerUrl,
    #[cfg(feature = "uki")] boot: &Arc<BootAgentState>,
) -> Result<Duration> {
    let entry = Arc::new(server.connect(url.clone()).await?);
    server.outbound.write().unwrap().push(entry.clone());

    // Drop the bookkeeping however this attempt ends, the early return below
    // included.
    let _guard = OutboundGuard {
        server,
        connection: entry.clone(),
    };

    // The websocket is what the server syncs this agent's database over and
    // what carries every stream, so a connection without one is no use to
    // anybody: fail the attempt and let the caller come back round. Waiting
    // here instead would wedge the agent for good, since the only thing that
    // cancels `entry` is its own `Drop` and this frame holds an `Arc` to it.
    let socket = entry.open_websocket(network, instance).await?;
    info!(url = %url, "Connected to server");

    // A boot agent announces itself so the server can place a boot hold (and
    // later release it).
    #[cfg(feature = "uki")]
    {
        *boot.server.lock().unwrap() = Some(format!("{}:{}", url.host, url.port));
        if let Err(e) = socket
            .open_stream(
                BootStreamRequester {
                    state: boot.clone(),
                },
                BootStreamRequest::Announce {
                    agent: instance.instance_id,
                },
            )
            .await
        {
            warn!(error = %e, "Failed to open boot stream");
        }
    }

    let established = tokio::time::Instant::now();

    // Reconnect when either token fires: the socket's, which is what a
    // server-side drop or a dead peer cancels, or the connection's own, which
    // only something outside this loop can cancel.
    tokio::select! {
        _ = socket.cancel.cancelled() => {}
        _ = entry.cancel.cancelled() => {}
    }

    #[cfg(feature = "uki")]
    {
        *boot.server.lock().unwrap() = None;
    }

    Ok(established.elapsed())
}

/// Removes a connection from `outbound` however the attempt ends, so a failure
/// partway through doesn't leave a dead entry behind.
struct OutboundGuard<'a> {
    server: &'a sandpolis_server::ServerManager,
    connection: Arc<sandpolis_server::ServerConnection>,
}

impl Drop for OutboundGuard<'_> {
    fn drop(&mut self) {
        self.server
            .outbound
            .write()
            .unwrap()
            .retain(|c| !Arc::ptr_eq(c, &self.connection));
    }
}

#[cfg(test)]
mod test_server_retry {
    use super::*;

    /// The backoff must grow and then stop growing. An agent whose server is
    /// down retries forever, so an uncapped delay would take absurdly long to
    /// recover and a flat one would hammer the server.
    #[test]
    fn backoff_grows_and_caps() {
        let waits: Vec<Duration> = server_retry().take(40).collect();

        assert!(
            waits[0] <= Duration::from_secs(2),
            "the first retry should be prompt, not {:?}",
            waits[0]
        );

        for pair in waits.windows(2) {
            assert!(
                pair[1] >= pair[0],
                "backoff must never shrink: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }

        let cap = Duration::from_secs(60);
        assert!(
            waits.iter().all(|w| *w <= cap),
            "backoff must stay under the cap"
        );
        assert_eq!(
            *waits.last().unwrap(),
            cap,
            "a long outage should settle at the cap"
        );
    }

    /// A connection has to stay up a while before it counts as healthy,
    /// otherwise a server that accepts and instantly drops us resets the
    /// backoff every time and the agent spins.
    #[test]
    fn stability_threshold_exceeds_first_backoff() {
        let first = server_retry().next().unwrap();
        assert!(
            STABLE_AFTER > first,
            "a connection that dies inside the first backoff window must not count as stable"
        );
    }
}
