use super::config::UsersConfig;
use super::{ClientAuthToken, UserData, UserManager, UserName};
use crate::login::LoginPassword;
use anyhow::{Result, anyhow, bail};
use aws_lc_rs::pbkdf2;
use axum::extract::{ConnectInfo, FromRequestParts, State, WebSocketUpgrade};
use axum::http::{StatusCode, request::Parts};
use axum::RequestPartsExt;
use axum_extra::TypedHeader;
use axum_extra::headers::{Authorization, authorization::Bearer};
use jsonwebtoken::{Validation, decode};
use native_db::ToKey;
use native_model::Model;
use rand::RngExt;
use sandpolis_instance::Permission;
use sandpolis_instance::database::DataScope;
use sandpolis_instance::network::ConnectionData;
use sandpolis_instance::network::InstanceConnection;
use sandpolis_instance::realm::RealmName;
use sandpolis_instance::realm::server::AuthenticatedRealm;
use sandpolis_macros::data;
use serde::{Deserialize, Serialize};
use std::fmt::{Debug, Display};
use std::num::NonZeroU32;
use totp_rs::{Builder, Secret};
use tracing::{info, warn};
use validator::Validate;

const SHA256_OUTPUT_LEN: usize = 32;

static USER_PASSWORD_HASH_ITERATIONS: NonZeroU32 = NonZeroU32::new(15000).unwrap();

#[data]
#[derive(Default)]
pub struct ServerJwtSecret {
    #[serde(with = "serde_bytes")]
    pub value: [u8; 32],
}

impl ServerJwtSecret {
    pub fn new() -> Self {
        Self {
            value: rand::rng().random::<[u8; 32]>(),
            ..Default::default()
        }
    }
}

#[data(temporal)]
#[derive(Validate, Default)]
pub struct PasswordData {
    /// User that this password belongs to
    #[secondary_key]
    pub user: UserName,

    /// Number of rounds to use when hashing password
    #[validate(range(min = 4284, max = 200000))]
    pub iterations: u32,

    /// Random data used to salt the password hash
    pub salt: Vec<u8>,

    /// Password hash
    pub hash: Vec<u8>,

    /// TOTP secret token
    pub totp_secret: Option<String>,
}

impl UserManager {
    pub async fn user(&self, realm: &RealmName, username: &UserName) -> Result<UserData> {
        let users = self
            .users
            .get(realm)
            .ok_or_else(|| anyhow!("Realm not found"))?;

        for user in users.iter() {
            if user.read().username == *username {
                return Ok(user.read().clone());
            }
        }

        bail!("User not found");
    }

    /// Whether the realm has any user accounts. A realm without users is open:
    /// no login is required beyond the realm certificate.
    pub fn users_configured(&self, realm: &RealmName) -> bool {
        self.users.get(realm).is_some_and(|users| users.len() > 0)
    }

    /// Whether the realm config requires every user to enroll a TOTP secret.
    /// Only the global stratum server knows the config.
    pub fn totp_required(&self, realm: &RealmName) -> bool {
        self.configs.get(realm).is_some_and(|config| config.totp)
    }

    /// The maximum (and default) token lifetime for the realm.
    pub fn token_lifetime(&self, realm: &RealmName) -> std::time::Duration {
        self.configs
            .get(realm)
            .and_then(|config| config.token_lifetime)
            .unwrap_or(UsersConfig::DEFAULT_TOKEN_LIFETIME)
    }

