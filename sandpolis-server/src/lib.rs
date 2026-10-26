use crate::banner::{GetBannerRequest, GetBannerResponse};
use crate::login::{LoginRequest, LoginResponse};
use anyhow::{Result, anyhow};
use axum::http::HeaderValue;
use cron::Schedule;
use native_db::ToKey;
use native_model::Model;
use reqwest::header::CONTENT_TYPE;
use reqwest::{ClientBuilder, Method};
use sandpolis_instance::database::DatabaseManager;
use sandpolis_instance::database::Resident;
use sandpolis_instance::database::ResidentVec;
use sandpolis_instance::network::{
    ConnectionData, InstanceConnection, NetworkManager, collected_responders,
};
use sandpolis_instance::realm::RealmName;
use sandpolis_instance::realm::RealmManager;

/// Server URLs are parsed out of certificate common names, so the type lives in
/// `sandpolis-instance` alongside the realm certificates. Re-exported here
/// because this is where callers expect to find it.
pub use sandpolis_instance::realm::url::ServerUrl;
use sandpolis_instance::{ClusterId, InstanceId, InstanceType};
use sandpolis_macros::data;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::fmt::Display;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use validator::Validate;

pub mod banner;
#[cfg(feature = "client")]
pub mod client;
pub mod config;
#[cfg(feature = "server")]
pub mod liveness;
pub mod location;
pub mod login;
#[cfg(feature = "server")]
pub mod ownership;
#[cfg(feature = "server")]
pub mod stratum;
pub mod user;

#[data]
#[derive(Default)]
pub struct ServerManagerData {}

#[derive(Clone)]
#[cfg_attr(feature = "client", derive(bevy::prelude::Resource))]
pub struct ServerManager {
    #[cfg(feature = "server")]
    pub banner: Resident<banner::ServerBannerData>,

    /// Which stratum this instance's server runs in. `Global` on instances that
    /// aren't running a server at all, which is inert since nothing consults it.
    pub stratum: ServerStratum,

    pub network: NetworkManager,
    pub realms: RealmManager,
    pub database: DatabaseManager,

    /// What this process is, which decides the certificate it presents when it
    /// dials a server.
    pub instance_type: InstanceType,

    #[cfg(feature = "client")]
    pub servers: ResidentVec<client::SavedServerData>,

    /// The per-instance ownership grant table (authoritative on the global
    /// stratum server, this server's own mirror on a local stratum server).
    #[cfg(feature = "server")]
    pub ownership: Arc<ownership::Ownership>,

    /// Outbound connections to servers
    pub outbound: Arc<RwLock<Vec<Arc<ServerConnection>>>>,

    /// The tokio runtime this manager was created under, so connections can be
    /// established from executors without a tokio context (the GUI polls
    /// [`connect`](Self::connect) futures on bevy's task pools).
    runtime: tokio::runtime::Handle,
}

impl ServerManager {
    pub async fn new(
        database: DatabaseManager,
        network: NetworkManager,
        realms: RealmManager,
        stratum: ServerStratum,
        instance_type: InstanceType,
    ) -> Result<Self> {
        // Purge stale ConnectionData rows left over from previous runs
        {
            let realm = database.realm(RealmName::default())?;
            let r = realm.r_transaction()?;
            let stale: Vec<ConnectionData> =
                r.scan().primary()?.all()?.collect::<Result<Vec<_>, _>>()?;
            drop(r);
            if !stale.is_empty() {
                // Connection rows describe this process's own sockets, so they
                // are local bookkeeping even on a replica.
                let rw = realm.local_write()?;
                for row in stale {
                    rw.remove(row)?;
                }
                rw.commit()?;
            }
        }

        Ok(Self {
            #[cfg(feature = "server")]
            banner: database.realm(RealmName::default())?.resident(())?,
            #[cfg(feature = "server")]
            ownership: {
                let ownership = Arc::new(ownership::Ownership::new(
                    &database.realm(RealmName::default())?,
                )?);

                // A local stratum server restores the scopes it owned before the
                // last shutdown, so a GS outage spanning a restart doesn't stop
                // it serving its instances.
                if let Some(table) = database.authority().scope_table() {
                    ownership.restore(table);
                }

                ownership
            },
            stratum,
            network,
            realms,
            instance_type,
            database: database.clone(),
            #[cfg(feature = "client")]
            servers: database.realm(RealmName::default())?.resident_vec(())?,
            outbound: Arc::new(RwLock::new(Vec::new())),
            runtime: tokio::runtime::Handle::current(),
        })
    }

