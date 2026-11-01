//! Per-instance data ownership across strata.
//!
//! Exactly one server owns a given instance's data at a time: the server that
//! instance is directly connected to. Writes happen at the owner and replicate
//! to the global stratum (GS) server, which holds the whole estate but owns
//! only the instances attached to it directly.
//!
//! The GS is the single arbiter. Servers *claim* the set of instances attached
//! to them — the GS claims locally, a local stratum (LS) server claims over an
//! [`Ownership`] stream on its upstream link — and the GS resolves every claim
//! against one persistent grant table, bumping a fencing epoch each time
//! ownership moves. Disconnecting is not a release: an LS keeps ownership (and
//! keeps writing) through a GS outage, and only loses an instance when it shows
//! up attached somewhere else.
//!
//! Replication follows ownership, always pull-based: the owner serves, the
//! replica subscribes. The GS pulls each LS's owned scopes (plus the LS's own
//! data); an owner pulls its attached agents' records and applies them locally.
//! Because the GS only ever pulls an instance's records from that instance's
//! current owner, a stale owner's writes can never enter the estate — the pull
//! subscription is the fence.
//!
//! A grant is not writable immediately: the new owner first *hydrates* the
//! scope from the GS (a snapshot-complete-bounded subscription), so its
//! revision counters continue where the previous owner left off.

use anyhow::Result;
use native_db::ToKey;
use native_model::Model;
use sandpolis_instance::InstanceId;
use sandpolis_instance::database::sync::SyncFilter;
use sandpolis_instance::database::{
    Data, DataScope, RealmDatabase, ResidentVec, ScopeState, ScopeTable,
};
use sandpolis_instance::network::stream::{
    StreamId, StreamMessage, StreamRequester, StreamResponder,
};
use sandpolis_instance::network::{InstanceConnection, NetworkManager};
use sandpolis_macros::{Stream, data};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Notify;
use tokio::sync::mpsc::Sender;
use tokio::time::{Duration, sleep};
use tracing::{debug, info, warn};

/// How long to wait after a change before acting, so a burst of connections
/// produces one recompute.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Safety-net re-check interval, in case a change slips past a notifier.
const RESYNC_INTERVAL: Duration = Duration::from_secs(30);

/// One row of the grant table: `instance`'s data is owned by `owner`.
///
/// On the GS this table is authoritative for the whole network. On an LS it is
/// a mirror of that server's own grants, maintained with `local_write` (never
/// replicated) so ownership survives a restart while the GS is unreachable.
#[data]
pub struct OwnershipData {
    /// The instance whose data this grant covers.
    #[secondary_key]
    pub instance: InstanceId,

    /// The server that currently owns it.
    #[secondary_key]
    pub owner: InstanceId,

    /// Fencing counter, bumped each time ownership moves. An owner seeing an
    /// unexpected epoch knows the scope left and returned behind its back.
    pub epoch: u64,
}

impl OwnershipData {
    /// A grant of `instance`'s data to `owner` at the given epoch.
    pub fn grant(instance: InstanceId, owner: InstanceId, epoch: u64) -> Self {
        Self {
            instance,
            owner,
            epoch,
            _id: Default::default(),
            _revision: Default::default(),
            _creation: Default::default(),
        }
    }
}

/// Sent by a server to the global stratum server.
#[derive(Serialize, Deserialize, Debug)]
pub enum OwnershipRequest {
    /// The complete set of instances directly attached to the sender, replacing
    /// any previous claim. Sent on connect and again whenever the set changes.
    ///
    /// Declarative and idempotent: the GS grants what is newly attached here and
    /// leaves everything else alone — an instance *absent* from the claim stays
    /// with its current owner until it shows up somewhere else.
    Claim { instances: Vec<InstanceId> },
}

/// One scope granted to a server.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OwnedScope {
    pub instance: InstanceId,
    pub epoch: u64,
}

/// Sent by the global stratum server back down the claim stream.
#[derive(Serialize, Deserialize, Debug)]
pub enum OwnershipEvent {
    /// The complete set of scopes the receiving server owns, replacing any
    /// previous set. A scope missing from the list has been revoked; a scope
    /// with an unfamiliar epoch must be (re)hydrated before it is written.
    Owned { scopes: Vec<OwnedScope> },
}

