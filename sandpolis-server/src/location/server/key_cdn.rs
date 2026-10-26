use super::LocationData;
use super::Locator;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::trace;

/// Uses the https://tools.keycdn.com/geo web service to resolve IP locations.
pub struct KeyCdnLocator {
    client: reqwest::Client,
}

impl Default for KeyCdnLocator {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyCdnLocator {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

/// Query response from ip-api.com.
#[derive(Serialize, Deserialize, Debug)]
struct Response {
    /// Continent name
    continent_name: Option<String>,
    /// Two-letter continent code
    continent_code: Option<String>,
    /// Two-letter continent code
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
    metro_code: Option<String>,
    /// Latitude
    latitude: Option<f64>,
    /// Longitude
    longitude: Option<f64>,
    /// Timezone
    timezone: Option<String>,
    /// Internet service provider
    isp: Option<String>,
    /// AS name (RIR). Empty for IP blocks not being announced in BGP tables.
    asn: Option<String>,
}

impl From<Response> for LocationData {
    fn from(value: Response) -> Self {
        Self {
            as_name: value.asn,
            as_code: None,
            city: value.city,
            continent: value.continent_name,
            continent_code: value.continent_code,
            country: value.country_name,
            country_code: value.country_code,
            currency: None,
            district: None,
            isp: value.isp,
            latitude: value.latitude,
            longitude: value.longitude,
            metro_code: value.metro_code.map(|m| m.parse().unwrap_or_default()),
            organization: None,
            postal_code: value.postal_code,
            region: value.region_name,
            region_code: value.region_code,
            timezone: value.timezone,
            ..Default::default()
        }
    }
}

impl Locator for KeyCdnLocator {
    async fn query(&self, ip: &str) -> Result<Option<LocationData>> {
        let response: Response = self
            .client
            .get(format!("https://tools.keycdn.com/geo.json?host={}", ip))
            .send()
            .await?
            .json()
            .await?;

        trace!(ip = ip, response = ?response, "Queried tools.keycdn.com");
        Ok(Some(response.into()))
    }
}

#[cfg(test)]
mod test_key_cdn {
    use super::*;
    use anyhow::Result;

    #[tokio::test]
    async fn test_query() -> Result<()> {
        let locator = KeyCdnLocator::new();
        locator.query("1.1.1.1").await?;

        Ok(())
    }
}
