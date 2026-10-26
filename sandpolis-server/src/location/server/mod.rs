use super::LocationData;
use anyhow::Result;
use serde::{Deserialize, Serialize};

pub mod ip_api;
pub mod key_cdn;

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub enum LocationService {
    IpApi,
    #[default]
    KeyCdn,
}

pub(crate) trait Locator {
    // TODO configurable field set?
    async fn query(&self, ip: &str) -> Result<Option<LocationData>>;
}
