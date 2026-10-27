//! Type-erased database replication.
//!
//! A [`SyncRegistry`] knows how to, for each registered [`Data`] type:
//! - apply an incoming [`SyncRecord`] to a local database (upsert/delete),
//! - produce a snapshot of matching records, and
//! - watch the database and emit matching records as they change.
//!
//! This is the engine behind the `SyncStream` (see `network::sync`): a responder
//! uses `snapshot` + `spawn_watch` to serve data; a requester uses `apply` to
//! ingest it. The wire is cbor-encoded `Data` keyed by `native_model_id`, so the
//! mechanism is generic over every subsystem's data.

use super::{Data, RealmDatabase};
use crate::InstanceId;
use anyhow::Result;
use native_db::transaction::RwTransaction;
use native_db::watch::Event;
use native_model::Model;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum SyncOp {
    Upsert,
    Delete,
}

/// A single replicated database record.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct SyncRecord {
    pub model_id: u32,
    pub op: SyncOp,
    pub bytes: Vec<u8>,
}

/// Describes a subset of data a `SyncStream` cares about.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
pub struct SyncFilter {
    /// Restrict to a single model; `None` matches every registered model.
    pub model_id: Option<u32>,
    /// Restrict by owning instance.
    pub scope: FilterScope,
}

/// Which records match a [`SyncFilter`], by owning instance.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
pub enum FilterScope {
    /// Every record: instance-scoped and estate-wide alike.
    #[default]
    All,
    /// Only models with no owning instance (estate-wide data such as users and
    /// accounts). This is what lets a local stratum server replicate global data
    /// without pulling every instance's records along with it.
    Global,
    /// Only records belonging to this instance.
    Instance(InstanceId),
}

impl SyncFilter {
    /// A filter matching the entire database.
    pub fn all() -> Self {
        Self::default()
    }

    /// A filter matching estate-wide data only (no instance-scoped models).
    pub fn global() -> Self {
        Self {
            model_id: None,
            scope: FilterScope::Global,
        }
    }

    /// A filter matching everything belonging to one instance.
    pub fn instance(id: InstanceId) -> Self {
        Self {
            model_id: None,
            scope: FilterScope::Instance(id),
        }
    }
}

type ApplyFn = Box<dyn Fn(&RwTransaction, SyncOp, &[u8]) -> Result<()> + Send + Sync>;
type SnapshotFn = Box<dyn Fn(&RealmDatabase, FilterScope) -> Result<Vec<SyncRecord>> + Send + Sync>;
type WatchFn = Box<
    dyn Fn(&RealmDatabase, FilterScope, Sender<SyncRecord>, CancellationToken) -> Result<()>
        + Send
        + Sync,
>;

pub struct SyncType {
    apply: ApplyFn,
    snapshot: SnapshotFn,
    spawn_watch: WatchFn,
}

#[derive(Default)]
pub struct SyncRegistry {
    types: HashMap<u32, SyncType>,
}

