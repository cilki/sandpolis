use super::Locator;
use super::{as_code, non_empty};
use crate::location::Location;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use tracing::{debug, trace};

/// Uses the ip-api.com web service to resolve IP locations.
pub struct IpApiLocator {
    key: Option<String>,
    client: reqwest::Client,
}

/// The fields to ask for. ip-api.com answers with a short default set unless
/// told otherwise — no continent, AS name, currency or district — so asking for
/// them explicitly is the difference between a populated row and a mostly empty
/// one.
const FIELDS: &str = "status,message,continent,continentCode,country,countryCode,\
                      region,regionName,city,district,zip,lat,lon,timezone,currency,isp,org,as,asname";

impl IpApiLocator {
    pub fn new(key: Option<String>) -> Self {
        Self {
            key,
            client: super::client(),
        }
    }

    /// Where to ask about `ip`.
    ///
    /// The free service is plain HTTP only (it answers nothing on 443), so a
    /// key is also what moves the query onto TLS — and onto `pro.ip-api.com`,
    /// which is the only host that accepts one.
    fn url(&self, ip: IpAddr) -> String {
        match self.key.as_deref() {
            Some(key) => format!("https://pro.ip-api.com/json/{ip}?fields={FIELDS}&key={key}"),
            None => format!("http://ip-api.com/json/{ip}?fields={FIELDS}"),
        }
    }
}

/// Query response from ip-api.com.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Response {
    /// success or fail
    status: String,
    /// Why the lookup failed, when it did
    message: Option<String>,
    /// Continent name
    continent: Option<String>,
    /// Two-letter continent code
    continent_code: Option<String>,
    /// Country name
    country: Option<String>,
    /// Two-letter country code ISO 3166-1 alpha-2
    country_code: Option<String>,
    /// Region/state short code (FIPS or ISO)
    region: Option<String>,
    /// Region/state
    region_name: Option<String>,
    /// City name
    city: Option<String>,
    /// District (subdivision of city)
    district: Option<String>,
    /// Postal code
    zip: Option<String>,
    /// Latitude
    lat: Option<f64>,
    /// Longitude
    lon: Option<f64>,
    /// Timezone
    timezone: Option<String>,
    /// National currency
    currency: Option<String>,
    /// Internet service provider
    isp: Option<String>,
    /// Organization name
    org: Option<String>,
    /// AS number and organization, separated by space (RIR). Empty for IP
    /// blocks not being announced in BGP tables.
    r#as: Option<String>,
    /// AS name (RIR). Empty for IP blocks not being announced in BGP tables.
    asname: Option<String>,
}

impl Response {
    /// Whether the service located the address at all. A reserved or private
    /// address comes back as `fail`, with every other field missing.
    fn located(&self) -> bool {
        self.status == "success"
    }
}

impl From<Response> for Location {
    fn from(value: Response) -> Self {
        Self {
            as_name: non_empty(value.asname),
            as_code: as_code(value.r#as.as_deref()),
            city: non_empty(value.city),
            continent: non_empty(value.continent),
            continent_code: non_empty(value.continent_code),
            country: non_empty(value.country),
            country_code: non_empty(value.country_code),
            currency: non_empty(value.currency),
            district: non_empty(value.district),
            isp: non_empty(value.isp),
            latitude: value.lat,
            longitude: value.lon,
            metro_code: None,
            organization: non_empty(value.org),
            postal_code: non_empty(value.zip),
            region: non_empty(value.region_name),
            region_code: non_empty(value.region),
            timezone: non_empty(value.timezone),
        }
    }
}

impl Locator for IpApiLocator {
    async fn query(&self, ip: IpAddr) -> Result<Option<Location>> {
        let response: Response = self
            .client
            .get(self.url(ip))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        trace!(%ip, ?response, "Queried ip-api.com");

        // Not an error: the service answered, and the answer is that it has
        // nothing for this address.
        if !response.located() {
            debug!(%ip, reason = ?response.message, "ip-api.com has no location for an address");
            return Ok(None);
        }

        Ok(Some(response.into()))
    }
}

#[cfg(test)]
mod test_ip_api {
    use super::*;
    use anyhow::Result;

