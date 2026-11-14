use anyhow::Result;
use native_db::ToKey;
use native_model::Model;
use regex::Regex;
use sandpolis_instance::InstanceManager;
use sandpolis_instance::Permission;
use sandpolis_instance::database::{DatabaseManager, Resident};
use sandpolis_instance::realm::RealmName;
use sandpolis_macros::data;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Display;
use std::ops::Deref;
use std::str::FromStr;
use std::sync::LazyLock;
use tracing::debug;
use validator::{Validate, ValidationErrors};

pub mod config;
#[cfg(feature = "server")]
pub mod server;

static USER_NAME_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new("^[a-z0-9]{4,32}$").unwrap());

/// A user's username is forever unchangable.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct UserName(String);

impl Deref for UserName {
    type Target = String;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl FromStr for UserName {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let name = UserName(s.to_string());
        name.validate()?;
        Ok(name)
    }
}

impl Validate for UserName {
    fn validate(&self) -> Result<(), ValidationErrors> {
        if USER_NAME_REGEX.is_match(&self.0) {
            Ok(())
        } else {
            Err(ValidationErrors::new())
        }
    }
}

impl Default for UserName {
    fn default() -> Self {
        UserName("admin".to_string())
    }
}

impl Display for UserName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl ToKey for UserName {
    fn to_key(&self) -> native_db::Key {
        native_db::Key::new(self.0.as_bytes().to_vec())
    }

    fn key_names() -> Vec<String> {
        vec!["UserName".to_string()]
    }
}

#[cfg(test)]
mod test_user_name {
    use super::*;

    #[test]
    fn test_valid_usernames() {
        assert!("test".parse::<UserName>().is_ok());
        assert!("admin".parse::<UserName>().is_ok());
        assert!("user123".parse::<UserName>().is_ok());
        assert!("1234".parse::<UserName>().is_ok());
        assert!("abcd".parse::<UserName>().is_ok());
        assert!("user0".parse::<UserName>().is_ok());
        assert!("0user".parse::<UserName>().is_ok());
        assert!(
            "longusername12345678901234567890"
                .parse::<UserName>()
                .is_ok()
        );
    }

    #[test]
    fn test_invalid_usernames() {
        // Too short
        assert!("a".parse::<UserName>().is_err());
        assert!("ab".parse::<UserName>().is_err());
        assert!("abc".parse::<UserName>().is_err());
        assert!("".parse::<UserName>().is_err());

        // Too long
        assert!(
            "verylongusernamethatexceedsthemaximumlengthallowed"
                .parse::<UserName>()
                .is_err()
        );
        assert!("a".repeat(33).parse::<UserName>().is_err());

        // Invalid characters
        assert!("user-name".parse::<UserName>().is_err());
        assert!("user_name".parse::<UserName>().is_err());
        assert!("user.name".parse::<UserName>().is_err());
        assert!("user@name".parse::<UserName>().is_err());
        assert!("user name".parse::<UserName>().is_err());
        assert!("User".parse::<UserName>().is_err());
        assert!("USER".parse::<UserName>().is_err());
        assert!("user!".parse::<UserName>().is_err());
        assert!("user#".parse::<UserName>().is_err());
        assert!("user$".parse::<UserName>().is_err());
        assert!("user%".parse::<UserName>().is_err());
        assert!("user^".parse::<UserName>().is_err());
        assert!("user&".parse::<UserName>().is_err());
        assert!("user*".parse::<UserName>().is_err());
        assert!("user(".parse::<UserName>().is_err());
        assert!("user)".parse::<UserName>().is_err());
        assert!("user+".parse::<UserName>().is_err());
        assert!("user=".parse::<UserName>().is_err());
        assert!("user[".parse::<UserName>().is_err());
        assert!("user]".parse::<UserName>().is_err());
        assert!("user{".parse::<UserName>().is_err());
        assert!("user}".parse::<UserName>().is_err());
        assert!("user|".parse::<UserName>().is_err());
        assert!("user\\".parse::<UserName>().is_err());
        assert!("user:".parse::<UserName>().is_err());
        assert!("user;".parse::<UserName>().is_err());
        assert!("user\"".parse::<UserName>().is_err());
        assert!("user'".parse::<UserName>().is_err());
        assert!("user<".parse::<UserName>().is_err());
        assert!("user>".parse::<UserName>().is_err());
        assert!("user,".parse::<UserName>().is_err());
        assert!("user?".parse::<UserName>().is_err());
        assert!("user/".parse::<UserName>().is_err());
        assert!("user~".parse::<UserName>().is_err());
        assert!("user`".parse::<UserName>().is_err());
    }