    /// Reconcile the realm config's user list into the realm database, which
    /// makes the config the sole authority over accounts: users found here and
    /// not there are created, changed fields are applied, and users removed from
    /// the config are deleted along with their password hash and TOTP secret —
    /// so re-adding the name later starts over at the first-login flow.
    pub async fn sync_users(&self, realm: &RealmName, config: &UsersConfig) -> Result<()> {
        let users = self
            .users
            .get(realm)
            .ok_or_else(|| anyhow!("Realm not found"))?;

        for user in users.iter() {
            let data = user.read().clone();
            if !config
                .users
                .iter()
                .any(|configured| configured.username == data.username)
            {
                info!(realm = %realm, username = %data.username, "Removing user absent from realm config");
                users.remove(data._id)?;
                self.delete_passwords(realm, &data.username)?;
            }
        }

        // A permission that grants nothing in this build is almost always a
        // typo, which would otherwise surface as a silently locked-out user.
        let known: Vec<&Permission> = self.stream_permissions.values().flatten().collect();

        for configured in &config.users {
            configured
                .username
                .validate()
                .map_err(|_| anyhow!("Invalid username in realm config: {}", configured.username))?;

            for permission in &configured.permissions {
                if !known.is_empty() && !known.iter().any(|required| permission.grants(required)) {
                    tracing::warn!(
                        realm = %realm,
                        username = %configured.username,
                        permission = %permission,
                        "Configured permission matches nothing in this build"
                    );
                }
            }

            let existing = users
                .iter()
                .find(|user| user.read().username == configured.username);

            match existing {
                Some(user) => {
                    user.update(|data| {
                        data.email = configured.email.clone();
                        data.phone = configured.phone.clone();
                        data.expiration = configured.expiration;
                        data.permissions = configured.permissions.clone();
                        Ok(())
                    })?;
                }
                None => {
                    info!(realm = %realm, username = %configured.username, "Creating user from realm config");
                    users.push(UserData {
                        username: configured.username.clone(),
                        email: configured.email.clone(),
                        phone: configured.phone.clone(),
                        expiration: configured.expiration,
                        permissions: configured.permissions.clone(),
                        ..Default::default()
                    })?;
                }
            }
        }

        Ok(())
    }

    /// Delete every password row belonging to the user, hash and TOTP secret
    /// included.
    fn delete_passwords(&self, realm: &RealmName, user: &UserName) -> Result<()> {
        let db = self.database.realm(realm.clone())?;
        let rw = db.write(DataScope::Global)?;

        let passwords: Vec<PasswordData> = rw
            .scan()
            .secondary(PasswordDataKey::user)?
            .equal(user.clone())?
            .collect::<Result<Vec<_>, _>>()?;

        for password in passwords {
            rw.remove(password)?;
        }
        rw.commit()?;

        Ok(())
    }

    /// Create a new password without a TOTP.
    pub async fn new_password(
        &self,
        realm: &RealmName,
        user: UserName,
        password: LoginPassword,
    ) -> Result<PasswordData> {
        let salt = rand::rng().random::<[u8; 32]>().to_vec();
        let mut hash = [0u8; SHA256_OUTPUT_LEN];

        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            USER_PASSWORD_HASH_ITERATIONS,
            &salt,
            password.0.as_bytes(),
            &mut hash,
        );

        let db = self.database.realm(realm.clone())?;
        let rw = db.write(DataScope::Global)?;

        let password = PasswordData {
            user,
            iterations: USER_PASSWORD_HASH_ITERATIONS.get(),
            salt,
            hash: hash.to_vec(),
            totp_secret: None,
            ..Default::default()
        };
        rw.insert(password.clone())?;
        rw.commit()?;

