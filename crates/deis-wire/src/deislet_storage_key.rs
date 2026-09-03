//! Deislet backend keys for canonical storage identities.
//!
//! Edge Canon's logical tenant is [`CanonicalTenantId`], spelled
//! `v1/<application UUIDv7>/<environment UUIDv7>`. That spelling is the one
//! sent over the wire, signed by a capability and written into audit records.
//! It is deliberately *not* a filesystem path: the two `/` characters are
//! grammar, not directories.
//!
//! The Deislet store and queue nevertheless need one bounded, path-safe value
//! for a SQLite key, database file or blob directory. The types below are that
//! backend-specific persistence boundary. They are shared here so migration,
//! store, queue, snapshot and restore cannot each invent a different encoding.
//!
//! This encoding is not an application-facing standard and must never be put
//! into `x-deis-tenant` or used as an HMAC subject. It is injective, reversible
//! and disjoint from the legacy `application.environment` family: every
//! legacy tenant contains `.`, while a canonical Deislet key contains none.

use std::{fmt, str::FromStr};

use crate::{CanonicalId, CanonicalIdError, CanonicalTenantId};

/// Versioned prefix for Deislet's canonical physical tenant encoding.
///
/// `ct1_` means "canonical tenant encoding 1". It is intentionally not the
/// logical `v1/` prefix: confusing the physical key with the logical authority
/// must be visible rather than looking like an innocent separator change.
pub const DEISLET_CANONICAL_TENANT_KEY_PREFIX: &str = "ct1_";

/// Separator between canonical identities in Deislet persistence keys.
///
/// UUID canonical text cannot contain `_`, so parsing is unambiguous. It is
/// also safe in one filename component on Linux, macOS and Windows.
const ID_SEPARATOR: char = '_';

/// Separator between a tenant key and an optional permanent binding ID.
///
/// This matches the existing local namespace separator, but the value after
/// it is a UUIDv7 rather than a mutable logical binding name.
const BINDING_SEPARATOR: char = '@';

/// Deislet's one-component persistence key for a canonical tenant.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeisletCanonicalTenantKey(CanonicalTenantId);

impl DeisletCanonicalTenantKey {
    #[must_use]
    pub const fn new(tenant: CanonicalTenantId) -> Self {
        Self(tenant)
    }

    /// The logical authority this backend key encodes.
    #[must_use]
    pub const fn tenant(&self) -> CanonicalTenantId {
        self.0
    }
}

impl fmt::Display for DeisletCanonicalTenantKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{DEISLET_CANONICAL_TENANT_KEY_PREFIX}{}{ID_SEPARATOR}{}",
            self.0.application_id(),
            self.0.environment_id()
        )
    }
}

impl FromStr for DeisletCanonicalTenantKey {
    type Err = DeisletStorageKeyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let rest = value
            .strip_prefix(DEISLET_CANONICAL_TENANT_KEY_PREFIX)
            .ok_or(DeisletStorageKeyError::InvalidTenantGrammar)?;
        let (application, environment) = rest
            .split_once(ID_SEPARATOR)
            .ok_or(DeisletStorageKeyError::InvalidTenantGrammar)?;
        if environment.contains(ID_SEPARATOR) || application.is_empty() || environment.is_empty() {
            return Err(DeisletStorageKeyError::InvalidTenantGrammar);
        }
        let application_id = application
            .parse()
            .map_err(DeisletStorageKeyError::InvalidApplicationId)?;
        let environment_id = environment
            .parse()
            .map_err(DeisletStorageKeyError::InvalidEnvironmentId)?;
        Ok(Self::new(CanonicalTenantId::new(
            application_id,
            environment_id,
        )))
    }
}

/// Deislet's one-component persistence key for a canonical storage namespace.
///
/// The unnamed namespace is the tenant key itself. A named logical binding is
/// represented by its permanent binding UUIDv7, never by its mutable display
/// name. The mapping from logical name to this ID belongs to an immutable
/// environment binding snapshot.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeisletCanonicalNamespaceKey {
    tenant: CanonicalTenantId,
    binding_id: Option<CanonicalId>,
}

impl DeisletCanonicalNamespaceKey {
    #[must_use]
    pub const fn new(tenant: CanonicalTenantId, binding_id: Option<CanonicalId>) -> Self {
        Self { tenant, binding_id }
    }

    #[must_use]
    pub const fn tenant(&self) -> CanonicalTenantId {
        self.tenant
    }

    #[must_use]
    pub const fn binding_id(&self) -> Option<CanonicalId> {
        self.binding_id
    }
}

impl fmt::Display for DeisletCanonicalNamespaceKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        DeisletCanonicalTenantKey::new(self.tenant).fmt(formatter)?;
        if let Some(binding_id) = self.binding_id {
            write!(formatter, "{BINDING_SEPARATOR}{binding_id}")?;
        }
        Ok(())
    }
}

impl FromStr for DeisletCanonicalNamespaceKey {
    type Err = DeisletStorageKeyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (tenant, binding_id) = match value.split_once(BINDING_SEPARATOR) {
            Some((tenant, binding)) => {
                if binding.contains(BINDING_SEPARATOR) || binding.is_empty() {
                    return Err(DeisletStorageKeyError::InvalidNamespaceGrammar);
                }
                let binding_id = binding
                    .parse()
                    .map_err(DeisletStorageKeyError::InvalidBindingId)?;
                (tenant, Some(binding_id))
            }
            None => (value, None),
        };
        let tenant = tenant.parse::<DeisletCanonicalTenantKey>()?.tenant();
        Ok(Self::new(tenant, binding_id))
    }
}

