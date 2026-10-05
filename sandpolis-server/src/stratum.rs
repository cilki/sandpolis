//! The local stratum server's link to its global stratum server.
//!
//! A local stratum (LS) server owns the instances attached to it and replicates
//! everything else:
//!
//! - **Estate data** (users, accounts, realms) replicates down: the LS holds a
//!   standing subscription to the GS for global-scope models.
//! - **Ownership** is claimed up. The LS announces its attached instances over
//!   the claim stream (see [`crate::ownership`]); grants come back, each scope
//!   is hydrated from the GS, and from then on the LS applies those agents'
//!   records to its own database — the GS pulls them back up.
//! - **Routing** works in both directions. The LS advertises its attached
//!   instances so the GS can reach them, and points its own default route at the
//!   GS so it can reach everything else.

use crate::ownership::{OwnershipRequest, OwnershipRequester, attached_instances};
use crate::{ServerManager, ServerStratum};
use anyhow::{Result, anyhow};
use sandpolis_instance::InstanceId;
use sandpolis_instance::InstanceManager;
use sandpolis_instance::database::sync::SyncFilter;
use sandpolis_instance::network::reachability::ReachabilityRequest;
use sandpolis_instance::network::stream::StreamMessage;
use sandpolis_instance::network::{InstanceConnection, NetworkManager, RetryWait};
use sandpolis_instance::realm::RealmName;
use sandpolis_instance::realm::server::AuthenticatedRealm;
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::{Duration, sleep};
use tracing::{debug, info, warn};

/// How long to wait after a connection change before rebuilding the
/// subscription, so a burst of peers connecting produces one rebuild.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Safety-net re-check interval, in case a change slips past the notifier.
const RESYNC_INTERVAL: Duration = Duration::from_secs(30);

/// How long an upstream link must stay up before it counts as healthy and the
/// backoff resets. Without this, a link that is accepted and immediately dropped
/// would reset the backoff every time and reconnect in a tight loop.
const STABLE_AFTER: Duration = Duration::from_secs(30);

/// Backoff for reaching the global stratum server. Starts fast (a restarting GS
/// is back in seconds) and caps out so a long outage doesn't turn into a busy
/// loop against it.
fn upstream_retry() -> RetryWait {
    RetryWait::Exponential {
        initial: Duration::from_secs(1),
        constant: 20.0,
        limit: Some(Duration::from_secs(60)),
        iteration: 0,
    }
}

/// Ask the global stratum server to issue this server's certificate.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct IssueServerCertRequest {
    pub realm: RealmName,
    /// The requesting server's own instance id. Decoding rejects anything that
    /// isn't a server id, so a non-server can't even ask.
    pub instance_id: sandpolis_instance::ServerId,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub enum IssueServerCertResponse {
    Ok {
        /// The realm CA certificate, **without** its private key.
        ca: Vec<u8>,
        cert: Vec<u8>,
        key: Vec<u8>,
    },
    /// Only the global stratum server holds the CA.
    NotGlobalStratum,
    /// The requested id isn't a server, the realm doesn't exist here, or it
    /// isn't the realm the caller's certificate authenticates it for.
    Rejected,
}

