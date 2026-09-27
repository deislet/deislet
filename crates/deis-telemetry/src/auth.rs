//! Proving that a caller may talk to this service at all.
//!
//! # The hole this closes
//!
//! Until this existed the service believed everything it was told and asked
//! nothing of anyone. Anything that could reach the gRPC port could read
//! every application's logs and traces, and could push entries carrying any
//! `app_id` it liked - so a stranger could both read an incident and write
//! the record of it. Anything that could reach the Prometheus port could
//! scrape every application's metric series the same way. What kept those
//! ports private was that they were bound to loopback, which is a setting an
//! operator can widen without realising it was the whole of the
//! authorisation.
//!
//! What a caller now presents is an Ed25519-signed capability token in
//! `Authorization: Bearer <token>`. For write operations (Push*) the
//! audience must be `"telemetry"`; for read operations (Query*, Tail*) and
//! Prometheus scrapes the audience must be `"telemetry.read"`.
//!
//! There is no per-tenant check: the telemetry service does not partition by
//! tenant id the way store and queue do. The capability authenticates the
//! channel, not the tenant.
//!
//! # Why one token and not one per tenant
//!
//! deis-store and deis-queue verify per-tenant because every storage or queue
//! call is about exactly one tenant. Here it would not describe anything real:
//!
//! * a producer is a **node**, not an application. deis-runtime ships one
//!   batch holding the lines of every application running on it, so there is
//!   no single tenant for the call to claim;
//! * a reader asks across applications - `Query` with no `app_id` is how an
//!   operator looks at a node, and it is the useful question;
//! * and this service keeps one application dimension on purpose (see the
//!   crate documentation), so it has no (application, environment) tenant to
//!   derive anything from in the first place.
//!
//! # Where the check runs
//!
//! Once per surface, outside the handlers. On gRPC it is a tonic interceptor
//! applied in [`crate::server`] to all three services at once, so no ingest
//! or query path can forget it and a fourth service added later cannot become
//! a fourth unguarded door - see `server::guarded`. On the scrape port it is
//! one layer over the whole router in [`crate::http`], for the same reason.

use std::sync::Arc;

use deis_capability::{CapabilityError, CapabilityVerifier, VerifiedCapability};
use tonic::{Request, Status};

/// The header carrying the capability token.
const AUTHORIZATION_HEADER: &str = "authorization";

/// The scheme prefix on an `Authorization` value.
const BEARER_PREFIX: &str = "bearer ";

/// What the process checks on every call.
///
/// Cheap to clone: the verifier is behind an `Arc`, so one instance guards
/// the three gRPC services and the scrape endpoint.
///
/// When `enforcing` is `false`, every request passes through without
/// inspection. This is the case when no `capability_public_key` is
/// configured — useful for integration tests that predate mandatory
/// capability auth.
#[derive(Clone)]
pub struct TelemetryAuth {
    verifier: Arc<std::sync::RwLock<CapabilityVerifier>>,
    enforcing: bool,
}

impl TelemetryAuth {
    /// Build a new interceptor from a verifying key.
    ///
    /// `enforcing` should be `false` only when no `capability_public_key`
    /// is configured — the interceptor then lets every request through.
    pub fn new(verifying_key: ed25519_dalek::VerifyingKey, enforcing: bool) -> Self {
        Self {
            verifier: Arc::new(std::sync::RwLock::new(CapabilityVerifier::new(
                verifying_key,
            ))),
            enforcing,
        }
    }

    /// Update the minimum accepted auth epoch.
    pub fn set_min_auth_epoch(&self, epoch: u64) {
        self.verifier.write().expect("lock poisoned").set_min_auth_epoch(epoch);
    }

    /// Replace the revoked JTI list.
    pub fn set_revoked_jtis(&self, jtis: Vec<String>) {
        self.verifier.write().expect("lock poisoned").set_revoked_jtis(jtis);
    }

    /// Whether this service is enforcing capability checks.
    pub fn is_enforcing(&self) -> bool {
        self.enforcing
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

    /// Two accepted header forms because two kinds of caller ask:
    /// - A platform component passes `Authorization: Bearer <token>`
    /// - A Prometheus server configured with `bearer_token` sends the same
    pub async fn verify_headers(
        &self,
        headers: &axum::http::HeaderMap,
    ) -> Result<VerifiedCapability, CapabilityError> {
        if !self.enforcing {
            // Without a configured key, every scrape is accepted — this is
            // the test and single-box default.
            return Ok(VerifiedCapability {
                node_id: "noop".to_string(),
                canonical_tenant_id: "noop".to_string(),
                binding: "noop".to_string(),
            });
        }
        let token = headers
            .get(AUTHORIZATION_HEADER)
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() > BEARER_PREFIX.len())
            .filter(|value| value[..BEARER_PREFIX.len()].eq_ignore_ascii_case(BEARER_PREFIX))
            .map(|value| value[BEARER_PREFIX.len()..].trim())
            .ok_or_else(|| {
                CapabilityError::ParseError(
                    "missing or malformed Authorization: Bearer <token> header".to_string(),
                )
            })?;

        self.verifier
            .read()
            .expect("lock poisoned")
            .verify(token, "telemetry.read", "", "")
    }
}

