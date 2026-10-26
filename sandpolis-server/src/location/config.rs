use serde::{Deserialize, Serialize};

/// The `server.location` section of a realm config, which decides whether this
/// server resolves where the instances attached to it are.
///
/// Per realm rather than per server, because the instances are the realm's: a
/// server serving two realms may be asked to geolocate one and not the other.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(default)]
pub struct LocationConfig {
    /// Resolve the approximate geographic location of every instance that
    /// attaches to this server, by asking [`service`](Self::service) about the
    /// address it connected from.
    ///
    /// Off unless asked for: it sends the address of every instance in the
    /// realm to somebody else's server — in cleartext, on ip-api.com's free
    /// tier — which is not a decision this project gets to make for an
    /// operator.
    pub enabled: bool,

    /// Which service to ask.
    pub service: LocationService,

    /// API key, for services that take one. Without it ip-api.com is queried
    /// over plain HTTP (its free tier doesn't serve TLS) and rate limited to 45
    /// lookups a minute; keycdn takes no key at all.
    pub key: Option<String>,
}

/// A geolocation service that can answer "where is this address?".
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LocationService {
    /// <https://ip-api.com>, which returns the most detail (AS, currency,
    /// district) and is the default because of it.
    #[default]
    IpApi,

    /// <https://tools.keycdn.com/geo>, which returns country, ISP and AS and
    /// often nothing else.
    KeyCdn,
}

impl std::fmt::Display for LocationService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IpApi => f.write_str("ip-api.com"),
            Self::KeyCdn => f.write_str("tools.keycdn.com"),
        }
    }
}
