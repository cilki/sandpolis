//! Resolving where the instances attached to this server are.
//!
//! The evidence is the address a connection came from, so this only runs where
//! the connections are, and only for addresses that mean anything outside the
//! local network — an estate behind one NAT resolves to the same place, so it
//! isn't asked about at all.
//!
//! A lookup costs a request to somebody else's server, which shapes everything
//! here: an address is asked about once ([`LocationData::address`] is what
//! records that it was), a failure is backed off rather than retried on every
//! sweep, and the queries in a pass are spaced out so a hundred agents
//! reconnecting at once doesn't look like an attack on the service.

use super::{Location, LocationConfig, LocationData, LocationService};
use anyhow::Result;
use chrono::{DateTime, TimeDelta, Utc};
use sandpolis_instance::InstanceId;
use sandpolis_instance::database::{RealmDatabase, ResidentVec};
use sandpolis_instance::network::NetworkManager;
use sandpolis_instance::realm::RealmName;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use tokio::sync::Notify;
use tokio::time::{Duration, sleep};
use tracing::{debug, trace, warn};

pub mod ip_api;
pub mod key_cdn;

/// How long to wait after a connection change before acting, so a burst of
/// reconnects produces one pass.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// How often to re-check with nothing having happened. Only a safety net: an
/// instance attaching is an event, and an attached one doesn't move.
const SWEEP_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Gap between lookups within a pass. ip-api.com's free tier allows 45 a
/// minute, and nothing here is in a hurry.
const QUERY_INTERVAL: Duration = Duration::from_secs(2);

/// How long to leave a subject alone after a failed lookup. Long enough that a
/// service which is down, rate limiting, or refusing our key isn't asked again
/// on every sweep.
const RETRY_AFTER: TimeDelta = TimeDelta::hours(1);

/// How long a lookup may take before it's abandoned. A background task must not
/// be able to hang on a service that accepted the connection and went quiet.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The HTTP client every locator uses.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .unwrap_or_default()
}

/// A service's field that is present but blank, which several of them use where
/// they mean "unknown" (ip-api.com's `district`, keycdn's `timezone`).
pub(crate) fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

/// The AS number out of a `"AS13335 Cloudflare, Inc."` style field, which is how
/// ip-api.com reports it.
pub(crate) fn as_code(value: Option<&str>) -> Option<u32> {
    value?
        .split_whitespace()
        .next()?
        .strip_prefix("AS")?
        .parse()
        .ok()
}

/// A service that can say where an address is.
///
/// Not part of the crate's public surface: which service answers is a realm
/// config decision ([`LocationConfig::service`]), so the only implementations
/// are the ones [`AnyLocator`] can select between.
pub(crate) trait Locator {
    /// Where `ip` is, or `None` when the service answered and had nothing for
    /// it (a reserved range, an address it doesn't cover). An `Err` means the
    /// question didn't get answered, which is the caller's cue to back off.
    async fn query(&self, ip: IpAddr) -> Result<Option<Location>>;
}

/// The locator a realm config selected.
///
/// A dispatch enum rather than a `dyn Locator`, because [`Locator::query`] is an
/// `async fn` in a trait and so isn't dyn-compatible.
pub enum AnyLocator {
    IpApi(ip_api::IpApiLocator),
    KeyCdn(key_cdn::KeyCdnLocator),

    /// Answers from a fixed script, and panics once it runs out — so a test can
    /// assert that no further lookup happened.
    #[cfg(test)]
    Stub(std::sync::Mutex<std::collections::VecDeque<Result<Option<Location>>>>),
}

impl AnyLocator {
    pub fn new(config: &LocationConfig) -> Self {
        match config.service {
            LocationService::IpApi => Self::IpApi(ip_api::IpApiLocator::new(config.key.clone())),
            LocationService::KeyCdn => Self::KeyCdn(key_cdn::KeyCdnLocator::new()),
        }
    }

    async fn query(&self, ip: IpAddr) -> Result<Option<Location>> {
        match self {
            Self::IpApi(locator) => locator.query(ip).await,
            Self::KeyCdn(locator) => locator.query(ip).await,
            #[cfg(test)]
            Self::Stub(answers) => answers
                .lock()
                .unwrap()
                .pop_front()
                .expect("an unexpected location lookup"),
        }
    }
}

/// This server's record of where the instances attached to it are.
pub struct Locations {
    rows: ResidentVec<LocationData>,

