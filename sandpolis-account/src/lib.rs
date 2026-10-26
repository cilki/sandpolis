//! Models online/offline accounts and the relationships between them.
//!
//! An account is identified by the `domain` it belongs to plus a
//! `username` and/or `email`.
//!
//! Whenever the account set changes, the server re-derives [`AccountLinkData`]
//! rows connecting accounts that share an identity (a common username, a common
//! email, or an email that matches another account's identity). Those links are
//! the substrate for the subsystem's eventual attack-surface and compromise-tracing
//! analysis, hence [`AccountLinkType::compromisability`].

use anyhow::Result;
use native_db::*;
use native_model::Model;
use sandpolis_instance::InstanceId;
use sandpolis_instance::config::ConfigPersistHook;
use sandpolis_instance::database::{DatabaseManager, RealmDatabase};
use sandpolis_instance::realm::RealmName;
use sandpolis_macros::data;
use serde::{Deserialize, Serialize};
use validator::{Validate, ValidationError, ValidationErrors};

#[cfg(feature = "client")]
pub mod client;
pub mod config;
pub mod favicon;
pub mod management;
pub mod scrape;

sandpolis_instance::register_layer!("Account", "Online accounts and their relationships");

use config::AccountConfig;

static ACCOUNT_PERSIST: ConfigPersistHook<AccountConfig> = ConfigPersistHook::new("account");

/// Install the persistence hook (see [`ACCOUNT_PERSIST`]). Idempotent: the first
/// caller wins.
pub fn set_account_persist(f: impl Fn(&[AccountConfig]) -> Result<()> + Send + Sync + 'static) {
    ACCOUNT_PERSIST.set(f);
}

/// Persist the current account set if a hook is installed.
pub fn persist_accounts(accounts: &[AccountData]) {
    ACCOUNT_PERSIST.persist(&accounts_to_config(accounts));
}

/// Rebuild the on-disk account list from the given accounts.
///
/// Sorted because the database scan returns rows in primary-key order, which is
/// effectively random; without this the file would reshuffle on every write.
pub fn accounts_to_config(accounts: &[AccountData]) -> Vec<AccountConfig> {
    let mut accounts: Vec<AccountConfig> = accounts
        .iter()
        .map(|a| AccountConfig {
            domain: a.domain.clone(),
            username: a.username.clone(),
            email: a.email.clone(),
        })
        .collect();
    accounts.sort_by(|a, b| {
        (&a.domain, &a.username, &a.email).cmp(&(&b.domain, &b.username, &b.email))
    });
    accounts
}

#[data]
#[derive(Default)]
pub struct AccountManagerData {}

#[derive(Clone)]
pub struct AccountManager {
    #[allow(dead_code)]
    database: DatabaseManager,
    realm: RealmDatabase,
}

impl AccountManager {
    pub async fn new(database: DatabaseManager) -> Result<Self> {
        let realm = database.realm(RealmName::default())?;

        // The management stream's responder is registered through the stateless
        // `inventory` path, so it reaches the database through this handle.
        #[cfg(feature = "server")]
        management::install_realm(realm.clone());

        Ok(Self { database, realm })
    }

    /// The realm this subsystem's data lives in.
    pub fn realm(&self) -> &RealmDatabase {
        &self.realm
    }

    /// Import accounts declared in the config file that aren't in the database
    /// yet, then write the merged set back out.
    ///
    /// Call once from the server's startup path, before any client can connect.
    #[cfg(feature = "server")]
    pub fn seed_accounts(&self, config: &config::AccountManagerConfig) -> Result<()> {
        management::seed(&self.realm, &config.accounts)
    }

    /// Add the subsystem's background services to the server's runner.
    ///
    /// The config flags here are coarse startup switches: a service they turn off
    /// is never registered, so it doesn't appear in the client at all. Toggling a
    /// registered service on and off at runtime is the runner's job.
    #[cfg(feature = "server")]
    pub fn register_services(
        &self,
        config: &config::AccountManagerConfig,
        runner: &mut sandpolis_instance::service::ServiceRunner,
    ) -> Result<()> {
        if !config.scrape.enabled {
            tracing::info!("Account scraping is disabled");
            return Ok(());
        }

        if config.scrape.favicon.enabled {
            runner.register(favicon::FaviconService::new(
                self.realm.clone(),
                &config.scrape,
            )?);
        }
        Ok(())
    }
}

