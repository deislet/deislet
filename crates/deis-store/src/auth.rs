//! Which tenant a caller is.
//!
//! Every request carries an Ed25519-signed capability token in
//! `Authorization: Bearer <token>`, verified against the control plane's
//! published keys (picked by the token's `kid`). The tenant the call acts
//! for is the one tenant the token names for this service's binding - the
//! request itself names none - and it reaches the handler as a
//! [`CallerTenant`] extension. A token naming no tenant or several for the
//! binding is refused.
//!
//! There is no loopback bypass and no anonymous mode: every call must
//! present a valid capability token. The verifying keys come from the
//! control plane's JWKS, kept current by a background refresher the binary
//! starts (see `deis_capability::fetch`); this module only reads them.
//!
//! # Where the check runs
//!
//! Once, as a tonic interceptor applied in [`crate::server`] to every service
//! the process registers. It runs before any handler, so no storage path can
//! forget it, and it sits outside the handlers precisely so that adding a
//! fourth service cannot add a fourth unguarded door - see `server::guarded`.
//! The health service is deliberately left open: it reports whether this
//! process can answer, names no tenant and reads no data, and a liveness
//! probe that needs a tenant credential is a probe the control plane cannot
//! make.

use crate::tenant::CallerTenant;
use deis_capability::{AUD_STORE, CapabilityError, SharedVerifier, VerifiedCapability};
use deis_wire::CanonicalTenantId;
use tonic::{Request, Status};

/// The header carrying the capability token.
const AUTHORIZATION_HEADER: &str = "authorization";

/// The scheme prefix on an `Authorization` value.
const BEARER_PREFIX: &str = "bearer ";

/// What the process checks on every storage call.
///
/// Cheap to clone. One [`deis_capability::CapabilityVerifier`] is shared by
/// all three services - it holds the key ring, the revocation list and the
/// T8 degraded-mode clock, and the revocation poller feeds that one copy - so
/// a revoked `jti` or a raised epoch is refused by kv, sql and blob alike. A
/// `std::sync::Mutex` (not `tokio::sync::RwLock`) because
/// [`tonic::service::Interceptor`] is a synchronous trait and `block_on`
/// panics when called from within a tokio runtime context.
///
/// Each service (KvStore, Database, BlobStore) gets its own `StorageAuth`
/// with the binding baked in at construction time, so the interceptor does
/// not need to inspect the request URI to decide which store is targeted.
#[derive(Clone)]
pub struct StorageAuth {
    verifier: SharedVerifier,
    /// The binding this guard enforces: `"kv"`, `"sql"`, or `"blob"`.
    binding: &'static str,
}

impl StorageAuth {
    /// A guard for `binding` (`"kv"`, `"sql"`, or `"blob"`) over the
    /// process's shared verifier.
    pub fn new(verifier: SharedVerifier, binding: &'static str) -> Self {
        Self { verifier, binding }
    }

    /// The verifier this guard checks against, shared with the other guards
    /// and with the revocation poller.
    pub fn verifier(&self) -> &SharedVerifier {
        &self.verifier
    }

    /// Whether this service is enforcing capability checks. Always true:
    /// there is no anonymous or loopback bypass.
    pub fn is_enforcing(&self) -> bool {
        true
    }

    /// Extract the bearer token from the Authorization header.
    fn token_from_request<T>(request: &Request<T>) -> Option<String> {
        let value = request
            .metadata()
            .get(AUTHORIZATION_HEADER)
            .and_then(|value| value.to_str().ok())?;
        if value.len() > BEARER_PREFIX.len()
            && value[..BEARER_PREFIX.len()].eq_ignore_ascii_case(BEARER_PREFIX)
        {
            Some(value[BEARER_PREFIX.len()..].trim().to_string())
        } else {
            None
        }
    }

    /// Verify the caller's capability token and return the tenant it names.
    pub fn verify<T>(
        &self,
        request: &Request<T>,
    ) -> Result<(VerifiedCapability, CanonicalTenantId), CapabilityError> {
        let token = StorageAuth::token_from_request(request).ok_or_else(|| {
            CapabilityError::ParseError(
                "missing or malformed Authorization: Bearer <token> header".to_string(),
            )
        })?;
        let verified = self
            .verifier
            .lock()
            .expect("lock poisoned")
            .verify_tenant_with_grace(&token, AUD_STORE, self.binding)?;
        let tenant = verified
            .canonical_tenant_id
            .parse::<CanonicalTenantId>()
            .map_err(|_| CapabilityError::TenantNotAuthorized {
                tenant: verified.canonical_tenant_id.clone(),
            })?;
        Ok((verified, tenant))
    }
}

