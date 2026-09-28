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
//! backend-specific persistence boundary. They are shared here so store,
//! queue, snapshot and restore cannot each invent a different encoding.
//!
//! This encoding is not an application-facing standard: nothing on the wire
//! carries it, a capability names the logical tenant. It is injective and
//! reversible, and it contains no `.`, `/` or `\`, so it is always exactly
//! one path component.

use std::{fmt, str::FromStr};

use crate::conventions::is_valid_binding_name;
use crate::{CanonicalIdError, CanonicalTenantId};

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

/// Separator between a tenant key and an optional binding name. Neither a
/// canonical id nor a binding name may contain it.
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
/// The unnamed namespace is the tenant key itself. A named binding adds the
/// binding name the application declared (`HOT`, `MEDIA`), validated with
/// [`is_valid_binding_name`]. The name only selects a namespace *inside* the
/// tenant the capability proved; it is never an authority of its own, so a
/// rename is a new, empty namespace of the same tenant and nothing else.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeisletCanonicalNamespaceKey {
    tenant: CanonicalTenantId,
    binding: Option<String>,
}

impl DeisletCanonicalNamespaceKey {
    /// The namespace of `tenant` that `binding` names, or the tenant's
    /// unnamed one for `None`. Refuses a binding name that
    /// [`is_valid_binding_name`] refuses.
    pub fn new(
        tenant: CanonicalTenantId,
        binding: Option<&str>,
    ) -> Result<Self, DeisletStorageKeyError> {
        if let Some(name) = binding
            && !is_valid_binding_name(name)
        {
            return Err(DeisletStorageKeyError::InvalidBindingName);
        }
        Ok(Self {
            tenant,
            binding: binding.map(str::to_string),
        })
    }

    #[must_use]
    pub const fn tenant(&self) -> CanonicalTenantId {
        self.tenant
    }

    #[must_use]
    pub fn binding(&self) -> Option<&str> {
        self.binding.as_deref()
    }
}

impl fmt::Display for DeisletCanonicalNamespaceKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        DeisletCanonicalTenantKey::new(self.tenant).fmt(formatter)?;
        if let Some(binding) = &self.binding {
            write!(formatter, "{BINDING_SEPARATOR}{binding}")?;
        }
        Ok(())
    }
}

impl FromStr for DeisletCanonicalNamespaceKey {
    type Err = DeisletStorageKeyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (tenant, binding) = match value.split_once(BINDING_SEPARATOR) {
            Some((tenant, binding)) => (tenant, Some(binding)),
            None => (value, None),
        };
        let tenant = tenant.parse::<DeisletCanonicalTenantKey>()?.tenant();
        Self::new(tenant, binding)
    }
}

/// Why a persisted canonical Deislet storage key could not be decoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeisletStorageKeyError {
    InvalidTenantGrammar,
    InvalidApplicationId(CanonicalIdError),
    InvalidEnvironmentId(CanonicalIdError),
    InvalidBindingName,
}

impl fmt::Display for DeisletStorageKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTenantGrammar => formatter.write_str(
                "canonical Deislet tenant key must be ct1_<application UUIDv7>_<environment UUIDv7>",
            ),
            Self::InvalidApplicationId(error) => {
                write!(formatter, "invalid application ID in Deislet tenant key: {error}")
            }
            Self::InvalidEnvironmentId(error) => {
                write!(formatter, "invalid environment ID in Deislet tenant key: {error}")
            }
            Self::InvalidBindingName => formatter.write_str(
                "a binding name must be 1 to 64 ASCII letters, digits, '_' or '-'",
            ),
        }
    }
}

impl std::error::Error for DeisletStorageKeyError {}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "018f2518-7e38-7f80-8000-000000000001";
    const ENV: &str = "018f2518-7e38-7f80-8000-000000000002";

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
    fn a_named_binding_is_a_namespace_inside_the_tenant() {
        let key = DeisletCanonicalNamespaceKey::new(canonical_tenant(), Some("HOT")).unwrap();
        let expected = format!("ct1_{APP}_{ENV}@HOT");
        assert_eq!(key.to_string(), expected);
        assert_eq!(
            expected.parse::<DeisletCanonicalNamespaceKey>().unwrap(),
            key
        );
        assert_eq!(key.binding(), Some("HOT"));
        assert_eq!(key.tenant(), canonical_tenant());
        let unnamed = DeisletCanonicalNamespaceKey::new(canonical_tenant(), None).unwrap();
        assert_ne!(unnamed, key);
        assert_eq!(unnamed.to_string(), format!("ct1_{APP}_{ENV}"));
    }

    #[test]
    fn physical_keys_are_one_cross_platform_path_component() {
        for key in [
            DeisletCanonicalNamespaceKey::new(canonical_tenant(), None).unwrap(),
            DeisletCanonicalNamespaceKey::new(canonical_tenant(), Some(&"x".repeat(64))).unwrap(),
        ] {
            let key = key.to_string();
            assert!(key.is_ascii());
            assert!(key.len() < 194);
            assert!(!key.ends_with(['.', ' ']));
            assert!(
                !key.chars().any(|character| matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\0' | '.'
                )),
                "{key} is not one portable path component"
            );
        }
    }

    #[test]
    fn malformed_keys_and_binding_names_fail_closed() {
        for bad in [
            format!("{APP}_{ENV}"),
            format!("ct1_{APP}"),
            format!("ct2_{APP}_{ENV}"),
            format!("ct1_{}_{ENV}", APP.to_ascii_uppercase()),
            format!("ct1_550e8400-e29b-41d4-a716-446655440000_{ENV}"),
            format!("ct1_{APP}_{ENV}@"),
            format!("ct1_{APP}_{ENV}@a.b"),
            format!("ct1_{APP}_{ENV}@.."),
            format!("ct1_{APP}_{ENV}@HOT@COLD"),
            "shop.production".to_string(),
            "shop.production@HOT".to_string(),
        ] {
            assert!(
                bad.parse::<DeisletCanonicalNamespaceKey>().is_err(),
                "accepted {bad}"
            );
        }
        for hostile in ["", "a/b", "a.b", "a@b", ".."] {
            assert!(DeisletCanonicalNamespaceKey::new(canonical_tenant(), Some(hostile)).is_err());
        }
    }
}
