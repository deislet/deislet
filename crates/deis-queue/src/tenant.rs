//! The one tenant dimension this service understands.
//!
//! A tenant id arrives out of band in the `x-deis-tenant` gRPC metadata
//! header, so the existing queue.proto messages need no tenant field. To this
//! the live service the value is one opaque legacy key. Migration and durable
//! recovery code may also construct or decode the canonical backend key, but
//! that path remains separate from request parsing.
//!
//! The rules are character-for-character the ones deis-store applies to the
//! bare tenant form: this service calls
//! [`deis_wire::conventions::split_tenant`], and deis-store's
//! `TenantId::parse` goes through `split_namespace`, whose bare half is that
//! same function. (The store also accepts the namespaced `app.env@BINDING`
//! form for its named bindings; no binding reaches this service, so the bare
//! form is the whole of what a queue tenant can be.) The queue keeps
//! everything in one SQLite file, so a tenant id never names a path here and
//! traversal is not the danger it is there - but a value deis-store refuses
//! and this service accepts would mean a tenant could own a queue it can
//! never own storage for, and the two services disagreeing about what a
//! tenant is would surface as a mystery much later. One definition, applied
//! twice.
//!
//! A bare application id, which is what callers sent before this contract
//! existed, has one part and is refused like any other malformed value.

// Re-exported so every component in the platform spells these the same
// way; see deis_wire::conventions for why they live in one place.
pub use deis_wire::conventions::TENANT_HEADER;

use crate::error::QueueError;
use deis_wire::conventions::split_tenant;
use deis_wire::{CanonicalTenantId, DeisletCanonicalTenantKey};
use std::fmt;
use tonic::Request;

/// A validated tenant id.
///
/// Live requests build one only through [`TenantId::parse`]. Migration and
/// recovery paths may use [`TenantId::from_canonical`] or
/// [`TenantId::parse_persisted`]. Either generation is printable, bounded and
/// validated before it reaches a queue query.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantId {
    persistence_key: String,
    logical_tenant: String,
    canonical: bool,
}

impl TenantId {
    /// Validate `raw` with [`split_tenant`], keeping the whole value.
    ///
    /// The halves are deliberately dropped on the floor: messages belong to a
    /// tenant, not to an application, so nothing below may be tempted to
    /// deliver one tenant's message to another because they share a prefix.
    pub fn parse(raw: &str) -> Result<Self, QueueError> {
        if split_tenant(raw).is_none() {
            return Err(QueueError::Tenant(format!(
                "{raw:?} is not a tenant id; it must be exactly two parts of ASCII letters, \
                 digits, '_' or '-', at most 64 bytes each, joined by '.'"
            )));
        }
        Ok(Self {
            persistence_key: raw.to_string(),
            logical_tenant: raw.to_string(),
            canonical: false,
        })
    }

    /// Build the canonical migration target used in the queue's tenant
    /// column. Live metadata remains legacy-only until writer fencing and
    /// canonical capabilities are installed by the migration coordinator.
    #[must_use]
    pub fn from_canonical(tenant: CanonicalTenantId) -> Self {
        Self {
            persistence_key: DeisletCanonicalTenantKey::new(tenant).to_string(),
            logical_tenant: tenant.to_string(),
            canonical: true,
        }
    }

    /// Decode either generation from durable queue state without making the
    /// canonical physical spelling a valid wire credential.
    pub fn parse_persisted(raw: &str) -> Result<Self, QueueError> {
        if let Ok(legacy) = Self::parse(raw) {
            return Ok(legacy);
        }
        let canonical = raw
            .parse::<DeisletCanonicalTenantKey>()
            .map_err(|canonical_error| {
                QueueError::Tenant(format!(
                    "{raw:?} is neither a legacy tenant nor a canonical Deislet persistence key: {canonical_error}"
                ))
            })?;
        Ok(Self::from_canonical(canonical.tenant()))
    }

    /// The id as it appears in metadata and in the `tenant` column.
    pub fn as_str(&self) -> &str {
        &self.persistence_key
    }

    /// The logical authority represented by this backend key.
    pub fn logical_tenant(&self) -> &str {
        &self.logical_tenant
    }

    #[must_use]
    pub const fn is_canonical(&self) -> bool {
        self.canonical
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.persistence_key)
    }
}

