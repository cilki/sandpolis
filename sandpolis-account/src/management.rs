//! The account-management stream: create and delete accounts.
//!
//! Unlike the probe subsystem's equivalent stream, this one carries writes only. The
//! read path is the database sync engine: the server writes [`AccountData`] and
//! [`AccountLinkData`] into its realm, and clients subscribe to those models (see
//! [`crate::client`]) to receive a snapshot plus live updates.

use crate::AccountId;
use serde::{Deserialize, Serialize};

/// Requests from a client to the server's account manager.
#[derive(Serialize, Deserialize, Debug)]
pub enum AccountMgmtRequest {
    /// Create a new account.
    Create {
        domain: String,
        username: Option<String>,
        email: Option<String>,
    },
    /// Delete the account with this id, along with every link that touches it.
    Delete { id: AccountId },
}

/// Responses from the server's account manager.
#[derive(Serialize, Deserialize, Debug)]
pub enum AccountMgmtResponse {
    /// The operation succeeded.
    Ok,
    /// The operation failed.
    Error(String),
}

#[cfg(feature = "server")]
mod server {
    use super::*;
    use crate::config::AccountConfig;
    use crate::{AccountData, AccountLinkData, AccountLinkType};
    use anyhow::{Result, bail};
    use sandpolis_instance::database::{DataScope, RealmDatabase};
    use sandpolis_instance::network::{
        RegisterResponders, ResponderRegistration, StreamRegistry, StreamResponder,
    };
    use sandpolis_macros::Stream;
    use std::sync::OnceLock;
    use tokio::sync::mpsc::Sender;

    /// The realm accounts live in, installed by `AccountManager::new`.
    ///
    /// Held in a static so [`AccountMgmtResponder`] can stay a unit struct and
    /// register through the stateless `inventory` path.
    static REALM: OnceLock<RealmDatabase> = OnceLock::new();

    /// Give the account manager access to the database. Called once at startup.
    pub fn install_realm(realm: RealmDatabase) {
        let _ = REALM.set(realm);
    }

