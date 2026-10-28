//! Deciding, and recording, whether the instances attached to this server are up.
//!
//! The connections themselves are the evidence: every agent stays attached for
//! as long as it runs, so an instance with a live session here is reachable and
//! one without is offline the moment its session ends.
//!
//! The conclusion is written to
//! [`LivenessData`](sandpolis_instance::network::liveness::LivenessData), which
//! replicates, so a client learns about agents it has no connection to.

use anyhow::Result;
use chrono::Utc;
use sandpolis_instance::network::NetworkManager;
use sandpolis_instance::network::liveness::LivenessData;
use sandpolis_instance::notification::{Notification, notify};
use sandpolis_instance::database::ResidentVec;
use sandpolis_instance::{InstanceId, InstanceType};
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::{Duration, sleep};
use tracing::{debug, info, warn};

/// How long to wait after a connection change before acting, so a burst of
/// reconnects produces one pass.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// How often to re-check with nothing having happened. A safety net, like the
/// ownership reconcilers': every change that matters arrives as a connection
/// event.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// This server's opinion of who is up.
pub struct Liveness {
    rows: ResidentVec<LivenessData>,

    /// The instance whose opinions these are. Only rows scoped to it are ours to
    /// write — another server's row is a replica, and writing it would fork a
    /// record that has exactly one writer by construction.
    observer: InstanceId,
}

impl Liveness {
    pub fn new(network: &NetworkManager, observer: InstanceId) -> Self {
        Self {
            rows: network.liveness.clone(),
            observer,
        }
    }

    /// Record that `subject` is attached, notifying if this server has never
    /// seen it before.
    fn mark_online(&self, subject: InstanceId) -> Result<()> {
        let now = Utc::now();

        match sandpolis_instance::network::liveness::find(&self.rows, self.observer, subject) {
            Some(row) => {
                let was_online = row.read().online;
                row.update(|d| {
                    d.online = true;
                    d.last_seen = now;
                    Ok(())
                })?;
                if !was_online {
                    info!(instance = %subject, "Instance came back online");
                }
            }
            None => {
                self.rows.push(LivenessData {
                    online: true,
                    last_seen: now,
                    ..LivenessData::observation(self.observer, subject)
                })?;

                // No row at all means this server has never seen it attach —
                // which is what "joined for the first time" means from here.
                info!(instance = %subject, "Instance joined for the first time");
                notify(
                    Notification::info(
                        LAYER,
                        format!("New {} joined", describe(subject).to_lowercase()),
                    )
                        .body(subject.to_string())
                        .about(subject),
                );
            }
        }

        Ok(())
    }

    /// Record that `subject` is no longer reachable.
    fn mark_offline(&self, subject: InstanceId) -> Result<()> {
        let Some(row) = sandpolis_instance::network::liveness::find(&self.rows, self.observer, subject)
        else {
            return Ok(());
        };

        row.update(|d| {
            d.online = false;
            Ok(())
        })?;

        warn!(instance = %subject, "Instance went offline");
        notify(offline_notification(subject));
        Ok(())
    }

    /// Bring every row this server owns in line with `attached`, the instances
    /// it currently holds a live connection to.
    fn reconcile(&self, attached: &BTreeSet<InstanceId>) {
        for subject in attached {
            // Write only on the edge into online. Rewriting an unchanged row on
            // every sweep would replicate a record per instance per sweep,
            // forever.
            let write = match sandpolis_instance::network::liveness::find(
                &self.rows,
                self.observer,
                *subject,
            ) {
                Some(row) => !row.read().online,
                None => true,
            };

            if write && let Err(e) = self.mark_online(*subject) {
                warn!(error = %e, instance = %subject, "Failed to record an instance as online");
            }
        }

        for subject in self.detached(attached) {
            if let Err(e) = self.mark_offline(subject) {
                warn!(error = %e, instance = %subject, "Failed to record an instance as offline");
            }
        }
    }

    /// The instances this server calls online that are no longer attached. The
    /// connection was the only signal, and it's gone.
    fn detached(&self, attached: &BTreeSet<InstanceId>) -> Vec<InstanceId> {
        self.rows
            .iter()
            .filter_map(|row| {
                let row = row.read();
                (row._instance_id == self.observer
                    && row.online
                    && !attached.contains(&row.subject))
                .then_some(row.subject)
            })
            .collect()
    }
}

/// The layer these notifications are attributed to.
const LAYER: &str = "Network";

