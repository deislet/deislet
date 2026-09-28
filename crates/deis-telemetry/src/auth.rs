//! Proving that a caller may talk to this service, and deciding what it may
//! touch once it has.
//!
//! # What a caller presents
//!
//! A capability minted by deis-control, in `Authorization: Bearer <token>`,
//! with audience [`AUD_TELEMETRY`]. It is verified against the control
//! plane's published keys (its JWKS, kept current by a background refresher
//! the binary starts, see `deis_capability::fetch`), so nothing that is not
//! the control plane can mint one: there is no shared secret any node or CLI
//! could derive a signing key from. There is no anonymous mode, no loopback
//! bypass and no switch that turns the check off.
//!
//! # What a capability allows
//!
//! Its resources, one (tenant, binding) pair each, read two ways:
//!
//! * [`TELEMETRY_INGEST`] may push records, and only records of the tenants
//!   it names. A node is issued one for every application environment it
//!   runs, plus [`TELEMETRY_PLATFORM_TENANT`] for its own node-level records
//!   (the ones with no application). A record for any other tenant is refused
//!   and counted as rejected; the rest of the batch is stored.
//! * [`TELEMETRY_READ`] may query and follow, and sees only the tenants it
//!   names. A read call has to name an application it may read, and whatever
//!   comes back is filtered to the tenants named, so one environment's reader
//!   does not see another's lines. `@platform` with this binding sees
//!   everything - every tenant and the platform's own records - and is what
//!   deis-control mints for an operator holding `read:*/*`. It is also what
//!   the Prometheus scrape needs, because that page is every series at once.
//!
//! The two never stand in for each other: an ingest-only capability cannot
//! read and a read-only one cannot write.
//!
//! # Where the check runs
//!
//! The token is verified once per call, outside the handlers: on gRPC by a
//! tonic interceptor applied in [`crate::server`] to all three services at
//! once, which leaves the verified [`Grant`] in the request's extensions; on
//! the scrape port by one layer over the whole router in [`crate::http`].
//! An interceptor cannot see which method it guards, so the per-binding and
//! per-tenant decision is the handler's, through [`Grant::ingest`] and
//! [`Grant::read_scope`] - a handler that does not ask gets no grant to act
//! on, because [`grant_of`] refuses a request that carries none.

// Every refusal here is a tonic `Status`, returned by value because that is
// what the generated service traits and the interceptor contract take.
#![allow(clippy::result_large_err)]

use std::collections::BTreeSet;
use std::sync::Arc;

use deis_capability::{
    AUD_TELEMETRY, CapabilityError, CapabilityVerifier, KeyRing, TELEMETRY_INGEST,
    SharedVerifier, TELEMETRY_PLATFORM_TENANT, TELEMETRY_READ, VerifiedGrant,
};
use deis_wire::conventions::tenant_id;
use tonic::{Request, Status};

/// The header carrying the capability token.
const AUTHORIZATION_HEADER: &str = "authorization";

/// The scheme prefix on an `Authorization` value.
const BEARER_PREFIX: &str = "bearer ";

/// Whose a stored or arriving record is.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Owner {
    /// A node's own record, about no application.
    Platform,
    /// One application environment, as `<app>.<environment>`.
    Tenant(String),
}

/// The tenants one binding of a capability names.
#[derive(Debug, Clone, Default)]
struct Named {
    platform: bool,
    tenants: BTreeSet<String>,
}

impl Named {
    fn of(grant: &VerifiedGrant, binding: &str) -> Self {
        let mut named = Self::default();
        for tenant in grant.tenants_with(binding) {
            if tenant == TELEMETRY_PLATFORM_TENANT {
                named.platform = true;
            } else {
                named.tenants.insert(tenant.to_string());
            }
        }
        named
    }

    fn is_empty(&self) -> bool {
        !self.platform && self.tenants.is_empty()
    }
}

/// What one verified caller may do. Left in the request extensions by the
/// interceptor; handlers take it with [`grant_of`].
#[derive(Debug, Clone)]
pub struct Grant {
    principal: String,
    ingest: Named,
    read: Named,
}

