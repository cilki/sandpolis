use super::{UserData, UserDataKey};
use anyhow::Result;
use sandpolis_agent::Collector;
use sandpolis_instance::InstanceId;
use sandpolis_instance::database::{Data, DataCondition, RealmDatabase, ResidentVec};
use sysinfo::{User, Users};
use tracing::trace;

pub struct UserCollector {
    data: ResidentVec<UserData>,
    users: Users,
    instance_id: InstanceId,
}

impl UserCollector {
    pub fn new(db: RealmDatabase, instance_id: InstanceId) -> Result<Self> {
        Ok(Self {
            users: Users::new()?,
            data: db.resident_vec(DataCondition::equal(UserDataKey::_instance_id, instance_id))?,
            instance_id,
        })
    }
}

impl Collector for UserCollector {
    async fn refresh(&mut self) -> Result<()> {
        self.users.refresh();
        trace!(info = ?self.users, "Polled user info");

        // Update or add any new users
        'next_user: for user in &self.users {
            for resident_user in self.data.iter() {
                if resident_user.read().uid == **user.id() {
                    resident_user.update(|u| {
                        u.gid = *user.group_id();
                        u.username = Some(user.name().to_string());
                        Ok(())
                    })?;
                    continue 'next_user;
                }
            }
            self.data
                .push(UserData::collected(user, self.instance_id))?;
        }

        // Remove residents whose uid is no longer present on the system.
        let live_uids: Vec<u32> = self.users.iter().map(|u| **u.id()).collect();
        let stale: Vec<_> = self
            .data
            .iter()
            .filter(|r| !live_uids.contains(&r.read().uid))
            .map(|r| r.read().id())
            .collect();
        for id in stale {
            self.data.remove(id)?;
        }

        Ok(())
    }
}

impl UserData {
    /// A row for a local account, scoped to the collecting instance.
    fn collected(value: &User, instance: InstanceId) -> Self {
        Self {
            uid: **value.id(),
            gid: *value.group_id(),
            username: Some(value.name().to_string()),
            ..UserData::scoped(instance)
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
    async fn test_user_collector() -> Result<()> {
        let database: DatabaseManager = test_db!(UserData);

        let instance_id = sandpolis_instance::ServerId::random().into();
        let mut collector = UserCollector::new(database.realm(RealmName::default())?, instance_id)?;
        collector.refresh().await?;

        // Every collected user must be scoped to this instance.
        for user in collector.data.iter() {
            assert_eq!(user.read()._instance_id, instance_id);
        }
        Ok(())
    }
}
