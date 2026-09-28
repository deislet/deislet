//! Canonical storage authority for one application environment.
//!
//! The product contract spells a tenant as
//! `v1/<application UUIDv7>/<environment UUIDv7>`.  Keeping that grammar in a
//! shared strong type prevents each backend from inventing a separator or
//! accepting a different UUID spelling.  This is the *logical* authority
//! string; a backend that needs one filesystem component must encode it at
//! its persistence boundary rather than treating the `/` characters as a
//! path.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::{CanonicalId, CanonicalIdError};

/// Version prefix of the canonical tenant grammar.
pub const CANONICAL_TENANT_VERSION: &str = "v1";

/// Permanent storage authority for one application in one environment.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalTenantId {
    application_id: CanonicalId,
    environment_id: CanonicalId,
}

impl CanonicalTenantId {
    #[must_use]
    pub const fn new(application_id: CanonicalId, environment_id: CanonicalId) -> Self {
        Self {
            application_id,
            environment_id,
        }
    }

    #[must_use]
    pub const fn application_id(&self) -> CanonicalId {
        self.application_id
    }

    #[must_use]
    pub const fn environment_id(&self) -> CanonicalId {
        self.environment_id
    }

    /// A tenant derived deterministically from `label`, for fixtures and
    /// tests; see [`CanonicalId::derived_from_label`].
    #[must_use]
    pub fn derived_from_label(label: &str) -> Self {
        Self::new(
            CanonicalId::derived_from_label(&format!("{label}/application")),
            CanonicalId::derived_from_label(&format!("{label}/environment")),
        )
    }

    /// Decode the additive two-field form used by protobuf and legacy-tolerant
    /// JSON contracts.
    ///
    /// Both empty means an older producer and is accepted as `None`.  One
    /// field without the other is refused: guessing would bind an application
    /// to an authority the producer never named.
    pub fn from_wire_fields(
        application_id: &str,
        environment_id: &str,
    ) -> Result<Option<Self>, CanonicalTenantIdError> {
        match (application_id.is_empty(), environment_id.is_empty()) {
            (true, true) => Ok(None),
            (true, false) => Err(CanonicalTenantIdError::MissingApplicationId),
            (false, true) => Err(CanonicalTenantIdError::MissingEnvironmentId),
            (false, false) => {
                let application_id = application_id
                    .parse()
                    .map_err(CanonicalTenantIdError::InvalidApplicationId)?;
                let environment_id = environment_id
                    .parse()
                    .map_err(CanonicalTenantIdError::InvalidEnvironmentId)?;
                Ok(Some(Self::new(application_id, environment_id)))
            }
        }
    }
}

impl fmt::Display for CanonicalTenantId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{CANONICAL_TENANT_VERSION}/{}/{}",
            self.application_id, self.environment_id
        )
    }
}

impl FromStr for CanonicalTenantId {
    type Err = CanonicalTenantIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some(rest) = value.strip_prefix("v1/") else {
            return Err(CanonicalTenantIdError::InvalidGrammar);
        };
        let Some((application_id, environment_id)) = rest.split_once('/') else {
            return Err(CanonicalTenantIdError::InvalidGrammar);
        };
        if environment_id.contains('/') {
            return Err(CanonicalTenantIdError::InvalidGrammar);
        }
        Self::from_wire_fields(application_id, environment_id)?
            .ok_or(CanonicalTenantIdError::InvalidGrammar)
    }
}

impl Serialize for CanonicalTenantId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for CanonicalTenantId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

/// Why two canonical ids could not name one tenant authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanonicalTenantIdError {
    InvalidGrammar,
    MissingApplicationId,
    MissingEnvironmentId,
    InvalidApplicationId(CanonicalIdError),
    InvalidEnvironmentId(CanonicalIdError),
}

impl fmt::Display for CanonicalTenantIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidGrammar => formatter
                .write_str("canonical tenant must be v1/<application UUIDv7>/<environment UUIDv7>"),
            Self::MissingApplicationId => formatter.write_str(
                "canonical environment identity was supplied without an application identity",
            ),
            Self::MissingEnvironmentId => formatter.write_str(
                "canonical application identity was supplied without an environment identity",
            ),
            Self::InvalidApplicationId(error) => {
                write!(formatter, "invalid canonical application identity: {error}")
            }
            Self::InvalidEnvironmentId(error) => {
                write!(formatter, "invalid canonical environment identity: {error}")
            }
        }
    }
}

impl std::error::Error for CanonicalTenantIdError {}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "018f2518-7e38-7f80-8000-000000000001";
    const ENV: &str = "018f2518-7e38-7f80-8000-000000000002";

    #[test]
    fn exact_contract_spelling_round_trips() {
        let value = format!("v1/{APP}/{ENV}");
        let tenant: CanonicalTenantId = value.parse().unwrap();
        assert_eq!(tenant.to_string(), value);
        assert_eq!(tenant.application_id().to_string(), APP);
        assert_eq!(tenant.environment_id().to_string(), ENV);
        assert_eq!(
            serde_json::to_string(&tenant).unwrap(),
            format!("\"{value}\"")
        );
        assert_eq!(
            serde_json::from_str::<CanonicalTenantId>(&format!("\"{value}\"")).unwrap(),
            tenant
        );
    }

    #[test]
    fn grammar_and_uuid_spelling_are_singular() {
        for bad in [
            format!("{APP}/{ENV}"),
            format!("v2/{APP}/{ENV}"),
            format!("v1/{APP}/{ENV}/extra"),
            format!("v1/{}/{ENV}", APP.to_ascii_uppercase()),
        ] {
            assert!(bad.parse::<CanonicalTenantId>().is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn additive_wire_fields_accept_only_both_or_neither() {
        assert_eq!(CanonicalTenantId::from_wire_fields("", "").unwrap(), None);
        assert_eq!(
            CanonicalTenantId::from_wire_fields(APP, ENV)
                .unwrap()
                .unwrap()
                .to_string(),
            format!("v1/{APP}/{ENV}")
        );
        assert_eq!(
            CanonicalTenantId::from_wire_fields(APP, "").unwrap_err(),
            CanonicalTenantIdError::MissingEnvironmentId
        );
        assert_eq!(
            CanonicalTenantId::from_wire_fields("", ENV).unwrap_err(),
            CanonicalTenantIdError::MissingApplicationId
        );
    }
}