impl SyncRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a type with no instance scoping. Instance-filtered subscriptions
    /// will never match this type (it has no owning instance).
    pub fn register<T>(&mut self)
    where
        T: Data + Model + 'static,
    {
        self.register_inner::<T>(None);
    }

    /// Register a type whose records belong to an instance, extracted by
    /// `instance_of` (typically `|d| d._instance_id`).
    pub fn register_scoped<T>(&mut self, instance_of: fn(&T) -> InstanceId)
    where
        T: Data + Model + 'static,
    {
        self.register_inner::<T>(Some(instance_of));
    }

    fn register_inner<T>(&mut self, instance_of: Option<fn(&T) -> InstanceId>)
    where
        T: Data + Model + 'static,
    {
        let model_id = T::native_model_id();

        let apply: ApplyFn = Box::new(|rw, op, bytes| {
            let (item, _): (T, u32) = native_model::decode(bytes.to_vec())
                .map_err(|e| anyhow::anyhow!("decode failed: {e}"))?;
            match op {
                SyncOp::Upsert => {
                    // There is exactly one writer per record, so `_revision` is
                    // totally ordered per row: a replayed or reordered older
                    // record must never clobber a newer one.
                    if let Some(existing) = rw.get().primary::<T>(item.id())?
                        && existing.revision() >= item.revision()
                    {
                        return Ok(());
                    }
                    rw.upsert(item)?;
                }
                SyncOp::Delete => {
                    // Ignore "not found" so deletes are idempotent.
                    let _ = rw.remove(item);
                }
            }
            Ok(())
        });

        let snapshot: SnapshotFn = Box::new(move |db, scope| {
            let r = db.r_transaction()?;
            let items: Vec<T> = r
                .scan()
                .primary::<T>()?
                .all()?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            drop(r);

            let mut out = Vec::new();
            for item in items {
                if !scope_matches(scope, instance_of, &item) {
                    continue;
                }
                out.push(SyncRecord {
                    model_id,
                    op: SyncOp::Upsert,
                    bytes: native_model::encode(&item)
                        .map_err(|e| anyhow::anyhow!("encode failed: {e}"))?,
                });
            }
            Ok(out)
        });

        let spawn_watch: WatchFn = Box::new(move |db, scope, tx, cancel| {
            let (mut channel, watch_id) = db.db().watch().scan().primary().all::<T>()?;
            let db = db.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        event = channel.recv() => match event {
                            Some(event) => {
                                if let Some(record) =
                                    event_to_record::<T>(event, model_id, scope, instance_of)
                                    && tx.send(record).await.is_err() {
                                        break;
                                    }
                            }
                            None => break,
                        }
                    }
                }
                let _ = db.db().unwatch(watch_id);
            });
            Ok(())
        });

        self.types.insert(
            model_id,
            SyncType {
                apply,
                snapshot,
                spawn_watch,
            },
        );
    }

    /// Apply a record to the local database in a transaction of its own.
    pub fn apply(&self, db: &RealmDatabase, record: &SyncRecord) -> Result<()> {
        // Replication is the one write a replica is entitled to.
        let rw = db.replica_write()?;
        self.apply_in(&rw, record)?;
        rw.commit()?;
        Ok(())
    }

    /// Apply a batch of records (a snapshot, typically) to the local database,
    /// returning the records that could not be applied and why.
    ///
    /// A commit costs several times what the write it commits does, so the
    /// batch goes into one transaction. Applying the records in order within it
    /// is equivalent to committing each in turn: later records see earlier ones,
    /// so the revision guard still decides between two copies of a row. If any
    /// record fails, that transaction is thrown away — a failed write may have
    /// left a row half-indexed — and the batch is replayed one transaction per
    /// record, so a bad record costs only itself.
    pub fn apply_all<'r>(
        &self,
        db: &RealmDatabase,
        records: &'r [SyncRecord],
    ) -> Vec<(&'r SyncRecord, anyhow::Error)> {
        if records.len() > 1 {
            let batch = db.replica_write().and_then(|rw| {
                for record in records {
                    self.apply_in(&rw, record)?;
                }
                rw.commit()?;
                Ok(())
            });
            match batch {
                Ok(()) => return Vec::new(),
                Err(e) => tracing::debug!(
                    error = %e,
                    "Batched sync apply failed; applying records one at a time"
                ),
            }
        }

        records
            .iter()
            .filter_map(|record| self.apply(db, record).err().map(|e| (record, e)))
            .collect()
    }

    fn apply_in(&self, rw: &RwTransaction, record: &SyncRecord) -> Result<()> {
        match self.types.get(&record.model_id) {
            Some(t) => (t.apply)(rw, record.op, &record.bytes),
            None => Ok(()), // unknown model — ignore
        }
    }

    /// Snapshot all records matching `filter` across the matching model(s).
    pub fn snapshot(&self, db: &RealmDatabase, filter: &SyncFilter) -> Result<Vec<SyncRecord>> {
        let mut out = Vec::new();
        for (id, t) in &self.types {
            if filter.model_id.is_some_and(|m| m != *id) {
                continue;
            }
            out.extend((t.snapshot)(db, filter.scope)?);
        }
        Ok(out)
    }

    /// Start watch tasks for every model matching `filter`, emitting changed
    /// records over `tx` until `cancel` fires.
    pub fn spawn_watch(
        &self,
        db: &RealmDatabase,
        filter: &SyncFilter,
        tx: Sender<SyncRecord>,
        cancel: CancellationToken,
    ) -> Result<()> {
        for (id, t) in &self.types {
            if filter.model_id.is_some_and(|m| m != *id) {
                continue;
            }
            (t.spawn_watch)(db, filter.scope, tx.clone(), cancel.clone())?;
        }
        Ok(())
    }
}

