use super::UserName;
use sandpolis_instance::Permission;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The `user` section of a realm config, which is the only place user accounts
/// are ever created or modified. The global stratum server reconciles this into
/// the realm database at startup, so removing a user here removes the account
/// (and its password hash and TOTP secret) from the estate.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct UsersConfig {
    /// Require every user to enroll a TOTP secret, which happens when they set
    /// their password on first login.
    pub totp: bool,

    /// Maximum (and default) lifetime of the auth tokens this realm issues.
    pub token_lifetime: Option<Duration>,

    /// Raise a notification whenever a login attempt against this realm fails.
    pub notify_login_failures: bool,

    /// The realm's user accounts, which clients (not agents) login to.
    pub users: Vec<UserConfig>,
}

impl Default for UsersConfig {
    fn default() -> Self {
        Self {
            totp: false,
            token_lifetime: None,
            notify_login_failures: true,
            users: Vec::new(),
        }
    }
}

impl UsersConfig {
    /// The default and maximum token lifetime when the config doesn't set one.
    pub const DEFAULT_TOKEN_LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct UserConfig {
    pub username: UserName,

    #[serde(default)]
    pub email: Option<String>,

    #[serde(default)]
    pub phone: Option<String>,

    /// Unix timestamp after which logins are refused.
    #[serde(default)]
    pub expiration: Option<i64>,

    /// What the user is allowed to do, layer by layer: for example
    /// `["shell:session", "filesystem:session"]`. `["shell:*"]` grants a whole
    /// layer and `["*"]` grants everything.
    #[serde(default)]
    pub permissions: Vec<Permission>,
}