    /// The server whose observations these are. Only rows scoped to it are ours
    /// to write — another server's row is a replica.
    observer: InstanceId,

    service: LocationService,
    locator: AnyLocator,

    /// When the last lookup for a subject failed, so a service that is down (or
    /// a key that is wrong) is retried on its own schedule rather than on every
    /// sweep. Deliberately in memory: a restart is as good a moment as any to
    /// try again.
    failures: RwLock<HashMap<InstanceId, DateTime<Utc>>>,
}

impl Locations {
    pub fn new(
        realm: &RealmDatabase,
        observer: InstanceId,
        config: &LocationConfig,
    ) -> Result<Self> {
        Self::with_locator(
            realm,
            observer,
            config.service.clone(),
            AnyLocator::new(config),
        )
    }

    fn with_locator(
        realm: &RealmDatabase,
        observer: InstanceId,
        service: LocationService,
        locator: AnyLocator,
    ) -> Result<Self> {
        Ok(Self {
            rows: realm.resident_vec(())?,
            observer,
            service,
            locator,
            failures: RwLock::new(HashMap::new()),
        })
    }

    /// The address this server last resolved a location for `subject` from, if
    /// it has. This is the whole of the "don't ask twice" memory, and it is
    /// persistent: a restart doesn't re-query the estate.
    fn located_address(&self, subject: InstanceId) -> Option<IpAddr> {
        self.row(subject).map(|row| row.read().address)
    }

    fn row(&self, subject: InstanceId) -> Option<sandpolis_instance::database::Resident<LocationData>> {
        self.rows.iter().find(|row| {
            let row = row.read();
            row._instance_id == self.observer && row.subject == subject
        })
    }

    /// Store where `subject`, connecting from `address`, turned out to be,
    /// replacing whatever this server said before.
    fn record(&self, subject: InstanceId, address: IpAddr, location: Location) -> Result<()> {
        match self.row(subject) {
            Some(row) => row.update(|d| {
                d.address = address;
                d.service = self.service.clone();
                d.location = location.clone();
                Ok(())
            })?,
            None => {
                self.rows.push(LocationData::observation(
                    self.observer,
                    subject,
                    address,
                    self.service.clone(),
                    location,
                ))?;
            }
        }

        Ok(())
    }

    /// Resolve every attached instance whose address this server hasn't already
    /// resolved.
    async fn reconcile(&self, attached: &[(InstanceId, IpAddr)], now: DateTime<Utc>) {
        let mut queried = false;

        for (subject, address) in attached {
            if !should_query(
                self.located_address(*subject),
                *address,
                self.failures.read().unwrap().get(subject).copied(),
                now,
            ) {
                continue;
            }

            // Spacing applies between lookups, not before the first one.
            if queried {
                sleep(QUERY_INTERVAL).await;
            }
            queried = true;

            match self.locator.query(*address).await {
                Ok(Some(location)) if location.is_empty() => {
                    // The service answered with a location holding nothing.
                    // Storing it would claim the instance was located.
                    debug!(instance = %subject, service = %self.service, "Location lookup resolved to nothing");
                    self.failures.write().unwrap().insert(*subject, now);
                }
                Ok(Some(location)) => {
                    debug!(
                        instance = %subject,
                        location = ?location.describe(),
                        "Resolved an instance's location"
                    );
                    self.failures.write().unwrap().remove(subject);
                    if let Err(e) = self.record(*subject, *address, location) {
                        warn!(error = %e, instance = %subject, "Failed to record an instance's location");
                    }
                }
                Ok(None) => {
                    // The service has nothing for this address and never will,
                    // so this is not worth asking again for an hour either.
                    trace!(instance = %subject, "No location for an instance's address");
                    self.failures.write().unwrap().insert(*subject, now);
                }
                Err(e) => {
                    warn!(error = %e, instance = %subject, service = %self.service, "Location lookup failed");
                    self.failures.write().unwrap().insert(*subject, now);
                }
            }
        }
    }
}

/// Whether to ask about `address` now.
///
/// A lookup is a request to a third party, so it happens once per address: the
/// stored row is the memory that it did, and a differing address means the
/// instance moved. `last_failure` keeps a service that isn't answering from
/// being asked once per instance per sweep, forever.
fn should_query(
    located: Option<IpAddr>,
    address: IpAddr,
    last_failure: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    if located == Some(address) {
        return false;
    }

    match last_failure {
        Some(at) => now - at >= RETRY_AFTER,
        None => true,
    }
}