    #[test]
    fn test_boundary_lengths() {
        // Exactly 4 characters (minimum)
        assert!("test".parse::<UserName>().is_ok());
        assert!("1234".parse::<UserName>().is_ok());
        assert!("abcd".parse::<UserName>().is_ok());

        // Exactly 32 characters (maximum)
        let max_length = "a".repeat(32);
        assert_eq!(max_length.len(), 32);
        assert!(max_length.parse::<UserName>().is_ok());

        // Just over 32 characters
        let over_max = "a".repeat(33);
        assert_eq!(over_max.len(), 33);
        assert!(over_max.parse::<UserName>().is_err());

        // Just under 4 characters
        assert!("abc".parse::<UserName>().is_err());
    }

    #[test]
    fn test_default() {
        let default_username = UserName::default();
        assert_eq!(default_username.to_string(), "admin");
        assert_eq!(*default_username, "admin");
    }

    #[test]
    fn test_display() {
        let username = UserName("testuser".to_string());
        assert_eq!(username.to_string(), "testuser");
        assert_eq!(format!("{}", username), "testuser");
    }

    #[test]
    fn test_deref() {
        let username = UserName("testuser".to_string());
        assert_eq!(username.len(), 8);
        assert_eq!(username.chars().count(), 8);
        assert!(username.starts_with("test"));
        assert!(username.ends_with("user"));
    }

    #[test]
    fn test_equality() {
        let username1 = UserName("testuser".to_string());
        let username2 = UserName("testuser".to_string());
        let username3 = UserName("different".to_string());

        assert_eq!(username1, username2);
        assert_ne!(username1, username3);
    }
}

#[data]
#[derive(Default)]
pub struct UserManagerData {}

#[derive(Clone)]
pub struct UserManager {
    pub data: Resident<UserManagerData>,
    pub instance: InstanceManager,
    pub database: DatabaseManager,

    /// Each realm's user accounts, live from that realm's database. Owned by
    /// the global stratum server and received by replication everywhere else.
    #[cfg(feature = "server")]
    pub users: HashMap<RealmName, sandpolis_instance::database::ResidentVec<UserData>>,

    #[cfg(feature = "server")]
    pub jwt_keys: HashMap<RealmName, (jsonwebtoken::EncodingKey, jsonwebtoken::DecodingKey)>,

    /// Each realm's `user` config section. Populated only on the global stratum
    /// server, which is the only instance that reads realm configs.
    #[cfg(feature = "server")]
    pub configs: HashMap<RealmName, config::UsersConfig>,

    #[cfg(feature = "server")]
    pub network: sandpolis_instance::network::NetworkManager,

    /// The stratum of the server this manager belongs to, so the connect handler
    /// can enforce the network's shape and pick the right sync behavior.
    #[cfg(feature = "server")]
    pub stratum: crate::ServerStratum,

    /// The ownership grant table, so the connect handler can accept claims from
    /// local stratum servers.
    #[cfg(feature = "server")]
    pub ownership: std::sync::Arc<crate::ownership::Ownership>,

    /// What each peer-initiated stream type requires of a client, keyed by
    /// stream tag: a permission, or `None` for infrastructure open to every
    /// authenticated client. Collected from the layer crates' `StreamPermission`
    /// declarations; a tag not in this map is denied to clients outright.
    #[cfg(feature = "server")]
    pub stream_permissions: std::sync::Arc<HashMap<u32, Option<Permission>>>,
}

