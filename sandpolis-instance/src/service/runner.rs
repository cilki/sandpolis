//! The runner that schedules services and records what they do.

use super::{
    ErasedService, Service, ServiceData, ServiceReport, ServiceSchedule, ServiceState, service_key,
};
use crate::InstanceId;
use crate::database::{DataScope, RealmDatabase};
use anyhow::{Result, bail};
use chrono::Utc;
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use tokio::sync::Notify;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Every service running in this process.
///
/// Held in a static so the control-stream responder can stay a unit struct and
/// register through the stateless `inventory` path. It accumulates rather than
/// being claimed by whoever starts first, so a process that builds more than one
/// runner (a test harness, say) still reports every service.
static RUNNING: LazyLock<ServiceHandle> = LazyLock::new(ServiceHandle::default);

/// The services running in this process.
pub fn handle() -> &'static ServiceHandle {
    &RUNNING
}

/// Collects services and puts them on their schedules.
pub struct ServiceRunner {
    realm: RealmDatabase,
    instance_id: InstanceId,
    services: Vec<Box<dyn ErasedService>>,
}

impl ServiceRunner {
    /// Create a runner for services hosted by `instance_id`, storing their state
    /// in `realm`.
    pub fn new(realm: RealmDatabase, instance_id: InstanceId) -> Self {
        Self {
            realm,
            instance_id,
            services: Vec::new(),
        }
    }

    /// Add a service. Subsystems call this from their own registration entry point.
    pub fn register(&mut self, service: impl Service) -> &mut Self {
        self.services.push(Box::new(service));
        self
    }

    /// Reconcile every registered service against the database, then supervise
    /// the ones that are enabled.
    ///
    /// Each service gets an independent tokio task, so a slow or wedged one
    /// can't hold up the others.
    pub fn start(self) -> Result<ServiceHandle> {
        if self.services.is_empty() {
            debug!("No services registered");
        }

        for service in self.services {
            let key = service_key(&service.layer(), service.name());
            if RUNNING.contains(&key) {
                warn!(service = %key, "Ignoring duplicate service registration");
                continue;
            }

            let supervisor = Arc::new(Supervisor {
                service: Arc::from(service),
                realm: self.realm.clone(),
                instance_id: self.instance_id,
                key: key.clone(),
                running: Mutex::new(None),
                notify: Notify::new(),
                last_error: Mutex::new(None),
            });

            if supervisor.reconcile()? {
                supervisor.start();
            } else {
                debug!(service = %key, "Service is disabled");
            }
            RUNNING.insert(key, supervisor);
        }

        Ok(RUNNING.clone())
    }
}

/// Control surface over the services running in this process.
#[derive(Clone, Default)]
pub struct ServiceHandle {
    supervisors: Arc<RwLock<BTreeMap<String, Arc<Supervisor>>>>,
}

impl ServiceHandle {
    /// Enable or disable a service, persisting the choice.
    pub fn set_enabled(&self, key: &str, enabled: bool) -> Result<()> {
        self.get(key)?.set_enabled(enabled)
    }

    /// Ask a periodic service to run its next pass immediately. Does nothing for
    /// a continuous service, which is already running.
    pub fn run_now(&self, key: &str) -> Result<()> {
        let supervisor = self.get(key)?;
        if supervisor.running.lock().unwrap().is_none() {
            bail!("Service {key} is disabled");
        }
        supervisor.notify.notify_one();
        Ok(())
    }

    /// The keys of every service running here.
    pub fn keys(&self) -> Vec<String> {
        self.supervisors.read().unwrap().keys().cloned().collect()
    }

    fn contains(&self, key: &str) -> bool {
        self.supervisors.read().unwrap().contains_key(key)
    }

    fn insert(&self, key: String, supervisor: Arc<Supervisor>) {
        self.supervisors.write().unwrap().insert(key, supervisor);
    }

    fn get(&self, key: &str) -> Result<Arc<Supervisor>> {
        match self.supervisors.read().unwrap().get(key) {
            Some(supervisor) => Ok(supervisor.clone()),
            None => bail!("No such service: {key}"),
        }
    }
}

/// Owns one service's schedule and its cancellation token.
struct Supervisor {
    service: Arc<dyn ErasedService>,
    realm: RealmDatabase,
    instance_id: InstanceId,
    key: String,

    /// Live while the service is scheduled. Taken and cancelled to stop it.
    running: Mutex<Option<CancellationToken>>,

    /// Wakes a periodic service's next pass early.
    notify: Notify,

    /// The previous pass's error, so a service failing the same way every pass
    /// (a display enumerator on a headless host, say) warns once instead of
    /// once per pass. The row's `last_error` still records every failure.
    last_error: Mutex<Option<String>>,
}