/// Whether an address is worth asking a geolocation service about.
///
/// Everything that isn't routable on the public internet is somewhere the
/// service has never heard of: it would answer "reserved range" at best, and
/// invent a location for the whole estate at worst.
fn is_locatable(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            !(ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                // 100.64.0.0/10, carrier-grade NAT (`Ipv4Addr::is_shared` is
                // still unstable)
                || (a == 100 && (64..128).contains(&b))
                // 0.0.0.0/8, "this network"
                || a == 0)
        }
        IpAddr::V6(ip) => {
            // Addresses inside a v6 transition mechanism are judged by the v4
            // address they carry, which is the one a service could locate.
            if let Some(v4) = ip.to_ipv4_mapped() {
                return is_locatable(IpAddr::V4(v4));
            }

            let first = ip.segments()[0];
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                // fc00::/7, unique local
                || first & 0xfe00 == 0xfc00
                // fe80::/10, link local
                || first & 0xffc0 == 0xfe80)
        }
    }
}

/// The instances attached to this server in `realm` right now, with the address
/// each connected from.
///
/// Clients are kept, unlike in the liveness reconciler: where an operator
/// logged in from is exactly as interesting as where their agents are.
fn attached(network: &NetworkManager, realm: &RealmName) -> Vec<(InstanceId, IpAddr)> {
    network
        .live_inbound()
        .iter()
        .filter(|c| &c.realm == realm)
        .filter_map(|c| {
            let data = c.data.read();
            Some((data.remote_instance?, data.remote_socket?.ip()))
        })
        .filter(|(_, address)| is_locatable(*address))
        .collect()
}

/// Keep this server's location rows current for one realm.
///
/// Wakes on connection changes (a socket opening, or a janitor removing the row
/// of one that died) and on a slow timer.
pub async fn maintain_locations(
    network: NetworkManager,
    realm: RealmName,
    locations: Arc<Locations>,
) {
    let notify = Arc::new(Notify::new());
    {
        let notify = notify.clone();
        network.connections.listen(move |_| notify.notify_one());
    }

    loop {
        locations
            .reconcile(&attached(&network, &realm), Utc::now())
            .await;

        tokio::select! {
            _ = notify.notified() => sleep(DEBOUNCE).await,
            _ = sleep(SWEEP_INTERVAL) => {}
        }
    }
}

#[cfg(test)]
mod test_locations {
    use super::*;
    use sandpolis_instance::ClusterId;
    use sandpolis_instance::database::DatabaseManager;
    use sandpolis_instance::network::liveness::LivenessData;
    use sandpolis_instance::network::{
        ConnectionData, InstanceConnection, NetworkManagerData, StreamRegistry,
    };
    use sandpolis_instance::test_db;
    use std::collections::VecDeque;
    use tokio_util::sync::CancellationToken;

    fn server() -> InstanceId {
        sandpolis_instance::ServerId::random().into()
    }

    fn agent() -> InstanceId {
        sandpolis_instance::AgentId::random().into()
    }

    fn ip(address: &str) -> IpAddr {
        address.parse().unwrap()
    }

    /// A `Locations` that answers lookups from `answers` instead of a service.
    fn stubbed(
        realm: &RealmDatabase,
        observer: InstanceId,
        answers: Vec<Result<Option<Location>>>,
    ) -> Result<Locations> {
        Locations::with_locator(
            realm,
            observer,
            LocationService::IpApi,
            AnyLocator::Stub(std::sync::Mutex::new(VecDeque::from(answers))),
        )
    }

    #[test]
    fn as_code_is_parsed_out_of_the_as_field() {
        assert_eq!(as_code(Some("AS13335 Cloudflare, Inc.")), Some(13335));
        assert_eq!(as_code(Some("AS13335")), Some(13335));

        // Not announced in BGP, or simply not reported.
        assert_eq!(as_code(Some("")), None);
        assert_eq!(as_code(None), None);
        // Nothing to salvage from a shape we don't recognize.
        assert_eq!(as_code(Some("13335 Cloudflare, Inc.")), None);
    }

    #[test]
    fn blank_fields_are_not_values() {
        assert_eq!(non_empty(Some("".into())), None);
        assert_eq!(non_empty(Some("  ".into())), None);
        assert_eq!(non_empty(Some("Brisbane".into())), Some("Brisbane".into()));
    }

