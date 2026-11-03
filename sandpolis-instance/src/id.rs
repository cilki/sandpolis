//! Typed 64-bit identifiers.
//!
//! Every id in the cluster shares one wire layout: the top four bits
//! discriminate the id type and the low 60 bits are the body, displayed as
//! `<prefix>-<base32>`. [`InstanceId`] is the union of the three id types that
//! name instances; other subsystems mint their own types with [`typed_id!`].

use crate::InstanceType;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fmt::Display;
use std::str::FromStr;

/// Bits below the type discriminant.
pub const DISCRIMINANT_SHIFT: u32 = 60;

/// The 60-bit body every typed id maintains as an invariant.
pub const BODY_MASK: u64 = (1 << DISCRIMINANT_SHIFT) - 1;

/// Lowercase RFC 4648 alphabet; 12 characters carry exactly 60 bits.
const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// Encode a 60-bit body as 12 base32 characters.
#[doc(hidden)]
pub fn base32_encode(body: u64) -> [u8; 12] {
    let mut out = [0u8; 12];
    for (i, c) in out.iter_mut().enumerate() {
        *c = ALPHABET[((body >> (5 * (11 - i))) & 0x1f) as usize];
    }
    out
}

/// Decode 12 base32 characters into a 60-bit body.
#[doc(hidden)]
pub fn base32_decode(s: &str) -> Result<u64> {
    if s.len() != 12 {
        anyhow::bail!("invalid id body {s:?}: expected 12 base32 characters");
    }
    let mut body = 0u64;
    for c in s.bytes() {
        let value = match c {
            b'a'..=b'z' => c - b'a',
            b'2'..=b'7' => c - b'2' + 26,
            _ => anyhow::bail!("invalid id body {s:?}: unexpected character {:?}", c as char),
        };
        body = (body << 5) | value as u64;
    }
    Ok(body)
}

#[doc(hidden)]
pub fn random_body() -> u64 {
    rand::random::<u64>() & BODY_MASK
}