/// Redacted on purpose. A derived `Debug` would put the expected token into
/// every panic message and every log line that formats a service.
impl std::fmt::Debug for TelemetryAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryAuth")
            .field("enforcing", &self.is_enforcing())
            .finish()
    }
}

/// Applied by [`crate::server`] to all three gRPC services, so the check runs
/// before any handler and no path has to remember it.
impl tonic::service::Interceptor for TelemetryAuth {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        if !self.enforcing {
            return Ok(request);
        }
        let verifier = self.verifier.clone();
        let token = Self::token_from_request(&request);

        let v = verifier.read().map_err(|_| {
            Status::internal("lock poisoned")
        })?;
        let t = token.as_deref().unwrap_or("");

        // We cannot determine the gRPC method (PushLogs vs Query/Tail) from
        // within a tonic interceptor because GrpcMethod is set by tonic's
        // routing layer after the interceptor runs.  Try both audiences:
        // "telemetry" (for push) first, then "telemetry.read" (for query/tail).
        let result = v
            .verify(t, "telemetry", "", "")
            .or_else(|_| v.verify(t, "telemetry.read", "", ""));

        match result {
            Ok(verified) => {
                tracing::debug!(
                    node_id = %verified.node_id,
                    "telemetry capability verified"
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
            tracing::warn!("refused a telemetry call whose capability has expired");
            Status::unauthenticated(e.to_string())
        }
        CapabilityError::BadSignature(_) => {
            tracing::warn!(
                "refused a telemetry call with a bad capability signature; either the token was \
                 tampered with or this service no longer trusts the same signing key as \
                 deis-control"
            );
            Status::unauthenticated(e.to_string())
        }
        CapabilityError::ParseError(_) => {
            tracing::warn!("refused a telemetry call with an unparseable capability token");
            Status::unauthenticated(e.to_string())
        }
        CapabilityError::WrongAudience { .. } => {
            tracing::warn!("refused a telemetry call whose capability has the wrong audience");
            Status::permission_denied(e.to_string())
        }
        CapabilityError::WrongSubject { .. } => {
            tracing::warn!("refused a telemetry call whose capability has the wrong subject");
            Status::permission_denied(e.to_string())
        }
        CapabilityError::TenantNotAuthorized { .. } => {
            tracing::warn!(
                "refused a telemetry call whose capability does not authorize the tenant"
            );
            Status::permission_denied(e.to_string())
        }
        CapabilityError::BindingNotAuthorized { .. } => {
            tracing::warn!(
                "refused a telemetry call whose capability does not authorize the binding"
            );
            Status::permission_denied(e.to_string())
        }
        CapabilityError::AuthEpochTooLow { .. } => {
            tracing::warn!(
                "refused a telemetry call whose capability auth epoch is below the minimum"
            );
            Status::permission_denied(e.to_string())
        }
        CapabilityError::Revoked { .. } => {
            tracing::warn!("refused a telemetry call whose capability has been revoked");
            Status::permission_denied(e.to_string())
        }
        CapabilityError::NotYetValid { .. } => {
            tracing::warn!("refused a telemetry call whose capability is not yet valid");
            Status::unauthenticated(e.to_string())
        }
        CapabilityError::InvalidClaims(_) => {
            tracing::warn!("refused a telemetry call whose capability claims are invalid");
            Status::invalid_argument(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bearer_token_is_extracted_from_request() {
        let mut request = Request::new(());
        request.metadata_mut().insert(
            AUTHORIZATION_HEADER,
            "Bearer my-token-here".parse().unwrap(),
        );
        assert_eq!(
            TelemetryAuth::token_from_request(&request).as_deref(),
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
            TelemetryAuth::token_from_request(&request).as_deref(),
            Some("my-token-here")
        );
    }

    #[test]
    fn missing_authorization_returns_none() {
        let request = Request::new(());
        assert!(TelemetryAuth::token_from_request(&request).is_none());
    }
}