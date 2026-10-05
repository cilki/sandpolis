use crate::ServerBanner;
#[cfg(feature = "server")]
use crate::ServerManager;
#[cfg(feature = "server")]
use crate::user::UserManager;
#[cfg(feature = "server")]
use axum::{Json, extract, extract::State};
use native_db::ToKey;
use native_model::Model;
#[cfg(feature = "server")]
use sandpolis_instance::network::RequestResult;
#[cfg(feature = "server")]
use sandpolis_instance::realm::server::AuthenticatedRealm;
use sandpolis_macros::data;
use serde::Deserialize;
use serde::Serialize;

#[data]
#[derive(Default)]
pub struct ServerBannerData {
    inner: ServerBanner,
}

#[derive(Serialize, Deserialize)]
pub struct GetBannerRequest {
    /// Whether to include the banner image in the response.
    pub include_image: bool,
}

#[derive(Serialize, Deserialize)]
pub struct GetBannerResponse(pub ServerBanner);

/// Return a "banner" containing server metadata.
#[cfg(feature = "server")]
pub async fn get_banner(
    state: State<ServerManager>,
    users: State<UserManager>,
    AuthenticatedRealm(realm): AuthenticatedRealm,
    extract::Json(request): extract::Json<GetBannerRequest>,
) -> RequestResult<GetBannerResponse> {
    let mut banner = state.banner.read().inner.clone();

    // Whether a login is needed (and how) is the realm's live state, not
    // something stored on the banner row.
    banner.users_configured = users.users_configured(&realm);
    banner.mfa = users.totp_required(&realm);

    if !request.include_image {
        banner.image = None;
    }

    Ok(Json(GetBannerResponse(banner)))
}