/// Define a new id type sharing the cluster-wide wire layout.
///
/// The discriminant must be unique across the whole project; claimed so far:
///
/// | Discriminant | Type        |
/// |--------------|-------------|
/// | 1            | `AgentId`   |
/// | 2            | `ClientId`  |
/// | 3            | `ServerId`  |
/// | 4            | `ProbeId`   |
/// | 5            | `AccountId` |
///
/// Zero is never valid, so an all-zero wire value names nothing.
#[macro_export]
macro_rules! typed_id {
    ($(#[$attr:meta])* $name:ident, $prefix:literal, $discriminant:literal) => {
        $(#[$attr])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            pub const PREFIX: &'static str = $prefix;
            pub const DISCRIMINANT: u64 = $discriminant;

            /// A new id with a random body.
            pub fn random() -> Self {
                Self($crate::id::random_body())
            }

            /// The id's 60-bit body.
            pub const fn body(self) -> u64 {
                self.0
            }

            /// The full 64-bit wire value: discriminant, then body.
            pub const fn to_wire(self) -> u64 {
                (Self::DISCRIMINANT << $crate::id::DISCRIMINANT_SHIFT) | self.0
            }

            /// Parse a wire value, rejecting every other id type.
            pub fn from_wire(value: u64) -> ::core::result::Result<Self, $crate::anyhow::Error> {
                if value >> $crate::id::DISCRIMINANT_SHIFT != Self::DISCRIMINANT {
                    $crate::anyhow::bail!("invalid {} id: {value:#018x}", $prefix);
                }
                Ok(Self(value & $crate::id::BODY_MASK))
            }
        }

        impl $crate::serde::Serialize for $name {
            fn serialize<S: $crate::serde::Serializer>(
                &self,
                serializer: S,
            ) -> ::core::result::Result<S::Ok, S::Error> {
                $crate::serde::Serialize::serialize(&self.to_wire(), serializer)
            }
        }

        impl<'de> $crate::serde::Deserialize<'de> for $name {
            fn deserialize<D: $crate::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::core::result::Result<Self, D::Error> {
                let value = <u64 as $crate::serde::Deserialize>::deserialize(deserializer)?;
                Self::from_wire(value).map_err($crate::serde::de::Error::custom)
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                let body = $crate::id::base32_encode(self.0);
                write!(f, "{}-{}", $prefix, ::std::str::from_utf8(&body).unwrap())
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::anyhow::Error;

            fn from_str(s: &str) -> ::core::result::Result<Self, Self::Err> {
                let Some(body) = s.strip_prefix(concat!($prefix, "-")) else {
                    $crate::anyhow::bail!(
                        "invalid id {s:?}: expected the form {}-<base32>",
                        $prefix
                    );
                };
                Ok(Self($crate::id::base32_decode(body)?))
            }
        }

        impl $crate::native_db::ToKey for $name {
            fn to_key(&self) -> $crate::native_db::Key {
                $crate::native_db::Key::new(self.to_wire().to_be_bytes().to_vec())
            }

            fn key_names() -> Vec<String> {
                vec![stringify!($name).to_string()]
            }
        }
    };
}

typed_id!(AgentId, "agent", 1);
typed_id!(ClientId, "client", 2);
typed_id!(ServerId, "server", 3);

/// Identifies an instance anywhere in the cluster. Generated randomly on first
/// start and reused for all subsequent runs; where "no instance" is a real
/// state, use `Option<InstanceId>`.
#[cfg_attr(feature = "client", derive(bevy::prelude::Component))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InstanceId {
    Agent(AgentId),
    Client(ClientId),
    Server(ServerId),
}

impl InstanceId {
    /// Generate a new random instance ID for an instance of the given type.
    pub fn random(instance_type: InstanceType) -> Self {
        match instance_type {
            InstanceType::Agent => Self::Agent(AgentId::random()),
            InstanceType::Client => Self::Client(ClientId::random()),
            InstanceType::Server => Self::Server(ServerId::random()),
        }
    }

    pub fn instance_type(&self) -> InstanceType {
        match self {
            Self::Agent(_) => InstanceType::Agent,
            Self::Client(_) => InstanceType::Client,
            Self::Server(_) => InstanceType::Server,
        }
    }

    pub fn is_type(&self, instance_type: InstanceType) -> bool {
        self.instance_type() == instance_type
    }

    pub fn is_agent(&self) -> bool {
        matches!(self, Self::Agent(_))
    }

    pub fn is_client(&self) -> bool {
        matches!(self, Self::Client(_))
    }

    pub fn is_server(&self) -> bool {
        matches!(self, Self::Server(_))
    }

    /// Whether an instance of this type can belong to a domain. Domains group
    /// the estate, not the servers running it.
    pub fn is_domain_member(&self) -> bool {
        !self.is_server()
    }

    /// The id's 60-bit body.
    pub const fn body(&self) -> u64 {
        match self {
            Self::Agent(id) => id.body(),
            Self::Client(id) => id.body(),
            Self::Server(id) => id.body(),
        }
    }

    /// The full 64-bit wire value: discriminant, then body.
    pub const fn to_wire(&self) -> u64 {
        match self {
            Self::Agent(id) => id.to_wire(),
            Self::Client(id) => id.to_wire(),
            Self::Server(id) => id.to_wire(),
        }
    }

    /// Parse a wire value, rejecting anything that isn't an instance id.
    pub fn from_wire(value: u64) -> Result<Self> {
        let body = value & BODY_MASK;
        Ok(match value >> DISCRIMINANT_SHIFT {
            AgentId::DISCRIMINANT => Self::Agent(AgentId(body)),
            ClientId::DISCRIMINANT => Self::Client(ClientId(body)),
            ServerId::DISCRIMINANT => Self::Server(ServerId(body)),
            _ => anyhow::bail!("invalid instance id: {value:#018x}"),
        })
    }
}

/// On the wire (CBOR, the database) an instance id is its 64-bit wire value.
/// In human-readable formats — the CLI's `--json` output, the database browser
/// — it's the `<type>-<base32>` form instead, the one `--instance` takes and
/// `agents list` prints; a raw wire value there can't be fed back to anything.
impl Serialize for InstanceId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.collect_str(self)
        } else {
            self.to_wire().serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for InstanceId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if !deserializer.is_human_readable() {
            return Self::from_wire(u64::deserialize(deserializer)?)
                .map_err(serde::de::Error::custom);
        }

        // Human-readable input takes the display form, and still the wire
        // value that was all these formats carried before.
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = InstanceId;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an instance id (`<type>-<base32>` or its wire value)")
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<InstanceId, E> {
                s.parse().map_err(E::custom)
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<InstanceId, E> {
                InstanceId::from_wire(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl Display for InstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Agent(id) => id.fmt(f),
            Self::Client(id) => id.fmt(f),
            Self::Server(id) => id.fmt(f),
        }
    }
}

impl FromStr for InstanceId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.split_once('-').map(|(prefix, _)| prefix) {
            Some(AgentId::PREFIX) => Ok(Self::Agent(s.parse()?)),
            Some(ClientId::PREFIX) => Ok(Self::Client(s.parse()?)),
            Some(ServerId::PREFIX) => Ok(Self::Server(s.parse()?)),
            _ => anyhow::bail!("invalid instance id {s:?}: expected the form <type>-<base32>"),
        }
    }
}

