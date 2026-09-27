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

use deis_capability::{CapabilityError, CapabilityVerifier, KeyRing, VerifiedCapability};
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
    verifier: std::sync::Arc<std::sync::Mutex<CapabilityVerifier>>,
}

impl QueueAuth {
    /// Build a new interceptor over the control plane's key ring.
    pub fn new(keys: KeyRing) -> Self {
        Self {
            verifier: std::sync::Arc::new(std::sync::Mutex::new(
                CapabilityVerifier::with_key_ring(keys),
            )),
        }
    }

    /// Update the minimum accepted auth epoch.
    pub fn set_min_auth_epoch(&self, epoch: u64) {
        self.verifier
            .lock()
            .expect("lock poisoned")
            .set_min_auth_epoch(epoch);
    }

    /// Replace the revoked JTI list.
    pub fn set_revoked_jtis(&self, jtis: Vec<String>) {
        self.verifier
            .lock()
            .expect("lock poisoned")
            .set_revoked_jtis(jtis);
    }

    /// Record a successful revocation-list pull, resetting the T8
    /// degraded-mode clock.
    pub fn record_sync(&self) {
        self.verifier
            .lock()
            .expect("lock poisoned")
            .record_sync();
    }

    /// Total number of expired tokens accepted under T8 degraded-mode grace
    /// rules since this auth was created.
    pub fn grace_accepted_total(&self) -> u64 {
        self.verifier
            .lock()
            .expect("lock poisoned")
            .grace_accepted_total()
    }

    /// Seconds since the last successful revocation-list pull, or `None` if a
    /// pull has never completed.
    pub fn degraded_since_seconds(&self) -> Option<u64> {
        self.verifier
            .lock()
            .expect("lock poisoned")
            .degraded_since_seconds()
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

/// The JSON body returned by `GET /capability/revocation-list`.
#[derive(Debug, serde::Deserialize)]
struct RevocationListResponse {
    auth_epoch: u64,
    revoked_jtis: Vec<String>,
}

/// Start a background task that polls the control plane's revocation-list
/// endpoint every 30 seconds, feeding the current `auth_epoch` and revoked
/// JTIs into the verifier.  A successful poll calls [`QueueAuth::record_sync`]
/// which resets the T8 degraded-mode clock.
pub fn start_revocation_poller(auth: QueueAuth, revocation_list_url: String) {
    let url = format!("{revocation_list_url}/capability/revocation-list");
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.tick().await;
        loop {
            interval.tick().await;
            match client.get(&url).send().await {
                Ok(response) => match response.json::<RevocationListResponse>().await {
                    Ok(body) => {
                        auth.set_min_auth_epoch(body.auth_epoch);
                        auth.set_revoked_jtis(body.revoked_jtis);
                        auth.record_sync();
                        tracing::debug!(
                            auth_epoch = body.auth_epoch,
                            "Refreshed revocation list from control plane"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Failed to parse revocation-list response; will retry"
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        url = %url,
                        "Failed to reach control-plane revocation-list endpoint; will retry"
                    );
                }
            }
        }
    });
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