/// Pull the tenant out of a request's metadata.
///
/// A missing or malformed header is refused before any message is touched.
/// It is logged at warn rather than error: the call is lost, but nothing
/// already queued is affected, and the usual cause is a caller that forgot
/// the header rather than a fault in the queue.
pub fn tenant_of<T>(request: &Request<T>) -> Result<TenantId, QueueError> {
    let raw = request
        .metadata()
        .get(TENANT_HEADER)
        .ok_or_else(|| {
            tracing::warn!(
                header = TENANT_HEADER,
                "request has no tenant header, refusing it; the caller must stamp the tenant or it \
                 would be consuming another tenant's messages"
            );
            QueueError::Tenant(format!("missing {TENANT_HEADER} header"))
        })?
        .to_str()
        .map_err(|_| {
            tracing::warn!(
                header = TENANT_HEADER,
                "tenant header is not printable ASCII, refusing the request"
            );
            QueueError::Tenant(format!("{TENANT_HEADER} is not valid ASCII"))
        })?;

    TenantId::parse(raw).inspect_err(|e| {
        tracing::warn!(
            raw = %raw.escape_debug(),
            error = %e,
            "rejected tenant id; the request is refused and no message is enqueued or delivered"
        );
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use deis_wire::conventions::{APP_ID_HEADER, tenant_id};

    const CANONICAL_APP: &str = "018f2518-7e38-7f80-8000-000000000001";
    const CANONICAL_ENV: &str = "018f2518-7e38-7f80-8000-000000000002";

    fn canonical_tenant() -> CanonicalTenantId {
        CanonicalTenantId::new(
            CANONICAL_APP.parse().unwrap(),
            CANONICAL_ENV.parse().unwrap(),
        )
    }

    #[test]
    fn accepts_what_the_platform_builds() {
        for id in [
            tenant_id("my-app", "production"),
            tenant_id("my-app", "preview-42"),
            tenant_id("_system", "production"),
            tenant_id(&"x".repeat(64), &"y".repeat(64)),
        ] {
            assert!(TenantId::parse(&id).is_ok(), "{id} should be accepted");
        }
    }

    #[test]
    fn a_bare_application_id_is_not_a_tenant() {
        // What callers sent before this contract existed. Accepting it would
        // let a preview deployment consume production's messages.
        for id in ["my-app", "_system", "a"] {
            assert!(
                TenantId::parse(id).is_err(),
                "{id} has no environment and must be refused"
            );
        }
    }

    #[test]
    fn rejects_the_same_ids_deis_store_rejects() {
        for id in [
            "..",
            ".",
            "../etc",
            "a/b",
            "a\\b",
            "a.sqlite/..",
            "a b.production",
            "app\0.production",
            "",
            ".production",
            "app.",
            "a.b.c",
            &format!("{}.production", "x".repeat(65)),
        ] {
            assert!(
                TenantId::parse(id).is_err(),
                "{} should be rejected",
                id.escape_debug()
            );
        }
    }

    #[test]
    fn two_environments_of_one_application_are_two_tenants() {
        let production = TenantId::parse(&tenant_id("shop", "production")).unwrap();
        let preview = TenantId::parse(&tenant_id("shop", "preview-7")).unwrap();
        assert_ne!(production, preview);
        assert_ne!(production.as_str(), preview.as_str());
    }

    #[test]
    fn canonical_target_is_disjoint_and_persistently_decodable() {
        let target = TenantId::from_canonical(canonical_tenant());
        assert_eq!(
            target.as_str(),
            format!("ct1_{CANONICAL_APP}_{CANONICAL_ENV}")
        );
        assert_eq!(
            target.logical_tenant(),
            format!("v1/{CANONICAL_APP}/{CANONICAL_ENV}")
        );
        assert!(target.is_canonical());
        assert!(!target.as_str().contains(['.', '/']));
        assert_eq!(TenantId::parse_persisted(target.as_str()).unwrap(), target);
    }

    #[test]
    fn canonical_keys_are_not_accepted_from_live_request_parsers_yet() {
        let physical = TenantId::from_canonical(canonical_tenant());
        assert!(TenantId::parse(physical.as_str()).is_err());
        assert!(TenantId::parse(&canonical_tenant().to_string()).is_err());

        let mut request = Request::new(());
        request
            .metadata_mut()
            .insert(TENANT_HEADER, physical.as_str().parse().unwrap());
        assert!(matches!(tenant_of(&request), Err(QueueError::Tenant(_))));
    }

    #[test]
    fn reads_the_tenant_from_metadata() {
        let mut req = Request::new(());
        req.metadata_mut()
            .insert(TENANT_HEADER, "my-app.production".parse().unwrap());
        assert_eq!(tenant_of(&req).unwrap().as_str(), "my-app.production");
    }

    #[test]
    fn the_application_header_alone_is_not_enough() {
        // deis-runtime has to stamp the tenant header; the app id header
        // names the application and is no longer the storage tenant. A
        // caller that only sends the old one is refused rather than served
        // from some default namespace.
        let mut req = Request::new(());
        req.metadata_mut()
            .insert(APP_ID_HEADER, "my-app".parse().unwrap());
        assert!(matches!(tenant_of(&req), Err(QueueError::Tenant(_))));
    }

    #[test]
    fn a_call_with_no_tenant_is_refused() {
        let req = Request::new(());
        let err = tenant_of(&req).unwrap_err();
        assert!(matches!(err, QueueError::Tenant(_)));
    }

    #[test]
    fn a_malformed_tenant_in_metadata_is_refused() {
        let mut req = Request::new(());
        req.metadata_mut()
            .insert(TENANT_HEADER, "../../etc".parse().unwrap());
        assert!(matches!(tenant_of(&req), Err(QueueError::Tenant(_))));
    }
}
