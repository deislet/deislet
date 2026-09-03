//! The one tenant dimension this service understands.
//!
//! A tenant id arrives out of band in the `x-deis-tenant` gRPC metadata
//! header, so the existing storage.proto messages need no tenant field. To
//! the live service the value is one opaque legacy key. Migration and recovery
//! code may also construct or decode the canonical backend key, but that path
//! remains separate from request parsing.
//!
//! Its shape is nonetheless fixed, because the platform builds it from an
//! application and an environment (`deis_wire::conventions::tenant_id`) and
//! two environments of one application must never land in the same namespace.
//! [`deis_wire::conventions::split_tenant`] is therefore the validator: it
//! accepts exactly two legal parts joined by a separator neither part may
//! contain, and the character set has no `.`, `/` or `\`.
//!
//! That last property is what makes the id safe to put on disk. It names
//! files and directories here - `db/<tenant>.sqlite`, `blob/<tenant>/...` -
//! so it is validated hard rather than sanitised: a value that could escape
//! the data directory is refused outright and no request is served from a
//! fallback namespace.
//!
//! A bare application id, which is what callers sent before this contract
//! existed, has one part and is refused like any other malformed value.
//!
//! # A second namespace inside one tenant
//!
//! An application may want more than one KV, more than one database or more
//! than one bucket. That is a *binding*, and it arrives in a second header,
//! `x-deis-binding`. What this service stores under is therefore the
//! **namespace**: the tenant, and the binding inside it when the call named
//! one. It is still one opaque key here - one more column value, one more
//! file name, one more directory - and the tenant boundary is untouched,
//! because a binding is always composed onto the tenant the call already
//! proved it may be.
//!
//! The composition is [`deis_wire::conventions::namespace_id`], and its one
//! property is that it is injective: the separator is refused inside a tenant
//! part and inside a binding name, so no pair spells another pair's namespace
//! and no named namespace spells a bare tenant. A call that names no binding
//! therefore addresses exactly the rows, files and objects it addressed
//! before bindings existed.

// Re-exported so every component in the platform spells these the same
// way; see deis_wire::conventions for why they live in one place.
pub use deis_wire::conventions::{BINDING_HEADER, TENANT_HEADER};

use crate::error::StoreError;
use deis_wire::conventions::{namespace_id, split_namespace};
use deis_wire::{CanonicalId, CanonicalTenantId, DeisletCanonicalNamespaceKey};
use std::fmt;
use tonic::Request;

/// A validated namespace id, safe to use as a single path component.
///
/// Live requests build one through [`TenantId::parse`] or
/// [`TenantId::of_binding`]. Migration, snapshot and recovery paths may use
/// [`TenantId::from_canonical`] or [`TenantId::parse_persisted`]. Both
/// generations are one validated path component, so `..`, path separators and
/// ambiguous cross-generation spellings are not expressible.
///
/// Still named `TenantId` because that is what it is to every caller below: a
/// single opaque key that partitions the data. The binding widens what a
/// tenant may address inside itself; it never widens which tenant.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantId {
    /// The backend key used in SQLite columns and as one path component.
    persistence_key: String,
    /// The logical authority. Equal to `persistence_key` for legacy names;
    /// `v1/<app UUIDv7>/<environment UUIDv7>` for canonical identities.
    tenant: String,
    /// Legacy logical binding name or canonical permanent binding UUIDv7.
    binding: Option<String>,
    canonical: bool,
}

impl TenantId {
    /// Validate `raw` with [`split_namespace`], keeping the whole value.
    ///
    /// Accepts both `app.environment` and `app.environment@binding`: the
    /// second is what a call naming a binding is stored under, and rows
    /// already on disk carry both forms, so anything that reads a namespace
    /// back out of the database has to accept what was written into it.
    ///
    /// The parts are deliberately dropped on the floor: this service stores
    /// per namespace, so nothing below may be tempted to treat two of them as
    /// one because they share a prefix.
    pub fn parse(raw: &str) -> Result<Self, StoreError> {
        let Some((tenant, binding)) = split_namespace(raw) else {
            return Err(StoreError::Tenant(format!(
                "{raw:?} is not a namespace id; it must be exactly two parts of ASCII letters, \
                 digits, '_' or '-', at most 64 bytes each, joined by '.', optionally followed \
                 by '@' and a binding name of the same character set"
            )));
        };
        Ok(Self {
            persistence_key: raw.to_string(),
            tenant: tenant.to_string(),
            binding: binding.map(str::to_string),
            canonical: false,
        })
    }

