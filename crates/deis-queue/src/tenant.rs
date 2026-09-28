//! Whose messages a call is about.
//!
//! The tenant is one application environment, named by its canonical id
//! `v1/<application id>/<environment id>`. It never comes from the request:
//! the interceptor in [`crate::auth`] verifies the caller's capability, takes
//! the one tenant it names for the `queue` binding, and hands it to the
//! handler as a [`CallerTenant`] request extension. A caller can only ever
//! reach the queues of the tenant its capability was issued for.
//!
//! The `tenant` column holds [`DeisletCanonicalTenantKey`]
//! (`ct1_<application id>_<environment id>`), the same key deis-store uses for
//! a tenant's unnamed namespace.

use crate::error::QueueError;
use deis_wire::{CanonicalTenantId, DeisletCanonicalTenantKey};
use std::fmt;
use tonic::Request;

/// The tenant a verified capability names, attached to the request by the
/// capability interceptor before any handler runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallerTenant(pub CanonicalTenantId);

/// A tenant as the queue stores it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantId {
    persistence_key: String,
    tenant: CanonicalTenantId,
}

impl TenantId {
    /// The queue tenant of `tenant`.
    #[must_use]
    pub fn new(tenant: CanonicalTenantId) -> Self {
        Self {
            persistence_key: DeisletCanonicalTenantKey::new(tenant).to_string(),
            tenant,
        }
    }

    /// Decode a key this service wrote into its `tenant` column.
    pub fn parse_persisted(raw: &str) -> Result<Self, QueueError> {
        let key = raw.parse::<DeisletCanonicalTenantKey>().map_err(|error| {
            QueueError::Tenant(format!("{raw:?} is not a Deislet tenant key: {error}"))
        })?;
        Ok(Self::new(key.tenant()))
    }

    /// A tenant derived from `label`, for tests (see
    /// [`CanonicalTenantId::derived_from_label`]).
    #[cfg(test)]
    pub(crate) fn for_label(label: &str) -> Self {
        Self::new(CanonicalTenantId::derived_from_label(label))
    }

    /// A request acting as this tenant, the way the capability interceptor
    /// leaves it, for tests that go through the gRPC surface.
    #[cfg(test)]
    pub(crate) fn request<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        request.extensions_mut().insert(CallerTenant(self.tenant));
        request
    }

    /// The key as it appears in the `tenant` column.
    pub fn as_str(&self) -> &str {
        &self.persistence_key
    }

    /// The canonical tenant this key stands for.
    pub fn tenant(&self) -> CanonicalTenantId {
        self.tenant
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.persistence_key)
    }
}

/// The tenant a request acts for: the one its verified capability names.
///
/// A request that reached a handler without passing the capability
/// interceptor carries no [`CallerTenant`] and is refused.
pub fn tenant_of<T>(request: &Request<T>) -> Result<TenantId, QueueError> {
    let CallerTenant(tenant) = *request.extensions().get::<CallerTenant>().ok_or_else(|| {
        tracing::warn!("a queue call reached a handler without a verified capability");
        QueueError::Tenant("the call carries no verified capability".to_string())
    })?;
    Ok(TenantId::new(tenant))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_environments_of_one_application_are_two_tenants() {
        let production = TenantId::for_label("shop.production");
        let preview = TenantId::for_label("shop.preview-7");
        assert_ne!(production, preview);
        assert_ne!(production.as_str(), preview.as_str());
    }

    #[test]
    fn a_key_round_trips() {
        let tenant = TenantId::for_label("shop.production");
        assert!(tenant.as_str().starts_with("ct1_"));
        assert_eq!(TenantId::parse_persisted(tenant.as_str()).unwrap(), tenant);
        for raw in ["shop.production", "..", "", "v1/a/b"] {
            assert!(TenantId::parse_persisted(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn the_tenant_is_the_one_the_capability_named() {
        let tenant = TenantId::for_label("shop.production");
        assert_eq!(tenant_of(&tenant.request(())).unwrap(), tenant);
    }

    #[test]
    fn a_request_without_a_verified_capability_is_refused() {
        let mut request = Request::new(());
        request
            .metadata_mut()
            .insert("x-deis-tenant", "shop.production".parse().unwrap());
        assert!(matches!(tenant_of(&request), Err(QueueError::Tenant(_))));
    }
}