    fn somewhere() -> Location {
        Location {
            country: Some("Australia".into()),
            city: Some("South Brisbane".into()),
            ..Default::default()
        }
    }

    /// Nothing inside the estate's own network is worth a lookup.
    #[test]
    fn only_routable_addresses_are_located() {
        for address in [
            "127.0.0.1",
            "10.4.0.9",
            "172.16.0.1",
            "192.168.1.50",
            "169.254.3.4",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "ff02::1",
            // A private v4 address wearing a v6 hat is still private.
            "::ffff:10.4.0.9",
        ] {
            assert!(!is_locatable(ip(address)), "{address} should be skipped");
        }

        for address in ["1.1.1.1", "203.0.113.7", "2606:4700:4700::1111", "::ffff:1.1.1.1"] {
            assert!(is_locatable(ip(address)), "{address} should be located");
        }
    }

    /// An address is asked about once: the stored row is the memory of it.
    #[test]
    fn a_resolved_address_is_not_asked_about_again() {
        let now = Utc::now();
        assert!(!should_query(Some(ip("1.1.1.1")), ip("1.1.1.1"), None, now));
        assert!(should_query(Some(ip("1.1.1.1")), ip("8.8.8.8"), None, now));
        assert!(should_query(None, ip("1.1.1.1"), None, now));
    }

    /// A service that isn't answering is left alone, rather than asked again on
    /// every sweep for every instance attached.
    #[test]
    fn a_failed_lookup_is_backed_off() {
        let now = Utc::now();

        assert!(!should_query(
            None,
            ip("1.1.1.1"),
            Some(now - TimeDelta::minutes(5)),
            now
        ));
        assert!(should_query(
            None,
            ip("1.1.1.1"),
            Some(now - RETRY_AFTER),
            now
        ));
    }

    /// An instance that moves updates the row this server already wrote for it
    /// rather than collecting a second one.
    #[tokio::test]
    async fn a_moved_instance_replaces_its_own_row() -> Result<()> {
        let db: DatabaseManager = test_db!(LocationData);
        let realm = db.realm(RealmName::default())?;
        let (observer, subject) = (server(), agent());
        let locations = Locations::new(&realm, observer, &LocationConfig::default())?;

        assert_eq!(locations.located_address(subject), None);

        locations.record(subject, ip("1.1.1.1"), somewhere())?;
        assert_eq!(locations.located_address(subject), Some(ip("1.1.1.1")));
        assert_eq!(locations.rows.iter().count(), 1);

        locations.record(subject, ip("8.8.8.8"), somewhere())?;
        assert_eq!(locations.located_address(subject), Some(ip("8.8.8.8")));
        assert_eq!(locations.rows.iter().count(), 1);

        Ok(())
    }

    /// Another server's opinion of the same instance is not this server's to
    /// read or overwrite.
    #[tokio::test]
    async fn another_servers_row_is_left_alone() -> Result<()> {
        let db: DatabaseManager = test_db!(LocationData);
        let realm = db.realm(RealmName::default())?;
        let (us, them, subject) = (server(), server(), agent());

        let ours = Locations::new(&realm, us, &LocationConfig::default())?;
        let theirs = Locations::new(&realm, them, &LocationConfig::default())?;

        theirs.record(subject, ip("1.1.1.1"), somewhere())?;

        assert_eq!(ours.located_address(subject), None);
        ours.record(subject, ip("8.8.8.8"), somewhere())?;
        assert_eq!(ours.located_address(subject), Some(ip("8.8.8.8")));
        assert_eq!(theirs.located_address(subject), Some(ip("1.1.1.1")));

        Ok(())
    }

    /// What the service says about an attached instance becomes its row, and the
    /// address already resolved is not asked about a second time.
    #[tokio::test]
    async fn an_answered_lookup_becomes_a_row_once() -> Result<()> {
        let db: DatabaseManager = test_db!(LocationData);
        let realm = db.realm(RealmName::default())?;
        let (observer, subject) = (server(), agent());
        let locations = stubbed(&realm, observer, vec![Ok(Some(somewhere()))])?;
        let attached = [(subject, ip("1.1.1.1"))];

        locations.reconcile(&attached, Utc::now()).await;
        assert_eq!(locations.located_address(subject), Some(ip("1.1.1.1")));
        assert_eq!(
            locations.row(subject).unwrap().read().location.describe(),
            somewhere().describe()
        );

        // The stub is empty, so a second lookup would panic.
        locations.reconcile(&attached, Utc::now()).await;
        Ok(())
    }