/// The grant table and the machinery to keep it consistent.
pub struct Ownership {
    /// Authoritative on the GS; this server's own mirror on an LS.
    pub grants: ResidentVec<OwnershipData>,

    /// Fires when the local ownership view changes, so the agent sync
    /// reconciler re-evaluates without polling.
    pub changed: Arc<Notify>,

    /// Serializes grant mutations so epochs are strictly monotonic per scope.
    lock: Mutex<()>,
}

impl Ownership {
    pub fn new(realm: &RealmDatabase) -> Result<Self> {
        Ok(Self {
            grants: realm.resident_vec(())?,
            changed: Arc::new(Notify::new()),
            lock: Mutex::new(()),
        })
    }

    /// Record that `claimer` is now the directly-connected server for these
    /// instances (global stratum only).
    ///
    /// Instances already owned by `claimer` are untouched; anything newly
    /// claimed moves with a bumped epoch. Instances *not* listed are also
    /// untouched — disconnection is not a release, which is what lets an owner
    /// keep serving through an outage.
    pub fn claim(&self, claimer: InstanceId, instances: &[InstanceId]) -> Result<()> {
        let _guard = self.lock.lock().unwrap();

        for &instance in instances {
            // A server needs no grant for its own scope.
            if instance == claimer {
                continue;
            }

            match self.grants.iter().find(|g| g.read().instance == instance) {
                Some(grant) => {
                    grant.update(|d| {
                        if d.owner != claimer {
                            d.owner = claimer;
                            d.epoch += 1;
                        }
                        Ok(())
                    })?;
                }
                None => {
                    self.grants.push(OwnershipData::grant(instance, claimer, 1))?;
                }
            }
        }

        self.changed.notify_one();
        Ok(())
    }

    /// Every scope currently owned by `owner`, sorted for stable comparison.
    pub fn owned_by(&self, owner: InstanceId) -> Vec<OwnedScope> {
        let mut scopes: Vec<OwnedScope> = self
            .grants
            .iter()
            .filter_map(|grant| {
                let g = grant.read();
                (g.owner == owner).then(|| OwnedScope {
                    instance: g.instance,
                    epoch: g.epoch,
                })
            })
            .collect();
        scopes.sort();
        scopes
    }

    /// Record one of this server's own grants in the mirror (local stratum
    /// only), called once the scope is fully hydrated — a half-hydrated scope
    /// must never be restored as owned.
    pub fn mirror_set(&self, self_id: InstanceId, scope: OwnedScope) -> Result<()> {
        let _guard = self.lock.lock().unwrap();

        match self
            .grants
            .iter()
            .find(|g| g.read().instance == scope.instance)
        {
            Some(grant) => grant.update_local(|d| {
                d.owner = self_id;
                d.epoch = scope.epoch;
                Ok(())
            }),
            None => self
                .grants
                .push_local(OwnershipData::grant(scope.instance, self_id, scope.epoch))
                .map(|_| ()),
        }
    }

    /// Drop a revoked grant from the mirror (local stratum only).
    pub fn mirror_remove(&self, instance: InstanceId) -> Result<()> {
        let _guard = self.lock.lock().unwrap();

        let id = self
            .grants
            .iter()
            .find(|g| g.read().instance == instance)
            .map(|g| g.read().id());
        if let Some(id) = id {
            self.grants.remove_local(id)?;
        }
        Ok(())
    }

    /// Restore ownership persisted by a previous run (local stratum only), so
    /// an LS restarting during a GS outage keeps serving its instances.
    pub fn restore(&self, table: &ScopeTable) {
        let Some(self_id) = table.self_id() else {
            return;
        };

        for grant in self.grants.iter() {
            let g = grant.read();
            if g.owner == self_id {
                info!(instance = %g.instance, epoch = g.epoch, "Restoring owned scope from a previous run");
                table.set_owned(g.instance, g.epoch);
            }
        }
    }
}

