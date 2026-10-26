//! The audit subsystem watches host activity logs for noteworthy events.
//!
//! On Linux agents, a continuous background service tails the auditd log,
//! runs every record through the detection rules in [`rules`], stores the
//! matches as [`AuditEventData`] rows, and raises a notification for the
//! serious ones. The rows replicate to the owning server like any other
//! instance-scoped data, so nothing here needs a protocol of its own.

use anyhow::Result;
use native_db::*;
use native_model::Model;
use sandpolis_instance::InstanceId;
use sandpolis_instance::InstanceManager;
use sandpolis_instance::database::{DatabaseManager, Resident};
use sandpolis_instance::notification::Severity;
use sandpolis_instance::realm::RealmName;
use sandpolis_macros::data;

pub mod rules;

#[cfg(feature = "agent")]
pub mod agent;

sandpolis_instance::register_layer!("Audit", "Host activity auditing and detection rules");

#[data]
#[derive(Default)]
pub struct AuditManagerData {
    /// Ingestion cursor: wall-clock milliseconds of the last audit record the
    /// agent has seen, so a restart doesn't re-ingest the same log lines.
    pub last_event_timestamp: Option<u64>,
    /// Sequence half of the ingestion cursor.
    pub last_event_sequence: Option<u32>,
}

/// One record from the host's audit log that matched a detection rule.
///
/// `timestamp` and `sequence` together reconstruct the kernel's event id, so
/// records can later be grouped into multi-record events without a schema
/// change.
#[data]
pub struct AuditEventData {
    #[secondary_key]
    pub _instance_id: InstanceId,

    /// Wall-clock milliseconds of the kernel's event id.
    #[secondary_key]
    pub timestamp: u64,

    /// Sequence number of the kernel's event id.
    pub sequence: u32,

    /// Symbolic record type, e.g. "USER_LOGIN".
    #[secondary_key]
    pub record_type: String,

    pub severity: Severity,

    /// The matched rule's label, e.g. "Failed login attempt".
    pub label: String,

    /// The record's own outcome (`res=`/`success=`), when it carries one.
    pub success: Option<bool>,

    pub uid: Option<u32>,

    /// Login uid — the human behind the action, surviving su/sudo.
    pub auid: Option<u32>,

    pub session: Option<u32>,
    pub pid: Option<u32>,
    pub exe: Option<String>,
    pub comm: Option<String>,

    /// The account the record is about, e.g. the name a login was attempted
    /// for.
    pub acct: Option<String>,

    pub terminal: Option<String>,
    pub addr: Option<String>,
    pub hostname: Option<String>,

    /// The audit rule key (`key=` field), if the record came from a loaded
    /// audit rule.
    pub key: Option<String>,

    /// The raw log line, truncated, for forensics.
    pub raw: String,
}

inventory::submit! {
    sandpolis_instance::database::sync::SyncRegistration(
        |r| r.register_scoped::<AuditEventData>(|d| d._instance_id))
}

#[derive(Clone)]
pub struct AuditManager {
    #[allow(dead_code)]
    data: Resident<AuditManagerData>,
    pub instance_id: InstanceId,

    /// Agent-side auditd log ingestion service.
    #[cfg(feature = "agent")]
    pub auditd: agent::AuditdService,
}

impl AuditManager {
    pub async fn new(database: DatabaseManager, instance: InstanceManager) -> Result<Self> {
        let realm = database.realm(RealmName::default())?;
        Ok(Self {
            #[cfg(feature = "agent")]
            auditd: agent::AuditdService::new(realm.clone(), instance.instance_id)?,
            data: realm.resident(())?,
            instance_id: instance.instance_id,
        })
    }

    /// Add the subsystem's background services to the agent's runner.
    #[cfg(feature = "agent")]
    pub fn register_services(&self, runner: &mut sandpolis_instance::service::ServiceRunner) {
        // Only Linux has auditd; on anything else there is nothing to run.
        #[cfg(target_os = "linux")]
        runner.register(self.auditd.clone());
        #[cfg(not(target_os = "linux"))]
        let _ = runner;
    }
}

/// Indicates the degree to which the user is currently participating in their
/// computing experience. We can vary auditing behavior as a result.
pub enum UserPresence {
    /// The user is actively using the machine. They might be making various
    /// changes and system load will vary. We should reduce auditing
    /// activity to avoid performance impacts.
    Active,

    /// The user has not interacted with the system in a sufficiently long time,
    /// but they could come back soon. We can increase auditing
    /// activity since the user won't notice.
    Idle,

    /// The user is intentionally "away" and thus any user activity is
    /// automatically considered suspicious.
    Away,
}

pub enum DeviceClass {
    Workstation,
    Server,
    Embedded,
}