impl Grant {
    fn from_verified(verified: VerifiedGrant) -> Self {
        Self {
            ingest: Named::of(&verified, TELEMETRY_INGEST),
            read: Named::of(&verified, TELEMETRY_READ),
            principal: verified.sub,
        }
    }

    /// The principal the capability was issued to, for log lines.
    pub fn principal(&self) -> &str {
        &self.principal
    }

    /// What this caller may push, or a refusal when it may push nothing.
    pub fn ingest(&self) -> Result<Ingest<'_>, Status> {
        if self.ingest.is_empty() {
            tracing::warn!(
                principal = %self.principal,
                "refused a telemetry push whose capability grants no ingest binding"
            );
            return Err(Status::permission_denied(
                "this capability does not allow pushing telemetry",
            ));
        }
        Ok(Ingest(&self.ingest))
    }

    /// What this caller may read, or a refusal when it may read nothing.
    pub fn read_scope(&self) -> Result<ReadScope, Status> {
        if self.read.platform {
            return Ok(ReadScope::All);
        }
        if self.read.tenants.is_empty() {
            tracing::warn!(
                principal = %self.principal,
                "refused a telemetry read whose capability grants no read binding"
            );
            return Err(Status::permission_denied(
                "this capability does not allow reading telemetry",
            ));
        }
        Ok(ReadScope::Tenants(Arc::new(self.read.tenants.clone())))
    }
}

/// The ingest half of a [`Grant`].
#[derive(Debug, Clone, Copy)]
pub struct Ingest<'a>(&'a Named);

impl Ingest<'_> {
    /// Whether a record owned by `owner` may be stored.
    pub fn admits(&self, owner: &Owner) -> bool {
        match owner {
            Owner::Platform => self.0.platform,
            Owner::Tenant(tenant) => self.0.tenants.contains(tenant),
        }
    }
}

/// The read half of a [`Grant`]: which records a query or a stream may hand
/// back.
#[derive(Debug, Clone)]
pub enum ReadScope {
    /// Everything: every tenant and the platform's own records.
    All,
    /// These `<app>.<environment>` tenants and nothing else.
    Tenants(Arc<BTreeSet<String>>),
}

impl ReadScope {
    /// Whether a record owned by `owner` may be handed back.
    pub fn admits(&self, owner: &Owner) -> bool {
        match (self, owner) {
            (Self::All, _) => true,
            (Self::Tenants(_), Owner::Platform) => false,
            (Self::Tenants(tenants), Owner::Tenant(tenant)) => tenants.contains(tenant),
        }
    }

    /// Whether a record of `app_id` in `environment` may be handed back.
    pub fn admits_parts(&self, app_id: &str, environment: &str) -> bool {
        match self {
            Self::All => true,
            Self::Tenants(tenants) => tenants.contains(&tenant_id(app_id, environment)),
        }
    }

    /// Refuse a read that does not name an application this scope reaches.
    ///
    /// A narrow reader has to say which application it is asking about. The
    /// records coming back are filtered anyway; refusing up front is what
    /// makes asking about somebody else's application an error rather than
    /// an empty page that looks like silence.
    pub fn check_app(&self, app_id: Option<&str>) -> Result<(), Status> {
        let Self::Tenants(tenants) = self else {
            return Ok(());
        };
        let Some(app_id) = app_id else {
            return Err(Status::permission_denied(
                "this capability reads named applications only; set app_id",
            ));
        };
        let reachable = tenants.iter().any(|tenant| {
            deis_wire::conventions::split_tenant(tenant).is_some_and(|(app, _)| app == app_id)
        });
        if reachable {
            Ok(())
        } else {
            tracing::warn!(
                app_id,
                "refused a telemetry read for an application its capability does not name"
            );
            Err(Status::permission_denied(format!(
                "this capability does not allow reading telemetry of {app_id}"
            )))
        }
    }
}

/// The grant the interceptor left on `request`, or a refusal when there is
/// none - which only happens to a service registered without the
/// interceptor, and that must fail closed.
pub fn grant_of<T>(request: &Request<T>) -> Result<Grant, Status> {
    request
        .extensions()
        .get::<Grant>()
        .cloned()
        .ok_or_else(|| Status::unauthenticated("no verified telemetry capability on this call"))
}