/// The instances directly attached to this server, excluding any server peer (a
/// server owns its own scope and is never owned).
pub(crate) fn attached_instances(network: &NetworkManager) -> BTreeSet<InstanceId> {
    network
        .live_inbound()
        .iter()
        .filter_map(|c| c.data.read().remote_instance)
        .filter(|id| !id.is_server())
        .collect()
}

/// Claim this server's directly attached instances for itself, forever (global
/// stratum only).
///
/// The GS is just another server in the ownership model; this local claim is
/// what revokes a local stratum server when an agent moves here. Remote claims
/// arrive through [`OwnershipResponder`] instead.
pub async fn maintain_local_claims(
    ownership: Arc<Ownership>,
    network: NetworkManager,
    local_instance: InstanceId,
) {
    let notify = Arc::new(Notify::new());
    {
        let notify = notify.clone();
        network.connections.listen(move |_| notify.notify_one());
    }

    let mut previous: Option<BTreeSet<InstanceId>> = None;
    loop {
        let attached = attached_instances(&network);
        if previous.as_ref() != Some(&attached) {
            let list: Vec<InstanceId> = attached.iter().copied().collect();
            if let Err(e) = ownership.claim(local_instance, &list) {
                warn!(error = %e, "Failed to claim attached instances");
            }
            previous = Some(attached);
        }

        tokio::select! {
            _ = notify.notified() => sleep(DEBOUNCE).await,
            _ = sleep(RESYNC_INTERVAL) => {}
        }
    }
}

#[cfg(test)]
mod test_grants {
    use super::*;
    
    use sandpolis_instance::database::DatabaseManager;
    use sandpolis_instance::realm::RealmName;
    use sandpolis_instance::{test_db, test_scoped_db};

    fn server() -> InstanceId {
        sandpolis_instance::ServerId::random().into()
    }

    fn agent() -> InstanceId {
        sandpolis_instance::AgentId::random().into()
    }

    /// A claim grants what is newly attached and bumps the epoch only when
    /// ownership actually moves, so a repeated claim is free.
    #[tokio::test]
    async fn claim_grants_and_bumps_epochs() -> Result<()> {
        let db: DatabaseManager = test_db!(OwnershipData);
        let ownership = Ownership::new(&db.realm(RealmName::default())?)?;

        let (ls1, ls2, x) = (server(), server(), agent());

        ownership.claim(ls1, &[x])?;
        assert_eq!(
            ownership.owned_by(ls1),
            vec![OwnedScope {
                instance: x,
                epoch: 1
            }]
        );

        // Idempotent: same owner, same epoch.
        ownership.claim(ls1, &[x])?;
        assert_eq!(ownership.owned_by(ls1)[0].epoch, 1);

        // The instance shows up somewhere else: ownership moves, epoch bumps.
        ownership.claim(ls2, &[x])?;
        assert!(ownership.owned_by(ls1).is_empty());
        assert_eq!(
            ownership.owned_by(ls2),
            vec![OwnedScope {
                instance: x,
                epoch: 2
            }]
        );

        Ok(())
    }

    /// Disconnection is not a release: an instance absent from a claim stays
    /// with its owner, which is what lets an edge server keep serving through
    /// an outage.
    #[tokio::test]
    async fn absent_claim_is_not_a_release() -> Result<()> {
        let db: DatabaseManager = test_db!(OwnershipData);
        let ownership = Ownership::new(&db.realm(RealmName::default())?)?;

        let (ls, x) = (server(), agent());
        ownership.claim(ls, &[x])?;
        ownership.claim(ls, &[])?;
        assert_eq!(ownership.owned_by(ls).len(), 1);
        Ok(())
    }

    /// A server never needs a grant for its own scope.
    #[tokio::test]
    async fn self_claims_are_ignored() -> Result<()> {
        let db: DatabaseManager = test_db!(OwnershipData);
        let ownership = Ownership::new(&db.realm(RealmName::default())?)?;

        let ls = server();
        ownership.claim(ls, &[ls])?;
        assert!(ownership.owned_by(ls).is_empty());
        Ok(())
    }