/// Issue a server certificate to a local stratum server (global stratum only).
///
/// The caller has already been authenticated by `auth_middleware` against the
/// realm's CA, so it holds a valid realm certificate for this network, and the
/// realm it holds one *for* is the only realm it can be issued from: a
/// certificate is signed by that realm's CA, which is the trust root of
/// everything in it.
///
/// TODO: this currently issues to any authenticated realm-certificate holder,
/// which is every agent in the realm. Issuance is meant to be gated on an
/// operator approving the request in the GUI — the certificate itself can't
/// carry that entitlement, since clients and agents hold the same kind.
pub async fn issue_server_cert(
    axum::extract::State(server): axum::extract::State<ServerManager>,
    AuthenticatedRealm(realm): AuthenticatedRealm,
    axum::extract::Json(request): axum::extract::Json<IssueServerCertRequest>,
) -> axum::Json<IssueServerCertResponse> {
    use sandpolis_instance::realm::{RealmCert, RealmCertType};

    if server.stratum.is_local() {
        return axum::Json(IssueServerCertResponse::NotGlobalStratum);
    }

    if request.realm != realm {
        warn!(
            requested = %request.realm,
            authenticated = %realm,
            "Refusing to issue a server certificate for another realm"
        );
        return axum::Json(IssueServerCertResponse::Rejected);
    }

    let instance_id = request.instance_id;

    let issued = (|| -> Result<(RealmCert, RealmCert)> {
        let db = server.realms.realm(request.realm.clone())?;
        let r = db.r_transaction()?;
        let cluster_cert: RealmCert = r
            .scan()
            .primary()?
            .all()?
            .collect::<std::result::Result<Vec<RealmCert>, _>>()?
            .into_iter()
            .find(|cert| cert.cert_type == RealmCertType::Cluster)
            .ok_or_else(|| anyhow!("no realm CA for {}", request.realm))?;
        drop(r);

        let server_cert = cluster_cert.server_cert(instance_id)?;
        Ok((cluster_cert, server_cert))
    })();

    match issued {
        Ok((cluster_cert, server_cert)) => {
            let Some(key) = server_cert.key.clone() else {
                return axum::Json(IssueServerCertResponse::Rejected);
            };
            info!(instance = %instance_id, realm = %request.realm, "Issued a server certificate to a local stratum server");
            axum::Json(IssueServerCertResponse::Ok {
                // The private half of the CA never leaves the global stratum
                // server: a local stratum server verifies with it, never issues.
                ca: cluster_cert.cert,
                cert: server_cert.cert,
                key,
            })
        }
        Err(e) => {
            warn!(error = %e, realm = %request.realm, "Failed to issue a server certificate");
            axum::Json(IssueServerCertResponse::Rejected)
        }
    }
}

/// Obtain this local stratum server's certificate from its global stratum
/// server, retrying until it succeeds.
///
/// Must complete before the listener binds, since the certificate is what the
/// listener presents. Returns immediately if we already hold one (from a
/// previous run) or if this is the global stratum server.
pub async fn enroll(server: &ServerManager, instance: &InstanceManager) -> Result<()> {
    let ServerStratum::Local { global } = server.stratum.clone() else {
        return Ok(());
    };

    let realm = global.realm.clone();
    if server
        .realms
        .has_server_cert(realm.clone(), instance.instance_id)
    {
        debug!(realm = %realm, "Already enrolled with the global stratum server");
        return Ok(());
    }

    // Without a realm certificate we can't authenticate to the global stratum
    // server at all. That's a misconfiguration, not an outage, so say so now
    // rather than retrying against it forever.
    server.realms.find_endpoint_cert(realm.clone()).map_err(|e| {
        anyhow!(
            "{e}. A local stratum server authenticates to its global stratum \
             server with a realm certificate; pass one with --realm."
        )
    })?;

    info!(url = %global, realm = %realm, "Requesting a server certificate from the global stratum server");

    let mut retry = upstream_retry();
    loop {
        match request_cert(server, instance, &global, realm.clone()).await {
            Ok(()) => return Ok(()),
            Err(EnrollError::Permanent(e)) => return Err(e),
            Err(EnrollError::Transient(e)) => {
                let wait = retry.next().unwrap();
                warn!(error = %e, url = %global, waiting = ?wait, "Enrollment failed; retrying");
                sleep(wait).await;
            }
        }
    }
}

/// Whether a failed enrollment is worth retrying.
enum EnrollError {
    /// The global stratum server is unreachable or briefly unhealthy.
    Transient(anyhow::Error),
    /// A human has to fix something; retrying will never succeed.
    Permanent(anyhow::Error),
}