/// What the process checks on every call.
///
/// Cheap to clone: the verifier is behind an `Arc`, so one instance guards
/// the three gRPC services and the scrape endpoint.
#[derive(Clone)]
pub struct TelemetryAuth {
    verifier: SharedVerifier,
}

impl TelemetryAuth {
    /// Build a new interceptor over the control plane's key ring.
    pub fn new(keys: KeyRing) -> Self {
        Self {
            verifier: CapabilityVerifier::with_key_ring(keys).shared(),
        }
    }

    /// The verifier this guard checks against, shared with the revocation
    /// poller and the `/metrics` export.
    pub fn verifier(&self) -> &SharedVerifier {
        &self.verifier
    }

    /// Verify one presented token.
    fn verify(&self, token: Option<&str>) -> Result<Grant, CapabilityError> {
        let token = token.ok_or_else(|| {
            CapabilityError::ParseError(
                "missing or malformed Authorization: Bearer <token> header".to_string(),
            )
        })?;
        let verified = self
            .verifier
            .lock()
            .expect("lock poisoned")
            .verify_grant_with_grace(token, AUD_TELEMETRY)?;
        Ok(Grant::from_verified(verified))
    }

    /// Extract the bearer token from an `Authorization` value.
    fn bearer(value: Option<&str>) -> Option<&str> {
        let value = value?;
        if value.len() > BEARER_PREFIX.len()
            && value[..BEARER_PREFIX.len()].eq_ignore_ascii_case(BEARER_PREFIX)
        {
            Some(value[BEARER_PREFIX.len()..].trim())
        } else {
            None
        }
    }

    fn token_from_request<T>(request: &Request<T>) -> Option<&str> {
        Self::bearer(
            request
                .metadata()
                .get(AUTHORIZATION_HEADER)
                .and_then(|value| value.to_str().ok()),
        )
    }

    /// Verify the token on an HTTP request - the scrape port, where a
    /// Prometheus server configured with `bearer_token` sends the same header
    /// a platform component does.
    pub fn verify_headers(
        &self,
        headers: &axum::http::HeaderMap,
    ) -> Result<Grant, CapabilityError> {
        self.verify(Self::bearer(
            headers
                .get(AUTHORIZATION_HEADER)
                .and_then(|value| value.to_str().ok()),
        ))
    }
}

/// Redacted on purpose: nothing about the verifier belongs in a log line.
impl std::fmt::Debug for TelemetryAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryAuth").finish_non_exhaustive()
    }
}

