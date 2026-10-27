//! Client-side database sync: short-lived subscriptions driven by the UI.
//!
//! The UI calls [`subscribe`] for the data a view shows and [`unsubscribe`] when
//! it goes away. Each subscription opens a `SyncStream` to the server; incoming
//! records are applied to the client's local database by the stream's requester,
//! so the UI can read them synchronously via [`client_database`].

use sandpolis_instance::InstanceId;
use sandpolis_instance::database::DatabaseManager;
use sandpolis_instance::database::sync::{FilterScope, SyncFilter};
use sandpolis_instance::network::InstanceConnection;
use sandpolis_instance::network::stream::{StreamId, StreamMessage};
use sandpolis_instance::realm::RealmName;
use sandpolis_server::ServerUrl;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock};
use tokio::runtime::Handle;
use tokio::sync::mpsc::Sender;

static HANDLE: OnceLock<SyncHandle> = OnceLock::new();
static DATABASE: OnceLock<DatabaseManager> = OnceLock::new();

/// All established server connections, for routing data to the server it's
/// associated with (e.g. a probe stream to its owning server). The first entry
/// is the primary (global-stratum) connection that also backs [`connection`] and
/// the DB-sync subscriptions.
static CONNECTIONS: LazyLock<RwLock<Vec<ServerConnectionEntry>>> = LazyLock::new(Default::default);

struct ServerConnectionEntry {
    url: ServerUrl,
    instance_id: InstanceId,
    connection: Arc<InstanceConnection>,
}

/// Record an established server connection for routing. Called for every server
/// session the client brings up (including the primary). Idempotent per server.
pub fn register_connection(url: ServerUrl, connection: Arc<InstanceConnection>) {
    // Servers always report their id in the upgrade response; a connection
    // without one can't be routed to and isn't worth tracking.
    let Some(instance_id) = connection.data.read().remote_instance else {
        tracing::warn!("Ignoring a server connection with no instance id");
        return;
    };
    {
        let mut conns = CONNECTIONS.write().unwrap();
        match conns.iter_mut().find(|c| c.instance_id == instance_id) {
            // A reconnect replaces the dead entry rather than being ignored,
            // which is what puts the server back on [`connected_instances`].
            Some(existing) => {
                if !existing.connection.cancel.is_cancelled() {
                    return;
                }
                existing.url = url;
                existing.connection = connection.clone();
            }
            None => conns.push(ServerConnectionEntry {
                url,
                instance_id,
                connection: connection.clone(),
            }),
        }
    }

    watch_connection(instance_id, connection);
}

/// Tell the user when a server this client is talking to goes away.
///
/// An unreachable server writes no liveness row of its own, so this side is the
/// only one that can report it — and it is the case the user is most likely to
/// be watching when it happens.
fn watch_connection(instance_id: InstanceId, connection: Arc<InstanceConnection>) {
    let cancel = connection.cancel.clone();
    drop(connection);

    // The first connection is registered before `init`, so this can't go through
    // the handle that `spawn` uses.
    let Ok(runtime) = Handle::try_current() else {
        return;
    };

    runtime.spawn(async move {
        cancel.cancelled().await;
        sandpolis_instance::notification::notify(
            sandpolis_instance::notification::Notification::warn("Network", "Server went offline")
                .body(instance_id.to_string())
                .about(instance_id),
        );
    });
}

/// The connection to the server at `url`, if one is established.
pub fn connection_for(url: &ServerUrl) -> Option<Arc<InstanceConnection>> {
    CONNECTIONS
        .read()
        .unwrap()
        .iter()
        .find(|c| &c.url == url)
        .map(|c| c.connection.clone())
}

/// The instance id of the server at `url`, if connected.
pub fn instance_for(url: &ServerUrl) -> Option<InstanceId> {
    CONNECTIONS
        .read()
        .unwrap()
        .iter()
        .find(|c| &c.url == url)
        .map(|c| c.instance_id)
}

