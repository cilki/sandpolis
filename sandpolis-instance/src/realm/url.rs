use crate::realm::RealmName;
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::fmt::Display;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::str::FromStr;
use url::Url;

/// Locates a server instance over the network. These have a format like:
///
/// ```text
/// https://example.com:8768/default
/// ```
///
/// With default information omitted, the URL can be as simple as:
///
/// ```text
/// https://example.com
/// ```
#[derive(Serialize, Deserialize, PartialEq, Clone, Debug)]
pub struct ServerUrl {
    pub host: String,
    pub port: u16,
    pub realm: RealmName,
}

impl ServerUrl {
    /// Resolve the URL into IP addresses.
    pub fn resolve(&self) -> Result<Vec<SocketAddr>> {
        Ok(format!("{}:{}", self.host, self.port)
            .to_socket_addrs()?
            .collect())
    }

    /// Official server port: <https://www.iana.org/assignments/service-names-port-numbers/service-names-port-numbers.xhtml?search=8768>
    pub const fn default_port() -> u16 {
        8768
    }

    /// Whether the URL points to localhost.
    pub fn is_localhost(&self) -> bool {
        if self.host == "localhost" {
            return true;
        }

        if let Ok(ip) = self.host.parse::<IpAddr>() {
            return ip.is_loopback();
        }

        false
    }

    /// The form encoded into a certificate's common name: always explicit about
    /// host, port, and realm so a loaded endpoint certificate names exactly one
    /// server without any ambient defaults.
    ///
    /// Unlike [`Display`], this never omits a default port or realm, and has no
    /// scheme.
    ///
    // TODO X.509 ub-common-name caps a common name at 64 characters, so a long
    // host plus a long realm name can't be encoded. Move the identity into a SAN
    // (or a custom extension) when that becomes a real constraint.
    pub fn canonical(&self) -> String {
        format!("{}:{}/{}", self.host, self.port, self.realm)
    }
}

impl FromStr for ServerUrl {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let url = Url::parse(&if s.starts_with("https://") {
            s.to_string()
        } else {
            format!("https://{s}")
        })?;

        let host = url
            .host_str()
            .ok_or_else(|| anyhow!("Invalid host in URL"))?;
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host)
            .to_string();

        Ok(Self {
            host,
            port: url.port().unwrap_or(ServerUrl::default_port()),
            realm: if url.path().len() > 1 {
                url.path().trim_start_matches('/').parse()?
            } else {
                RealmName::default()
            },
        })
    }
}

impl Display for ServerUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("https://")?;
        f.write_str(&self.host)?;

        if self.port != ServerUrl::default_port() {
            f.write_str(":")?;
            f.write_str(&format!("{}", self.port))?;
        }

        if self.realm != RealmName::default() {
            f.write_str("/")?;
            f.write_str(&self.realm.to_string())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_url_from_str_basic() {
        let url: ServerUrl = "example.com".parse().unwrap();
        assert_eq!(url.host, "example.com");
        assert_eq!(url.port, 8768);
        assert_eq!(url.realm, RealmName::default());
    }

    #[test]
    fn test_server_url_from_str_with_port() {
        let url: ServerUrl = "example.com:9000".parse().unwrap();
        assert_eq!(url.host, "example.com");
        assert_eq!(url.port, 9000);
        assert_eq!(url.realm, RealmName::default());
    }

    #[test]
    fn test_server_url_from_str_with_https() {
        let url: ServerUrl = "https://example.com".parse().unwrap();
        assert_eq!(url.host, "example.com");
        assert_eq!(url.port, 8768);
        assert_eq!(url.realm, RealmName::default());
    }

    #[test]
    fn test_server_url_from_str_with_realm() {
        let url: ServerUrl = "example.com/myrealm".parse().unwrap();
        assert_eq!(url.host, "example.com");
        assert_eq!(url.port, 8768);
        assert_eq!(url.realm, "myrealm".parse().unwrap());
    }

    #[test]
    fn test_server_url_from_str_full() {
        let url: ServerUrl = "https://example.com:9000/myrealm".parse().unwrap();
        assert_eq!(url.host, "example.com");
        assert_eq!(url.port, 9000);
        assert_eq!(url.realm, "myrealm".parse().unwrap());
    }

    #[test]
    fn test_server_url_from_str_ip_address() {
        let url: ServerUrl = "192.168.1.1:8080".parse().unwrap();
        assert_eq!(url.host, "192.168.1.1");
        assert_eq!(url.port, 8080);
        assert_eq!(url.realm, RealmName::default());
    }

    #[test]
    fn test_server_url_display_default() {
        let url = ServerUrl {
            host: "example.com".to_string(),
            port: 8768,
            realm: RealmName::default(),
        };
        assert_eq!(url.to_string(), "https://example.com");
    }

    #[test]
    fn test_server_url_display_with_port() {
        let url = ServerUrl {
            host: "example.com".to_string(),
            port: 9000,
            realm: RealmName::default(),
        };
        assert_eq!(url.to_string(), "https://example.com:9000");
    }

    #[test]
    fn test_server_url_display_with_realm() {
        let url = ServerUrl {
            host: "example.com".to_string(),
            port: 8768,
            realm: "myrealm".parse().unwrap(),
        };
        assert_eq!(url.to_string(), "https://example.com/myrealm");
    }

    #[test]
    fn test_server_url_display_full() {
        let url = ServerUrl {
            host: "example.com".to_string(),
            port: 9000,
            realm: "myrealm".parse().unwrap(),
        };
        assert_eq!(url.to_string(), "https://example.com:9000/myrealm");
    }

    #[test]
    fn test_server_url_is_localhost() {
        let url = ServerUrl {
            host: "localhost".to_string(),
            port: 8768,
            realm: RealmName::default(),
        };
        assert!(url.is_localhost());

        let url = ServerUrl {
            host: "127.0.0.1".to_string(),
            port: 8768,
            realm: RealmName::default(),
        };
        assert!(url.is_localhost());

        let url = ServerUrl {
            host: "example.com".to_string(),
            port: 8768,
            realm: RealmName::default(),
        };
        assert!(!url.is_localhost());
    }

    #[test]
    fn test_server_url_default_port() {
        assert_eq!(ServerUrl::default_port(), 8768);
    }

    #[test]
    fn test_server_url_roundtrip() {
        let original = "https://example.com:9000/myrealm";
        let url: ServerUrl = original.parse().unwrap();
        assert_eq!(url.to_string(), original);
    }

    #[test]
    fn test_server_url_invalid_scheme() {
        let result: Result<ServerUrl, _> = "http://example.com".parse();
        assert!(result.is_err());
    }

    #[test]
    fn test_server_url_invalid_host() {
        let result: Result<ServerUrl, _> = "https://".parse();
        assert!(result.is_err());
    }

    #[test]
    fn test_server_url_ipv6() {
        let url: ServerUrl = "[::1]:8080".parse().unwrap();
        assert_eq!(url.host, "::1");
        assert_eq!(url.port, 8080);
        assert!(url.is_localhost());
    }

    /// The canonical form is always explicit, so it round-trips back through
    /// `FromStr` to the same host, port and realm.
    #[test]
    fn canonical_is_explicit_and_round_trips() {
        let url = ServerUrl {
            host: "gs.example.com".to_string(),
            port: 8768,
            realm: RealmName::default(),
        };
        assert_eq!(url.canonical(), "gs.example.com:8768/default");

        let parsed: ServerUrl = url.canonical().parse().unwrap();
        assert_eq!(parsed.host, url.host);
        assert_eq!(parsed.port, url.port);
        assert_eq!(parsed.realm, url.realm);
    }
}