    /// Build the future canonical migration target without opening it to a
    /// request carrying a legacy tenant-wide capability.
    ///
    /// This is an internal persistence constructor, not a wire parser. Live
    /// requests still enter through [`tenant_of`] and [`Self::parse`], both of
    /// which accept only the legacy grammar until the storage writer fence and
    /// binding-scoped capability contract are enforced.
    #[must_use]
    pub fn from_canonical(tenant: CanonicalTenantId, binding_id: Option<CanonicalId>) -> Self {
        let persistence_key = DeisletCanonicalNamespaceKey::new(tenant, binding_id).to_string();
        Self {
            persistence_key,
            tenant: tenant.to_string(),
            binding: binding_id.map(|id| id.to_string()),
            canonical: true,
        }
    }

    /// Decode a key already stored by this backend.
    ///
    /// Snapshot, restore and orphan scans need to understand both generations
    /// once copy starts. This deliberately remains separate from [`Self::parse`]
    /// so merely placing a canonical physical key in `x-deis-tenant` cannot
    /// bypass the migration coordinator.
    pub fn parse_persisted(raw: &str) -> Result<Self, StoreError> {
        if let Ok(legacy) = Self::parse(raw) {
            return Ok(legacy);
        }
        let canonical = raw
            .parse::<DeisletCanonicalNamespaceKey>()
            .map_err(|canonical_error| {
                StoreError::Tenant(format!(
                    "{raw:?} is neither a legacy namespace nor a canonical Deislet persistence key: {canonical_error}"
                ))
            })?;
        Ok(Self::from_canonical(
            canonical.tenant(),
            canonical.binding_id(),
        ))
    }

    /// One binding's namespace inside a tenant.
    ///
    /// `binding` is `None` for the tenant's unnamed namespace, which is what
    /// every call made before bindings existed addressed and what
    /// `context.services.kv` still addresses.
    pub fn of_binding(tenant: &str, binding: Option<&str>) -> Result<Self, StoreError> {
        Self::parse(&namespace_id(tenant, binding))
    }

    /// The id as it appears on disk and in the `tenant` column.
    pub fn as_str(&self) -> &str {
        &self.persistence_key
    }

    /// The `app.environment` half: what the capability token authorises, and
    /// what a log line should name when it is talking about the tenant rather
    /// than about one of its namespaces.
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// The binding this namespace belongs to, or `None` for the tenant's
    /// unnamed one.
    pub fn binding(&self) -> Option<&str> {
        self.binding.as_deref()
    }

    /// Whether this value names the canonical migration target generation.
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
/// A missing or malformed header is refused before any storage is touched.
/// It is logged at warn rather than error: the request is lost, but nothing
/// stored is affected, and the usual cause is a caller that forgot the header
/// rather than a fault in the store.
pub fn tenant_of<T>(request: &Request<T>) -> Result<TenantId, StoreError> {
    let raw = request
        .metadata()
        .get(TENANT_HEADER)
        .ok_or_else(|| {
            tracing::warn!(
                header = TENANT_HEADER,
                "request has no tenant header, refusing it; the caller must stamp the tenant or it \
                 would be reading another tenant's data"
            );
            StoreError::Tenant(format!("missing {TENANT_HEADER} header"))
        })?
        .to_str()
        .map_err(|_| {
            tracing::warn!(
                header = TENANT_HEADER,
                "tenant header is not printable ASCII, refusing the request"
            );
            StoreError::Tenant(format!("{TENANT_HEADER} is not valid ASCII"))
        })?;

    // Absent is the tenant's unnamed namespace, which is what every caller
    // that predates bindings sends and what `context.services.kv` still
    // reaches. A header that is present but unreadable is refused rather than
    // dropped: falling back to the unnamed namespace would silently serve a
    // caller data belonging to a different namespace of the same tenant.
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

    TenantId::of_binding(raw, binding).inspect_err(|e| {
        tracing::warn!(
            raw = %raw.escape_debug(),
            binding = %binding.unwrap_or_default().escape_debug(),
            error = %e,
            "rejected namespace id; the request is refused and nothing on disk is touched"
        );
    })
}

#[cfg(test)]
mod tests {

    use super::*;
    use deis_wire::conventions::{APP_ID_HEADER, tenant_id};

    const CANONICAL_APP: &str = "018f2518-7e38-7f80-8000-000000000001";
    const CANONICAL_ENV: &str = "018f2518-7e38-7f80-8000-000000000002";
    const CANONICAL_BINDING: &str = "018f2518-7e38-7f80-8000-000000000003";

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
        // put every environment of an application back in one namespace.
        for id in ["my-app", "_system", "a"] {
            assert!(
                TenantId::parse(id).is_err(),
                "{id} has no environment and must be refused"
            );
        }
    }