    /// Get all server connections.
    ///
    /// There's nothing to filter: every entry in `outbound` was dialed by this
    /// instance, and the only thing an instance dials is a server. The peer's id
    /// isn't even known until its websocket upgrade reports it, which is after a
    /// client needs the entry.
    pub fn server_connections(&self) -> Vec<Arc<ServerConnection>> {
        self.outbound.read().unwrap().clone()
    }

    #[cfg(any(feature = "agent", feature = "client", feature = "server"))]
    fn connect_server(
        &self,
        url: ServerUrl,
        strategy: ServerConnectStrategy,
    ) -> Result<ServerConnection> {
        debug!(url = %url, ?strategy, "Configuring server connection");

        // Everything that dials a server presents the same kind of certificate,
        // including a local stratum server dialing its global stratum server
        // with one supplied via a realm cert.
        let cert = self.realms.find_endpoint_cert(url.realm.clone())?;

        let client_builder = || -> Result<reqwest::Client> {
            // Only the realm CA may vouch for a server, so skip the platform
            // trust store entirely — merging with it would also need
            // rustls-platform-verifier, which can't take extra roots on Android.
            Ok(ClientBuilder::new()
                .tls_certs_only([cert.root_certificate()?])
                .identity(cert.identity()?)
                .resolve_to_addrs(
                    &format!("{}.{}", cert.cluster_id()?, cert.name),
                    &url.resolve()?,
                )
                .build()?)
        };

        Ok(ServerConnection {
            inner: Arc::new(RwLock::new(None)),
            runtime: self.runtime.clone(),
            strategy,
            client: Arc::new(tokio::sync::RwLock::new(Some(client_builder()?))),
            cancel: CancellationToken::new(),
            banner: ServerBanner::default(),
            realm: cert.name.clone(),
            cluster_id: cert.cluster_id()?,
            url,
            token: Arc::new(RwLock::new(None)),
            #[cfg(feature = "server")]
            stratum: (self.instance_type == InstanceType::Server).then(|| self.stratum.clone()),
        })
    }

    /// Connect to a server in the default `Continuous` strategy (the live
    /// connection is held open by the websocket).
    #[cfg(any(feature = "agent", feature = "client", feature = "server"))]
    pub async fn connect(&self, url: ServerUrl) -> Result<ServerConnection> {
        self.connect_with_strategy(url, ServerConnectStrategy::Continuous)
            .await
    }

    /// Connect to a server with an explicit [`ServerConnectStrategy`]. Agents
    /// that only check in periodically pass `Polling`; everything else uses
    /// `Continuous` via [`connect`](Self::connect).
    #[cfg(any(feature = "agent", feature = "client", feature = "server"))]
    pub async fn connect_with_strategy(
        &self,
        url: ServerUrl,
        strategy: ServerConnectStrategy,
    ) -> Result<ServerConnection> {
        let this = self.clone();
        self.runtime
            .spawn(async move { this.connect_on_runtime(url, strategy).await })
            .await?
    }

    /// [`connect_with_strategy`](Self::connect_with_strategy) minus the hop
    /// onto the tokio runtime, which the sockets created here need.
    #[cfg(any(feature = "agent", feature = "client", feature = "server"))]
    async fn connect_on_runtime(
        &self,
        url: ServerUrl,
        strategy: ServerConnectStrategy,
    ) -> Result<ServerConnection> {
        let mut inner = self.connect_server(url, strategy)?;

        debug!("Fetching server banner");

        // Fetch banner before we return a complete connection
        let response: GetBannerResponse = inner
            .get(
                "server/banner",
                GetBannerRequest {
                    #[cfg(feature = "client")]
                    include_image: true,
                    #[cfg(not(feature = "client"))]
                    include_image: false,
                },
            )
            .await?;

        debug!(banner = ?response.0, "Fetched server banner");

        inner.banner = response.0;
        Ok(inner)
    }
}

/// Contains information about the server useful for prospective logins
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct ServerBanner {
    /// Indicates that only admin users will be allowed to login
    pub maintenance: bool,

    /// A string to display on the login screen
    pub message: Option<String>,

    /// An image to display on the login screen
    #[serde(with = "serde_bytes")]
    pub image: Option<Vec<u8>>,

    /// Whether users are required to provide a second authentication mechanism
    /// on login
    pub mfa: bool,

    /// Whether the realm has user accounts at all. A realm without users is
    /// open, and clients skip the login dialog entirely.
    pub users_configured: bool,
}

