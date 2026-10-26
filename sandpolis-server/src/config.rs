use serde::{Deserialize, Serialize};

/// The `server` section of a realm config.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct ServerManagerConfig {
    /// Raise a notification whenever a connection is rejected because
    /// certificate authentication failed (a missing, untrusted, or malformed
    /// client certificate).
    ///
    /// Certificate authentication runs before the realm is known, so this
    /// stays enabled unless every served realm disables it.
    pub notify_cert_failures: bool,

    /// Whether to resolve where the instances attached to this server are, and
    /// which service to ask. Off by default.
    pub location: crate::location::config::LocationConfig,
}

impl Default for ServerManagerConfig {
    fn default() -> Self {
        Self {
            notify_cert_failures: true,
            location: Default::default(),
        }
    }
}