sandpolis_instance::typed_id!(
    /// Stable identity of an account.
    ///
    /// Distinct from the record's `_id`, which changes with every revision. Links
    /// reference accounts by this id so they survive account updates.
    AccountId,
    "account",
    5
);

/// A fresh random id, so `AccountData::default()` never aliases an existing
/// account.
impl Default for AccountId {
    fn default() -> Self {
        Self::random()
    }
}

/// A subjective indicator of how serious a compromise of an `Account` would be.
/// Accounts with higher ratings are more valuable than those with lower.
#[derive(
    Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default,
)]
pub struct AccountValue(i32);

/// An account on some service, identified by its domain plus a username and/or
/// email address.
#[data]
#[derive(Default)]
pub struct AccountData {
    #[secondary_key(unique)]
    pub account_id: AccountId,

    /// The service domain this account belongs to, for example "github.com".
    #[secondary_key]
    pub domain: String,

    /// The username used to authenticate. At least one of `username` or `email`
    /// is always present.
    pub username: Option<String>,

    /// The email address associated with the account.
    pub email: Option<String>,

    pub value: AccountValue,

    /// An instance associated with this account.
    pub instance: Option<InstanceId>,
}

impl AccountData {
    /// The best available human-readable identity for the account.
    pub fn identity(&self) -> &str {
        self.username
            .as_deref()
            .or(self.email.as_deref())
            .unwrap_or("(unknown)")
    }
}

inventory::submit! {
    sandpolis_instance::database::sync::SyncRegistration(|r| r.register::<AccountData>())
}

/// A relationship between two accounts.
///
/// `source` and `target` are stored in ascending order for derived links so a
/// relationship is never represented by two mirrored rows.
#[data]
#[derive(Default)]
pub struct AccountLinkData {
    #[secondary_key]
    pub source: AccountId,

    #[secondary_key]
    pub target: AccountId,

    pub r#type: AccountLinkType,

    /// Whether the server derived this link from the accounts' fields. Derived
    /// links are recomputed wholesale whenever the account set changes;
    /// user-supplied links are left alone.
    pub derived: bool,
}

inventory::submit! {
    sandpolis_instance::database::sync::SyncRegistration(|r| r.register::<AccountLinkData>())
}

/// Represents a directional or bidirectional relationship between two accounts.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum AccountLinkType {
    CommonEmail(String),
    CommonPassword {
        /// Whether multiple authentication factors are required
        mfa: Option<bool>,
    },
    /// The accounts share a username for authentication
    CommonUsername(String),
    Recovery,
    SshAuthorizedKey {
        /// Whether the SSH key is stored encrypted
        encrypted: Option<bool>,
    },
}

impl Default for AccountLinkType {
    // Needed so `AccountLinkData` can derive `Default`; `#[default]` can't apply
    // to a variant with a payload.
    fn default() -> Self {
        AccountLinkType::CommonUsername(String::new())
    }
}

/// A subjective indicator of how likely a compromise of an `Account` can
/// occur given some criteria (as a decimal percentage).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Compromisability(f32);

impl Validate for Compromisability {
    fn validate(&self) -> Result<(), ValidationErrors> {
        if (0.0..=1.0).contains(&self.0) {
            Ok(())
        } else {
            let mut errors = ValidationErrors::new();
            errors.add("0", ValidationError::new("range"));
            Err(errors)
        }
    }
}

impl AccountLinkType {
    pub fn compromisability(&self) -> Compromisability {
        // Weights are relative
        match self {
            AccountLinkType::CommonUsername(_) => Compromisability(0.05),
            AccountLinkType::CommonEmail(_) => Compromisability(0.10),
            AccountLinkType::CommonPassword { mfa } => match mfa {
                Some(true) => Compromisability(0.20),
                Some(false) => Compromisability(0.80),
                None => Compromisability(0.50),
            },
            AccountLinkType::Recovery => Compromisability(0.95),
            AccountLinkType::SshAuthorizedKey { encrypted } => match encrypted {
                Some(true) => Compromisability(0.20),
                Some(false) => Compromisability(0.95),
                None => Compromisability(0.50),
            },
        }
    }
}

// What a client must be granted to open this layer's streams.
inventory::submit! {
    sandpolis_instance::network::stream::StreamPermission::require(
        sandpolis_macros::stream_tag!(AccountMgmt), "account:manage")
}