/// Why a persisted canonical Deislet storage key could not be decoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeisletStorageKeyError {
    InvalidTenantGrammar,
    InvalidNamespaceGrammar,
    InvalidApplicationId(CanonicalIdError),
    InvalidEnvironmentId(CanonicalIdError),
    InvalidBindingId(CanonicalIdError),
}

impl fmt::Display for DeisletStorageKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTenantGrammar => formatter.write_str(
                "canonical Deislet tenant key must be ct1_<application UUIDv7>_<environment UUIDv7>",
            ),
            Self::InvalidNamespaceGrammar => formatter.write_str(
                "canonical Deislet namespace key may contain at most one non-empty @<binding UUIDv7> suffix",
            ),
            Self::InvalidApplicationId(error) => {
                write!(formatter, "invalid application ID in Deislet tenant key: {error}")
            }
            Self::InvalidEnvironmentId(error) => {
                write!(formatter, "invalid environment ID in Deislet tenant key: {error}")
            }
            Self::InvalidBindingId(error) => {
                write!(formatter, "invalid binding ID in Deislet namespace key: {error}")
            }
        }
    }
}

impl std::error::Error for DeisletStorageKeyError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conventions::{namespace_id, tenant_id};

    const APP: &str = "018f2518-7e38-7f80-8000-000000000001";
    const ENV: &str = "018f2518-7e38-7f80-8000-000000000002";
    const BINDING: &str = "018f2518-7e38-7f80-8000-000000000003";

    fn canonical_tenant() -> CanonicalTenantId {
        CanonicalTenantId::new(APP.parse().unwrap(), ENV.parse().unwrap())
    }

    #[test]
    fn exact_physical_spelling_round_trips_to_the_logical_authority() {
        let logical = canonical_tenant();
        let key = DeisletCanonicalTenantKey::new(logical);
        let expected = format!("ct1_{APP}_{ENV}");
        assert_eq!(key.to_string(), expected);
        assert_eq!(expected.parse::<DeisletCanonicalTenantKey>().unwrap(), key);
        assert_eq!(key.tenant(), logical);
        assert_eq!(logical.to_string(), format!("v1/{APP}/{ENV}"));
    }

    #[test]
    fn a_namespace_uses_permanent_binding_identity_not_a_display_name() {
        let binding_id = BINDING.parse().unwrap();
        let key = DeisletCanonicalNamespaceKey::new(canonical_tenant(), Some(binding_id));
        let expected = format!("ct1_{APP}_{ENV}@{BINDING}");
        assert_eq!(key.to_string(), expected);
        assert_eq!(
            expected.parse::<DeisletCanonicalNamespaceKey>().unwrap(),
            key
        );
        assert_eq!(key.binding_id(), Some(binding_id));
        assert!(!key.to_string().contains("HOT"));
    }

    #[test]
    fn physical_keys_are_one_cross_platform_path_component() {
        for key in [
            DeisletCanonicalNamespaceKey::new(canonical_tenant(), None).to_string(),
            DeisletCanonicalNamespaceKey::new(canonical_tenant(), Some(BINDING.parse().unwrap()))
                .to_string(),
        ] {
            assert!(key.is_ascii());
            assert!(key.len() < 194);
            assert!(!key.ends_with(['.', ' ']));
            assert!(
                !key.chars().any(|character| matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\0'
                )),
                "{key} is not one portable path component"
            );
        }
    }

    #[test]
    fn canonical_and_every_legacy_namespace_family_are_disjoint() {
        let canonical =
            DeisletCanonicalNamespaceKey::new(canonical_tenant(), Some(BINDING.parse().unwrap()))
                .to_string();
        for legacy in [
            namespace_id(&tenant_id("ct1", "production"), None),
            namespace_id(&tenant_id("ct1_018f2518", "production"), Some("HOT")),
            namespace_id(&tenant_id("shop", "production"), Some(BINDING)),
        ] {
            assert!(legacy.contains('.'));
            assert!(!canonical.contains('.'));
            assert_ne!(canonical, legacy);
            assert!(legacy.parse::<DeisletCanonicalNamespaceKey>().is_err());
        }
    }

    #[test]
    fn malformed_or_noncanonical_id_spellings_fail_closed() {
        for bad in [
            format!("{APP}_{ENV}"),
            format!("ct1_{APP}"),
            format!("ct2_{APP}_{ENV}"),
            format!("ct1_{}_{ENV}", APP.to_ascii_uppercase()),
            format!("ct1_550e8400-e29b-41d4-a716-446655440000_{ENV}"),
            format!("ct1_{APP}_{ENV}@"),
            format!("ct1_{APP}_{ENV}@HOT"),
            format!("ct1_{APP}_{ENV}@{BINDING}@{BINDING}"),
        ] {
            assert!(
                bad.parse::<DeisletCanonicalNamespaceKey>().is_err(),
                "accepted {bad}"
            );
        }
    }
}