    /// A local stratum server's mirrored grants survive a restart: `restore`
    /// re-owns them so the server keeps writing while the GS is unreachable.
    #[tokio::test]
    async fn mirror_survives_restart() -> Result<()> {
        let table = Arc::new(ScopeTable::default());
        let db: DatabaseManager = test_scoped_db!(table, OwnershipData);
        let realm = db.realm(RealmName::default())?;

        let (self_id, x) = (server(), agent());
        table.set_self(self_id);

        let ownership = Ownership::new(&realm)?;
        ownership.mirror_set(
            self_id,
            OwnedScope {
                instance: x,
                epoch: 3,
            },
        )?;

        // "Restart": a fresh table and a fresh view of the same database.
        let table = ScopeTable::default();
        table.set_self(self_id);
        let ownership = Ownership::new(&realm)?;
        ownership.restore(&table);

        assert!(table.may_write(DataScope::Instance(x)));
        assert_eq!(table.state(x), Some(ScopeState::Owned { epoch: 3 }));

        // A revoked grant leaves the mirror and is not restored again.
        ownership.mirror_remove(x)?;
        let table = ScopeTable::default();
        table.set_self(self_id);
        let ownership = Ownership::new(&realm)?;
        ownership.restore(&table);
        assert!(!table.may_write(DataScope::Instance(x)));

        Ok(())
    }
}

/// GS side of the claim stream: resolves claims against the grant table, then
/// keeps the peer informed of its owned set and keeps our replication of that
/// set flowing.
#[derive(Stream)]
pub struct OwnershipResponder {
    ownership: Arc<Ownership>,
    realm: RealmDatabase,
    peer: InstanceId,
    /// The claiming connection, held weakly: it owns the stream registry that
    /// owns this responder, so a strong reference would be a cycle.
    via: Weak<InstanceConnection>,
    /// Whether the per-connection watcher task has started.
    watching: Mutex<bool>,
}

impl StreamResponder for OwnershipResponder {
    type In = OwnershipRequest;
    type Out = OwnershipEvent;

    async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
        let OwnershipRequest::Claim { instances } = request;

        debug!(peer = %self.peer, count = instances.len(), "Received ownership claim");
        self.ownership.claim(self.peer, &instances)?;

        // The watcher answers this claim (and every later change) with the full
        // owned set, and maintains our pull of everything the peer owns.
        self.ensure_watcher(sender);
        Ok(())
    }
}

impl OwnershipResponder {
    fn ensure_watcher(&self, sender: Sender<OwnershipEvent>) {
        {
            let mut watching = self.watching.lock().unwrap();
            if *watching {
                // A claim was already processed above; the running watcher picks
                // up any resulting change through its grant-table listener.
                return;
            }
            *watching = true;
        }

        let notify = Arc::new(Notify::new());
        {
            let notify = notify.clone();
            self.ownership.grants.listen(move |_| notify.notify_one());
        }

        let ownership = self.ownership.clone();
        let realm = self.realm.clone();
        let peer = self.peer;
        let via = self.via.clone();

        tokio::spawn(async move {
            let Some(cancel) = via.upgrade().map(|c| c.cancel.clone()) else {
                return;
            };

            let mut previous: Option<Vec<OwnedScope>> = None;
            let mut pull: Option<(StreamId, Sender<StreamMessage>)> = None;

            loop {
                let owned = ownership.owned_by(peer);
                if previous.as_ref() != Some(&owned) {
                    // Tell the peer its full owned set (it diffs and hydrates).
                    if sender
                        .send(OwnershipEvent::Owned {
                            scopes: owned.clone(),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }

                    // Rebuild our pull of the peer's scopes: the owner serves,
                    // we replicate. The peer's own scope is always included —
                    // its data (services, identity) reaches the estate this way.
                    if let Some(connection) = via.upgrade() {
                        if let Some((id, tx)) = pull.take() {
                            let _ = connection.close_sync(id, &tx).await;
                        }

                        let filters: Vec<SyncFilter> = owned
                            .iter()
                            .map(|scope| SyncFilter::instance(scope.instance))
                            .chain(std::iter::once(SyncFilter::instance(peer)))
                            .collect();

                        match connection.open_sync(realm.clone(), filters).await {
                            Ok(handle) => pull = Some(handle),
                            Err(e) => {
                                warn!(error = %e, %peer, "Failed to open pull toward the owner")
                            }
                        }
                    }

                    previous = Some(owned);
                }

                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = notify.notified() => sleep(DEBOUNCE).await,
                    _ = sleep(RESYNC_INTERVAL) => {}
                }
            }

            debug!(%peer, "Ownership watcher stopped");
        });
    }
}