/// The URL of the primary (first-established, global-stratum) server connection.
pub fn primary_server_url() -> Option<ServerUrl> {
    CONNECTIONS.read().unwrap().first().map(|c| c.url.clone())
}

/// The instances the client currently has a live connection to.
///
/// Entries are never removed from [`CONNECTIONS`], so a dropped connection is
/// still listed — its cancellation token is what says it's gone.
pub fn connected_instances() -> Vec<InstanceId> {
    CONNECTIONS
        .read()
        .unwrap()
        .iter()
        .filter(|c| !c.connection.cancel.is_cancelled())
        .map(|c| c.instance_id)
        .collect()
}

/// All known server connections as `(url, instance_id)`, for grouping in the UI.
pub fn servers() -> Vec<(ServerUrl, InstanceId)> {
    CONNECTIONS
        .read()
        .unwrap()
        .iter()
        .map(|c| (c.url.clone(), c.instance_id))
        .collect()
}

/// Install the client's database and session connection for sync. Called after
/// the session to the server is established, and again after each reconnect.
///
/// A reconnect swaps the socket underneath the existing handle and forgets the
/// subscriptions that were open on the dead one, so the views that want them ask
/// again. Installing a whole new handle instead would strand every caller that
/// already holds one.
pub fn init(connection: Arc<InstanceConnection>, database: DatabaseManager) {
    let _ = DATABASE.set(database.clone());

    if let Some(handle) = HANDLE.get() {
        if Arc::ptr_eq(&handle.inner.connection.read().unwrap(), &connection) {
            return;
        }
        handle.inner.subs.lock().unwrap().clear();
        *handle.inner.connection.write().unwrap() = connection;
        return;
    }

    let _ = HANDLE.set(SyncHandle {
        inner: Arc::new(SyncHandleInner {
            connection: RwLock::new(connection),
            database,
            subs: Mutex::new(HashMap::new()),
            // Captured here because `init` runs inside the Tokio runtime. UI
            // callers (e.g. Bevy systems) invoke `subscribe`/`unsubscribe` from
            // threads with no runtime context, so we spawn through this handle.
            runtime: Handle::current(),
        }),
    });
}

/// Spawn a future onto the client's Tokio runtime from any thread.
///
/// UI code (Bevy systems, etc.) runs off the runtime's worker threads, so
/// `tokio::spawn` would panic there. This routes through the runtime handle
/// captured in [`init`]. Returns `false` if sync hasn't been initialized yet.
pub fn spawn<F>(future: F) -> bool
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    if let Some(handle) = HANDLE.get() {
        handle.inner.runtime.spawn(future);
        true
    } else {
        false
    }
}

/// The client's local database, if initialized. UI query functions read from it.
pub fn client_database() -> Option<DatabaseManager> {
    DATABASE.get().cloned()
}