/// Subsystems register their syncable data types by submitting one of these via
/// `inventory::submit!`, mirroring the stream responder registration pattern.
///
/// ```ignore
/// inventory::submit! {
///     SyncRegistration(|r| r.register_scoped::<MyData>(|d| d._instance_id))
/// }
/// ```
pub struct SyncRegistration(pub fn(&mut SyncRegistry));
inventory::collect!(SyncRegistration);

/// The global registry of all syncable data types, assembled from every
/// `SyncRegistration` linked into the binary.
pub static SYNC: LazyLock<SyncRegistry> = LazyLock::new(|| {
    let mut registry = SyncRegistry::new();
    for registration in inventory::iter::<SyncRegistration> {
        (registration.0)(&mut registry);
    }
    registry
});

fn scope_matches<T>(
    scope: FilterScope,
    instance_of: Option<fn(&T) -> InstanceId>,
    item: &T,
) -> bool {
    match (scope, instance_of) {
        (FilterScope::All, _) => true,
        // Estate-wide data is exactly the models with no owning instance.
        (FilterScope::Global, None) => true,
        (FilterScope::Global, Some(_)) => false,
        (FilterScope::Instance(want), Some(get)) => get(item) == want,
        // Instance-scoped query against a type with no instance — no match.
        (FilterScope::Instance(_), None) => false,
    }
}