/// Accept ownership claims from `connection` (global stratum only).
///
/// **Call this only for connections whose peer is a server.** A grant carries
/// the right to write an instance's data into the estate, so an agent or client
/// must never be able to claim.
///
/// Registered after the connection exists (rather than through the handler list
/// passed at construction) because the responder needs a handle to the very
/// connection it is being attached to.
pub fn accept_claims(
    connection: &Arc<InstanceConnection>,
    ownership: Arc<Ownership>,
    realm: RealmDatabase,
) {
    // Only identified server peers are given the claim stream, so a peer
    // without an id has nothing to claim.
    let Some(peer) = connection.data.read().remote_instance else {
        return;
    };
    let via = Arc::downgrade(connection);

    connection
        .streams
        .register_responder(move || OwnershipResponder {
            ownership: ownership.clone(),
            realm: realm.clone(),
            peer,
            via: via.clone(),
            watching: Mutex::new(false),
        });
}

/// LS side of the claim stream: applies granted/revoked scopes to the local
/// [`ScopeTable`], hydrating each new grant from the GS before it becomes
/// writable.
#[derive(Stream)]
pub struct OwnershipRequester {
    pub table: Arc<ScopeTable>,
    pub ownership: Arc<Ownership>,
    pub realm: RealmDatabase,
    /// The upstream link, for hydration subscriptions.
    pub via: Weak<InstanceConnection>,
}

impl StreamRequester for OwnershipRequester {
    type In = OwnershipEvent;
    type Out = OwnershipRequest;

    async fn on_message(&self, event: Self::In, _: Sender<Self::Out>) -> Result<()> {
        let OwnershipEvent::Owned { scopes } = event;

        // Anything we hold that the GS no longer grants is gone: stop writing
        // immediately and forget the mirror row. The replica data itself stays —
        // it is still a valid cache.
        for instance in self.table.tracked() {
            if !scopes.iter().any(|s| s.instance == instance) {
                info!(%instance, "Ownership revoked");
                self.table.release(instance);
                if let Err(e) = self.ownership.mirror_remove(instance) {
                    warn!(error = %e, %instance, "Failed to remove mirrored grant");
                }
            }
        }

        for scope in scopes {
            match self.table.state(scope.instance) {
                // Already owned at this epoch, or already hydrating: nothing to do.
                Some(ScopeState::Owned { epoch }) if epoch == scope.epoch => {}
                Some(ScopeState::Hydrating) => {}
                // An unfamiliar epoch means ownership left and returned while we
                // weren't looking; the data may have advanced elsewhere, so
                // treat it exactly like a new grant.
                _ => {
                    if let Err(e) = self.ownership.mirror_remove(scope.instance) {
                        warn!(error = %e, instance = %scope.instance, "Failed to clear stale mirrored grant");
                    }
                    self.hydrate(scope);
                }
            }
        }

        self.ownership.changed.notify_one();
        Ok(())
    }
}

