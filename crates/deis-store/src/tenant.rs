//! Whose data a call is about.
//!
//! The tenant is one application environment, named by its canonical id
//! `v1/<application id>/<environment id>`. It never comes from the request:
//! the interceptor in [`crate::auth`] verifies the caller's capability, takes
//! the one tenant that capability names for the service's binding, and hands
//! it to the handler as a [`CallerTenant`] request extension. A caller can
//! therefore only ever reach the tenant its capability was issued for, and
//! there is no header to forge.
//!
//! # A second namespace inside one tenant
//!
//! An application may want more than one KV, more than one database or more
//! than one bucket. That is a *binding*, and it arrives in the
//! [`BINDING_HEADER`]. What this service stores under is therefore the
//! **namespace**: the tenant, and the binding inside it when the call named
//! one. A binding is always composed onto the tenant the capability proved,
//! so it widens what a tenant may address inside its own data and never
//! which tenant.
//!
//! On disk a namespace is [`DeisletCanonicalNamespaceKey`]
//! (`ct1_<application id>_<environment id>[@BINDING]`): one column value, one
//! file name, one directory. It holds no `.`, `/` or `\`, so it can never
//! name anything outside the data directory.

// Re-exported so every component in the platform spells it the same way.
pub use deis_wire::conventions::BINDING_HEADER;

use crate::error::StoreError;
use deis_wire::{CanonicalTenantId, DeisletCanonicalNamespaceKey};
use std::fmt;
use tonic::Request;

/// The tenant a verified capability names, attached to the request by the
/// capability interceptor before any handler runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallerTenant(pub CanonicalTenantId);

/// A validated namespace, safe to use as a single path component.
///
/// Still named `TenantId` because that is what it is to every caller below: a
/// single opaque key that partitions the data. The binding widens what a
/// tenant may address inside itself; it never widens which tenant.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantId {
    /// The backend key used in SQLite columns and as one path component.
    persistence_key: String,
    key: DeisletCanonicalNamespaceKey,
}

impl TenantId {
    /// The namespace `binding` names inside `tenant`, or the tenant's unnamed
    /// one for `None`.
    pub fn new(tenant: CanonicalTenantId, binding: Option<&str>) -> Result<Self, StoreError> {
        let key = DeisletCanonicalNamespaceKey::new(tenant, binding).map_err(|error| {
            StoreError::Tenant(format!(
                "binding {:?} is not a usable namespace name: {error}",
                binding.unwrap_or_default()
            ))
        })?;
        Ok(Self {
            persistence_key: key.to_string(),
            key,
        })
    }

    /// Decode a key this service wrote: a column value, a database file stem
    /// or a blob directory name.
    pub fn parse_persisted(raw: &str) -> Result<Self, StoreError> {
        let key = raw
            .parse::<DeisletCanonicalNamespaceKey>()
            .map_err(|error| {
                StoreError::Tenant(format!("{raw:?} is not a Deislet namespace key: {error}"))
            })?;
        Ok(Self {
            persistence_key: raw.to_string(),
            key,
        })
    }

    /// A namespace of the tenant derived from `label` (see
    /// [`CanonicalTenantId::derived_from_label`]), for tests: `"shop.production"`
    /// or `"shop.production@HOT"`.
    #[cfg(test)]
    pub(crate) fn for_label(label: &str) -> Self {
        let (tenant, binding) = match label.split_once('@') {
            Some((tenant, binding)) => (tenant, Some(binding)),
            None => (label, None),
        };
        Self::new(CanonicalTenantId::derived_from_label(tenant), binding).unwrap()
    }

    /// A request acting as this namespace, the way the capability
    /// interceptor and the caller's binding header leave it, for tests that
    /// go through the gRPC surface.
    #[cfg(test)]
    pub(crate) fn request<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        request.extensions_mut().insert(CallerTenant(self.tenant()));
        if let Some(binding) = self.binding() {
            request
                .metadata_mut()
                .insert(BINDING_HEADER, binding.parse().unwrap());
        }
        request
    }

    /// The key as it appears on disk and in the `tenant` column.
    pub fn as_str(&self) -> &str {
        &self.persistence_key
    }

    /// The tenant this namespace belongs to: what the capability authorised,
    /// and what a log line should name when it talks about the tenant rather
    /// than about one of its namespaces.
    pub fn tenant(&self) -> CanonicalTenantId {
        self.key.tenant()
    }

    /// The binding this namespace belongs to, or `None` for the tenant's
    /// unnamed one.
    pub fn binding(&self) -> Option<&str> {
        self.key.binding()
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.persistence_key)
    }
}