fn event_to_record<T>(
    event: Event,
    model_id: u32,
    scope: FilterScope,
    instance_of: Option<fn(&T) -> InstanceId>,
) -> Option<SyncRecord>
where
    T: Data + Model + 'static,
{
    let (op, item): (SyncOp, T) = match event {
        Event::Insert(d) => (SyncOp::Upsert, d.inner::<T>().ok()?),
        Event::Update(d) => (SyncOp::Upsert, d.inner_new::<T>().ok()?),
        Event::Delete(d) => (SyncOp::Delete, d.inner::<T>().ok()?),
    };
    if !scope_matches(scope, instance_of, &item) {
        return None;
    }
    Some(SyncRecord {
        model_id,
        op,
        bytes: native_model::encode(&item).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use crate::id::AgentId;
    use super::*;
    use crate::InstanceId;
    use crate::database::DatabaseManager;
    use crate::realm::RealmName;
    use crate::test_db;
    use anyhow::Result;
    use native_db::ToKey;
    use native_model::Model;
    use sandpolis_macros::data;
    use std::time::Duration;

    #[data(defaults)]
    struct SyncTestData {
        #[secondary_key]
        _instance_id: InstanceId,
        name: String,
        value: u32,
    }

    fn record(op: SyncOp, instance: InstanceId, name: &str, value: u32) -> SyncRecord {
        let item = SyncTestData {
            name: name.into(),
            value,
        ..SyncTestData::scoped(instance)
        };
        SyncRecord {
            model_id: <SyncTestData as Model>::native_model_id(),
            op,
            bytes: native_model::encode(&item).unwrap(),
        }
    }

    #[tokio::test]
    async fn registry_apply_snapshot_delete() -> Result<()> {
        let mut reg = SyncRegistry::new();
        reg.register_scoped::<SyncTestData>(|d| d._instance_id);

        let db: DatabaseManager = test_db!(SyncTestData);
        let realm = db.realm(RealmName::default())?;
        let a = InstanceId::from(crate::AgentId::random());
        let b = InstanceId::from(crate::AgentId::random());

        reg.apply(&realm, &record(SyncOp::Upsert, a, "x", 1))?;
        reg.apply(&realm, &record(SyncOp::Upsert, b, "y", 2))?;

        // Snapshot everything.
        assert_eq!(reg.snapshot(&realm, &SyncFilter::all())?.len(), 2);

        // Snapshot filtered to one instance.
        let only_a = reg.snapshot(&realm, &SyncFilter::instance(a))?;
        assert_eq!(only_a.len(), 1);

        // An instance-scoped model doesn't match a global-only filter.
        assert_eq!(reg.snapshot(&realm, &SyncFilter::global())?.len(), 0);

        // An instance filter against the wrong model id matches nothing.
        let wrong_model = reg.snapshot(
            &realm,
            &SyncFilter {
                model_id: Some(0xDEAD),
                scope: FilterScope::All,
            },
        )?;
        assert_eq!(wrong_model.len(), 0);

        // Delete a's record by replaying its bytes as a Delete.
        let del = SyncRecord {
            op: SyncOp::Delete,
            ..only_a[0].clone()
        };
        reg.apply(&realm, &del)?;
        assert_eq!(reg.snapshot(&realm, &SyncFilter::all())?.len(), 1);

        Ok(())
    }

    /// A global-only filter matches unscoped models and nothing else, so a local
    /// stratum server can replicate estate-wide data without pulling every
    /// instance's records along with it.
    #[tokio::test]
    async fn global_filter_matches_unscoped_models_only() -> Result<()> {
        #[data]
        #[derive(Default)]
        struct GlobalTestData {
            name: String,
        }

        let mut reg = SyncRegistry::new();
        reg.register_scoped::<SyncTestData>(|d| d._instance_id);
        reg.register::<GlobalTestData>();

        let db: DatabaseManager = test_db!(SyncTestData, GlobalTestData);
        let realm = db.realm(RealmName::default())?;

        let instance = InstanceId::from(crate::AgentId::random());
        reg.apply(&realm, &record(SyncOp::Upsert, instance, "x", 1))?;
        let global_item = GlobalTestData {
            name: "g".into(),
            ..Default::default()
        };
        reg.apply(
            &realm,
            &SyncRecord {
                model_id: <GlobalTestData as Model>::native_model_id(),
                op: SyncOp::Upsert,
                bytes: native_model::encode(&global_item).unwrap(),
            },
        )?;

        let global = reg.snapshot(&realm, &SyncFilter::global())?;
        assert_eq!(global.len(), 1);
        assert_eq!(
            global[0].model_id,
            <GlobalTestData as Model>::native_model_id()
        );

        // The unscoped model never matches an instance filter, not even the one
        // whose scoped records are sitting right there.
        let by_instance = reg.snapshot(&realm, &SyncFilter::instance(instance))?;
        assert_eq!(by_instance.len(), 1);
        assert_eq!(
            by_instance[0].model_id,
            <SyncTestData as Model>::native_model_id()
        );

        assert_eq!(reg.snapshot(&realm, &SyncFilter::all())?.len(), 2);
        Ok(())
    }

    /// An older or replayed record must never clobber a newer one: with exactly
    /// one writer per record, `_revision` is totally ordered per row and arrival
    /// order is not.
    #[tokio::test]
    async fn apply_rejects_stale_revisions() -> Result<()> {
        use crate::database::{Data, DataRevision};

        let mut reg = SyncRegistry::new();
        reg.register_scoped::<SyncTestData>(|d| d._instance_id);

        let db: DatabaseManager = test_db!(SyncTestData);
        let realm = db.realm(RealmName::default())?;

        let mut item = SyncTestData {
            name: "x".into(),
            value: 1,
            ..SyncTestData::scoped(AgentId::random().into())
        };
        item.set_revision(DataRevision::Latest(2));

        let encode = |item: &SyncTestData| SyncRecord {
            model_id: <SyncTestData as Model>::native_model_id(),
            op: SyncOp::Upsert,
            bytes: native_model::encode(item).unwrap(),
        };

        reg.apply(&realm, &encode(&item))?;

        // A stale revision of the same row arrives late: dropped.
        let mut stale = item.clone();
        stale.set_revision(DataRevision::Latest(1));
        stale.value = 99;
        reg.apply(&realm, &encode(&stale))?;

        let snapshot = reg.snapshot(&realm, &SyncFilter::all())?;
        assert_eq!(snapshot.len(), 1);
        let (stored, _): (SyncTestData, u32) =
            native_model::decode(snapshot[0].bytes.clone()).unwrap();
        assert_eq!(stored.value, 1);

        // An equal revision is an idempotent replay: also dropped.
        let mut replay = item.clone();
        replay.value = 50;
        reg.apply(&realm, &encode(&replay))?;
        let snapshot = reg.snapshot(&realm, &SyncFilter::all())?;
        let (stored, _): (SyncTestData, u32) =
            native_model::decode(snapshot[0].bytes.clone()).unwrap();
        assert_eq!(stored.value, 1);

        // A newer revision goes through.
        let mut newer = item.clone();
        newer.set_revision(DataRevision::Latest(3));
        newer.value = 3;
        reg.apply(&realm, &encode(&newer))?;
        let snapshot = reg.snapshot(&realm, &SyncFilter::all())?;
        let (stored, _): (SyncTestData, u32) =
            native_model::decode(snapshot[0].bytes.clone()).unwrap();
        assert_eq!(stored.value, 3);

        Ok(())
    }

    /// A batch lands in one transaction but must behave like the records were
    /// applied one by one: the revision guard sees rows written earlier in the
    /// same batch, and a record that fails costs only itself.
    #[tokio::test]
    async fn apply_all_matches_one_at_a_time() -> Result<()> {
        use crate::database::{Data, DataRevision};

        let mut reg = SyncRegistry::new();
        reg.register_scoped::<SyncTestData>(|d| d._instance_id);

        let db: DatabaseManager = test_db!(SyncTestData);
        let realm = db.realm(RealmName::default())?;

        let encode = |item: &SyncTestData| SyncRecord {
            model_id: <SyncTestData as Model>::native_model_id(),
            op: SyncOp::Upsert,
            bytes: native_model::encode(item).unwrap(),
        };
        let stored = |name: &str| -> Result<Option<u32>> {
            Ok(reg
                .snapshot(&realm, &SyncFilter::all())?
                .into_iter()
                .map(|r| native_model::decode::<SyncTestData>(r.bytes).unwrap().0)
                .find(|d| d.name == name)
                .map(|d| d.value))
        };

        let mut item = SyncTestData {
            name: "x".into(),
            value: 2,
            ..SyncTestData::scoped(AgentId::random().into())
        };
        item.set_revision(DataRevision::Latest(2));
        let mut stale = item.clone();
        stale.set_revision(DataRevision::Latest(1));
        stale.value = 1;
        let other = SyncTestData {
            name: "y".into(),
            value: 7,
            ..SyncTestData::scoped(AgentId::random().into())
        };

        // The stale copy arrives after the newer one within one batch, so the
        // guard has to see the newer row before the batch commits.
        let batch = [encode(&item), encode(&stale), encode(&other)];
        assert!(reg.apply_all(&realm, &batch).is_empty());
        assert_eq!(stored("x")?, Some(2));
        assert_eq!(stored("y")?, Some(7));

        // One undecodable record sinks the shared transaction, but replaying
        // the batch record by record still lands everything else.
        let mut newer = item.clone();
        newer.set_revision(DataRevision::Latest(3));
        newer.value = 3;
        // A valid model header over a truncated body.
        let mut garbage = encode(&other);
        garbage.bytes.truncate(10);
        let batch = [garbage, encode(&newer)];
        let failed = reg.apply_all(&realm, &batch);
        assert_eq!(failed.len(), 1);
        assert!(std::ptr::eq(failed[0].0, &batch[0]));
        assert_eq!(stored("x")?, Some(3));

        Ok(())
    }

    #[tokio::test]
    async fn registry_watch_emits_changes() -> Result<()> {
        let mut reg = SyncRegistry::new();
        reg.register_scoped::<SyncTestData>(|d| d._instance_id);

        let db: DatabaseManager = test_db!(SyncTestData);
        let realm = db.realm(RealmName::default())?;

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let cancel = CancellationToken::new();
        reg.spawn_watch(&realm, &SyncFilter::all(), tx, cancel.clone())?;

        // Let the watch register before mutating.
        tokio::time::sleep(Duration::from_millis(50)).await;
        reg.apply(
            &realm,
            &record(SyncOp::Upsert, AgentId::random().into(), "z", 9),
        )?;

        let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await?
            .expect("watch emitted a record");
        assert_eq!(got.op, SyncOp::Upsert);

        cancel.cancel();
        Ok(())
    }
}
