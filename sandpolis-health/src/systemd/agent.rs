use super::{ActiveState, SystemdUnitData};
use anyhow::Result;
use sandpolis_agent::Collector;
use sandpolis_instance::InstanceId;
use sandpolis_instance::database::{RealmDatabase, ResidentVec};
use sandpolis_instance::notification::{Notification, notify};
use std::str::FromStr;
use tracing::trace;
use zbus::Connection;

/// A single entry returned by the systemd manager's `ListUnits` method.
///
/// Maps the D-Bus signature `(ssssssouso)`.
#[derive(Debug, serde::Deserialize, zbus::zvariant::Type)]
pub struct UnitListEntry {
    pub name: String,
    pub description: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    pub followed: String,
    pub object_path: zbus::OwnedObjectPath,
    pub job_id: u32,
    pub job_type: String,
    pub job_object_path: zbus::OwnedObjectPath,
}

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
pub trait Manager {
    /// Return a list of all currently loaded units.
    fn list_units(&self) -> zbus::Result<Vec<UnitListEntry>>;
}

pub struct SystemdCollector {
    data: ResidentVec<SystemdUnitData>,
    connection: Option<Connection>,
    instance_id: InstanceId,
}

impl SystemdCollector {
    pub fn new(db: RealmDatabase, instance_id: InstanceId) -> Result<Self> {
        Ok(Self {
            data: db.resident_vec(())?,
            connection: None,
            instance_id,
        })
    }

    /// Snapshot the currently known units for uploading to a server.
    pub fn units(&self) -> Vec<SystemdUnitData> {
        self.data.iter().map(|r| r.read().clone()).collect()
    }

    /// Tell the user a unit has failed.
    ///
    /// The notification is raised against this agent, so it replicates up to
    /// whichever server owns it and on to any connected client.
    fn notify_failed(&self, unit: &UnitListEntry) {
        let mut notification =
            Notification::error("Health", format!("{} failed", unit.name)).about(self.instance_id);

        // The description reads better than the unit name on its own, but
        // systemd leaves it empty often enough to be worth checking.
        if !unit.description.is_empty() {
            notification = notification.body(unit.description.clone());
        }

        notify(notification);
    }

    /// Connect to the system bus on first use and reuse the connection after.
    async fn connection(&mut self) -> Result<&Connection> {
        if self.connection.is_none() {
            self.connection = Some(Connection::system().await?);
        }
        Ok(self.connection.as_ref().unwrap())
    }
}

impl Collector for SystemdCollector {
    async fn refresh(&mut self) -> Result<()> {
        let connection = self.connection().await?.clone();
        let manager = ManagerProxy::new(&connection).await?;
        let units = manager.list_units().await?;
        trace!(count = units.len(), "Polled systemd units");

        // Update or add any units reported by systemd
        'next_unit: for unit in &units {
            let reported = ActiveState::from_str(&unit.active_state)
                .unwrap_or(ActiveState::Unknown(unit.active_state.clone()));

            for resident in self.data.iter() {
                if resident.read().name == unit.name {
                    // Read the old state before the update rather than from
                    // inside it: `Resident::update` takes an `Fn`, so the
                    // closure can't hand anything back.
                    let was_failed = resident.read().active_state == ActiveState::Failed;

                    resident.update(|u| {
                        u.description = Some(unit.description.clone());
                        u.load_state = Some(unit.load_state.clone());
                        u.active_state = reported.clone();
                        u.sub_state = Some(unit.sub_state.clone());
                        Ok(())
                    });

                    // Only on the edge into `failed`. A unit that stays failed
                    // would otherwise re-notify on every poll.
                    if !was_failed && reported == ActiveState::Failed {
                        self.notify_failed(unit);
                    }
                    continue 'next_unit;
                }
            }

            // A unit already failed the first time we see it is worth saying
            // once — the agent may have started after it broke.
            if reported == ActiveState::Failed {
                self.notify_failed(unit);
            }

            self.data
                .push(SystemdUnitData::collected(unit, self.instance_id))?;
        }

        Ok(())
    }
}

impl SystemdUnitData {
    /// A row for a listed unit, scoped to the collecting instance.
    fn collected(value: &UnitListEntry, instance: InstanceId) -> Self {
        Self {
            name: value.name.clone(),
            description: Some(value.description.clone()),
            load_state: Some(value.load_state.clone()),
            active_state: ActiveState::from_str(&value.active_state)
                .unwrap_or(ActiveState::Unknown(value.active_state.clone())),
            sub_state: Some(value.sub_state.clone()),
            ..SystemdUnitData::scoped(instance)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandpolis_instance::database::DatabaseManager;
    use sandpolis_instance::realm::RealmName;
    use sandpolis_instance::test_db;

    #[tokio::test]
    #[test_log::test]
    #[ignore = "requires a running systemd / system D-Bus"]
    async fn test_systemd_collector() -> Result<()> {
        let database: DatabaseManager = test_db!(SystemdUnitData);

        let mut collector =
            SystemdCollector::new(
            database.realm(RealmName::default())?,
            sandpolis_instance::AgentId::random().into(),
        )?;
        collector.refresh().await?;

        assert!(!collector.data.iter().next().is_none());
        Ok(())
    }
}
