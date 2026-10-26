//! Where in the world an instance is, as far as a geolocation service will say.
//!
//! An instance never reports its own location: a laptop has no idea where it
//! is, and a compromised agent would be free to claim anything. What the server
//! has instead is the address the instance connected *from* — the one piece of
//! evidence the instance doesn't get to choose — so resolving that is the whole
//! of this subsystem. It is off unless a realm config asks for it
//! ([`LocationConfig`]), because turning it on means handing the address of
//! every instance in the realm to a third party.
//!
//! The lookup itself lives in [`server`], behind the [`server::Locator`] trait
//! so a realm can pick which service answers.

use native_db::ToKey;
use native_model::Model;
use sandpolis_instance::InstanceId;
use sandpolis_macros::data;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

pub mod config;
#[cfg(feature = "server")]
pub mod server;

pub use config::{LocationConfig, LocationService};

/// One server's view of where one instance is.
///
/// Scoped by the *observing* server (`_instance_id`) with the located instance
/// in [`subject`](Self::subject), the same split
/// [`LivenessData`](sandpolis_instance::network::liveness::LivenessData) uses:
/// the server is the one making the claim, so the row belongs to that server's
/// own scope and replicates along with the rest of its data. Scoping it by the
/// subject would put it in the scope the server *pulls* from that instance,
/// where it isn't the server's to write.
#[data(instance)]
pub struct LocationData {
    /// The instance this row is about. Deliberately not unique: an instance that
    /// moves between servers collects an opinion from each.
    #[secondary_key]
    pub subject: InstanceId,

    /// The address the location was resolved from, which is where the subject's
    /// connection came from rather than anything the subject said. The row is
    /// resolved again when this changes, which is how a moved instance catches
    /// up.
    pub address: IpAddr,

    /// Which service answered, so a row that looks wrong can be blamed on the
    /// right place.
    pub service: LocationService,

    /// What the service said. Every field is optional because every service
    /// leaves out most of them most of the time.
    pub location: Location,
}

impl LocationData {
    /// `observer`'s record of where `subject`, connecting from `address`, is.
    pub fn observation(
        observer: InstanceId,
        subject: InstanceId,
        address: IpAddr,
        service: LocationService,
        location: Location,
    ) -> Self {
        Self {
            subject,
            address,
            service,
            location,
            _id: Default::default(),
            _revision: Default::default(),
            _creation: Default::default(),
            _instance_id: observer,
        }
    }
}

// Scoped by the observing server so its rows replicate as part of that server's
// own data, exactly like the liveness it records for the same instances.
inventory::submit! {
    sandpolis_instance::database::sync::SyncRegistration(
        |r| r.register_scoped::<LocationData>(|d| d._instance_id))
}

/// The sync `model_id` for [`LocationData`], used by clients to subscribe.
pub fn location_model_id() -> u32 {
    <LocationData as Model>::native_model_id()
}

/// A geographic location, in the union of what the supported services return.
///
/// Kept separate from [`LocationData`] so a [`Locator`](server::Locator) can
/// produce one without knowing which instance it describes, and so adding a
/// service can't change the shape of the stored row.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct Location {
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

impl Location {
    /// Whether the service said anything at all. A response that resolved to
    /// nothing is not worth a row: it would claim the instance was located when
    /// all that's known is that somebody was asked.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// How to name this location in one line, as a client would show it.
    pub fn describe(&self) -> Option<String> {
        let parts: Vec<&str> = [
            self.city.as_deref(),
            self.region.as_deref(),
            self.country.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();

        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

#[cfg(test)]
mod test_location {
    use super::*;

    #[test]
    fn a_location_that_resolved_to_nothing_is_empty() {
        assert!(Location::default().is_empty());
        assert!(
            !Location {
                country: Some("Australia".into()),
                ..Default::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn describe_uses_whatever_is_known() {
        assert_eq!(Location::default().describe(), None);
        assert_eq!(
            Location {
                city: Some("South Brisbane".into()),
                region: Some("Queensland".into()),
                country: Some("Australia".into()),
                ..Default::default()
            }
            .describe()
            .as_deref(),
            Some("South Brisbane, Queensland, Australia")
        );
        assert_eq!(
            Location {
                country: Some("Australia".into()),
                ..Default::default()
            }
            .describe()
            .as_deref(),
            Some("Australia")
        );
    }
}