/// What to call an instance in a notification.
fn describe(subject: InstanceId) -> &'static str {
    match subject.instance_type() {
        InstanceType::Agent => "Agent",
        InstanceType::Server => "Server",
        InstanceType::Client => "Client",
    }
}

/// The instances attached to this server right now.
///
/// Unlike [`ownership::attached_instances`](crate::ownership::attached_instances)
/// this keeps server peers: a local stratum server going quiet is exactly the
/// kind of thing the global stratum server should notice. Clients are dropped —
/// someone opening the GUI is not an event worth a notification.
fn attached(network: &NetworkManager) -> BTreeSet<InstanceId> {
    network
        .live_inbound()
        .iter()
        .filter_map(|c| c.data.read().remote_instance)
        .filter(|id| !id.is_client())
        .collect()
}

/// Keep this server's liveness rows current.
///
/// Wakes on connection changes (a socket opening, or a janitor removing the row
/// of one that died), and on a timer as a safety net.
pub async fn maintain_liveness(network: NetworkManager, liveness: Arc<Liveness>) {
    let notify = Arc::new(Notify::new());
    {
        let notify = notify.clone();
        network.connections.listen(move |_| notify.notify_one());
    }

    loop {
        liveness.reconcile(&attached(&network));

        tokio::select! {
            _ = notify.notified() => sleep(DEBOUNCE).await,
            _ = sleep(SWEEP_INTERVAL) => {}
        }
    }
}

/// Record that a server this instance dialed is no longer reachable.
///
/// The peer at the other end of a link we opened is the one case the reconciler
/// above cannot cover: an unreachable server writes nothing, so somebody on this
/// side has to say it.
pub fn report_unreachable(peer: InstanceId) {
    debug!(instance = %peer, "Reporting an unreachable peer");
    notify(offline_notification(peer));
}

/// The notification raised whenever an instance stops being reachable, however
/// this side found out. Shared so the reconciler and the dialing side can't
/// drift into wording the same event two ways.
fn offline_notification(subject: InstanceId) -> Notification {
    Notification::warn(LAYER, format!("{} went offline", describe(subject)))
        .body(subject.to_string())
        .about(subject)
}

#[cfg(test)]
mod test_liveness {
    use super::*;
    use sandpolis_instance::database::DatabaseManager;
    use sandpolis_instance::network::{ConnectionData, NetworkManagerData};
    use sandpolis_instance::test_db;

    fn online(liveness: &Liveness, subject: InstanceId) -> Option<bool> {
        sandpolis_instance::network::liveness::find(&liveness.rows, liveness.observer, subject)
            .map(|row| row.read().online)
    }

    /// Attached is online and detached is offline, with no grace in between:
    /// every agent stays connected while it runs, so a session ending is the
    /// whole signal.
    #[tokio::test]
    async fn online_exactly_while_attached() -> Result<()> {
        let db: DatabaseManager = test_db!(ConnectionData, LivenessData, NetworkManagerData);
        let network = NetworkManager::new(db).await?;
        let observer: InstanceId = sandpolis_instance::ServerId::random().into();
        let agent: InstanceId = sandpolis_instance::AgentId::random().into();
        let liveness = Liveness::new(&network, observer);

        liveness.reconcile(&BTreeSet::from([agent]));
        assert_eq!(online(&liveness, agent), Some(true));

        liveness.reconcile(&BTreeSet::new());
        assert_eq!(online(&liveness, agent), Some(false));

        liveness.reconcile(&BTreeSet::from([agent]));
        assert_eq!(online(&liveness, agent), Some(true));
        Ok(())
    }

    /// Another server's opinion is a replica: this one never writes it, even
    /// when the subject isn't attached here.
    #[tokio::test]
    async fn leaves_other_observers_rows_alone() -> Result<()> {
        let db: DatabaseManager = test_db!(ConnectionData, LivenessData, NetworkManagerData);
        let network = NetworkManager::new(db).await?;
        let observer: InstanceId = sandpolis_instance::ServerId::random().into();
        let other: InstanceId = sandpolis_instance::ServerId::random().into();
        let agent: InstanceId = sandpolis_instance::AgentId::random().into();
        network.liveness.push(LivenessData {
            online: true,
            ..LivenessData::observation(other, agent)
        })?;

        Liveness::new(&network, observer).reconcile(&BTreeSet::new());

        let row = sandpolis_instance::network::liveness::find(&network.liveness, other, agent)
            .expect("the replica is still there");
        assert!(row.read().online);
        Ok(())
    }
}