    /// A real response, with every requested field present.
    const LOCATED: &str = r#"{"status":"success","continent":"Oceania","continentCode":"OC",
        "country":"Australia","countryCode":"AU","region":"QLD","regionName":"Queensland",
        "city":"South Brisbane","district":"","zip":"4101","lat":-27.4766,"lon":153.0166,
        "timezone":"Australia/Brisbane","currency":"AUD","isp":"Cloudflare, Inc",
        "org":"APNIC and Cloudflare DNS Resolver project","as":"AS13335 Cloudflare, Inc.",
        "asname":"CLOUDFLARENET"}"#;

    /// What a private or reserved address comes back as.
    const UNLOCATED: &str = r#"{"status":"fail","message":"private range"}"#;

    #[test]
    fn a_located_response_maps_onto_every_field() -> Result<()> {
        let response: Response = serde_json::from_str(LOCATED)?;
        assert!(response.located());

        let location: Location = response.into();
        assert_eq!(location.country.as_deref(), Some("Australia"));
        assert_eq!(location.country_code.as_deref(), Some("AU"));
        assert_eq!(location.continent.as_deref(), Some("Oceania"));
        assert_eq!(location.continent_code.as_deref(), Some("OC"));
        assert_eq!(location.city.as_deref(), Some("South Brisbane"));
        // `region` on the wire is the short code, `regionName` the name.
        assert_eq!(location.region.as_deref(), Some("Queensland"));
        assert_eq!(location.region_code.as_deref(), Some("QLD"));
        assert_eq!(location.postal_code.as_deref(), Some("4101"));
        assert_eq!(location.timezone.as_deref(), Some("Australia/Brisbane"));
        assert_eq!(location.currency.as_deref(), Some("AUD"));
        assert_eq!(location.isp.as_deref(), Some("Cloudflare, Inc"));
        assert_eq!(location.as_name.as_deref(), Some("CLOUDFLARENET"));
        assert_eq!(location.as_code, Some(13335));
        assert_eq!(location.latitude, Some(-27.4766));
        assert_eq!(location.longitude, Some(153.0166));
        // Sent as "", which means unknown rather than a district named "".
        assert_eq!(location.district, None);
        Ok(())
    }

    /// The case that would otherwise be stored as a row claiming the instance
    /// was located somewhere with no country, city or coordinates.
    #[test]
    fn a_failed_lookup_is_not_a_location() -> Result<()> {
        let response: Response = serde_json::from_str(UNLOCATED)?;
        assert!(!response.located());
        assert_eq!(response.message.as_deref(), Some("private range"));
        Ok(())
    }

    /// A key moves the query to the only host that accepts one, over TLS.
    #[test]
    fn the_key_decides_the_endpoint() {
        let ip: IpAddr = "1.1.1.1".parse().unwrap();

        let free = IpApiLocator::new(None).url(ip);
        assert!(free.starts_with("http://ip-api.com/json/1.1.1.1?"), "{free}");
        assert!(!free.contains("key="));

        let pro = IpApiLocator::new(Some("secret".into())).url(ip);
        assert!(
            pro.starts_with("https://pro.ip-api.com/json/1.1.1.1?"),
            "{pro}"
        );
        assert!(pro.ends_with("&key=secret"), "{pro}");
    }

    /// Hits the live service, so it only runs when asked for by name:
    /// `cargo test -p sandpolis-server --features server -- --ignored`.
    #[tokio::test]
    #[ignore = "queries ip-api.com over the network"]
    async fn query_against_the_live_service() -> Result<()> {
        let location = IpApiLocator::new(None)
            .query("1.1.1.1".parse()?)
            .await?
            .expect("1.1.1.1 is locatable");

        assert!(!location.is_empty());
        assert_eq!(location.as_code, Some(13335));
        Ok(())
    }
}
