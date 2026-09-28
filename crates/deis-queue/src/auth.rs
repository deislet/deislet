//! Proving that a caller may act as the tenant it names.
//!
//! Every request names its tenant in `x-deis-tenant` and carries an
//! Ed25519-signed capability token in `Authorization: Bearer <token>`.
//! The capability is verified against the control plane's published keys
//! (picked by the token's `kid`), and
//! the token's resources must include at least one (tenant, "queue") pair.
//!
//! There is no loopback bypass and no anonymous mode: every call must
//! present a valid capability token. The verifying keys come from the
//! control plane's JWKS, kept current by a background refresher the binary
//! starts (see `deis_capability::fetch`); this module only reads them.

use deis_capability::{CapabilityError, CapabilityVerifier, KeyRing, SharedVerifier, VerifiedCapability};
use deis_wire::conventions::TENANT_HEADER;
use tonic::{Request, Status};

/// The header carrying the capability token.
const AUTHORIZATION_HEADER: &str = "authorization";

/// The scheme prefix on an `Authorization` value.
const BEARER_PREFIX: &str = "bearer ";

/// What the process checks on every queue call.
///
/// Cheap to clone: the verifier is behind an `Arc` so one instance guards
/// all services. Uses a `std::sync::Mutex` (not `tokio::sync::RwLock`)
/// because [`tonic::service::Interceptor`] is a synchronous trait and
/// `block_on` panics when called from within a tokio runtime context.
#[derive(Clone)]
pub struct QueueAuth {
    verifier: SharedVerifier,
}

impl QueueAuth {
    /// Build a new interceptor over the control plane's key ring.
    pub fn new(keys: KeyRing) -> Self {
        Self {
            verifier: CapabilityVerifier::with_key_ring(keys).shared(),
        }
    }

    /// The verifier this guard checks against, shared with the revocation
    /// poller and the metrics endpoint.
    pub fn verifier(&self) -> &SharedVerifier {
        &self.verifier
    }

    /// Whether this service is enforcing capability checks. Always true:
    /// there is no anonymous or loopback bypass.
    pub fn is_enforcing(&self) -> bool {
        true
    }

    /// Extract the tenant from request headers.
    fn tenant_from_request<T>(request: &Request<T>) -> Option<String> {
        request
            .metadata()
            .get(TENANT_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
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

    /// Verify the caller's capability token.
    ///
    /// The binding is always `"queue"` for the queue service.
    pub async fn verify<T>(
        &self,
        request: &Request<T>,
    ) -> Result<VerifiedCapability, CapabilityError> {
        let token = Self::token_from_request(request).ok_or_else(|| {
            CapabilityError::ParseError(
                "missing or malformed Authorization: Bearer <token> header".to_string(),
            )
        })?;
        let tenant = Self::tenant_from_request(request).ok_or_else(|| {
            CapabilityError::ParseError(format!("missing or unreadable {TENANT_HEADER} header"))
        })?;

        self.verifier
            .lock()
            .expect("lock poisoned")
            .verify_with_grace(&token, "queue", &tenant, "queue")
    }
}

/// Redacted on purpose. A derived `Debug` would put credentials into every
/// panic message and every log line that formats a service.
impl std::fmt::Debug for QueueAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueAuth")
            .field("enforcing", &self.is_enforcing())
            .finish()
    }
}

/// Applied by [`crate::server`] to every service, so the check runs before
/// any handler and no queue path has to remember it.
impl tonic::service::Interceptor for QueueAuth {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let verifier = self.verifier.clone();

        let tenant = Self::tenant_from_request(&request);
        let token = Self::token_from_request(&request);

        let v = verifier.lock().expect("lock poisoned");
        let t = token.as_deref().unwrap_or("");
        let ten = tenant.as_deref().unwrap_or("");
        let result = v.verify_with_grace(t, "queue", ten, "queue");

        match result {
            Ok(verified) => {
                tracing::debug!(
                    node_id = %verified.node_id,
                    tenant = %verified.canonical_tenant_id,
                    "queue capability verified"
                );
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
            tracing::warn!("refused a queue call whose capability has expired");
            Status::unauthenticated(e.to_string())
        }
        CapabilityError::BadSignature(_) => {
            tracing::warn!(
                "refused a queue call with a bad capability signature; either the token was \
                 tampered with or this service no longer trusts the same signing key as \
                 deis-control"
            );
            Status::unauthenticated(e.to_string())
        }
        CapabilityError::ParseError(_) => {
            tracing::warn!("refused a queue call with an unparseable capability token");
            Status::unauthenticated(e.to_string())
        }
        CapabilityError::WrongAudience { .. } => {
            tracing::warn!("refused a queue call whose capability has the wrong audience");
            Status::permission_denied(e.to_string())
        }
        CapabilityError::WrongSubject { .. } => {
            tracing::warn!("refused a queue call whose capability has the wrong subject");
            Status::permission_denied(e.to_string())
        }
        CapabilityError::TenantNotAuthorized { .. } => {
            tracing::warn!(
                "refused a queue call whose capability does not authorize the requested tenant"
            );
            Status::permission_denied(e.to_string())
        }
        CapabilityError::BindingNotAuthorized { .. } => {
            tracing::warn!("refused a queue call whose capability does not authorize the binding");
            Status::permission_denied(e.to_string())
        }
        CapabilityError::AuthEpochTooLow { .. } => {
            tracing::warn!("refused a queue call whose capability auth epoch is below the minimum");
            Status::permission_denied(e.to_string())
        }
        CapabilityError::Revoked { .. } => {
            tracing::warn!("refused a queue call whose capability has been revoked");
            Status::permission_denied(e.to_string())
        }
        CapabilityError::NodeRevoked { node } => {
            tracing::warn!(node = %node, "refused a queue call from a revoked node");
            Status::permission_denied(e.to_string())
        }
        CapabilityError::NotYetValid { .. } => {
            tracing::warn!("refused a queue call whose capability is not yet valid");
            Status::unauthenticated(e.to_string())
        }
        CapabilityError::InvalidClaims(_) => {
            tracing::warn!("refused a queue call whose capability claims are invalid");
            Status::invalid_argument(e.to_string())
        }
        CapabilityError::UnknownKey { .. } => {
            tracing::warn!(
                "refused a queue call whose capability names a signing key this service does not \
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

    #[test]
    fn bearer_token_is_extracted_from_request() {
        let mut request = Request::new(());
        request.metadata_mut().insert(
            AUTHORIZATION_HEADER,
            "Bearer my-token-here".parse().unwrap(),
        );
        assert_eq!(
            QueueAuth::token_from_request(&request).as_deref(),
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
            QueueAuth::token_from_request(&request).as_deref(),
            Some("my-token-here")
        );
    }

    #[test]
    fn missing_authorization_returns_none() {
        let request = Request::new(());
        assert!(QueueAuth::token_from_request(&request).is_none());
    }

    #[test]
    fn authorization_without_bearer_is_ignored() {
        let mut request = Request::new(());
        request
            .metadata_mut()
            .insert(AUTHORIZATION_HEADER, "Basic dGVzdDp0ZXN0".parse().unwrap());
        assert!(QueueAuth::token_from_request(&request).is_none());
    }
}