impl native_db::ToKey for InstanceId {
    fn to_key(&self) -> native_db::Key {
        native_db::Key::new(self.to_wire().to_be_bytes().to_vec())
    }

    fn key_names() -> Vec<String> {
        vec!["InstanceId".to_string()]
    }
}

macro_rules! instance_id_variant {
    ($name:ident, $variant:ident) => {
        impl From<$name> for InstanceId {
            fn from(id: $name) -> Self {
                InstanceId::$variant(id)
            }
        }

        impl TryFrom<InstanceId> for $name {
            type Error = anyhow::Error;

            fn try_from(id: InstanceId) -> Result<Self> {
                match id {
                    InstanceId::$variant(id) => Ok(id),
                    other => anyhow::bail!("expected {}-… id, got {other}", $name::PREFIX),
                }
            }
        }
    };
}

instance_id_variant!(AgentId, Agent);
instance_id_variant!(ClientId, Client);
instance_id_variant!(ServerId, Server);

#[cfg(test)]
mod test_id {
    use super::*;
    use strum::IntoEnumIterator;

    #[test]
    fn test_base32_round_trip() {
        for body in [0, 1, 31, 32, 0xdeadbeef, BODY_MASK] {
            let encoded = base32_encode(body);
            let s = std::str::from_utf8(&encoded).unwrap();
            assert_eq!(base32_decode(s).unwrap(), body);
        }
        assert_eq!(base32_encode(0), *b"aaaaaaaaaaaa");
        assert_eq!(base32_encode(1), *b"aaaaaaaaaaab");
    }

    #[test]
    fn test_display_format() {
        for instance_type in InstanceType::iter() {
            let id = InstanceId::random(instance_type);
            let s = id.to_string();
            let (prefix, body) = s.split_once('-').unwrap();
            assert_eq!(
                prefix,
                match instance_type {
                    InstanceType::Agent => "agent",
                    InstanceType::Client => "client",
                    InstanceType::Server => "server",
                }
            );
            assert_eq!(body.len(), 12);
            assert!(body.bytes().all(|c| ALPHABET.contains(&c)));
        }
    }

    #[test]
    fn test_display_shares_body_across_types() {
        let agent = AgentId::random();
        let server = ServerId(agent.body());
        assert_eq!(
            agent.to_string().split_once('-').unwrap().1,
            server.to_string().split_once('-').unwrap().1
        );
    }

    #[test]
    fn test_from_str_round_trip() {
        for instance_type in InstanceType::iter() {
            let id = InstanceId::random(instance_type);
            assert_eq!(id.to_string().parse::<InstanceId>().unwrap(), id);
        }
        let agent = AgentId::random();
        assert_eq!(agent.to_string().parse::<AgentId>().unwrap(), agent);
    }

    #[test]
    fn test_from_str_rejects() {
        assert!("".parse::<InstanceId>().is_err());
        assert!("agent".parse::<InstanceId>().is_err());
        assert!("probe-aaaaaaaaaaaa".parse::<InstanceId>().is_err());
        assert!("agent-aaaaaaaaaaa".parse::<InstanceId>().is_err());
        assert!("agent-aaaaaaaaaaaaa".parse::<InstanceId>().is_err());
        assert!("agent-AAAAAAAAAAAA".parse::<InstanceId>().is_err());
        assert!("agent-aaaaaaaaaaa1".parse::<InstanceId>().is_err());
        assert!(
            "01234567-89ab-cdef-0123-456789abcdef"
                .parse::<InstanceId>()
                .is_err()
        );
        assert!("server-aaaaaaaaaaaa".parse::<AgentId>().is_err());
    }