impl Validate for ServerBanner {
    fn validate(&self) -> Result<(), validator::ValidationErrors> {
        if let Some(image_data) = &self.image {
            // Validate PNG format using png crate
            let cursor = std::io::Cursor::new(image_data);
            let decoder = png::Decoder::new(cursor);

            if decoder.read_info().is_err() {
                return Err(validator::ValidationErrors::new());
            }
        }

        Ok(())
    }
}

#[derive(Clone)]
pub struct ServerConnection {
    client: Arc<tokio::sync::RwLock<Option<reqwest::Client>>>,

    /// The tokio runtime whose reactor this connection's sockets belong to;
    /// requests hop onto it so callers may poll from non-tokio executors
    /// (the GUI uses bevy's task pools).
    runtime: tokio::runtime::Handle,
    pub strategy: ServerConnectStrategy,
    pub cancel: CancellationToken,
    pub banner: ServerBanner,
    /// Active websocket connection used for streams / sync, once established.
    pub inner: Arc<RwLock<Option<Arc<InstanceConnection>>>>,
    pub realm: RealmName,
    pub cluster_id: ClusterId,
    /// The URL this connection dials, retained so clients can associate data
    /// (e.g. probes) with a particular server.
    pub url: ServerUrl,

    /// Auth token from `/user/login`, presented as a bearer token on every
    /// request once set. Empty on open realms and before login.
    pub token: Arc<RwLock<Option<user::ClientAuthToken>>>,

    /// The stratum of the server making this connection, announced to the peer
    /// so it can enforce that a network has exactly one global stratum server.
    /// `None` unless this process is actually running a server: a client or
    /// agent built with the `server` feature still has a (inert) stratum, and
    /// announcing it would make the peer mistake it for a second server.
    #[cfg(feature = "server")]
    pub stratum: Option<ServerStratum>,
}

impl Drop for ServerConnection {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl ServerConnection {
    /// The peer's instance id, known once the websocket upgrade has reported it.
    pub fn remote_instance(&self) -> Option<InstanceId> {
        self.inner
            .read()
            .unwrap()
            .as_ref()
            .and_then(|connection| connection.data.read().remote_instance)
    }

    /// Establish the websocket connection used for streams and DB sync, retaining
    /// it on this `ServerConnection`. The connection is deliberately *not* added
    /// to `network.inbound`: that list backs the server-side stream relay, which
    /// forwards to instances attached to *this* server. A connection this
    /// instance dialed points the other way.
    #[cfg(any(feature = "client", feature = "agent", feature = "server"))]
    pub async fn open_websocket(
        &self,
        network: &NetworkManager,
        instance: &sandpolis_instance::InstanceManager,
    ) -> Result<Arc<InstanceConnection>> {
        use reqwest_websocket::Upgrade;

        let instance_id = instance.instance_id;

        let url = format!("https://{}.{}/connect", self.cluster_id, self.realm);
        let response = {
            let guard = self.client.read().await;
            let client = guard
                .as_ref()
                .ok_or_else(|| anyhow!("connection has no http client"))?;
            let request = client
                .get(&url)
                .header("x-realm", self.realm.to_string())
                .header("x-instance-id", instance_id.to_string());

            let request = match self.token.read().unwrap().as_ref() {
                Some(token) => request.bearer_auth(&token.0),
                None => request,
            };

            // Servers announce their stratum so the peer can enforce the
            // network's shape; agents and clients send nothing here.
            #[cfg(feature = "server")]
            let request = match self.stratum.as_ref() {
                Some(stratum) => request.header("x-stratum", stratum.header_value()),
                None => request,
            };

            // A polling agent announces its schedule, because otherwise the
            // server cannot tell it apart from an agent that died: both look
            // like a connection that closed and did not come back.
            let request = match &self.strategy {
                ServerConnectStrategy::Polling { schedule, timeout } => request
                    .header("x-poll-schedule", schedule.to_string())
                    .header("x-poll-timeout", timeout.as_secs().to_string()),
                ServerConnectStrategy::Continuous => request,
            };

            request.upgrade().send().await?
        };

        // The server refusing the upgrade for authentication means the user has
        // to log in, which callers must be able to tell apart from the server
        // being down.
        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            || response.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(AuthRequired.into());
        }

        // The server reports its own instance id in the upgrade response so we can
        // record the real peer instead of a freshly-generated default (which would
        // surface as a phantom graph node, growing on every reconnect).
        let remote_instance = response
            .headers()
            .get("x-instance-id")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<InstanceId>().ok());

        let socket = response.into_websocket().await?;