        Ok(password)
    }

    /// Create a new password with a TOTP.
    pub async fn new_password_with_totp(
        &self,
        realm: &RealmName,
        user: UserName,
        password: LoginPassword,
    ) -> Result<PasswordData> {
        let salt = rand::rng().random::<[u8; 32]>().to_vec();
        let mut hash = [0u8; SHA256_OUTPUT_LEN];

        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            USER_PASSWORD_HASH_ITERATIONS,
            &salt,
            password.0.as_bytes(),
            &mut hash,
        );

        let db = self.database.realm(realm.clone())?;
        let rw = db.write(DataScope::Global)?;

        let password = PasswordData {
            iterations: USER_PASSWORD_HASH_ITERATIONS.get(),
            salt,
            hash: hash.to_vec(),
            totp_secret: Some(
                Builder::new()
                    .with_algorithm(totp_rs::Algorithm::SHA1)
                    .with_digits(6)
                    .with_skew(1)
                    .with_step_duration(30)
                    .with_secret(Secret::default())
                    .with_issuer(Some("Sandpolis"))
                    .with_account_name(user.to_string())
                    .build()?
                    .to_url()?,
            ),
            user,
            ..Default::default()
        };
        rw.insert(password.clone())?;
        rw.commit()?;

        Ok(password)
    }

    /// Attach a freshly generated TOTP secret to an existing password, for
    /// accounts that set their password before the realm config began requiring
    /// TOTP. The hash carries over; only the secret is new.
    pub async fn add_totp(&self, realm: &RealmName, current: PasswordData) -> Result<PasswordData> {
        let db = self.database.realm(realm.clone())?;
        let rw = db.write(DataScope::Global)?;

        let mut password = current;
        password.totp_secret = Some(
            Builder::new()
                .with_algorithm(totp_rs::Algorithm::SHA1)
                .with_digits(6)
                .with_skew(1)
                .with_step_duration(30)
                .with_secret(Secret::default())
                .with_issuer(Some("Sandpolis"))
                .with_account_name(password.user.to_string())
                .build()?
                .to_url()?,
        );

        rw.upsert(password.clone())?;
        rw.commit()?;

        Ok(password)
    }

    /// The user's current password, or `None` if one was never set (the
    /// first-login case).
    pub async fn password(&self, realm: &RealmName, user: UserName) -> Result<Option<PasswordData>> {
        use sandpolis_instance::database::DataRevision;

        let db = self.database.realm(realm.clone())?;
        let r = db.r_transaction()?;

        let mut passwords: Vec<PasswordData> = r
            .scan()
            .secondary(PasswordDataKey::user)?
            .equal(user)?
            .collect::<Result<Vec<_>, _>>()?;

        // Only the latest revision is the password; older rows are history
        // retained by the temporal machinery.
        passwords.retain(|password| matches!(password._revision, DataRevision::Latest(_)));
        passwords.sort_by_key(|password| password._creation.timestamp());

        Ok(passwords.pop())
    }

    pub fn new_token(&self, claims: Claims) -> Result<ClientAuthToken> {
        Ok(ClientAuthToken(jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &claims,
            &self
                .jwt_keys
                .get(&claims.realm)
                .ok_or(anyhow!("Realm not found"))?
                .0,
        )?))
    }
}

impl Display for PasswordData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasswordData")
            .field("iterations", &self.iterations)
            .field("salt", &self.salt)
            .finish()
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    /// Username
    pub sub: UserName,

    /// Claim expiration
    pub exp: usize,

    /// Permissions held when the token was minted. Live checks read the current
    /// `UserData` instead, so a config change isn't outrun by an old token.
    pub perms: Vec<Permission>,

    /// Realm in which these claims exist
    pub realm: RealmName,
}

impl FromRequestParts<UserManager> for Claims {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &UserManager,
    ) -> Result<Self, Self::Rejection> {
        // Extract the token from the authorization header
        let TypedHeader(Authorization(bearer)) = parts
            .extract::<TypedHeader<Authorization<Bearer>>>()
            .await
            .map_err(|_| StatusCode::BAD_REQUEST)?;

        // The realm is the connection's, not the request's: a token is only
        // ever valid in the realm whose certificate authenticated the peer
        // presenting it, and that realm's key is what verifies it.
        let AuthenticatedRealm(realm) = parts
            .extract::<AuthenticatedRealm>()
            .await
            .map_err(|(status, _)| status)?;

        let token_data = decode::<Claims>(
            bearer.token(),
            &state.jwt_keys.get(&realm).ok_or(StatusCode::BAD_REQUEST)?.1,
            &Validation::default(),
        )
        .map_err(|_| StatusCode::FORBIDDEN)?;

        Ok(token_data.claims)
    }
}