    #[test]
    fn rejects_ids_that_could_escape_the_data_dir() {
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
    fn canonical_target_is_disjoint_path_safe_and_persistently_decodable() {
        let binding_id = CANONICAL_BINDING.parse().unwrap();
        let target = TenantId::from_canonical(canonical_tenant(), Some(binding_id));
        assert_eq!(
            target.as_str(),
            format!("ct1_{CANONICAL_APP}_{CANONICAL_ENV}@{CANONICAL_BINDING}")
        );
        assert_eq!(
            target.tenant(),
            format!("v1/{CANONICAL_APP}/{CANONICAL_ENV}")
        );
        assert_eq!(target.binding(), Some(CANONICAL_BINDING));
        assert!(target.is_canonical());
        assert!(!target.as_str().contains('/'));
        assert_eq!(TenantId::parse_persisted(target.as_str()).unwrap(), target);
        assert_ne!(target.as_str(), "ct1.production@HOT");
    }

    #[test]
    fn canonical_keys_are_not_accepted_from_live_request_parsers_yet() {
        let physical = TenantId::from_canonical(canonical_tenant(), None);
        assert!(TenantId::parse(physical.as_str()).is_err());
        assert!(TenantId::parse(&canonical_tenant().to_string()).is_err());

        let mut request = Request::new(());
        request
            .metadata_mut()
            .insert(TENANT_HEADER, physical.as_str().parse().unwrap());
        assert!(matches!(tenant_of(&request), Err(StoreError::Tenant(_))));
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
        assert!(matches!(tenant_of(&req), Err(StoreError::Tenant(_))));
    }

    #[test]
    fn a_binding_is_a_second_namespace_inside_one_tenant() {
        let plain = TenantId::of_binding("shop.production", None).unwrap();
        let hot = TenantId::of_binding("shop.production", Some("HOT")).unwrap();
        let cold = TenantId::of_binding("shop.production", Some("COLD")).unwrap();

        // Three different keys on disk, one tenant. That is the whole
        // guarantee: a second binding widens what this tenant can address
        // inside itself and nothing else.
        assert_eq!(plain.as_str(), "shop.production");
        assert_eq!(hot.as_str(), "shop.production@HOT");
        assert_ne!(hot, cold);
        assert_eq!(hot.tenant(), "shop.production");
        assert_eq!(cold.tenant(), "shop.production");
        assert_eq!(plain.binding(), None);
        assert_eq!(hot.binding(), Some("HOT"));
    }

    #[test]
    fn no_binding_name_can_be_made_to_collide_with_another() {
        // The attempt: pick a binding name that, composed onto this tenant,
        // spells a namespace belonging to some other pair. Every route is
        // refused before the value can become a file name, so the composed id
        // is unique to the pair that produced it.
        for hostile in [
            "HOT@COLD",   // a second separator
            "production", // fine on its own, and still its own namespace
            "..",         // a parent directory
            ".",
            "a/b",
            "a.b", // the tenant separator
            "",
            &"x".repeat(65),
        ] {
            let composed = TenantId::of_binding("shop.production", Some(hostile));
            match hostile {
                "production" => assert_eq!(
                    composed.unwrap().as_str(),
                    "shop.production@production",
                    "a legal name that happens to read like an environment is still its own \
                     namespace of this tenant"
                ),
                _ => assert!(
                    composed.is_err(),
                    "binding {} must be refused",
                    hostile.escape_debug()
                ),
            }
        }

        // And the one that would actually cross a boundary: a tenant whose
        // environment is spelled to look like somebody else's binding.
        let a = TenantId::of_binding("shop.production", Some("preview")).unwrap();
        let b = TenantId::parse("shop.production").unwrap();
        assert_ne!(a, b);
        assert_ne!(a.as_str(), b.as_str());
        // There is no legal environment name containing '@', so no tenant can
        // ever spell a namespace that carries a binding.
        assert!(TenantId::parse("shop.production@HOT").unwrap().binding() == Some("HOT"));
        assert!(TenantId::of_binding("shop.production@HOT", Some("X")).is_err());
    }

    #[test]
    fn the_binding_header_selects_the_namespace() {
        let mut req = Request::new(());
        req.metadata_mut()
            .insert(TENANT_HEADER, "shop.production".parse().unwrap());
        assert_eq!(tenant_of(&req).unwrap().as_str(), "shop.production");

        req.metadata_mut()
            .insert(BINDING_HEADER, "HOT".parse().unwrap());
        assert_eq!(tenant_of(&req).unwrap().as_str(), "shop.production@HOT");

        // A binding that is not a legal name is refused outright. Serving the
        // unnamed namespace instead would hand the caller another namespace's
        // data under the name it asked for.
        req.metadata_mut()
            .insert(BINDING_HEADER, "a.b".parse().unwrap());
        assert!(matches!(tenant_of(&req), Err(StoreError::Tenant(_))));
    }

    #[test]
    fn missing_metadata_is_a_tenant_error() {
        let req = Request::new(());
        let err = tenant_of(&req).unwrap_err();
        assert!(matches!(err, StoreError::Tenant(_)));
    }

    #[test]
    fn traversal_attempt_in_metadata_is_refused() {
        let mut req = Request::new(());
        req.metadata_mut()
            .insert(TENANT_HEADER, "../../etc".parse().unwrap());
        assert!(matches!(tenant_of(&req), Err(StoreError::Tenant(_))));
    }
}