impl OwnershipRequester {
    /// Replicate everything the GS holds for this scope, then flip it writable.
    ///
    /// Writes must wait for the full snapshot: the previous owner's revisions
    /// have to be locally present before new ones are minted on top of them.
    fn hydrate(&self, scope: OwnedScope) {
        self.table.begin_hydrating(scope.instance);

        let Some(link) = self.via.upgrade() else {
            self.table.release(scope.instance);
            return;
        };

        let table = self.table.clone();
        let ownership = self.ownership.clone();
        let realm = self.realm.clone();

        tokio::spawn(async move {
            info!(instance = %scope.instance, epoch = scope.epoch, "Hydrating granted scope");

            let subscription = link
                .open_sync_notified(realm, vec![SyncFilter::instance(scope.instance)])
                .await;

            match subscription {
                Ok((id, tx, complete)) => {
                    tokio::select! {
                        _ = complete.notified() => {
                            let _ = link.close_sync(id, &tx).await;

                            // The grant may have been revoked while we hydrated.
                            if table.state(scope.instance) == Some(ScopeState::Hydrating) {
                                table.set_owned(scope.instance, scope.epoch);
                                if let Some(self_id) = table.self_id()
                                    && let Err(e) = ownership.mirror_set(self_id, scope) {
                                        warn!(error = %e, instance = %scope.instance, "Failed to mirror grant");
                                    }
                                ownership.changed.notify_one();
                                info!(instance = %scope.instance, "Scope hydrated; now owned");
                            }
                        }
                        _ = link.cancel.cancelled() => {
                            // Link died mid-hydration: never partially owned.
                            // The reconnect re-claims and starts over.
                            table.release(scope.instance);
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, instance = %scope.instance, "Failed to open hydration subscription");
                    table.release(scope.instance);
                }
            }
        });
    }
}

/// Keep one pull subscription open toward every attached agent whose scope this
/// server owns, applying its records to the local database (local stratum only;
/// the global stratum server opens its pulls in the connect handler).
///
/// Runs independently of the upstream link: with the GS unreachable, a
/// reconnecting agent whose scope is still owned resumes syncing immediately —
/// which is the point of an edge server.
pub async fn maintain_agent_sync(
    network: NetworkManager,
    realm: RealmDatabase,
    table: Arc<ScopeTable>,
    ownership: Arc<Ownership>,
) {
    let notify = ownership.changed.clone();
    {
        let notify = notify.clone();
        network.connections.listen(move |_| notify.notify_one());
    }

    struct OpenPull {
        connection: Weak<InstanceConnection>,
        id: StreamId,
        tx: Sender<StreamMessage>,
    }
    let mut open: HashMap<InstanceId, OpenPull> = HashMap::new();

    loop {
        // Live inbound connections by instance.
        let connections: HashMap<InstanceId, Arc<InstanceConnection>> = network
            .inbound
            .read()
            .unwrap()
            .iter()
            .filter(|c| !c.cancel.is_cancelled())
            .filter_map(|c| c.data.read().remote_instance.map(|id| (id, c.clone())))
            .filter(|(id, _)| !id.is_server())
            .collect();

        // Drop pulls whose agent left, whose scope we lost, or whose connection
        // was replaced.
        let mut stale = Vec::new();
        open.retain(|id, pull| {
            let keep = connections.contains_key(id)
                && table.may_write(DataScope::Instance(*id))
                && pull
                    .connection
                    .upgrade()
                    .is_some_and(|c| !c.cancel.is_cancelled());
            if !keep {
                stale.push((pull.connection.clone(), pull.id, pull.tx.clone()));
            }
            keep
        });
        for (connection, id, tx) in stale {
            if let Some(connection) = connection.upgrade() {
                let _ = connection.close_sync(id, &tx).await;
            }
        }

        // Open pulls for owned, attached, not-yet-synced agents. The filter is
        // scoped to the agent's own instance, so a peer can never smuggle in
        // records belonging to someone else.
        for (id, connection) in &connections {
            if table.may_write(DataScope::Instance(*id)) && !open.contains_key(id) {
                match connection
                    .open_sync(realm.clone(), vec![SyncFilter::instance(*id)])
                    .await
                {
                    Ok((stream_id, tx)) => {
                        debug!(instance = %id, "Opened pull toward owned agent");
                        open.insert(
                            *id,
                            OpenPull {
                                connection: Arc::downgrade(connection),
                                id: stream_id,
                                tx,
                            },
                        );
                    }
                    Err(e) => warn!(error = %e, instance = %id, "Failed to open agent pull"),
                }
            }
        }

        tokio::select! {
            _ = notify.notified() => sleep(DEBOUNCE).await,
            _ = sleep(RESYNC_INTERVAL) => {}
        }
    }
}
