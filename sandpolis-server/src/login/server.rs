use super::{LoginAttemptData, LoginRequest, LoginResponse};
use crate::user::UserManager;
use crate::user::server::Claims;
use aws_lc_rs::pbkdf2;
use axum::Json;
use axum::extract::{self, ConnectInfo, State};
use sandpolis_instance::network::RequestResult;
use sandpolis_instance::realm::RealmName;
use sandpolis_instance::realm::server::AuthenticatedRealm;
use std::net::SocketAddr;
use std::time::SystemTime;
use totp_rs::Totp;
use tracing::{debug, error, info, warn};
use validator::Validate;

#[axum_macros::debug_handler]
pub async fn post_login(
    state: State<UserManager>,
    AuthenticatedRealm(realm): AuthenticatedRealm,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    extract::Json(request): extract::Json<LoginRequest>,
) -> RequestResult<LoginResponse> {
    request
        .validate()
        .map_err(|_| Json(LoginResponse::Invalid))?;

    let Ok(user) = state.user(&realm, &request.username).await else {
        debug!(username = %request.username, "User does not exist");
        record_attempt(&state, &realm, &request, peer, false);
        return Err(Json(LoginResponse::Denied));
    };

    if user
        .expiration
        .is_some_and(|expiration| expiration <= chrono::Utc::now().timestamp())
    {
        debug!(username = %request.username, "User account is expired");
        record_attempt(&state, &realm, &request, peer, false);
        return Err(Json(LoginResponse::Expired));
    }

    let totp_required = state.totp_required(&realm);

    // Clamp the requested token lifetime to the realm's maximum, which is also
    // the default when the client requests nothing.
    let max_lifetime = state.token_lifetime(&realm);
    let lifetime = request.lifetime.unwrap_or(max_lifetime).min(max_lifetime);

    let Ok(password) = state.password(&realm, request.username.clone()).await else {
        error!("Failed to get user password");
        return Err(Json(LoginResponse::Invalid));
    };

    let password = match password {
        Some(password) => password,
        None => {
            // First login: the config declared the user but nobody has set a
            // password yet. This is first-come-first-served, gated only by
            // possession of the realm certificate and the username.
            if !request.setup {
                return Ok(Json(LoginResponse::PasswordSetupRequired { totp_required }));
            }

            let stored = if totp_required {
                state
                    .new_password_with_totp(&realm, request.username.clone(), request.password.clone())
                    .await
            } else {
                state
                    .new_password(&realm, request.username.clone(), request.password.clone())
                    .await
            }
            .map_err(|e| {
                error!(error = %e, "Failed to store initial password");
                Json(LoginResponse::Invalid)
            })?;

            info!(username = %request.username, "Initial password set");
            match stored.totp_secret {
                Some(otpauth_url) => {
                    // The user still has to prove the enrollment by logging in
                    // with a code, which the client does right after this.
                    return Ok(Json(LoginResponse::TotpSetupRequired { otpauth_url }));
                }
                None => {
                    record_attempt(&state, &realm, &request, peer, true);
                    return mint(&state, &realm, user, lifetime);
                }
            }
        }
    };

    // Check TOTP token if there is one
    if let Some(totp_url) = password.totp_secret.as_ref() {
        let Ok(totp) = Totp::from_url(totp_url) else {
            error!("Failed to parse stored TOTP secret");
            return Err(Json(LoginResponse::Invalid));
        };
        if totp
            .check_current(request.totp_token.as_deref().unwrap_or_default())
            .is_none()
        {
            debug!("TOTP check failed");
            record_attempt(&state, &realm, &request, peer, false);
            return Err(Json(LoginResponse::Denied));
        }
    }

    // Check password
    // TODO argon2
    if pbkdf2::verify(
        pbkdf2::PBKDF2_HMAC_SHA256,
        std::num::NonZero::new(password.iterations).unwrap(),
        &password.salt,
        request.password.0.as_bytes(),
        &password.hash,
    )
    .is_err()
    {
        debug!("Password check failed");
        record_attempt(&state, &realm, &request, peer, false);
        return Err(Json(LoginResponse::Denied));
    }

    // The account predates the config requiring TOTP, so enroll it now that the
    // password has been verified.
    if totp_required && password.totp_secret.is_none() {
        let enrolled = state.add_totp(&realm, password).await.map_err(|e| {
            error!(error = %e, "Failed to generate TOTP secret");
            Json(LoginResponse::Invalid)
        })?;
        return Ok(Json(LoginResponse::TotpSetupRequired {
            otpauth_url: enrolled
                .totp_secret
                .expect("add_totp always sets a secret"),
        }));
    }

    record_attempt(&state, &realm, &request, peer, true);
    mint(&state, &realm, user, lifetime)
}

fn mint(
    state: &UserManager,
    realm: &RealmName,
    user: crate::user::UserData,
    lifetime: std::time::Duration,
) -> RequestResult<LoginResponse> {
    let claims = Claims {
        sub: user.username.clone(),
        exp: (SystemTime::now() + lifetime)
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as usize,
        perms: user.permissions,
        realm: realm.clone(),
    };

    info!(claims = ?claims, "Login succeeded");
    Ok(Json(LoginResponse::Ok(
        state
            .new_token(claims)
            .map_err(|_| Json(LoginResponse::Denied))?,
    )))
}

/// Record the attempt for the audit trail. Best-effort: a full audit log must
/// never be able to lock users out.
///
/// Failures also log a canonical `Authentication failure` line with the peer
/// address, which the shipped fail2ban filter matches (see `fail2ban/`).
fn record_attempt(
    state: &UserManager,
    realm: &RealmName,
    request: &LoginRequest,
    peer: SocketAddr,
    allowed: bool,
) {
    if !allowed {
        warn!(peer = %peer.ip(), username = %request.username, "Authentication failure");

        // The realm config decides whether the user hears about it too. A local
        // stratum server has no realm configs, so it defaults to notifying.
        if state
            .configs
            .get(realm)
            .is_none_or(|config| config.notify_login_failures)
        {
            sandpolis_instance::notification::notify(
                sandpolis_instance::notification::Notification::warn(
                    "Server",
                    format!("Login failure for {}", request.username),
                )
                .body(format!("A login attempt from {} failed", peer.ip())),
            );
        }
    }

    let result = || -> anyhow::Result<()> {
        let db = state.database.realm(realm.clone())?;
        // Attempts against this server are its own bookkeeping, not estate data.
        let rw = db.local_write()?;
        rw.insert(LoginAttemptData {
            timestamp: chrono::Utc::now().timestamp() as u64,
            username: request.username.clone(),
            source: Some(peer),
            allowed,
            ..Default::default()
        })?;
        rw.commit()?;
        Ok(())
    }();

    if let Err(e) = result {
        warn!(error = %e, "Failed to record login attempt");
    }
}