impl Supervisor {
    /// Bring the service's row in line with the binary and report whether it
    /// should be running.
    ///
    /// Descriptive fields follow the binary, which may have changed across a
    /// restart; `enabled` follows the database, so a toggle from a client isn't
    /// undone by a restart.
    fn reconcile(&self) -> Result<bool> {
        let layer = self.service.layer().0;
        let name = self.service.name().to_string();
        let description = self.service.description().to_string();
        let schedule = self.service.schedule().describe();

        let row = self.update(|row, is_new| {
            if is_new {
                row.enabled = true;
            }
            row.layer = layer;
            row.name = name;
            row.description = description;
            row.schedule = schedule;
            // Nothing is running yet; `start` flips this as supervisors come up.
            row.state = ServiceState::Stopped;
        })?;

        Ok(row.enabled)
    }

    /// Put the service on its schedule. No-op if it's already running.
    fn start(self: &Arc<Self>) {
        let cancel = {
            let mut running = self.running.lock().unwrap();
            if running.is_some() {
                return;
            }
            let cancel = CancellationToken::new();
            *running = Some(cancel.clone());
            cancel
        };

        info!(
            service = %self.key,
            schedule = %self.service.schedule().describe(),
            "Starting service"
        );
        self.set_state(ServiceState::Idle);

        let this = self.clone();
        tokio::spawn(async move { this.supervise(cancel).await });
    }

    /// Take the service off its schedule. No-op if it isn't running.
    ///
    /// The stopped state is written here rather than by the supervising task,
    /// which only ever exits through this path anyway — that keeps a rapid
    /// disable/enable from racing a departing task into the row.
    fn stop(&self) {
        let cancel = self.running.lock().unwrap().take();
        if let Some(cancel) = cancel {
            info!(service = %self.key, "Stopping service");
            cancel.cancel();
            self.set_state(ServiceState::Stopped);
        }
    }

    fn set_enabled(self: &Arc<Self>, enabled: bool) -> Result<()> {
        // Persist the desire first, so it holds even if the process dies here.
        self.update(|row, _| row.enabled = enabled)?;
        if enabled {
            self.start();
        } else {
            self.stop();
        }
        Ok(())
    }

    /// Drive the service until it's cancelled.
    async fn supervise(self: Arc<Self>, cancel: CancellationToken) {
        match self.service.schedule() {
            ServiceSchedule::Periodic {
                interval,
                run_at_startup,
            } => {
                if !run_at_startup {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(interval) => {}
                        _ = self.notify.notified() => {}
                    }
                }

                // Built after the optional initial sleep because a tokio
                // interval's first tick completes immediately, which is what we
                // want in both cases.
                let mut ticker = tokio::time::interval(interval);
                // A pass that overruns its interval shouldn't cause a burst of
                // catch-up passes against the targets we just finished hammering.
                ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

                loop {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        _ = ticker.tick() => {}
                        _ = self.notify.notified() => {}
                    }

                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        _ = self.pass(&cancel) => {}
                    }
                }
            }

            ServiceSchedule::Continuous { restart_backoff } => loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = self.pass(&cancel) => {}
                }

                // `run` returning means the service fell over; give it a moment
                // rather than spinning on whatever made it stop.
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(restart_backoff) => {}
                }
            },
        }
    }

    /// One invocation of the service, plus its bookkeeping.
    async fn pass(&self, cancel: &CancellationToken) {
        let started = Utc::now().timestamp_millis();
        self.set_state(ServiceState::Running);

        let result = self.service.run(cancel.clone()).await;
        match &result {
            Ok(report) => {
                if self.last_error.lock().unwrap().take().is_some() {
                    info!(service = %self.key, "Service pass succeeded again");
                }
                debug!(
                    service = %self.key,
                    scanned = report.scanned,
                    updated = report.updated,
                    failed = report.failed,
                    "Service pass finished"
                )
            }
            Err(e) => {
                let error = e.to_string();
                let mut last_error = self.last_error.lock().unwrap();
                if last_error.as_deref() == Some(&error) {
                    debug!(service = %self.key, error = %error, "Service pass failed the same way");
                } else {
                    warn!(service = %self.key, error = %error, "Service pass failed");
                    *last_error = Some(error);
                }
            }
        }

        // Bookkeeping is best-effort: losing a run record is not a reason to
        // stop the service.
        if let Err(e) = self.record(started, &result, cancel.is_cancelled()) {
            debug!(service = %self.key, error = %e, "Failed to record service pass");
        }
    }

    /// Fold one pass's outcome into the service's row.
    ///
    /// A pass that finished as the service was being cancelled still counts, but
    /// leaves the state alone — [`stop`](Self::stop) already claimed it.
    fn record(&self, started: i64, result: &Result<ServiceReport>, cancelled: bool) -> Result<()> {
        self.update(|row, _| {
            row.runs += 1;
            row.last_run = Some(started);
            match result {
                Ok(report) => {
                    row.items_updated += report.updated;
                    row.last_failed_items = report.failed;
                    row.last_success = Some(Utc::now().timestamp_millis());
                    row.last_error = None;
                    if !cancelled {
                        row.state = ServiceState::Idle;
                    }
                }
                Err(e) => {
                    row.failures += 1;
                    row.last_error = Some(e.to_string());
                    if !cancelled {
                        row.state = ServiceState::Failed;
                    }
                }
            }
        })
        .map(|_| ())
    }

    fn set_state(&self, state: ServiceState) {
        if let Err(e) = self.update(|row, _| row.state = state) {
            debug!(service = %self.key, error = %e, "Failed to record service state");
        }
    }

    fn update(&self, f: impl FnOnce(&mut ServiceData, bool)) -> Result<ServiceData> {
        update(&self.realm, self.instance_id, &self.key, f)
    }
}