    fn realm() -> Result<&'static RealmDatabase> {
        match REALM.get() {
            Some(realm) => Ok(realm),
            None => bail!("Account subsystem is not initialized"),
        }
    }

    /// Server side of the management stream.
    #[derive(Stream, Default)]
    pub struct AccountMgmtResponder;

    impl StreamResponder for AccountMgmtResponder {
        type In = AccountMgmtRequest;
        type Out = AccountMgmtResponse;

        async fn on_message(&self, request: Self::In, sender: Sender<Self::Out>) -> Result<()> {
            let response = match handle(request) {
                Ok(()) => AccountMgmtResponse::Ok,
                Err(e) => {
                    tracing::warn!(error = %e, "Account management request failed");
                    AccountMgmtResponse::Error(e.to_string())
                }
            };
            sender.send(response).await?;
            Ok(())
        }
    }

    fn handle(request: AccountMgmtRequest) -> Result<()> {
        let realm = realm()?;
        match request {
            AccountMgmtRequest::Create {
                domain,
                username,
                email,
            } => create(realm, domain, username, email)?,
            AccountMgmtRequest::Delete { id } => delete(realm, id)?,
        }
        recompute_links(realm)?;
        crate::persist_accounts(&accounts(realm)?);
        Ok(())
    }

    /// Trim a field, treating whitespace-only input as absent.
    fn clean(value: Option<String>) -> Option<String> {
        value
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    /// Trim an account's fields and reject one that can't identify anything.
    fn validated(
        domain: String,
        username: Option<String>,
        email: Option<String>,
    ) -> Result<(String, Option<String>, Option<String>)> {
        let domain = domain.trim().to_string();
        if domain.is_empty() {
            bail!("A domain is required");
        }

        let username = clean(username);
        let email = clean(email);
        if username.is_none() && email.is_none() {
            bail!("A username or email is required");
        }

        Ok((domain, username, email))
    }

    /// Every account currently stored.
    fn accounts(realm: &RealmDatabase) -> Result<Vec<AccountData>> {
        let r = realm.r_transaction()?;
        Ok(r.scan()
            .primary::<AccountData>()?
            .all()?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    fn create(
        realm: &RealmDatabase,
        domain: String,
        username: Option<String>,
        email: Option<String>,
    ) -> Result<()> {
        let (domain, username, email) = validated(domain, username, email)?;

        let rw = realm.write(DataScope::Global)?;
        rw.insert(AccountData {
            account_id: AccountId::default(),
            domain,
            username,
            email,
            ..Default::default()
        })?;
        rw.commit()?;
        Ok(())
    }

    /// An account's identity, for comparing a config entry against a stored row.
    ///
    /// Compared case-insensitively, like the identities [`derive_links`] matches
    /// on, so re-casing an entry in the config file doesn't duplicate it.
    fn identity_key(
        domain: &str,
        username: Option<&str>,
        email: Option<&str>,
    ) -> (String, Option<String>, Option<String>) {
        (
            domain.to_lowercase(),
            username.map(str::to_lowercase),
            email.map(str::to_lowercase),
        )
    }

    /// Import configured accounts that aren't stored yet, then write the merged
    /// set back to the config file.
    ///
    /// An unusable entry is skipped rather than fatal: a typo in a hand-edited
    /// config shouldn't stop the server from starting.
    pub fn seed(realm: &RealmDatabase, configured: &[AccountConfig]) -> Result<()> {
        let mut seen: Vec<_> = accounts(realm)?
            .iter()
            .map(|a| identity_key(&a.domain, a.username.as_deref(), a.email.as_deref()))
            .collect();

        let mut added = 0;
        for account in configured {
            let (domain, username, email) = match validated(
                account.domain.clone(),
                account.username.clone(),
                account.email.clone(),
            ) {
                Ok(fields) => fields,
                Err(e) => {
                    tracing::warn!(account = ?account, error = %e, "Ignoring configured account");
                    continue;
                }
            };

            // Also catches duplicates within the config itself.
            let key = identity_key(&domain, username.as_deref(), email.as_deref());
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);

            let rw = realm.write(DataScope::Global)?;
            rw.insert(AccountData {
                account_id: AccountId::default(),
                domain,
                username,
                email,
                ..Default::default()
            })?;
            rw.commit()?;
            added += 1;
        }

        if added > 0 {
            tracing::info!(count = added, "Imported accounts from configuration");
            recompute_links(realm)?;
        }

        // Unconditional: accounts created before the config knew about them (or
        // written in a different order) get picked up on the first run.
        crate::persist_accounts(&accounts(realm)?);
        Ok(())
    }

    fn delete(realm: &RealmDatabase, id: AccountId) -> Result<()> {
        let rw = realm.write(DataScope::Global)?;

        let accounts: Vec<AccountData> = rw
            .scan()
            .primary::<AccountData>()?
            .all()?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for account in accounts.into_iter().filter(|a| a.account_id == id) {
            rw.remove(account)?;
        }

        let links: Vec<AccountLinkData> = rw
            .scan()
            .primary::<AccountLinkData>()?
            .all()?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for link in links
            .into_iter()
            .filter(|l| l.source == id || l.target == id)
        {
            rw.remove(link)?;
        }

        rw.commit()?;
        Ok(())
    }

    /// Rebuild every derived link from the current account set.
    ///
    /// Derived links are cheap and few relative to accounts, so they're replaced
    /// wholesale rather than diffed. Links the user created explicitly (which
    /// can't be inferred from account fields) are left untouched.
    fn recompute_links(realm: &RealmDatabase) -> Result<()> {
        let rw = realm.write(DataScope::Global)?;

        let accounts: Vec<AccountData> = rw
            .scan()
            .primary::<AccountData>()?
            .all()?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let existing: Vec<AccountLinkData> = rw
            .scan()
            .primary::<AccountLinkData>()?
            .all()?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for link in existing.into_iter().filter(|l| l.derived) {
            rw.remove(link)?;
        }

        for (i, x) in accounts.iter().enumerate() {
            for y in &accounts[i + 1..] {
                // Order the pair by account id so a relationship yields one row
                // rather than two mirrored ones, and so the link's payload is
                // independent of the arbitrary order the scan returned.
                let (a, b) = if x.account_id <= y.account_id {
                    (x, y)
                } else {
                    (y, x)
                };
                for r#type in derive_links(a, b) {
                    rw.insert(AccountLinkData {
                        source: a.account_id,
                        target: b.account_id,
                        r#type,
                        derived: true,
                        ..Default::default()
                    })?;
                }
            }
        }

        rw.commit()?;
        Ok(())
    }

    /// Every derived relationship between two distinct accounts.
    ///
    /// Where the two accounts' values differ only by case, the link carries `a`'s
    /// spelling. Callers pass the pair ordered by account id so that choice is
    /// deterministic.
    fn derive_links(a: &AccountData, b: &AccountData) -> Vec<AccountLinkType> {
        let mut links = Vec::new();

        if let (Some(x), Some(y)) = (&a.username, &b.username)
            && x.eq_ignore_ascii_case(y)
        {
            links.push(AccountLinkType::CommonUsername(x.clone()));
        }

        // A shared email, or one account's email serving as the other's login
        // identity.
        let mut emails = Vec::new();
        for (x, y) in [
            (&a.email, &b.email),
            (&a.email, &b.username),
            (&b.email, &a.username),
        ] {
            if let (Some(x), Some(y)) = (x, y)
                && x.eq_ignore_ascii_case(y)
            {
                emails.push(x.clone());
            }
        }
        emails.sort();
        emails.dedup();
        links.extend(emails.into_iter().map(AccountLinkType::CommonEmail));

        links
    }

    /// Registers [`AccountMgmtResponder`] on each connection.
    pub struct AccountMgmtResponderRegistration;

    impl RegisterResponders for AccountMgmtResponderRegistration {
        fn register_responders(&self, registry: &StreamRegistry) {
            registry.register_responder(AccountMgmtResponder::default);
        }
    }

    inventory::submit!(ResponderRegistration(&AccountMgmtResponderRegistration));

    #[cfg(test)]
    mod tests {
        use super::*;
        use sandpolis_instance::database::DatabaseManager;
        use sandpolis_instance::realm::RealmName;
        use sandpolis_instance::test_db;

        fn links(realm: &RealmDatabase) -> Result<Vec<AccountLinkData>> {
            let r = realm.r_transaction()?;
            Ok(r.scan()
                .primary::<AccountLinkData>()?
                .all()?
                .collect::<std::result::Result<Vec<_>, _>>()?)
        }

        fn add(
            realm: &RealmDatabase,
            domain: &str,
            username: Option<&str>,
            email: Option<&str>,
        ) -> Result<()> {
            create(
                realm,
                domain.into(),
                username.map(Into::into),
                email.map(Into::into),
            )?;
            recompute_links(realm)
        }

        fn realm() -> Result<RealmDatabase> {
            let db: DatabaseManager = test_db!(AccountData, AccountLinkData);
            db.realm(RealmName::default())
        }

        fn configured(domain: &str, username: Option<&str>, email: Option<&str>) -> AccountConfig {
            AccountConfig {
                domain: domain.into(),
                username: username.map(Into::into),
                email: email.map(Into::into),
            }
        }

        /// Identities of the stored accounts, sorted for comparison.
        fn stored(realm: &RealmDatabase) -> Result<Vec<(String, Option<String>)>> {
            let mut out: Vec<_> = accounts(realm)?
                .into_iter()
                .map(|a| (a.domain, a.username))
                .collect();
            out.sort();
            Ok(out)
        }

        #[test]
        fn create_requires_domain_and_identity() -> Result<()> {
            let realm = realm()?;

            assert!(create(&realm, "  ".into(), Some("alice".into()), None).is_err());
            assert!(create(&realm, "github.com".into(), None, None).is_err());
            // Whitespace-only fields count as absent.
            assert!(create(&realm, "github.com".into(), Some("  ".into()), None).is_err());
            assert!(accounts(&realm)?.is_empty());

            create(&realm, " github.com ".into(), Some(" alice ".into()), None)?;
            let stored = accounts(&realm)?;
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0].domain, "github.com");
            assert_eq!(stored[0].username.as_deref(), Some("alice"));
            assert_eq!(stored[0].email, None);
            Ok(())
        }

        #[test]
        fn links_shared_username_and_email() -> Result<()> {
            let realm = realm()?;

            add(
                &realm,
                "github.com",
                Some("alice"),
                Some("alice@example.com"),
            )?;
            add(&realm, "gitlab.com", Some("Alice"), None)?;
            // Linked to the first account through its email, not a username.
            add(&realm, "mail.example.com", None, Some("alice@example.com"))?;
            // Shares nothing with anyone.
            add(&realm, "example.org", Some("bob"), None)?;

            let links = links(&realm)?;
            assert_eq!(links.len(), 2, "{links:#?}");
            assert!(links.iter().all(|l| l.derived));
            assert!(links.iter().all(|l| l.source <= l.target));
            // The two usernames differ only in case, so which spelling the link
            // carries depends on the pair's ordering; only the match matters.
            assert!(links.iter().any(|l| matches!(
                &l.r#type,
                AccountLinkType::CommonUsername(u) if u.eq_ignore_ascii_case("alice")
            )));
            assert!(
                links
                    .iter()
                    .any(|l| l.r#type == AccountLinkType::CommonEmail("alice@example.com".into()))
            );
            Ok(())
        }

        #[test]
        fn links_email_matching_another_username() -> Result<()> {
            let realm = realm()?;

            add(&realm, "github.com", Some("alice@example.com"), None)?;
            add(&realm, "gitlab.com", None, Some("alice@example.com"))?;

            let links = links(&realm)?;
            assert_eq!(links.len(), 1, "{links:#?}");
            assert_eq!(
                links[0].r#type,
                AccountLinkType::CommonEmail("alice@example.com".into())
            );
            Ok(())
        }

        /// Recomputation must be a pure function of the account set: the scan
        /// returns accounts in primary-key (random id) order, so an unordered
        /// pairing would let a link's payload flip between two spellings.
        #[test]
        fn recompute_is_deterministic() -> Result<()> {
            let realm = realm()?;

            add(&realm, "github.com", Some("Alice"), None)?;
            add(&realm, "gitlab.com", Some("alice"), None)?;

            let first: Vec<_> = links(&realm)?
                .into_iter()
                .map(|l| (l.source, l.target, l.r#type))
                .collect();
            for _ in 0..5 {
                recompute_links(&realm)?;
                let next: Vec<_> = links(&realm)?
                    .into_iter()
                    .map(|l| (l.source, l.target, l.r#type))
                    .collect();
                assert_eq!(first, next);
            }
            Ok(())
        }

        #[test]
        fn delete_removes_account_and_its_links() -> Result<()> {
            let realm = realm()?;

            add(&realm, "github.com", Some("alice"), None)?;
            add(&realm, "gitlab.com", Some("alice"), None)?;
            assert_eq!(links(&realm)?.len(), 1);

            let target = accounts(&realm)?
                .into_iter()
                .find(|a| a.domain == "gitlab.com")
                .expect("account exists");
            delete(&realm, target.account_id)?;
            recompute_links(&realm)?;

            assert_eq!(accounts(&realm)?.len(), 1);
            assert!(links(&realm)?.is_empty());
            Ok(())
        }

        #[test]
        fn seed_imports_configured_accounts() -> Result<()> {
            let realm = realm()?;

            let config = vec![
                configured("github.com", Some("alice"), None),
                configured("gitlab.com", Some("alice"), Some("alice@example.com")),
            ];
            seed(&realm, &config)?;

            assert_eq!(
                stored(&realm)?,
                vec![
                    ("github.com".to_string(), Some("alice".to_string())),
                    ("gitlab.com".to_string(), Some("alice".to_string())),
                ]
            );
            // Seeding recomputes links just like a create does.
            assert_eq!(links(&realm)?.len(), 1);

            // Seeding again is a no-op, including across a re-cased entry.
            seed(&realm, &config)?;
            seed(&realm, &[configured("GitHub.com", Some("Alice"), None)])?;
            assert_eq!(accounts(&realm)?.len(), 2);
            Ok(())
        }

        #[test]
        fn seed_skips_accounts_that_already_exist() -> Result<()> {
            let realm = realm()?;

            add(&realm, "github.com", Some("alice"), None)?;
            seed(
                &realm,
                &[
                    configured("github.com", Some("alice"), None),
                    // Same domain, different identity: a separate account.
                    configured("github.com", Some("bob"), None),
                ],
            )?;

            assert_eq!(
                stored(&realm)?,
                vec![
                    ("github.com".to_string(), Some("alice".to_string())),
                    ("github.com".to_string(), Some("bob".to_string())),
                ]
            );
            Ok(())
        }

        #[test]
        fn seed_skips_unusable_entries() -> Result<()> {
            let realm = realm()?;

            seed(
                &realm,
                &[
                    configured("  ", Some("alice"), None),
                    configured("github.com", None, None),
                    // Duplicated within the config itself.
                    configured("gitlab.com", Some("alice"), None),
                    configured("gitlab.com", Some("alice"), None),
                ],
            )?;

            assert_eq!(
                stored(&realm)?,
                vec![("gitlab.com".to_string(), Some("alice".to_string()))]
            );
            Ok(())
        }

        /// Sorting prevents reshuffling the file on every write.
        #[test]
        fn export_is_sorted() -> Result<()> {
            let realm = realm()?;

            add(&realm, "gitlab.com", Some("bob"), None)?;
            add(&realm, "github.com", Some("bob"), None)?;
            add(&realm, "github.com", Some("alice"), None)?;

            let exported = crate::accounts_to_config(&accounts(&realm)?);
            assert_eq!(
                exported,
                vec![
                    configured("github.com", Some("alice"), None),
                    configured("github.com", Some("bob"), None),
                    configured("gitlab.com", Some("bob"), None),
                ]
            );
            Ok(())
        }
    }
}

#[cfg(feature = "server")]
pub use server::{AccountMgmtResponder, install_realm, seed};

#[cfg(feature = "client")]
mod client {
    use super::*;
    use anyhow::Result;
    use sandpolis_instance::network::InstanceConnection;
    use sandpolis_instance::network::StreamRequester;
    use sandpolis_instance::network::stream::StreamMessage;
    use sandpolis_macros::Stream;
    use std::sync::Arc;
    use tokio::sync::mpsc::Sender;

    /// Client side of the management stream. Errors are surfaced in the log; the
    /// resulting account (or its absence) arrives through the sync subscription.
    #[derive(Stream, Default)]
    pub struct AccountMgmtRequester;

    impl StreamRequester for AccountMgmtRequester {
        type In = AccountMgmtResponse;
        type Out = AccountMgmtRequest;

        async fn on_message(&self, response: Self::In, _: Sender<Self::Out>) -> Result<()> {
            if let AccountMgmtResponse::Error(e) = response {
                tracing::warn!(error = %e, "Account management error");
            }
            Ok(())
        }
    }

    /// Send a one-shot management request to the server.
    fn send_request(conn: Arc<InstanceConnection>, request: AccountMgmtRequest) {
        sandpolis_client::sync::spawn(async move {
            let (id, tx) = conn.register_stream(AccountMgmtRequester);
            let payload = match serde_cbor::to_vec(&request) {
                Ok(p) => p,
                Err(_) => return,
            };
            let _ = tx.send(StreamMessage::local(id, payload)).await;
            conn.close_stream(id);
        });
    }

    /// Create a new account on the server.
    pub fn create_account(
        conn: Arc<InstanceConnection>,
        domain: String,
        username: Option<String>,
        email: Option<String>,
    ) {
        send_request(
            conn,
            AccountMgmtRequest::Create {
                domain,
                username,
                email,
            },
        );
    }

    /// Delete the account with `id`.
    pub fn delete_account(conn: Arc<InstanceConnection>, id: AccountId) {
        send_request(conn, AccountMgmtRequest::Delete { id });
    }
}

#[cfg(feature = "client")]
pub use client::{AccountMgmtRequester, create_account, delete_account};