    /// A service that answers with nothing — a range it doesn't cover, or a
    /// location holding no fields — must not leave a row claiming the instance
    /// was located.
    #[tokio::test]
    async fn an_empty_answer_is_not_a_row() -> Result<()> {
        let db: DatabaseManager = test_db!(LocationData);
        let realm = db.realm(RealmName::default())?;
        let observer = server();
        let (nothing, blank) = (agent(), agent());
        let locations = stubbed(
            &realm,
            observer,
            vec![Ok(None), Ok(Some(Location::default()))],
        )?;

        locations
            .reconcile(
                &[(nothing, ip("1.1.1.1")), (blank, ip("8.8.8.8"))],
                Utc::now(),
            )
            .await;

        assert_eq!(locations.located_address(nothing), None);
        assert_eq!(locations.located_address(blank), None);
        assert_eq!(locations.rows.iter().count(), 0);
        Ok(())
    }

    /// A failed lookup is left alone until the backoff expires, then retried.
    #[tokio::test]
    async fn a_failed_lookup_is_retried_only_after_the_backoff() -> Result<()> {
        let db: DatabaseManager = test_db!(LocationData);
        let realm = db.realm(RealmName::default())?;
        let (observer, subject) = (server(), agent());
        let locations = stubbed(
            &realm,
            observer,
            vec![
                Err(anyhow::anyhow!("the service is down")),
                Ok(Some(somewhere())),
            ],
        )?;
        let attached = [(subject, ip("1.1.1.1"))];
        let now = Utc::now();

        locations.reconcile(&attached, now).await;
        assert_eq!(locations.located_address(subject), None);

        // Within the backoff nothing is asked, which the exhausted stub would
        // otherwise catch.
        locations
            .reconcile(&attached, now + TimeDelta::minutes(5))
            .await;
        assert_eq!(locations.located_address(subject), None);

        locations.reconcile(&attached, now + RETRY_AFTER).await;
        assert_eq!(locations.located_address(subject), Some(ip("1.1.1.1")));
        Ok(())
    }

    /// The addresses to resolve come from the connections this server is
    /// holding, which is the only place a peer's address is known.
    #[tokio::test]
    async fn attached_reports_routable_peers_of_this_realm() -> Result<()> {
        let db: DatabaseManager = test_db!(ConnectionData, LivenessData, NetworkManagerData);
        let network = NetworkManager::new(db.clone()).await?;
        let observer = server();
        let other_realm: RealmName = "other".parse()?;

        let connect = |peer: Option<InstanceId>, socket: Option<&str>, realm: RealmName| {
            let mut data = ConnectionData::scoped(observer);
            data.remote_instance = peer;
            data.remote_socket = socket.map(|socket| socket.parse().unwrap());
            Ok::<_, anyhow::Error>(Arc::new(InstanceConnection {
                data: network.connections.push(data)?,
                realm,
                cluster_id: ClusterId::default(),
                cancel: CancellationToken::new(),
                streams: Arc::new(StreamRegistry::new(tokio::sync::mpsc::channel(1).0)),
                poll: Default::default(),
            }))
        };

        let routable = agent();
        let behind_nat = agent();
        let elsewhere = agent();
        let unidentified = connect(None, Some("203.0.113.9:40000"), RealmName::default())?;
        let closed = connect(Some(agent()), Some("203.0.113.8:40000"), RealmName::default())?;
        closed.cancel.cancel();

        network.inbound.write().unwrap().extend([
            connect(
                Some(routable),
                Some("203.0.113.7:40000"),
                RealmName::default(),
            )?,
            connect(Some(behind_nat), Some("10.0.0.5:40000"), RealmName::default())?,
            connect(Some(elsewhere), Some("203.0.113.6:40000"), other_realm)?,
            connect(Some(agent()), None, RealmName::default())?,
            unidentified,
            closed,
        ]);

        // Only the peer that is identified, routable, still live, and in this
        // realm has an address worth asking about.
        assert_eq!(
            attached(&network, &RealmName::default()),
            vec![(routable, ip("203.0.113.7"))]
        );
        Ok(())
    }
}