/// Every record of a model in the client's local (synced) database.
///
/// This is how a subsystem's client code reads what its subscription replicated
/// down. An empty result before the database exists is deliberate: views are
/// built before the connection is, and a view with nothing to show yet is
/// correct rather than an error.
pub fn scan_all<T>() -> anyhow::Result<Vec<T>>
where
    T: sandpolis_instance::database::Data + native_model::Model + 'static,
{
    let Some(database) = client_database() else {
        return Ok(vec![]);
    };
    let realm = database.realm(RealmName::default())?;
    let r = realm.r_transaction()?;
    Ok(r.scan()
        .primary::<T>()?
        .all()?
        .collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Like [`scan_all`], but only the latest revision of each record.
///
/// This is what a view of current state wants for temporal models, whose
/// replicated revision history would otherwise show up as duplicate rows. Note
/// `DataRevision` equality compares only the number, so the variant has to be
/// matched explicitly.
pub fn scan_latest<T>() -> anyhow::Result<Vec<T>>
where
    T: sandpolis_instance::database::Data + native_model::Model + 'static,
{
    use sandpolis_instance::database::DataRevision;
    Ok(scan_all::<T>()?
        .into_iter()
        .filter(|item| matches!(item.revision(), DataRevision::Latest(_)))
        .collect())
}

/// Subscribe to live updates for several models at once, optionally scoped to
/// one instance. The mirror of [`unsubscribe_all`].
pub fn subscribe_all(model_ids: impl IntoIterator<Item = u32>, instance: Option<InstanceId>) {
    for model_id in model_ids {
        subscribe(model_id, instance);
    }
}

/// Drop the subscriptions created by [`subscribe_all`].
pub fn unsubscribe_all(model_ids: impl IntoIterator<Item = u32>, instance: Option<InstanceId>) {
    for model_id in model_ids {
        unsubscribe(model_id, instance);
    }
}

/// The client's session with the server, if established. Used to open
/// relayed streams to agents (desktop, shell, filesystem).
pub fn connection() -> Option<Arc<InstanceConnection>> {
    HANDLE
        .get()
        .map(|h| h.inner.connection.read().unwrap().clone())
}

/// Wait until the server connection is established (or `timeout` elapses),
/// returning it. One-shot noninteractive commands use this since the connection
/// is brought up asynchronously after startup.
pub async fn wait_for_connection(timeout: std::time::Duration) -> Option<Arc<InstanceConnection>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(c) = connection() {
            return Some(c);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Subscribe to a model's records (optionally scoped to one instance). Idempotent
/// per `(model_id, instance)`.
pub fn subscribe(model_id: u32, instance: Option<InstanceId>) {
    if let Some(handle) = HANDLE.get() {
        handle.subscribe(model_id, instance);
    }
}

/// Drop a subscription previously created with [`subscribe`].
pub fn unsubscribe(model_id: u32, instance: Option<InstanceId>) {
    if let Some(handle) = HANDLE.get() {
        handle.unsubscribe(model_id, instance);
    }
}

type SubKey = (u32, Option<InstanceId>);

enum SubState {
    Pending,
    Active {
        id: StreamId,
        tx: Sender<StreamMessage>,
    },
}

#[derive(Clone)]
struct SyncHandle {
    inner: Arc<SyncHandleInner>,
}

struct SyncHandleInner {
    /// Swapped out on reconnect, so callers that hold the handle keep working
    /// across a server restart.
    connection: RwLock<Arc<InstanceConnection>>,
    database: DatabaseManager,
    subs: Mutex<HashMap<SubKey, SubState>>,
    runtime: Handle,
}

impl SyncHandle {
    fn subscribe(&self, model_id: u32, instance: Option<InstanceId>) {
        let key = (model_id, instance);
        {
            let mut subs = self.inner.subs.lock().unwrap();
            if subs.contains_key(&key) {
                return;
            }
            subs.insert(key, SubState::Pending);
        }

        let realm = match self.inner.database.realm(RealmName::default()) {
            Ok(realm) => realm,
            Err(_) => {
                self.inner.subs.lock().unwrap().remove(&key);
                return;
            }
        };
        let this = self.clone();
        let connection = self.inner.connection.read().unwrap().clone();
        self.inner.runtime.spawn(async move {
            let filters = vec![SyncFilter {
                model_id: Some(model_id),
                scope: instance.map_or(FilterScope::All, FilterScope::Instance),
            }];
            match connection.open_sync(realm, filters).await {
                Ok((id, tx)) => {
                    let mut subs = this.inner.subs.lock().unwrap();
                    // Only keep it if it wasn't unsubscribed while pending.
                    if subs.remove(&key).is_some() {
                        subs.insert(key, SubState::Active { id, tx });
                    } else {
                        connection.close_stream(id);
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "Failed to open sync subscription");
                    this.inner.subs.lock().unwrap().remove(&key);
                }
            }
        });
    }

    fn unsubscribe(&self, model_id: u32, instance: Option<InstanceId>) {
        let key = (model_id, instance);
        let state = self.inner.subs.lock().unwrap().remove(&key);
        if let Some(SubState::Active { id, tx }) = state {
            let connection = self.inner.connection.read().unwrap().clone();
            self.inner.runtime.spawn(async move {
                let _ = connection.close_sync(id, &tx).await;
            });
        }
    }
}