        let mut cd = ConnectionData::scoped(instance_id);
        cd.remote_instance = remote_instance;
        cd.established = chrono::Utc::now();
        // Live connection bookkeeping is local state, allowed on a replica.
        let data = network
            .connections
            .push_local(cd)
            .map_err(|e| anyhow!("{e}"))?;

        // Serve our local realm database to the peer's sync subscriptions
        // (an agent answering the server's all-filter requester).
        let realm_db = network.database.realm(self.realm.clone())?;
        let sync_reg = sandpolis_instance::network::sync::SyncResponderRegistration::new(realm_db);
        let mut handlers: Vec<&dyn sandpolis_instance::network::RegisterResponders> =
            collected_responders().collect();
        handlers.push(&sync_reg);
        let connection = InstanceConnection::websocket_client(
            socket,
            data,
            self.realm.clone(),
            self.cluster_id,
            &handlers,
        );
        *self.inner.write().unwrap() = Some(connection.clone());

        // Clean up after the socket however it ends: the row leaves the database
        // (which is what wakes the `connections.listen` reconcilers) and the slot
        // goes empty, so the reconnect loops here and in the client see there is
        // work to do.
        {
            let cancel = connection.cancel.clone();
            let row = connection.data.read()._id;
            let connections = network.connections.clone();
            let inner = self.inner.clone();
            tokio::spawn(async move {
                cancel.cancelled().await;
                if let Err(e) = connections.remove_local(row) {
                    debug!(error = %e, "Failed to remove a closed connection");
                }
                let mut slot = inner.write().unwrap();
                if slot.as_ref().is_some_and(|c| c.cancel.is_cancelled()) {
                    *slot = None;
                }
            });
        }

        Ok(connection)
    }

    /// Close the sync websocket opened by [`open_websocket`](Self::open_websocket),
    /// if one is active. Agents in `Polling` mode call this to end a check-in
    /// window: the socket is cancelled and its bookkeeping (the tracked
    /// `ConnectionData` row) is cleaned up so repeated windows don't accumulate
    /// stale connections.
    #[cfg(any(feature = "client", feature = "agent", feature = "server"))]
    pub fn close_websocket(&self) {
        let Some(connection) = self.inner.write().unwrap().take() else {
            return;
        };

        // Cancel the socket task explicitly. `Drop` would also do this once every
        // Arc is gone, but a stream may still hold a reference. The janitor
        // spawned by `open_websocket` removes the row once this lands.
        connection.cancel.cancel();
    }

    pub async fn get<Response>(&self, endpoint: &str, body: impl Serialize) -> Result<Response>
    where
        Response: DeserializeOwned,
    {
        self.request(Method::GET, endpoint, body).await
    }

    pub async fn post<Response>(&self, endpoint: &str, body: impl Serialize) -> Result<Response>
    where
        Response: DeserializeOwned,
    {
        self.request(Method::POST, endpoint, body).await
    }

    pub async fn request<Response>(
        &self,
        method: Method,
        endpoint: &str,
        body: impl Serialize,
    ) -> Result<Response>
    where
        Response: DeserializeOwned,
    {
        // Serialize request and record bytes
        let body = serde_json::to_vec(&body)?;

        // One-off requests use the pooled `reqwest` client regardless of
        // strategy: it opens connections on demand, so this works the same
        // whether the agent holds a `Continuous` websocket or only connects
        // during a `Polling` window. The strategy only governs the lifetime of
        // the long-lived sync websocket (see `open_websocket`).
        debug!(endpoint = %endpoint, "Sending request");
        let client = self
            .client
            .read()
            .await
            .as_ref()
            .ok_or_else(|| anyhow!("connection has no http client"))?
            .clone();

        let request = client
            .request(
                method,
                format!("https://{}.{}/{endpoint}", self.cluster_id, self.realm),
            )
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .header("x-realm", self.realm.to_string());

        let request = match self.token.read().unwrap().as_ref() {
            Some(token) => request.bearer_auth(&token.0),
            None => request,
        };

        // The send opens sockets, which need the tokio reactor even when the
        // caller polls from a non-tokio executor; deserialization stays out
        // here so `Response` needs no `Send + 'static` bounds.
        let (status, bytes) = self
            .runtime
            .spawn(async move {
                let response = request.body(body).send().await?;
                let status = response.status();
                let bytes = response.bytes().await?;
                Ok::<_, anyhow::Error>((status, bytes))
            })
            .await??;

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(AuthRequired.into());
        }

        Ok(serde_json::from_slice(&bytes)?)
    }
}

