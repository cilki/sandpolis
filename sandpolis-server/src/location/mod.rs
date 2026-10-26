use native_db::ToKey;
use native_model::Model;
use sandpolis_macros::data;

#[cfg(feature = "server")]
pub mod server;

#[data(temporal)]
#[derive(Default)]
pub struct LocationData {
    /// The instance this location describes, once it has been associated with
    /// one. A freshly queried location is not yet owned by any instance.
    #[secondary_key(optional)]
    pub _instance_id: Option<sandpolis_instance::InstanceId>,

    /// The AS name
    pub as_name: Option<String>,
    /// The numerical AS code
    pub as_code: Option<u32>,
    /// The city name
    pub city: Option<String>,
    /// The continent name
    pub continent: Option<String>,
    /// The ISO continent code
    pub continent_code: Option<String>,
    /// The country name
    pub country: Option<String>,
    /// Two-letter country code (ISO 3166-1 alpha-2)
    pub country_code: Option<String>,
    /// The currency name
    pub currency: Option<String>,
    /// The city district name
    pub district: Option<String>,
    /// The Internet Service Provider name
    pub isp: Option<String>,
    /// The approximate latitude in degrees
    pub latitude: Option<f64>,
    /// The approximate longitude in degrees
    pub longitude: Option<f64>,
    /// The metro code
    pub metro_code: Option<u32>,
    /// The organization name
    pub organization: Option<String>,
    /// The zip code
    pub postal_code: Option<String>,
    /// The region name
    pub region: Option<String>,
    /// The region short code (FIPS or ISO)
    pub region_code: Option<String>,
    /// The timezone name
    pub timezone: Option<String>,
}