async fn request_cert(
    server: &ServerManager,
    instance: &InstanceManager,
    global: &crate::ServerUrl,
    realm: RealmName,
) -> std::result::Result<(), EnrollError> {
    let connection = server
        .connect(global.clone())
        .await
        .map_err(EnrollError::Transient)?;

    let response: IssueServerCertResponse = connection
        .post(
            "realm/server-cert",
            IssueServerCertRequest {
                realm: realm.clone(),
                instance_id: instance
                    .instance_id
                    .try_into()
                    .map_err(EnrollError::Permanent)?,
            },
        )
        .await
        .map_err(EnrollError::Transient)?;

    match response {
        IssueServerCertResponse::Ok { ca, cert, key } => server
            .realms
            .install_enrollment(realm, ca, cert, key, instance.instance_id)
            .map_err(EnrollError::Permanent),
        IssueServerCertResponse::NotGlobalStratum => Err(EnrollError::Permanent(anyhow!(
            "{global} is itself a local stratum server; the realm cert must name \
             the network's global stratum server"
        ))),
        IssueServerCertResponse::Rejected => Err(EnrollError::Permanent(anyhow!(
            "{global} refused to issue a server certificate for realm {realm}"
        ))),
    }
}

/// Maintain this local stratum server's link to its global stratum server.
///
/// Runs until cancelled, reconnecting whenever the link drops. Returns
/// immediately (as a no-op) on a global stratum server, which has no upstream.
pub async fn maintain_upstream(
    server: ServerManager,
    network: NetworkManager,
    instance: InstanceManager,
) -> Result<()> {
    let ServerStratum::Local { global } = server.stratum.clone() else {
        return Ok(());
    };

    info!(url = %global, "Local stratum server connecting to its global stratum server");

    let mut retry = upstream_retry();
    loop {
        // Every path through this loop backs off before trying again. A link
        // that fails at any stage — dial, upgrade, or after being established —
        // must not turn into a tight reconnect loop against the global stratum
        // server, which is likely already struggling if we got here.
        let wait = match attempt_link(&server, &network, &instance, &global).await {
            // The link was up long enough to count as healthy, so treat the next
            // outage as a fresh one and reconnect promptly.
            Ok(uptime) if uptime >= STABLE_AFTER => {
                warn!(url = %global, uptime = ?uptime, "Upstream link closed, reconnecting");
                retry = upstream_retry();
                retry.next().unwrap()
            }
            // Established but immediately dropped: keep backing off, otherwise a
            // server that accepts and then rejects us produces a hot loop.
            Ok(uptime) => {
                let wait = retry.next().unwrap();
                warn!(url = %global, uptime = ?uptime, waiting = ?wait, "Upstream link dropped quickly; backing off");
                wait
            }
            Err(e) => {
                let wait = retry.next().unwrap();
                debug!(error = %e, url = %global, waiting = ?wait, "Upstream connection attempt failed");
                wait
            }
        };

        sleep(wait).await;
    }
}

/// One attempt at establishing and holding the upstream link.
///
/// Returns how long the link stayed up, or an error if it never came up.
async fn attempt_link(
    server: &ServerManager,
    network: &NetworkManager,
    instance: &InstanceManager,
    global: &crate::ServerUrl,
) -> Result<Duration> {
    let upstream = Arc::new(server.connect(global.clone()).await?);
    server.outbound.write().unwrap().push(upstream.clone());

    // Drop our bookkeeping however this attempt ends.
    let _guard = OutboundGuard {
        server,
        connection: upstream.clone(),
    };

    let link = upstream.open_websocket(network, instance).await?;
    info!(url = %global, "Connected to global stratum server");

    let established = tokio::time::Instant::now();
    if let Err(e) = run_link(server, &link, network).await {
        warn!(error = %e, "Upstream link setup failed");
    }

    // Block until the link drops.
    link.cancel.cancelled().await;

    // Nobody else can report this: an unreachable global stratum server writes
    // no liveness row of its own, and this server is the one that noticed. The
    // notification waits here until the link is back, since that is the only way
    // out of an edge server during an outage.
    if let Some(peer) = link.data.read().remote_instance {
        crate::liveness::report_unreachable(peer);
    }

    Ok(established.elapsed())
}

/// Removes a connection from `outbound` however the attempt ends, so a failure
/// partway through doesn't leave a dead entry behind.
struct OutboundGuard<'a> {
    server: &'a ServerManager,
    connection: Arc<crate::ServerConnection>,
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