/// The namespace a request acts in: the tenant its verified capability names
/// and the binding its [`BINDING_HEADER`] selects inside that tenant.
///
/// A request that reached a handler without passing the capability
/// interceptor carries no [`CallerTenant`] and is refused. A binding header
/// that is present but unreadable, or not a legal name, is refused rather
/// than dropped: falling back to the unnamed namespace would silently serve a
/// caller another namespace of its own tenant.
pub fn tenant_of<T>(request: &Request<T>) -> Result<TenantId, StoreError> {
    let CallerTenant(tenant) = *request.extensions().get::<CallerTenant>().ok_or_else(|| {
        tracing::warn!("a storage call reached a handler without a verified capability");
        StoreError::Tenant("the call carries no verified capability".to_string())
    })?;

    let binding = match request.metadata().get(BINDING_HEADER) {
        None => None,
        Some(value) => Some(value.to_str().map_err(|_| {
            tracing::warn!(
                header = BINDING_HEADER,
                "binding header is not printable ASCII, refusing the request rather than \
                 serving the tenant's unnamed namespace in its place"
            );
            StoreError::Tenant(format!("{BINDING_HEADER} is not valid ASCII"))
        })?),
    };

    TenantId::new(tenant, binding).inspect_err(|e| {
        tracing::warn!(
            tenant = %tenant,
            binding = %binding.unwrap_or_default().escape_debug(),
            error = %e,
            "rejected namespace; the request is refused and nothing on disk is touched"
        );
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant(label: &str) -> CanonicalTenantId {
        CanonicalTenantId::derived_from_label(label)
    }

    fn verified<T>(message: T, label: &str) -> Request<T> {
        let mut request = Request::new(message);
        request.extensions_mut().insert(CallerTenant(tenant(label)));
        request
    }

    #[test]
    fn two_environments_of_one_application_are_two_namespaces() {
        let production = TenantId::new(tenant("shop.production"), None).unwrap();
        let preview = TenantId::new(tenant("shop.preview"), None).unwrap();
        assert_ne!(production, preview);
        assert_ne!(production.as_str(), preview.as_str());
    }

    #[test]
    fn a_namespace_key_round_trips_and_is_one_path_component() {
        let hot = TenantId::new(tenant("shop.production"), Some("HOT")).unwrap();
        assert!(hot.as_str().starts_with("ct1_"));
        assert!(hot.as_str().ends_with("@HOT"));
        assert!(!hot.as_str().contains(['.', '/', '\\']));
        assert_eq!(TenantId::parse_persisted(hot.as_str()).unwrap(), hot);
        assert_eq!(hot.tenant(), tenant("shop.production"));
        assert_eq!(hot.binding(), Some("HOT"));
    }

    #[test]
    fn names_that_are_not_keys_this_service_wrote_are_refused() {
        for raw in ["shop.production", "shop.production@HOT", "..", "a/b", ""] {
            assert!(TenantId::parse_persisted(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn the_tenant_is_the_one_the_capability_named() {
        let request = verified((), "shop.production");
        let namespace = tenant_of(&request).unwrap();
        assert_eq!(namespace.tenant(), tenant("shop.production"));
        assert_eq!(namespace.binding(), None);
    }

    #[test]
    fn a_request_without_a_verified_capability_is_refused() {
        let mut request = Request::new(());
        request
            .metadata_mut()
            .insert("x-deis-tenant", "shop.production".parse().unwrap());
        assert!(matches!(tenant_of(&request), Err(StoreError::Tenant(_))));
    }

    #[test]
    fn the_binding_header_selects_a_namespace_inside_the_tenant() {
        let mut request = verified((), "shop.production");
        request
            .metadata_mut()
            .insert(BINDING_HEADER, "HOT".parse().unwrap());
        let hot = tenant_of(&request).unwrap();
        assert_eq!(hot.binding(), Some("HOT"));
        assert_eq!(hot.tenant(), tenant("shop.production"));

        for hostile in ["a.b", "..", "HOT@COLD", "a/b"] {
            request
                .metadata_mut()
                .insert(BINDING_HEADER, hostile.parse().unwrap());
            assert!(
                matches!(tenant_of(&request), Err(StoreError::Tenant(_))),
                "{hostile} must be refused"
            );
        }
    }
}
