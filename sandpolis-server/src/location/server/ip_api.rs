use super::LocationData;
use super::Locator;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::trace;

/// Uses the ip-api.com web service to resolve IP locations.
pub struct IpApiLocator {
    key: Option<String>,
    client: reqwest::Client,
}

impl IpApiLocator {
    pub fn new(key: Option<String>) -> Self {
        Self {
            key,
            client: reqwest::Client::new(),
        }
    }
}

/// Query response from ip-api.com.
#[allow(non_snake_case)]
#[derive(Serialize, Deserialize, Debug)]
struct Response {
    /// success or fail
    status: String,
    /// Continent name
    continent: Option<String>,
    /// Two-letter continent code
    continentCode: Option<String>,
    /// Two-letter continent code
    country: Option<String>,
    /// Two-letter country code ISO 3166-1 alpha-2
    countryCode: Option<String>,
    /// Region/state short code (FIPS or ISO)
    region: Option<String>,
    /// Region/state
    regionName: Option<String>,
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
    /// Timezone UTC DST offset in seconds
    offset: Option<i64>,
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
    /// AS name (RIR). Empty for IP blocks not being announced in BGP tables.
    mobile: Option<bool>,
    /// Proxy, VPN or Tor exit address
    proxy: Option<bool>,
    /// Hosting, colocated or data center
    hosting: Option<bool>,
}

impl From<Response> for LocationData {
    fn from(value: Response) -> Self {
        Self {
            as_name: value.asname,
            as_code: None,
            city: value.city,
            continent: value.continent,
            continent_code: value.continentCode,
            country: value.country,
            country_code: value.countryCode,
            currency: value.currency,
            district: value.district,
            isp: value.isp,
            latitude: value.lat,
            longitude: value.lon,
            metro_code: None,
            organization: value.org,
            postal_code: None,
            region: value.regionName,
            region_code: value.region,
            timezone: value.timezone,
            ..Default::default()
        }
    }
}

impl Locator for IpApiLocator {
    async fn query(&self, ip: &str) -> Result<Option<LocationData>> {
        let response: Response = self
            .client
            .get(if let Some(key) = self.key.as_ref() {
                format!("https://ip-api.com/json/{}?access_key={}", ip, key)
            } else {
                format!("http://ip-api.com/json/{}", ip)
            })
            .send()
            .await?
            .json()
            .await?;

        trace!(ip = ip, response = ?response, "Queried ip-api.com");
        Ok(Some(response.into()))
    }
}

#[cfg(test)]
mod test_ip_api {
    use super::*;
    use anyhow::Result;

    #[tokio::test]
    async fn test_query() -> Result<()> {
        let locator = IpApiLocator::new(None);
        locator.query("1.1.1.1").await?;

        Ok(())
    }
}
