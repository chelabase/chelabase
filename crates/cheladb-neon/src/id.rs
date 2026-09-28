//! Neon tenant and timeline ids: 32 lowercase hex characters, no dashes.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

/// Always 32 lowercase hex characters: the form the storage controller accepts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NeonId(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid Neon id {0:?}; expected 32 lowercase hex characters")]
pub struct InvalidNeonId(String);

impl NeonId {
    /// A random id.
    pub fn new() -> Self {
        Self::from(Uuid::new_v4())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A random id, like [`NeonId::new`].
impl Default for NeonId {
    fn default() -> Self {
        Self::new()
    }
}

/// The uuid's simple form, so one of our ids always maps to the same Neon id.
impl From<Uuid> for NeonId {
    fn from(id: Uuid) -> Self {
        Self(id.simple().to_string())
    }
}

impl FromStr for NeonId {
    type Err = InvalidNeonId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(Self(s.to_string()))
        } else {
            Err(InvalidNeonId(s.to_string()))
        }
    }
}

impl fmt::Display for NeonId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for NeonId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for NeonId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_ids_are_32_lowercase_hex() {
        let (a, b) = (NeonId::new(), NeonId::new());
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), 32);
        assert!(a.as_str().parse::<NeonId>().is_ok());
    }

    #[test]
    fn a_uuid_maps_to_its_simple_form() {
        let u: Uuid = "67e55044-10b1-426f-9247-bb680e5fe0c8".parse().unwrap();
        assert_eq!(NeonId::from(u).as_str(), "67e5504410b1426f9247bb680e5fe0c8");
        assert_eq!(NeonId::from(u), NeonId::from(u));
        assert_eq!(
            NeonId::from(u).to_string(),
            "67e5504410b1426f9247bb680e5fe0c8"
        );
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "",
            "67e5504410b1426f9247bb680e5fe0c",
            "67e5504410b1426f9247bb680e5fe0c8a",
            "67E5504410B1426F9247BB680E5FE0C8",
            "67e55044-10b1-426f-9247-bb680e5fe0c8",
            "g7e5504410b1426f9247bb680e5fe0c8",
        ] {
            assert!(bad.parse::<NeonId>().is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn serde_round_trips_and_validates() {
        let id = NeonId::from(Uuid::nil());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"00000000000000000000000000000000\"");
        assert_eq!(serde_json::from_str::<NeonId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<NeonId>("\"ABC\"").is_err());
    }
}