/// Applied by [`crate::server`] to all three gRPC services, so the check runs
/// before any handler and no path has to remember it.
impl tonic::service::Interceptor for TelemetryAuth {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        match self.verify(Self::token_from_request(&request)) {
            Ok(grant) => {
                tracing::debug!(
                    principal = %grant.principal(),
                    "telemetry capability verified"
                );
                request.extensions_mut().insert(grant);
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
                 tampered with or it was not signed by deis-control"
            );
            Status::unauthenticated(e.to_string())
        }
        CapabilityError::ParseError(_) => {
            tracing::warn!(
                "refused a telemetry call with a missing or unparseable capability token"
            );
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
            Status::unauthenticated(e.to_string())
        }
        CapabilityError::UnknownKey { .. } => {
            tracing::warn!(
                "refused a telemetry call whose capability names a signing key this service does \
                 not hold; a refresh of the control plane's keys has been requested, so a token \
                 from a newly rotated key is accepted once it lands"
            );
            Status::unauthenticated(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deis_capability::{CapabilityClaims, CapabilityResource, encode};
    use ed25519_dalek::SigningKey;
    use std::time::Duration;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn token(resources: &[(&str, &str)]) -> String {
        let claims = CapabilityClaims::new(
            AUD_TELEMETRY,
            "spiffe://deis.test/deislet/control/main",
            "spiffe://deis.test/deislet/node/n1",
            1,
            Duration::from_secs(900),
            resources
                .iter()
                .map(|(tenant, binding)| CapabilityResource {
                    tenant: tenant.to_string(),
                    binding: binding.to_string(),
                    generation: None,
                })
                .collect(),
        );
        encode(&claims, &key())
    }

    fn auth() -> TelemetryAuth {
        TelemetryAuth::new(KeyRing::from_keys([key().verifying_key()]))
    }

    fn call(token: Option<&str>) -> Result<Grant, Status> {
        let mut request = Request::new(());
        if let Some(token) = token {
            request.metadata_mut().insert(
                AUTHORIZATION_HEADER,
                format!("Bearer {token}").parse().unwrap(),
            );
        }
        let passed = tonic::service::Interceptor::call(&mut auth(), request)?;
        grant_of(&passed)
    }

    fn tenant(name: &str) -> Owner {
        Owner::Tenant(name.to_string())
    }

    #[test]
    fn an_unauthenticated_call_is_refused() {
        let status = call(None).unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
        let status = call(Some("v1.not.a-token")).unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn an_ingest_capability_writes_its_tenants_and_reads_nothing() {
        let grant = call(Some(&token(&[
            ("shop.production", TELEMETRY_INGEST),
            (TELEMETRY_PLATFORM_TENANT, TELEMETRY_INGEST),
        ])))
        .unwrap();
        let ingest = grant.ingest().unwrap();
        assert!(ingest.admits(&tenant("shop.production")));
        assert!(ingest.admits(&Owner::Platform));
        assert!(!ingest.admits(&tenant("bank.production")));
        assert!(!ingest.admits(&tenant("shop.preview")));
        assert_eq!(
            grant.read_scope().unwrap_err().code(),
            tonic::Code::PermissionDenied
        );
    }

    #[test]
    fn a_read_capability_reads_its_tenants_and_writes_nothing() {
        let grant = call(Some(&token(&[("shop.production", TELEMETRY_READ)]))).unwrap();
        assert_eq!(
            grant.ingest().unwrap_err().code(),
            tonic::Code::PermissionDenied
        );

        let scope = grant.read_scope().unwrap();
        assert!(scope.admits(&tenant("shop.production")));
        assert!(!scope.admits(&tenant("shop.preview")));
        assert!(!scope.admits(&Owner::Platform));
        assert!(scope.check_app(Some("shop")).is_ok());
        assert_eq!(
            scope.check_app(Some("bank")).unwrap_err().code(),
            tonic::Code::PermissionDenied
        );
        assert_eq!(
            scope.check_app(None).unwrap_err().code(),
            tonic::Code::PermissionDenied
        );
    }

    #[test]
    fn the_platform_read_sees_everything() {
        let grant = call(Some(&token(&[(TELEMETRY_PLATFORM_TENANT, TELEMETRY_READ)]))).unwrap();
        let scope = grant.read_scope().unwrap();
        assert!(matches!(scope, ReadScope::All));
        assert!(scope.admits(&Owner::Platform));
        assert!(scope.admits(&tenant("bank.production")));
        assert!(scope.check_app(None).is_ok());
    }

    #[test]
    fn a_capability_for_another_audience_is_refused() {
        let claims = CapabilityClaims::new(
            "store",
            "spiffe://deis.test/deislet/control/main",
            "spiffe://deis.test/deislet/node/n1",
            1,
            Duration::from_secs(900),
            vec![],
        );
        let status = call(Some(&encode(&claims, &key()))).unwrap_err();
        assert_eq!(status.code(), tonic::Code::PermissionDenied);
    }

    #[test]
    fn a_request_without_a_grant_fails_closed() {
        let status = grant_of(&Request::new(())).unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn bearer_prefix_is_case_insensitive_and_required() {
        assert_eq!(TelemetryAuth::bearer(Some("bearer abc")), Some("abc"));
        assert_eq!(TelemetryAuth::bearer(Some("Bearer abc")), Some("abc"));
        assert_eq!(TelemetryAuth::bearer(Some("Basic dGVzdA==")), None);
        assert_eq!(TelemetryAuth::bearer(None), None);
    }
}