/// Accept a websocket from a client, agent, or local stratum server.
///
/// Authentication is by realm cert, and the realm this connection lands in is
/// the one that certificate names (resolved upstream by `auth_middleware`) —
/// never one the peer asks for, since that realm decides which database it gets
/// to serve and subscribe to. The peer reports its own `InstanceId` via `x-instance-id`
/// so the server can tell agents (whose own data it pulls, directly or through
/// the ownership machinery) from clients (which subscribe to subsets). The
/// resulting connection is retained in `network.inbound`; dropping it would
/// cancel the socket.
///
/// The peer also reports its stratum via `x-stratum`, which enforces the shape of
/// the network: there is exactly one global stratum server, and local stratum
/// servers only ever connect upward.
// TODO: verify the reported instance id against the connection's certificate.
#[axum_macros::debug_handler]
pub async fn connect(
    State(state): State<UserManager>,
    AuthenticatedRealm(realm): AuthenticatedRealm,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let network = state.network.clone();
    let cluster_id = state.instance.cluster_id;
    let local_instance = state.instance.instance_id;
    let stratum = state.stratum.clone();
    let ownership = state.ownership.clone();
    let remote_instance = headers
        .get("x-instance-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<sandpolis_instance::InstanceId>().ok());
    let peer_stratum = headers
        .get("x-stratum")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // A polling peer says how often it means to come back, which is the only
    // way this server can tell "away until the next window" from "gone".
    let poll = headers
        .get("x-poll-schedule")
        .and_then(|v| v.to_str().ok())
        .filter(|schedule| {
            !schedule.is_empty()
                && schedule.len()
                    <= sandpolis_instance::network::liveness::MAX_SCHEDULE_LEN
        })
        .map(|schedule| sandpolis_instance::network::liveness::PollAnnouncement {
            schedule: schedule.to_string(),
            timeout: std::time::Duration::from_secs(
                headers
                    .get("x-poll-timeout")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
            ),
        });

    // A peer identifying as a server, by its instance id or its stratum header.
    // The header is what a *server* build sends; the id bit is the fallback.
    let peer_is_server =
        !peer_stratum.is_empty() || remote_instance.is_some_and(|id| id.is_server());

    // Anything that isn't a server or a self-identified agent is treated as a
    // client, so an unidentified peer lands on the most restricted path. (The
    // id is self-reported; tying it to the connection's certificate is the
    // standing TODO above.)
    let peer_is_client = !peer_is_server && !remote_instance.is_some_and(|id| id.is_agent());

    // What this peer is called in the logs, from the same classification the
    // rest of the handler runs on.
    let peer_kind = if peer_is_server {
        "server"
    } else if peer_is_client {
        "client"
    } else {
        "agent"
    };

    // When the realm has user accounts, a client connection must carry a token
    // from `/user/login`; its user decides which streams the connection may
    // open. A realm with no users is open, preserving the zero-setup workflow.
    let user_gate = if peer_is_client && state.users_configured(&realm) {
        let token = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));

        let claims = token
            .zip(state.jwt_keys.get(&realm))
            .and_then(|(token, (_, decoding))| {
                decode::<Claims>(token, decoding, &Validation::default()).ok()
            })
            .map(|data| data.claims);

        let Some(claims) = claims else {
            // Canonical line matched by the shipped fail2ban filter
            warn!(peer = %peer.ip(), "Authentication failure");
            return (StatusCode::UNAUTHORIZED, "login required").into_response();
        };

        // The gate reads the live user record on every check rather than the
        // token's snapshot, so a config change is not outrun by an old token.
        let username = claims.sub;
        let users = state.users.get(&realm).cloned();
        let stream_permissions = state.stream_permissions.clone();
        Some(std::sync::Arc::new(move |tag: u32| {
            // A stream type no layer declared is closed to clients entirely.
            let Some(requirement) = stream_permissions.get(&tag) else {
                return false;
            };
            // Declared with no permission: infrastructure (sync, ping).
            let Some(required) = requirement else {
                return true;
            };
            users.as_ref().is_some_and(|users| {
                users.iter().any(|user| {
                    let user = user.read();
                    user.username == username
                        && user
                            .permissions
                            .iter()
                            .any(|granted| granted.grants(required))
                })
            })
        }) as std::sync::Arc<dyn Fn(u32) -> bool + Send + Sync>)
    } else {
        None
    };

    // A network has exactly one global stratum server, so another one announcing
    // itself is a misconfiguration rather than a topology to accommodate.
    if peer_stratum == "global" {
        tracing::warn!(
            "Rejecting connection from another global stratum server; a network has exactly one"
        );
        return (
            StatusCode::CONFLICT,
            "this network already has a global stratum server",
        )
            .into_response();
    }

    // Local stratum servers connect upward only, never to each other, so an
    // inbound server connection here means someone is pointed at the wrong host.
    if stratum.is_local() && peer_is_server {
        tracing::warn!(
            "Rejecting inbound server connection: local stratum servers connect upward only"
        );
        return (
            StatusCode::CONFLICT,
            "connect servers to the global stratum server, not a local stratum server",
        )
            .into_response();
    }

    let mut response = ws.on_upgrade(move |socket| async move {
        let mut cd = ConnectionData::scoped(local_instance);
        cd.remote_instance = remote_instance;
        cd.established = chrono::Utc::now();
        let instance = cd.remote_instance;

        // Live connection bookkeeping is local state, allowed on a replica.
        let data = network.connections.push_local(cd).unwrap();

        info!(
            kind = peer_kind,
            instance = ?instance,
            realm = %realm,
            peer = %peer,
            "Instance connected"
        );

        // Serve this peer's subscriptions from our local realm database.
        let realm_db = network.database.realm(realm.clone()).unwrap();
        let sync_reg =
            sandpolis_instance::network::sync::SyncResponderRegistration::new(realm_db.clone());

        let mut handlers: Vec<&dyn sandpolis_instance::network::RegisterResponders> =
            sandpolis_instance::network::collected_responders().collect();
        handlers.push(&sync_reg);

        let connection = InstanceConnection::websocket(socket, data, realm, cluster_id, &handlers);

        if let Some(poll) = poll {
            let _ = connection.poll.set(poll);
        }

        // Let this connection relay streams to other connections (client -> agent).
        connection
            .streams
            .set_relay(std::sync::Arc::downgrade(&network.relay));

        // Streams this client opens (relayed or answered here) are checked
        // against its user's permissions.
        if let Some(gate) = user_gate {
            connection.streams.set_gate(gate);
        }

        // Likewise server-only: advertising lets a peer claim to carry traffic
        // for other instances, and an ownership claim carries the right to
        // write an instance's data into the estate — an agent or client must
        // never be able to do either.
        if peer_is_server {
            sandpolis_instance::network::reachability::accept_advertisements(
                &connection,
                network.relay.clone(),
            );
            crate::ownership::accept_claims(&connection, ownership.clone(), realm_db.clone());
        }

        // Pull everything an attached agent owns (the long-lived sync). The
        // filter is scoped to the agent's own instance, so a peer can never
        // smuggle in records belonging to someone else. A server peer's data
        // arrives through the ownership machinery instead.
        if let Some(id) = remote_instance.filter(|id| id.is_agent()) {
            if stratum.is_local() {
                // Ownership decides: the reconciler (`ownership::maintain_agent_sync`)
                // opens this pull once the scope is granted and hydrated.
            } else if let Err(e) = connection
                .open_sync(
                    realm_db,
                    vec![sandpolis_instance::database::sync::SyncFilter::instance(id)],
                )
                .await
            {
                tracing::warn!(error = %e, "Failed to open agent sync stream");
            }
        }

        network.track_inbound(connection);
    });

    // Report our own instance id back to the dialer (mirrors the `x-instance-id`
    // request header) so it can record `remote_instance` instead of leaving a
    // freshly-generated default, which would surface as a phantom graph node.
    if let Ok(value) = axum::http::HeaderValue::from_str(&local_instance.to_string()) {
        response.headers_mut().insert("x-instance-id", value);
    }

    response
}