    #[test]
    fn test_serde_round_trip() {
        for instance_type in InstanceType::iter() {
            let id = InstanceId::random(instance_type);
            let bytes = serde_cbor::to_vec(&id).unwrap();
            assert_eq!(serde_cbor::from_slice::<InstanceId>(&bytes).unwrap(), id);
        }

        // A typed id and the equivalent union value are identical on the wire
        let agent = AgentId::random();
        assert_eq!(
            serde_cbor::to_vec(&agent).unwrap(),
            serde_cbor::to_vec(&InstanceId::Agent(agent)).unwrap()
        );
        assert_eq!(
            serde_cbor::from_slice::<AgentId>(&serde_cbor::to_vec(&agent).unwrap()).unwrap(),
            agent
        );
    }

    #[test]
    fn test_human_readable_serde() {
        for instance_type in InstanceType::iter() {
            let id = InstanceId::random(instance_type);

            // The form `--instance` parses, not the wire value
            let json = serde_json::to_string(&id).unwrap();
            assert_eq!(json, format!("\"{id}\""));
            assert_eq!(serde_json::from_str::<InstanceId>(&json).unwrap(), id);

            // Wire values written before still read back
            let legacy = id.to_wire().to_string();
            assert_eq!(serde_json::from_str::<InstanceId>(&legacy).unwrap(), id);
        }

        assert!(serde_json::from_str::<InstanceId>("\"probe-aaaaaaaaaaaa\"").is_err());
        assert!(serde_json::from_str::<InstanceId>("0").is_err());

        // The binary encoding is unaffected
        let id = InstanceId::random(InstanceType::Agent);
        assert_eq!(
            serde_cbor::to_vec(&id).unwrap(),
            serde_cbor::to_vec(&id.to_wire()).unwrap()
        );
    }

    #[test]
    fn test_wire_rejects_foreign_discriminants() {
        assert!(InstanceId::from_wire(0).is_err());
        assert!(InstanceId::from_wire(123).is_err());
        assert!(InstanceId::from_wire(4 << DISCRIMINANT_SHIFT).is_err());
        assert!(AgentId::from_wire(ServerId::random().to_wire()).is_err());

        let bytes = serde_cbor::to_vec(&0u64).unwrap();
        assert!(serde_cbor::from_slice::<InstanceId>(&bytes).is_err());
    }

    #[test]
    fn test_wire_round_trip() {
        for instance_type in InstanceType::iter() {
            let id = InstanceId::random(instance_type);
            assert_eq!(InstanceId::from_wire(id.to_wire()).unwrap(), id);
        }
    }

    #[test]
    fn test_ordering_by_type_then_body() {
        // The derived Ord agrees with the big-endian wire bytes ToKey stores,
        // so database iteration and in-memory sorts see the same order.
        let low_server = InstanceId::Server(ServerId(1));
        let high_agent = InstanceId::Agent(AgentId(BODY_MASK));
        assert!(high_agent < low_server);
        assert!(high_agent.to_wire().to_be_bytes() < low_server.to_wire().to_be_bytes());
    }

    #[test]
    fn test_typed_conversions() {
        let server = ServerId::random();
        let id: InstanceId = server.into();
        assert!(id.is_server());
        assert_eq!(ServerId::try_from(id).unwrap(), server);
        assert!(AgentId::try_from(id).is_err());
    }

    #[test]
    fn test_domain_membership() {
        // Domains group the estate, not the servers running it.
        assert!(!InstanceId::from(ServerId::random()).is_domain_member());

        assert!(InstanceId::from(AgentId::random()).is_domain_member());
        assert!(InstanceId::from(ClientId::random()).is_domain_member());
    }

    #[test]
    fn test_instance_type_method() {
        for instance_type in InstanceType::iter() {
            assert_eq!(
                InstanceId::random(instance_type).instance_type(),
                instance_type
            );
        }
    }
}