impl UserManager {
    /// `realms` names every realm this instance serves or connects to; `configs`
    /// carries the `user` section of each realm config, which only the global
    /// stratum server has.
    pub async fn new(
        instance: InstanceManager,
        database: DatabaseManager,
        network: sandpolis_instance::network::NetworkManager,
        #[cfg(feature = "server")] realms: Vec<RealmName>,
        #[cfg(feature = "server")] configs: HashMap<RealmName, config::UsersConfig>,
        #[cfg(feature = "server")] stratum: crate::ServerStratum,
        #[cfg(feature = "server")] ownership: std::sync::Arc<crate::ownership::Ownership>,
    ) -> Result<Self> {
        debug!("Initializing user manager");
        let user_manager = Self {
            instance,
            data: database.realm(RealmName::default())?.resident(())?,
            #[cfg(feature = "server")]
            users: {
                let mut users = HashMap::new();
                for realm in &realms {
                    users.insert(realm.clone(), database.realm(realm.clone())?.resident_vec(())?);
                }
                users
            },
            #[cfg(feature = "server")]
            network,
            #[cfg(feature = "server")]
            stratum,
            #[cfg(feature = "server")]
            ownership,
            #[cfg(feature = "server")]
            stream_permissions: {
                let mut map = HashMap::new();
                for declared in sandpolis_instance::inventory::iter::<
                    sandpolis_instance::network::stream::StreamPermission,
                >() {
                    let permission = declared
                        .permission
                        .map(|permission| permission.parse::<Permission>())
                        .transpose()?;
                    map.insert(declared.tag, permission);
                }
                std::sync::Arc::new(map)
            },
            #[cfg(feature = "server")]
            jwt_keys: {
                let mut jwt_keys = HashMap::new();
                for realm in &realms {
                    let db = database.realm(realm.clone())?;
                    // This server's own signing secret is local state; a local
                    // stratum server still needs one to authenticate its clients.
                    let rw = db.local_write()?;
                    let secrets: Vec<server::ServerJwtSecret> =
                        rw.scan().primary()?.all()?.collect::<Result<Vec<_>, _>>()?;

                    assert!(secrets.len() <= 1);
                    let secret = if secrets.is_empty() {
                        // Time to generate
                        debug!(realm = %realm, "Generating new JWT secret");

                        let secret = server::ServerJwtSecret::new();
                        rw.insert(secret.clone())?;
                        rw.commit()?;

                        secret
                    } else {
                        secrets[0].clone()
                    };

                    jwt_keys.insert(
                        realm.clone(),
                        (
                            jsonwebtoken::EncodingKey::from_secret(&secret.value)?,
                            jsonwebtoken::DecodingKey::from_secret(&secret.value)?,
                        ),
                    );
                }

                jwt_keys
            },
            #[cfg(feature = "server")]
            configs,
            database,
        };

        // The realm config is the sole authority over user accounts, so it is
        // reconciled into the database on every start. Only the global stratum
        // server has configs; everything else receives users by replication.
        #[cfg(feature = "server")]
        for (realm, config) in &user_manager.configs {
            user_manager.sync_users(realm, config).await?;
        }

        Ok(user_manager)
    }
}

#[data]
#[derive(Validate, Default)]
pub struct UserData {
    pub username: UserName,

    /// What the user is allowed to do; `["*"]` grants everything.
    pub permissions: Vec<Permission>,

    /// Email address
    #[validate(email)]
    pub email: Option<String>,

    /// Phone number
    pub phone: Option<String>,

    /// Unix timestamp after which logins are refused.
    pub expiration: Option<i64>,
}

#[derive(Serialize, Deserialize, PartialEq, Clone, Debug, Default)]
pub struct ClientAuthToken(pub String);

impl ClientAuthToken {
    /// Whether the token is worth presenting at all: non-empty and not past its
    /// own expiration claim. The signature isn't checked — that's the server's
    /// job — this only saves the client a round trip it knows would fail.
    pub fn is_usable(&self) -> bool {
        use base64::prelude::*;

        if self.0.is_empty() {
            return false;
        }

        let Some(payload) = self.0.split('.').nth(1) else {
            return false;
        };
        let Ok(payload) = BASE64_URL_SAFE_NO_PAD.decode(payload) else {
            return false;
        };
        let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&payload) else {
            return false;
        };
        let Some(exp) = payload.get("exp").and_then(|exp| exp.as_i64()) else {
            return false;
        };

        exp > chrono::Utc::now().timestamp()
    }
}
