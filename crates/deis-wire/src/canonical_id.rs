//! Canonical product identities shared by the control plane and its clients.
//!
//! A field called `canonical_id` is an authority and storage boundary, not a
//! display string. Keeping the validation here means a malformed database row
//! or API response cannot quietly become a second spelling of the same object.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use uuid::Uuid;

/// A lowercase, hyphenated UUIDv7 used as a permanent product identity.
///
/// UUIDv7 provides time ordering without deriving identity from a mutable
/// name. The accepted text form is deliberately singular: uppercase, compact
/// and braced UUID spellings are rejected even though the UUID parser can
/// understand them.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalId(Uuid);

impl CanonicalId {
    /// Allocate a new canonical identity using the current Unix timestamp.
    #[must_use]
    pub fn new_v7() -> Self {
        Self(Uuid::now_v7())
    }

    /// A canonical identity derived deterministically from `label`.
    ///
    /// For fixtures and tests that need a stable, readable way to name a
    /// tenant (`CanonicalId::derived_from_label("shop")` is the same id on
    /// every run). Never used to allocate a real identity: the catalog
    /// allocates with [`Self::new_v7`].
    #[must_use]
    pub fn derived_from_label(label: &str) -> Self {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(label.as_bytes());
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        Self(
            uuid::Builder::from_custom_bytes(bytes)
                .with_version(uuid::Version::SortRand)
                .into_uuid(),
        )
    }

    /// Expose the parsed UUID when code needs its version or byte layout.
    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

/// Why a string could not be used as a canonical product identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanonicalIdError {
    /// The value is not a UUID in any supported textual form.
    InvalidUuid,
    /// The UUID parses, but its version nibble is not 7.
    NotVersion7,
    /// The UUID is v7 but is not lowercase canonical hyphenated text.
    NonCanonicalSpelling,
}

impl fmt::Display for CanonicalIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidUuid => "canonical identity is not a UUID",
            Self::NotVersion7 => "canonical identity is not UUIDv7",
            Self::NonCanonicalSpelling => {
                "canonical identity must use lowercase hyphenated UUID spelling"
            }
        })
    }
}

impl std::error::Error for CanonicalIdError {}

impl FromStr for CanonicalId {
    type Err = CanonicalIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let uuid = Uuid::parse_str(value).map_err(|_| CanonicalIdError::InvalidUuid)?;
        if uuid.get_version_num() != 7 {
            return Err(CanonicalIdError::NotVersion7);
        }
        if uuid.to_string() != value {
            return Err(CanonicalIdError::NonCanonicalSpelling);
        }
        Ok(Self(uuid))
    }
}

impl fmt::Display for CanonicalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for CanonicalId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for CanonicalId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "018f2518-7e38-7f80-8000-000000000001";

    #[test]
    fn a_derived_id_is_a_stable_canonical_v7() {
        let id = CanonicalId::derived_from_label("shop");
        assert_eq!(id, CanonicalId::derived_from_label("shop"));
        assert_ne!(id, CanonicalId::derived_from_label("shop-eu"));
        assert_eq!(id.to_string().parse::<CanonicalId>().unwrap(), id);
    }

    #[test]
    fn generated_id_has_the_only_allowed_text_form() {
        let id = CanonicalId::new_v7();
        let text = id.to_string();
        assert_eq!(id.as_uuid().get_version_num(), 7);
        assert_eq!(text.parse::<CanonicalId>().unwrap(), id);
        assert_eq!(text, text.to_ascii_lowercase());
        assert_eq!(text.len(), 36);
    }

    #[test]
    fn parser_rejects_other_versions_and_other_spellings() {
        assert_eq!(
            "550e8400-e29b-41d4-a716-446655440000"
                .parse::<CanonicalId>()
                .unwrap_err(),
            CanonicalIdError::NotVersion7
        );
        assert_eq!(
            VALID
                .to_ascii_uppercase()
                .parse::<CanonicalId>()
                .unwrap_err(),
            CanonicalIdError::NonCanonicalSpelling
        );
        assert_eq!(
            VALID.replace('-', "").parse::<CanonicalId>().unwrap_err(),
            CanonicalIdError::NonCanonicalSpelling
        );
        assert_eq!(
            "not-an-id".parse::<CanonicalId>().unwrap_err(),
            CanonicalIdError::InvalidUuid
        );
    }

    #[test]
    fn serde_round_trip_is_a_json_string_and_validation_is_not_bypassed() {
        let id = VALID.parse::<CanonicalId>().unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{VALID}\""));
        assert_eq!(serde_json::from_str::<CanonicalId>(&json).unwrap(), id);
        assert!(
            serde_json::from_str::<CanonicalId>("\"550e8400-e29b-41d4-a716-446655440000\"")
                .is_err()
        );
    }
}