/// Redacted on purpose. A derived `Debug` would put credentials into every
/// panic message and every log line that formats a service.
impl std::fmt::Debug for StorageAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageAuth")
            .field("enforcing", &self.is_enforcing())
            .finish()
    }
}

/// Applied by [`crate::server`] to every service, so the check runs before
/// any handler and no storage path has to remember it.
impl tonic::service::Interceptor for StorageAuth {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        match self.verify(&request) {
            Ok((verified, tenant)) => {
                tracing::debug!(
                    node_id = %verified.node_id,
                    tenant = %verified.canonical_tenant_id,
                    binding = %verified.binding,
                    "capability verified"
                );
                request.extensions_mut().insert(CallerTenant(tenant));
                Ok(request)
            }
            Err(e) => Err(map_capability_error(e)),
        }
    }
}

/// Map a [`CapabilityError`] to the appropriate gRPC [`Status`].
fn map_capability_error(e: CapabilityError) -> Status {
    match &e {
        CapabilityError::Expired { .. } => {
            tracing::warn!("refused a storage call whose capability has expired");
            Status::unauthenticated(e.to_string())
        }
        CapabilityError::BadSignature(_) => {
            tracing::warn!(
                "refused a storage call with a bad capability signature; either the token was \
                 tampered with or this service no longer trusts the same signing key as \
                 deis-control"
            );
            Status::unauthenticated(e.to_string())
        }
        CapabilityError::ParseError(_) => {
            tracing::warn!("refused a storage call with an unparseable capability token");
            Status::unauthenticated(e.to_string())
        }
        CapabilityError::WrongAudience { .. } => {
            tracing::warn!("refused a storage call whose capability has the wrong audience");
            Status::permission_denied(e.to_string())
        }
        CapabilityError::WrongSubject { .. } => {
            tracing::warn!("refused a storage call whose capability has the wrong subject");
            Status::permission_denied(e.to_string())
        }
        CapabilityError::TenantNotAuthorized { .. } => {
            tracing::warn!(
                "refused a storage call whose capability does not authorize the requested tenant"
            );
            Status::permission_denied(e.to_string())
        }
        CapabilityError::BindingNotAuthorized { .. } => {
            tracing::warn!(
                "refused a storage call whose capability does not authorize the requested binding"
            );
            Status::permission_denied(e.to_string())
        }
        CapabilityError::NoSingleTenant { .. } => {
            tracing::warn!(
                "refused a storage call whose capability does not name exactly one tenant for \
                 this binding"
            );
            Status::permission_denied(e.to_string())
        }
        CapabilityError::AuthEpochTooLow { .. } => {
            tracing::warn!(
                "refused a storage call whose capability auth epoch is below the minimum"
            );
            Status::permission_denied(e.to_string())
        }
        CapabilityError::Revoked { .. } => {
            tracing::warn!("refused a storage call whose capability has been revoked");
            Status::permission_denied(e.to_string())
        }
        CapabilityError::NodeRevoked { node } => {
            tracing::warn!(node = %node, "refused a storage call from a revoked node");
            Status::permission_denied(e.to_string())
        }
        CapabilityError::NotYetValid { .. } => {
            tracing::warn!("refused a storage call whose capability is not yet valid");
            Status::unauthenticated(e.to_string())
        }
        CapabilityError::InvalidClaims(_) => {
            tracing::warn!("refused a storage call whose capability claims are invalid");
            Status::invalid_argument(e.to_string())
        }
        CapabilityError::NoRevocationList => {
            tracing::warn!(
                "refused a storage call because this service has not adopted the control plane's \
                 revocation list yet; it answers once the first list arrives"
            );
            Status::unavailable(e.to_string())
        }
        CapabilityError::UnknownKey { .. } => {
            tracing::warn!(
                "refused a storage call whose capability names a signing key this service does not \
                 hold; a refresh of the control plane's keys has been requested, so a token from \
                 a newly rotated key is accepted once it lands"
            );
            Status::unauthenticated(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn test_key() -> (SigningKey, ed25519_dalek::VerifyingKey) {
        let mut seed = [0u8; 32];
        seed[0..8].copy_from_slice(b"store-ts");
        let signing = SigningKey::from_bytes(&seed);
        let verifying = signing.verifying_key();
        (signing, verifying)
    }

    #[test]
    fn binding_is_baked_in_at_construction() {
        let (_, vk) = test_key();
        let verifier = deis_capability::CapabilityVerifier::new(vk).shared();
        let kv_auth = StorageAuth::new(verifier.clone(), "kv");
        let sql_auth = StorageAuth::new(verifier.clone(), "sql");
        let blob_auth = StorageAuth::new(verifier, "blob");
        assert_eq!(kv_auth.binding, "kv");
        assert_eq!(sql_auth.binding, "sql");
        assert_eq!(blob_auth.binding, "blob");
    }

    /// The three services share one verifier, so a revocation list adopted
    /// once is enforced by kv, sql and blob alike - not only by kv, which is
    /// the one the poller used to be wired to.
    #[test]
    fn a_revocation_reaches_every_binding() {
        use deis_capability::{
            AUD_STORE, CapabilityClaims, RevocationList, encode, resources_for_audience, revocation,
        };
        let (signing, vk) = test_key();
        let verifier = deis_capability::CapabilityVerifier::new(vk).shared();
        let guards: Vec<StorageAuth> = ["kv", "sql", "blob"]
            .into_iter()
            .map(|binding| StorageAuth::new(verifier.clone(), binding))
            .collect();
        let claims = CapabilityClaims::new(
            AUD_STORE,
            "iss",
            "spiffe://deis.internal/deislet/node/n1",
            1,
            std::time::Duration::from_secs(900),
            resources_for_audience(
                &CanonicalTenantId::derived_from_label("app.production").to_string(),
                AUD_STORE,
            ),
        );
        let token = encode(&claims, &signing);
        let request = |token: &str| {
            let mut request = Request::new(());
            request.metadata_mut().insert(
                AUTHORIZATION_HEADER,
                format!("Bearer {token}").parse().unwrap(),
            );
            request
        };
        // Before any list is adopted every binding refuses, and says the
        // service is not ready rather than that the caller is wrong.
        for mut guard in guards.clone() {
            let refused = tonic::service::Interceptor::call(&mut guard, request(&token))
                .expect_err("nothing is accepted before the first revocation list");
            assert_eq!(refused.code(), tonic::Code::Unavailable);
        }
        verifier
            .lock()
            .unwrap()
            .apply_revocation_list(RevocationList::new("iss", 1, vec![]))
            .unwrap();
        for mut guard in guards.clone() {
            let admitted = tonic::service::Interceptor::call(&mut guard, request(&token)).unwrap();
            assert_eq!(
                admitted.extensions().get::<CallerTenant>(),
                Some(&CallerTenant(CanonicalTenantId::derived_from_label(
                    "app.production"
                )))
            );
        }

        let signed = revocation::sign(
            &RevocationList::new("iss", 1, vec![claims.jti.clone()]),
            &signing,
        );
        let list = revocation::verify(&signed, verifier.lock().unwrap().key_ring()).unwrap();
        verifier
            .lock()
            .unwrap()
            .apply_revocation_list(list)
            .unwrap();
        for mut guard in guards {
            let refusal =
                tonic::service::Interceptor::call(&mut guard, request(&token)).unwrap_err();
            assert_eq!(
                refusal.code(),
                tonic::Code::PermissionDenied,
                "{}",
                guard.binding
            );
        }
    }

    #[test]
    fn bearer_token_is_extracted_from_request() {
        let mut request = Request::new(());
        request.metadata_mut().insert(
            AUTHORIZATION_HEADER,
            "Bearer my-token-here".parse().unwrap(),
        );
        assert_eq!(
            StorageAuth::token_from_request(&request).as_deref(),
            Some("my-token-here")
        );
    }

    #[test]
    fn bearer_prefix_is_case_insensitive() {
        let mut request = Request::new(());
        request.metadata_mut().insert(
            AUTHORIZATION_HEADER,
            "bearer my-token-here".parse().unwrap(),
        );
        assert_eq!(
            StorageAuth::token_from_request(&request).as_deref(),
            Some("my-token-here")
        );
    }

    #[test]
    fn missing_authorization_returns_none() {
        let request = Request::new(());
        assert!(StorageAuth::token_from_request(&request).is_none());
    }

    #[test]
    fn authorization_without_bearer_is_ignored() {
        let mut request = Request::new(());
        request
            .metadata_mut()
            .insert(AUTHORIZATION_HEADER, "Basic dGVzdDp0ZXN0".parse().unwrap());
        assert!(StorageAuth::token_from_request(&request).is_none());
    }
}
