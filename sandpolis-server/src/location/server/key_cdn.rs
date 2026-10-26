use super::Locator;
use super::non_empty;
use crate::location::Location;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use tracing::trace;

/// Uses the <https://tools.keycdn.com/geo> web service to resolve IP locations.
pub struct KeyCdnLocator {
    client: reqwest::Client,
}

/// keycdn refuses any request whose `User-Agent` isn't `keycdn-tools:<url>`
/// naming the site making the query — without it every lookup is a 403.
const USER_AGENT: &str = "keycdn-tools:https://sandpolis.com";

impl Default for KeyCdnLocator {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyCdnLocator {
    pub fn new() -> Self {
        Self {
            client: super::client(),
        }
    }
}

/// Query response from tools.keycdn.com, which wraps the location it found in
/// an envelope reporting whether it found one.
#[derive(Serialize, Deserialize, Debug)]
struct Response {
    /// success or error
    status: String,
    /// Why the lookup failed, when it did
    description: Option<String>,
    data: Option<Data>,
}

#[derive(Serialize, Deserialize, Debug)]
struct Data {
    geo: Geo,
}

#[derive(Serialize, Deserialize, Debug)]
struct Geo {
    /// Continent name
    continent_name: Option<String>,
    /// Two-letter continent code
    continent_code: Option<String>,
    /// Country name
    country_name: Option<String>,
    /// Two-letter country code ISO 3166-1 alpha-2
    country_code: Option<String>,
    /// Region/state short code (FIPS or ISO)
    region_code: Option<String>,
    /// Region/state
    region_name: Option<String>,
    /// City name
    city: Option<String>,
    /// Postal code
    postal_code: Option<String>,
    /// Metro code
    metro_code: Option<u32>,
    /// Latitude
    latitude: Option<f64>,
    /// Longitude
    longitude: Option<f64>,
    /// Timezone
    timezone: Option<String>,
    /// Internet service provider
    isp: Option<String>,
    /// AS number (RIR). Null for IP blocks not being announced in BGP tables.
    asn: Option<u32>,
}

impl From<Geo> for Location {
    fn from(value: Geo) -> Self {
        Self {
            // keycdn reports the AS as a number only, with no name to go with it.
            as_name: None,
            as_code: value.asn,
            city: non_empty(value.city),
            continent: non_empty(value.continent_name),
            continent_code: non_empty(value.continent_code),
            country: non_empty(value.country_name),
            country_code: non_empty(value.country_code),
            currency: None,
            district: None,
            isp: non_empty(value.isp),
            latitude: value.latitude,
            longitude: value.longitude,
            metro_code: value.metro_code,
            organization: None,
            postal_code: non_empty(value.postal_code),
            region: non_empty(value.region_name),
            region_code: non_empty(value.region_code),
            timezone: non_empty(value.timezone),
        }
    }
}

impl Locator for KeyCdnLocator {
    async fn query(&self, ip: IpAddr) -> Result<Option<Location>> {
        let response: Response = self
            .client
            .get(format!("https://tools.keycdn.com/geo.json?host={ip}"))
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send()
            .await?
            .json()
            .await?;

        trace!(%ip, ?response, "Queried tools.keycdn.com");

        // keycdn reports a rejected request (a bad user agent, a rate limit) in
        // the body with a 403, so the status is the only thing that says whether
        // there is an answer here at all. Treated as an error rather than "no
        // location": nothing about the address made it fail.
        if response.status != "success" {
            bail!(
                "tools.keycdn.com refused the lookup: {}",
                response
                    .description
                    .unwrap_or_else(|| response.status.clone())
            );
        }

        Ok(response.data.map(|data| data.geo.into()))
    }
}

#[cfg(test)]
mod test_key_cdn {
    use super::*;
    use anyhow::Result;

    /// A real response. Note the envelope, the numeric `asn`, and that keycdn
    /// leaves most of the location null even for an address it knows.
    const LOCATED: &str = r#"{"status":"success","description":"Data successfully received.",
        "data":{"geo":{"host":"1.1.1.1","ip":"1.1.1.1","asn":13335,"isp":"Cloudflare, Inc.",
        "country_name":null,"country_code":null,"region_name":null,"region_code":null,
        "city":null,"postal_code":null,"continent_name":null,"continent_code":null,
        "latitude":null,"longitude":null,"metro_code":null,"timezone":"","datetime":""}}}"#;

    /// What a request keycdn won't serve comes back as, with HTTP 403.
    const REFUSED: &str = r#"{"status":"error",
        "description":"User-Agent not properly defined. Please check the docs: https://tools.keycdn.com/geo"}"#;

    /// The envelope is what this got wrong before: parsed as a flat object,
    /// every field came back `None` and a refusal looked like a location.
    #[test]
    fn a_located_response_is_read_out_of_the_envelope() -> Result<()> {
        let response: Response = serde_json::from_str(LOCATED)?;
        assert_eq!(response.status, "success");

        let location: Location = response.data.expect("data").geo.into();
        assert_eq!(location.as_code, Some(13335));
        assert_eq!(location.isp.as_deref(), Some("Cloudflare, Inc."));
        // Sent as "", which is keycdn's way of saying it doesn't know.
        assert_eq!(location.timezone, None);
        Ok(())
    }

    #[test]
    fn a_refusal_is_not_a_location() -> Result<()> {
        let response: Response = serde_json::from_str(REFUSED)?;
        assert_eq!(response.status, "error");
        assert!(response.data.is_none());
        assert!(response.description.unwrap().contains("User-Agent"));
        Ok(())
    }

    /// Hits the live service, so it only runs when asked for by name:
    /// `cargo test -p sandpolis-server --features server -- --ignored`.
    #[tokio::test]
    #[ignore = "queries tools.keycdn.com over the network"]
    async fn query_against_the_live_service() -> Result<()> {
        let location = KeyCdnLocator::new()
            .query("1.1.1.1".parse()?)
            .await?
            .expect("1.1.1.1 is locatable");

        assert!(!location.is_empty());
        assert_eq!(location.as_code, Some(13335));
        Ok(())
    }
}