/// Read-modify-write this instance's row for `key`, creating it if absent.
///
/// `f` is told whether the row is new, so seeding can set defaults that a
/// subsequent reconcile must not clobber. The scan is over every row rather than
/// the `key` index because a server's database also holds its agents' rows,
/// which share keys; identity here is `(_instance_id, key)`.
fn update(
    realm: &RealmDatabase,
    instance_id: InstanceId,
    key: &str,
    f: impl FnOnce(&mut ServiceData, bool),
) -> Result<ServiceData> {
    // A runner only ever books its own instance's rows, so this is the self
    // scope — writable even on a local stratum server.
    let rw = realm.write(DataScope::Instance(instance_id))?;

    let rows: Vec<ServiceData> = rw
        .scan()
        .primary::<ServiceData>()?
        .all()?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let previous = rows
        .into_iter()
        .find(|row| row._instance_id == instance_id && row.key == key);

    // Cloning the previous row keeps its `_id`, so this replaces rather than
    // duplicates.
    let mut row = previous.clone().unwrap_or_else(|| ServiceData {
        key: key.to_string(),
        ..ServiceData::scoped(instance_id)
    });
    f(&mut row, previous.is_none());

    match previous {
        Some(previous) => {
            if previous != row {
                rw.upsert(row.clone())?;
            }
        }
        None => rw.insert(row.clone())?,
    }

    rw.commit()?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::ServerId;
    use crate::LayerName;
    use crate::database::DatabaseManager;
    use crate::realm::RealmName;
    use crate::test_db;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    fn realm() -> Result<RealmDatabase> {
        let db: DatabaseManager = test_db!(ServiceData);
        db.realm(RealmName::default())
    }

    fn row(realm: &RealmDatabase, instance: InstanceId, key: &str) -> Result<ServiceData> {
        let r = realm.r_transaction()?;
        let all: Vec<ServiceData> = r
            .scan()
            .primary::<ServiceData>()?
            .all()?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(all
            .into_iter()
            .find(|d| d._instance_id == instance && d.key == key)
            .expect("row exists"))
    }

    /// A service that counts its passes and honours cancellation.
    struct Counter {
        passes: Arc<AtomicU64>,
        schedule: ServiceSchedule,
    }

    impl Service for Counter {
        fn name(&self) -> &'static str {
            "counter"
        }
        fn layer(&self) -> LayerName {
            LayerName::from("Test")
        }
        fn description(&self) -> &'static str {
            "Counts its passes"
        }
        fn schedule(&self) -> ServiceSchedule {
            self.schedule
        }
        async fn run(&self, cancel: CancellationToken) -> Result<ServiceReport> {
            self.passes.fetch_add(1, Ordering::SeqCst);
            cancel.cancelled().await;
            Ok(ServiceReport::default())
        }
    }

    #[test]
    fn passes_accumulate_into_one_row() -> Result<()> {
        let realm = realm()?;
        let instance = InstanceId::from(ServerId::random());
        let key = "Account/favicon";

        let record = |started, result: &Result<ServiceReport>| {
            update(&realm, instance, key, |row, _| {
                row.runs += 1;
                row.last_run = Some(started);
                match result {
                    Ok(report) => {
                        row.items_updated += report.updated;
                        row.last_failed_items = report.failed;
                        row.last_success = Some(Utc::now().timestamp_millis());
                        row.last_error = None;
                    }
                    Err(e) => {
                        row.failures += 1;
                        row.last_error = Some(e.to_string());
                    }
                }
            })
        };

        record(
            1,
            &Ok(ServiceReport {
                scanned: 3,
                updated: 2,
                failed: 1,
            }),
        )?;
        let first = row(&realm, instance, key)?;
        assert_eq!(first.runs, 1);
        assert_eq!(first.items_updated, 2);
        assert_eq!(first.last_failed_items, 1);
        assert_eq!(first.last_run, Some(1));
        assert!(first.last_success.is_some());
        assert_eq!(first.last_error, None);

        // A failing pass counts as a run and a failure, and records why.
        record(2, &Err(anyhow::anyhow!("boom")))?;
        let second = row(&realm, instance, key)?;
        assert_eq!(second.runs, 2);
        assert_eq!(second.failures, 1);
        // Totals carry over rather than resetting.
        assert_eq!(second.items_updated, 2);
        assert_eq!(second.last_run, Some(2));
        assert_eq!(second.last_success, first.last_success);
        assert_eq!(second.last_error.as_deref(), Some("boom"));

        // The next success clears the error.
        record(
            3,
            &Ok(ServiceReport {
                scanned: 1,
                updated: 1,
                failed: 0,
            }),
        )?;
        let third = row(&realm, instance, key)?;
        assert_eq!(third.runs, 3);
        assert_eq!(third.failures, 1);
        assert_eq!(third.items_updated, 3);
        assert_eq!(third.last_failed_items, 0);
        assert_eq!(third.last_error, None);
        Ok(())
    }

    /// Two instances running the same service keep separate rows, since a
    /// server's database holds its agents' rows alongside its own.
    #[test]
    fn rows_are_per_instance() -> Result<()> {
        let realm = realm()?;
        let key = "Health/systemd";
        let a = InstanceId::from(crate::AgentId::random());
        let b = InstanceId::from(crate::AgentId::random());

        update(&realm, a, key, |row, _| row.runs = 5)?;
        update(&realm, b, key, |row, _| row.runs = 9)?;

        assert_eq!(row(&realm, a, key)?.runs, 5);
        assert_eq!(row(&realm, b, key)?.runs, 9);
        Ok(())
    }

    /// A service switched off stays off across a restart: reconcile refreshes
    /// the descriptive fields but never the stored `enabled`.
    #[test]
    fn reconcile_preserves_stored_enabled() -> Result<()> {
        let realm = realm()?;
        let instance = InstanceId::from(ServerId::random());
        let key = "Test/counter";

        let supervisor = |passes: Arc<AtomicU64>| {
            Arc::new(Supervisor {
                service: Arc::new(Counter {
                    passes,
                    schedule: ServiceSchedule::every(Duration::from_secs(60)),
                }),
                realm: realm.clone(),
                instance_id: instance,
                key: key.to_string(),
                running: Mutex::new(None),
                notify: Notify::new(),
                last_error: Mutex::new(None),
            })
        };

        // First sight of the service: enabled by default.
        assert!(supervisor(Arc::new(AtomicU64::new(0))).reconcile()?);

        // Someone disables it.
        update(&realm, instance, key, |row, _| row.enabled = false)?;

        // A restart re-reconciles and must leave it disabled.
        assert!(!supervisor(Arc::new(AtomicU64::new(0))).reconcile()?);
        let stored = row(&realm, instance, key)?;
        assert!(!stored.enabled);
        assert_eq!(stored.description, "Counts its passes");
        assert_eq!(stored.schedule, "every 1m");
        Ok(())
    }

    /// Disabling a running service cancels it promptly, even though its `run`
    /// only returns when cancelled.
    #[tokio::test]
    async fn disabling_stops_the_service() -> Result<()> {
        let realm = realm()?;
        let instance = InstanceId::from(ServerId::random());
        let passes = Arc::new(AtomicU64::new(0));

        let mut runner = ServiceRunner::new(realm.clone(), instance);
        runner.register(Counter {
            passes: passes.clone(),
            schedule: ServiceSchedule::every(Duration::from_millis(10)),
        });
        let handle = runner.start()?;
        let key = "Test/counter";

        // The first pass starts immediately.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(passes.load(Ordering::SeqCst), 1);
        assert!(row(&realm, instance, key)?.enabled);

        handle.set_enabled(key, false)?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stopped = row(&realm, instance, key)?;
        assert!(!stopped.enabled);
        assert_eq!(stopped.state, ServiceState::Stopped);

        // No further passes once it's off.
        let after = passes.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(passes.load(Ordering::SeqCst), after);

        // Turning it back on resumes the schedule.
        handle.set_enabled(key, true)?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(passes.load(Ordering::SeqCst) > after);
        assert!(row(&realm, instance, key)?.enabled);
        Ok(())
    }
}