/// The server refused the request or websocket upgrade for lack of a valid auth
/// token: the caller should have the user log in (or log in again) and retry.
#[derive(Debug)]
pub struct AuthRequired;

impl Display for AuthRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("authentication required")
    }
}

impl std::error::Error for AuthRequired {}

impl ServerConnection {
    pub async fn login(&self, request: LoginRequest) -> Result<LoginResponse> {
        debug!(username = %request.username, "Attempting login");

        let result = self.post("user/login", request).await;
        if let Ok(LoginResponse::Ok(token)) = &result {
            info!("Login succeeded");
            *self.token.write().unwrap() = Some(token.clone());
        }
        result
    }
}

/// In continuous mode, the agent maintains its primary connection at all times.
/// If the connection is lost, the agent will periodically attempt to
/// reestablish the connection using the same parameters it used to establish
/// the initial connection.
///
/// The connection mode can be changed on-the-fly by a user or scheduled to
/// change automatically according to the time and day.
///
/// In polling mode, the agent intentionally closes the primary connection
/// unless there exists an active stream. On a configurable schedule, the agent
/// reconnects to a server, flushes any cached data, and checks for any new work
/// items. After executing all available work items, the primary connection is
/// closed again.
///
/// The agent may attempt a spontaneous connection outside of the regular
/// schedule if an internal agent process triggers it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub enum ServerConnectStrategy {
    /// The connection will never intentionally disconnect. This is best when
    /// latency is important.
    #[default]
    Continuous,

    /// The connection will intentionally disconnect. This is best when latency
    /// is not important.
    Polling {
        /// Cron schedule for when to poll (e.g., "0 */5 * * * *" for every 5
        /// minutes)
        schedule: Schedule,

        /// How long the connection will stay alive
        timeout: Duration,
    },
}

impl ServerConnectStrategy {
    /// Build a [`Polling`](ServerConnectStrategy::Polling) strategy from a cron
    /// expression and a per-window keep-alive duration. Lets callers construct
    /// the strategy without depending on the `cron` crate directly.
    pub fn polling(schedule: &str, timeout: Duration) -> Result<Self> {
        Ok(Self::Polling {
            schedule: Schedule::from_str(schedule)
                .map_err(|e| anyhow!("invalid cron schedule {schedule:?}: {e}"))?,
            timeout,
        })
    }
}

/// The role a server plays in a Sandpolis network.
///
/// A network has **exactly one** global stratum (GS) server and **any number**
/// of local stratum (LS) servers. The distinction decides three things:
///
/// - **Configuration.** Only the GS reads realm configs, which declare the
///   realms it serves. Every other instance — LS servers, agents, clients — is
///   configured by CLI flags plus the realm cert naming the server it
///   trusts.
/// - **Writability.** The GS holds full write authority over the estate. An LS
///   holds scoped authority ([`WriteAuthority::Scoped`]): it owns the data of
///   the instances directly connected to it — as granted by the GS — and
///   writes to anything else must happen at the owner and arrive back through
///   replication.
/// - **Scope.** An LS holds the data belonging to its own instances plus a
///   replica of the estate-wide data, not the whole estate.
///
/// [`WriteAuthority::Scoped`]: sandpolis_instance::database::WriteAuthority
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub enum ServerStratum {
    /// The single trust root, holding the whole estate and full write authority
    /// over everything not owned by a local stratum server. Owns the config
    /// file.
    #[default]
    Global,

    /// An optional edge server owning exactly the instances attached to it.
    ///
    /// An LS connects to exactly one GS and never to another LS. It is useful
    /// for on-premise installations, where it keeps serving (and recording for)
    /// the instances around it even while the link to the GS is down.
    Local {
        // TODO remove
        /// The global stratum server this one enrolls with and replicates from.
        global: ServerUrl,
    },
}

impl ServerStratum {
    pub fn is_global(&self) -> bool {
        matches!(self, Self::Global)
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local { .. })
    }

    /// The upstream global stratum server, if this is a local stratum server.
    pub fn global_url(&self) -> Option<&ServerUrl> {
        match self {
            Self::Global => None,
            Self::Local { global } => Some(global),
        }
    }

    /// The value this server sends in the `x-stratum` header, and that a peer
    /// checks to enforce "exactly one GS".
    pub fn header_value(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Local { .. } => "local",
        }
    }
}

// TODO GS or LS
impl Display for ServerStratum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Global => write!(f, "global stratum"),
            Self::Local { global } => write!(f, "local stratum (via {global})"),
        }
    }
}
