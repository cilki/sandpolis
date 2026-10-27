use crate::{InstanceState, RuntimeOptions};
use anyhow::Result;
#[cfg(feature = "uki")]
use sandpolis_agent::bootagent::BootAgentState;
#[cfg(feature = "uki")]
use sandpolis_agent::bootagent::streams::{BootStreamRequest, BootStreamRequester};
use sandpolis_instance::network::RetryWait;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn};

#[cfg(feature = "uki")]
use tracing::error;

#[cfg(feature = "uki")]
pub mod boot;

/// How long a server connection must stay up before it counts as healthy and
/// the backoff resets. Without it, a server that accepts the session and then
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

/// The error and every cause under it, on one line.
///
/// The reason a connection failed is always the innermost cause — "Connection
/// refused", "certificate has expired" — and `Display` on the outer error shows
/// none of it, which is the difference between a log line someone can act on
/// and "error sending request".
fn error_chain(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(": ")
}

/// Bring up everything an agent needs and run it: the realm named by its realm
/// cert, the database its collectors write into, and the subsystems over both.
#[cfg(not(target_os = "android"))]
pub async fn start(args: crate::cli::AgentArgs) -> Result<std::process::ExitCode> {
    let mut options = args.options();

    // The realm cert names the server, carries the realm CA, and holds this
    // agent's own certificate.
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
        // The agent runs on a task of its own, so that its connections can be
        // taken off it before it stops: the reconnect loop forgets a
        // connection the moment its future is dropped, and left running it
        // would take the hang-up below for an outage and go round again.
        let server = state.server.clone();
        let mut agent = tokio::spawn(main(options, state));
        tokio::select! {
            result = &mut agent => result??,
            () = shutdown_signal() => {
                info!("Shutting down");
                let connections = server.outbound.read().unwrap().clone();
                agent.abort();
                for connection in connections {
                    connection.close(SHUTDOWN_GRACE).await;
                }
            }
        }
        Ok(std::process::ExitCode::SUCCESS)
    }
}

/// The longest an agent on its way out waits for the server to hear it go.
#[cfg(not(feature = "uki"))]
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// Resolve when the agent is asked to stop: Ctrl-C, or the `SIGTERM` a service
/// manager or `docker stop` sends.
///
/// Left to the default disposition, either signal kills the process without
/// closing its connection, and the server goes on showing the agent attached
/// until the keepalive deadline runs out.
#[cfg(not(feature = "uki"))]
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
                return;
            }
            Err(e) => warn!(error = %e, "Cannot listen for SIGTERM; only Ctrl-C will stop cleanly"),
        }
    }

    if let Err(e) = tokio::signal::ctrl_c().await {
        // Without a handler the default disposition still applies, so the
        // signal still stops the agent, just not cleanly.
        warn!(error = %e, "Cannot listen for Ctrl-C");
        std::future::pending::<()>().await;
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

    let server = state.server.clone();
    let network = state.network.clone();
    let instance = state.instance.clone();

    // Hold a single connection open for the agent's lifetime, reconnecting
    // whenever it drops.
    let mut retry = server_retry();
    let mut failures = 0;
    loop {
        // Every path through this loop comes back round, because an agent that
        // stops trying is indistinguishable from one that died — and nobody is
        // watching its logs to find out which.
        let wait = match attempt_connection(
            &server,
            &network,
            &instance,
            &url,
            failures,
            #[cfg(feature = "uki")]
            &boot,
        )
        .await
        {
            // The connection was up long enough to count as healthy, so treat
            // this outage as a fresh one and reconnect promptly.
            Ok(uptime) if uptime >= STABLE_AFTER => {
                warn!(url = %url, uptime = ?uptime, "Server connection cancelled, reconnecting");
                retry = server_retry();
                failures = 0;
                retry.next().unwrap()
            }
            // Established but immediately dropped: keep backing off, otherwise
            // a server that accepts and then drops us produces a hot loop.
            Ok(uptime) => {
                let wait = retry.next().unwrap();
                failures = 0;
                warn!(url = %url, uptime = ?uptime, waiting = ?wait, "Server connection dropped quickly; backing off");
                wait
            }
            // An agent nobody is watching interactively gets one log line and
            // nothing else, so this has to be loud: at `debug` an agent that
            // could never reach its server said nothing at all. Warning on every
            // attempt is affordable because the backoff above caps at a minute
            // — the same trade `sandpolis_server::stratum` makes on its own
            // retry loop.
            Err(e) => {
                let wait = retry.next().unwrap();
                failures += 1;
                warn!(
                    error = error_chain(&e),
                    url = %url,
                    waiting = ?wait,
                    failures,
                    "Cannot reach the server; retrying"
                );
                wait
            }
        };

        sleep(wait).await;
    }
}

/// One attempt at connecting to the server and holding the connection open.
///
/// Returns how long the session stayed up, or an error if it never came up.
/// Nothing is waited out in here: the caller owns the backoff, so that every way
/// an attempt can fail is retried the same way.
///
/// `failures` is how many consecutive attempts failed before this one, which is
/// the recovery half of the story the warnings tell: it says the agent is up
/// *and* how long it spent getting there, so a flapping link is visible without
/// turning on `--debug`.
async fn attempt_connection(
    server: &sandpolis_server::ServerManager,
    network: &sandpolis_instance::network::NetworkManager,
    instance: &sandpolis_instance::InstanceManager,
    url: &sandpolis_server::ServerUrl,
    failures: u32,
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

    // The session is what the server syncs this agent's database over and
    // what carries every stream, so a connection without one is no use to
    // anybody: fail the attempt and let the caller come back round. Waiting
    // here instead would wedge the agent for good, since the only thing that
    // cancels `entry` is its own `Drop` and this frame holds an `Arc` to it.
    let socket = entry.open_session(network, instance).await?;
    info!(url = %url, failures, "Connected to server");

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

#[cfg(test)]
mod test_connect_reporting {
    use super::*;

    /// The innermost cause is what says *why* the connection failed, so it has
    /// to survive into the one line the log prints.
    #[test]
    fn the_report_names_the_root_cause() {
        let error = anyhow::Error::msg("Connection refused")
            .context("client error (Connect)")
            .context("error sending request");
        assert_eq!(
            error_chain(&error),
            "error sending request: client error (Connect): Connection refused"
        );
    }

    /// Every failed attempt is warned about, which is only affordable because
    /// the backoff spaces them out: an agent may be pointed at a server that is
    /// down for hours, and a warning every few seconds for that long would bury
    /// everything else in the log. Over a long outage the reports have to
    /// settle to roughly one a minute.
    #[test]
    fn a_long_outage_reports_about_once_a_minute() {
        let hour = Duration::from_secs(3600);
        let mut elapsed = Duration::ZERO;
        let mut reports = 0;
        for wait in server_retry() {
            if elapsed >= hour {
                break;
            }
            elapsed += wait;
            reports += 1;
        }

        assert!(
            reports <= 75,
            "{reports} warnings in the first hour of an outage is too many"
        );
        assert!(
            reports >= 60,
            "{reports} warnings in the first hour of an outage is too few to show the agent is still trying"
        );
    }
}