/// Set up everything that rides on one established upstream link.
async fn run_link(
    server: &ServerManager,
    link: &Arc<InstanceConnection>,
    network: &NetworkManager,
) -> Result<()> {
    // Anything this server can't resolve locally goes up to the global stratum
    // server, which knows the whole estate. Attaching the relay lets messages
    // arriving *from* the GS be forwarded on to our own agents.
    network.relay.set_upstream(Arc::downgrade(link));
    link.streams.set_relay(Arc::downgrade(&network.relay));

    let realm = network.database.realm(RealmName::default())?;

    // Estate-wide data always replicates down from the global stratum server.
    // Instance data flows the other way: scopes owned here are pulled *by* the
    // GS, so there is nothing per-instance to subscribe to.
    if let Err(e) = link
        .open_sync(realm.clone(), vec![SyncFilter::global()])
        .await
    {
        warn!(error = %e, "Failed to open the global subscription");
    }

    // The claim stream: grants and revocations arrive here and drive the scope
    // table (hydrating each new grant before it becomes writable).
    let table = network
        .database
        .authority()
        .scope_table()
        .cloned()
        .ok_or_else(|| anyhow!("a local stratum server holds scoped write authority"))?;
    let (claim_id, claim_tx) = link.streams.register(OwnershipRequester {
        table,
        ownership: server.ownership.clone(),
        realm,
        via: Arc::downgrade(link),
    });

    // Recompute the attached set on change: the reachability advertisement and
    // the ownership claim answer the same question. The first pass always runs,
    // even with nothing attached — the empty claim is what prompts the GS to
    // report any scopes still owned from a previous run.
    let notify = Arc::new(Notify::new());
    {
        let notify = notify.clone();
        network.connections.listen(move |_| notify.notify_one());
    }

    let link = link.clone();
    let network = network.clone();
    tokio::spawn(async move {
        let mut previous: Option<BTreeSet<InstanceId>> = None;

        loop {
            let peers = attached_instances(&network);

            if previous.as_ref() != Some(&peers) {
                debug!(count = peers.len(), "Attached instance set changed");
                let list: Vec<InstanceId> = peers.iter().copied().collect();

                // Let the global stratum server route to our peers.
                match serde_cbor::to_vec(&ReachabilityRequest::advertise(&list)) {
                    Ok(payload) => {
                        let (advert_id, advert_tx) = link.open_reachability();
                        if let Err(e) = advert_tx
                            .send(StreamMessage::local(advert_id, payload))
                            .await
                        {
                            warn!(error = %e, "Failed to advertise reachability");
                        }
                    }
                    Err(e) => warn!(error = %e, "Failed to encode reachability advertisement"),
                }

                // Claim ownership of the instances attached to us.
                match serde_cbor::to_vec(&OwnershipRequest::Claim { instances: list }) {
                    Ok(payload) => {
                        if let Err(e) = claim_tx.send(StreamMessage::local(claim_id, payload)).await
                        {
                            warn!(error = %e, "Failed to send ownership claim");
                        }
                    }
                    Err(e) => warn!(error = %e, "Failed to encode ownership claim"),
                }

                previous = Some(peers);
            }

            tokio::select! {
                _ = link.cancel.cancelled() => break,
                _ = notify.notified() => sleep(DEBOUNCE).await,
                _ = sleep(RESYNC_INTERVAL) => {}
            }
        }

        debug!("Upstream link torn down; stopping claim watcher");
    });

    Ok(())
}

#[cfg(test)]
mod test_upstream_retry {
    use super::*;

    /// The backoff must grow and then stop growing. A local stratum server that
    /// can't reach its global stratum server retries forever, so an uncapped or
    /// flat delay would either hammer the GS or take absurdly long to recover.
    #[test]
    fn backoff_grows_and_caps() {
        let waits: Vec<Duration> = upstream_retry().take(40).collect();

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

    /// A link has to stay up a while before it counts as healthy, otherwise a
    /// server that accepts and instantly drops us resets the backoff every time
    /// and we spin.
    #[test]
    fn stability_threshold_exceeds_first_backoff() {
        let first = upstream_retry().next().unwrap();
        assert!(
            STABLE_AFTER > first,
            "a link that dies inside the first backoff window must not count as stable"
        );
    }
}
