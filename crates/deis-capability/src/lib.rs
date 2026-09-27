//! Ed25519-signed JWT capabilities for Deislet service-to-service authorization.
//!
//! # Overview
//!
//! This crate implements the capability system described in ADR-025:
//! short-lived (≤ 15 min), Ed25519-signed tokens that authorize a node to
//! access a specific storage, queue, or telemetry service for a specific
//! tenant. The control plane mints capabilities; store, queue, and telemetry
//! verify them.
//!
//! # Wire format
//!
//! `v1.<base64url(JCS-canonicalized claims)>.<base64url(Ed25519 signature)>`
//!
//! - `v1` is the format version prefix.
//! - The claims are canonicalized with [JCS](https://datatracker.ietf.org/doc/html/rfc8785)
//!   (JSON Canonicalization Scheme) before signing, so the exact byte string
//!   the signer produced can be compared byte-for-byte.
//! - The signature is a raw Ed25519 signature (64 bytes) over the
//!   canonicalized claims bytes.
//! - Both payload and signature use base64url without padding.
//! - The claims name the signing key in `kid` (its RFC 7638 thumbprint, see
//!   [`keys`]); a verifier holding several keys picks the one named, which
//!   is what lets the control plane rotate its key without a flag day.
//!
//! # Token lifetime
//!
//! Capabilities are short-lived (≤ 15 minutes) and carry an `auth_epoch` for
//! mass revocation. A verifier can reject any token whose `auth_epoch` is
//! below a configured minimum, and can also check a list of revoked `jti`
//! values.

use base64::Engine;
use deis_wire::CanonicalTenantId;
use ed25519_dalek::Signer;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, atomic::AtomicU64, atomic::Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub mod keys;
#[cfg(feature = "jwks-fetch")]
pub mod control;
#[cfg(feature = "jwks-fetch")]
pub mod exposition;
#[cfg(feature = "jwks-fetch")]
pub mod fetch;
pub mod revocation;

#[cfg(feature = "jwks-fetch")]
pub use control::{ControlClient, ControlClientConfig, ControlClientError, ServiceIdentityConfig};

/// Where the control plane serves its capability verifying keys as a JWKS
/// document, on its machine channel.
pub const JWKS_PATH: &str = "/.well-known/deis/capability-jwks.json";

pub use keys::{Jwk, Jwks, JwksError, KeyRing, key_id};
pub use revocation::{RevocationList, RevocationListError, SignedRevocationList};

/// One verifier shared by every guard of a process, so the revocation poller
/// feeds all of them at once. A `std` mutex because the guards run inside
/// synchronous tonic interceptors.
pub type SharedVerifier = Arc<std::sync::Mutex<CapabilityVerifier>>;

// ---- audiences and bindings -----------------------------------------------

/// Audience of a capability for deis-store.
pub const AUD_STORE: &str = "store";
/// Audience of a capability for deis-queue.
pub const AUD_QUEUE: &str = "queue";

/// Audience of a capability for deis-telemetry.
pub const AUD_TELEMETRY: &str = "telemetry";

/// The bindings deis-store serves, one per service; each verifies its own.
pub const STORE_BINDINGS: &[&str] = &["kv", "sql", "blob"];
/// The one binding deis-queue serves.
pub const QUEUE_BINDINGS: &[&str] = &["queue"];

/// The deis-telemetry binding that may push logs, traces and metric samples
/// for its tenant. It reads nothing.
pub const TELEMETRY_INGEST: &str = "ingest";
/// The deis-telemetry binding that may query and follow its tenant's logs,
/// traces and metric samples. It writes nothing.
pub const TELEMETRY_READ: &str = "read";
/// What a node's tenant resource for deis-telemetry names: a node ships, it
/// never reads.
pub const TELEMETRY_BINDINGS: &[&str] = &[TELEMETRY_INGEST];

/// The tenant a deis-telemetry resource names when it is about the platform
/// rather than about one application environment.
///
/// With [`TELEMETRY_INGEST`] it lets a node push its own, node-level records
/// (the ones carrying no application); with [`TELEMETRY_READ`] it lets an
/// operator read everything, every tenant's records and the platform's own.
/// Never a real tenant: a tenant is always `<app>.<environment>`, and this
/// has no `.`.
pub const TELEMETRY_PLATFORM_TENANT: &str = "@platform";

/// The longest a capability may be valid for, `exp - iat`, in seconds.
///
/// Every issuer mints 15-minute tokens (ADR-025); a verifier refuses anything
/// that claims to last longer, so a token minted outside the control plane's
/// rules - a year-long one, say - is refused however it was signed.
pub const MAX_CAPABILITY_LIFETIME_SECS: u64 = 15 * 60;

/// The bindings a capability for `aud` has to name so every service behind
/// that audience accepts it, or `None` for an audience with no per-tenant
/// bindings.
pub fn bindings_for_audience(aud: &str) -> Option<&'static [&'static str]> {
    match aud {
        AUD_STORE => Some(STORE_BINDINGS),
        AUD_QUEUE => Some(QUEUE_BINDINGS),
        AUD_TELEMETRY => Some(TELEMETRY_BINDINGS),
        _ => None,
    }
}

/// One resource per binding of `aud` for `tenant`: what a capability that
/// may use every service of that audience for the tenant carries. Empty for
/// an audience with no per-tenant bindings.
///
/// Every issuer builds its resource list here, so a token minted anywhere
/// in the control plane names exactly the bindings the verifiers check.
pub fn resources_for_audience(tenant: &str, aud: &str) -> Vec<CapabilityResource> {
    bindings_for_audience(aud)
        .unwrap_or_default()
        .iter()
        .map(|binding| CapabilityResource {
            tenant: tenant.to_string(),
            binding: (*binding).to_string(),
            generation: None,
        })
        .collect()
}

// ---- wire format constants ------------------------------------------------

/// The version prefix on every capability token.
const VERSION_PREFIX: &str = "v1.";

/// The separator between payload and signature in the token string.
const TOKEN_SEPARATOR: char = '.';

/// How many seconds ahead we allow for clock skew when checking expiry.
const CLOCK_SKEW_SECS: u64 = 30;

// ---- T8 degraded-mode constants -------------------------------------------

/// Maximum grace period for expired node capabilities when the control plane
/// is unreachable: 24 hours (86 400 seconds).  Per decision D-8, this is the
/// hard upper bound — `grace_max_secs` is capped at this value.
pub const MAX_GRACE_PERIOD_SECS: u64 = 86_400;

/// Default grace period: 24 hours.
pub const DEFAULT_GRACE_PERIOD_SECS: u64 = 86_400;

/// Default clock-skew tolerance for degraded-mode expiry checks: 30 seconds.
pub const DEFAULT_DEGRADED_SKEW_SECS: u64 = 30;

/// Default revocation-list pull interval: 30 seconds.
pub const DEFAULT_PULL_INTERVAL_SECS: u64 = 30;

/// Degraded mode activates when `last_sync` is older than this factor ×
/// `pull_interval_secs`.
const DEGRADED_PULL_INTERVAL_FACTOR: u64 = 2;

// ---- error types ----------------------------------------------------------

/// Ways a capability may be refused.
#[derive(Debug, thiserror::Error)]
pub enum CapabilityError {
    /// The bearer token could not be parsed at all: wrong prefix, missing
    /// separator, base64 decode failure.
    #[error("invalid capability token: {0}")]
    ParseError(String),

    /// The payload is valid JSON but the signature does not verify. Either
    /// the token was tampered with or the verifying key is wrong.
    #[error("bad signature: {0}")]
    BadSignature(String),

    /// The token has expired. `exp` is the expiration timestamp (Unix
    /// seconds).
    #[error("capability expired at {exp} (now: {now})")]
    Expired { exp: u64, now: u64 },

    /// The token's `aud` field does not match the expected audience.
    #[error("wrong audience: expected {expected}, got {got}")]
    WrongAudience { expected: String, got: String },

    /// The token's subject does not match the expected node identity.
    #[error("wrong subject: expected {expected}, got {got}")]
    WrongSubject { expected: String, got: String },

    /// The token does not grant access to the requested tenant.
    #[error("tenant not authorized: {tenant}")]
    TenantNotAuthorized { tenant: String },

    /// The token does not grant access to the requested binding type.
    #[error("binding {binding} not authorized for tenant {tenant}")]
    BindingNotAuthorized { tenant: String, binding: String },

    /// The token's `auth_epoch` is below the verifier's minimum.
    #[error("auth epoch {got} is below the minimum {minimum}")]
    AuthEpochTooLow { got: u64, minimum: u64 },

    /// The token's `jti` has been revoked.
    #[error("capability {jti} has been revoked")]
    Revoked { jti: String },

    /// The payload did not deserialize to a valid capability.
    #[error("invalid capability claims: {0}")]
    InvalidClaims(String),

    /// The token is not yet valid (iat in the future beyond clock skew).
    #[error("capability not yet valid (iat: {iat}, now: {now})")]
    NotYetValid { iat: u64, now: u64 },

    /// The token names a signing key this verifier does not hold: signed by
    /// a key published after the verifier last refreshed, by a retired key,
    /// or by nothing the control plane ever published.
    #[error("capability signed by unknown key {kid:?}")]
    UnknownKey { kid: String },
}

// ---- claims types ---------------------------------------------------------

/// A single resource access grant within a capability.
///
/// Each resource names one tenant+binding pair the holder may access.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityResource {
    /// The canonical tenant id, e.g. `ct1_<app_uuid>/<env_uuid>`.
    pub tenant: String,

    /// The binding type: `kv`, `sql`, `blob`, or `queue`.
    pub binding: String,

    /// Optional deployment generation, for binding to a specific deployment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
}

impl CapabilityResource {
    /// Create a resource grant for a canonical tenant and binding.
    pub fn new(tenant: &CanonicalTenantId, binding: &str) -> Self {
        Self {
            tenant: tenant.to_string(),
            binding: binding.to_string(),
            generation: None,
        }
    }

    /// Attach a deployment generation to the resource grant.
    pub fn with_generation(mut self, generation: impl Into<String>) -> Self {
        self.generation = Some(generation.into());
        self
    }
}

/// The claims carried in a capability token.
///
/// Unrecognized fields are deliberately *not* denied at deserialization:
/// ADR-025 requires that future extensions be silently ignored so a newer
/// control plane can add fields without breaking older verifiers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityClaims {
    /// Audience: `store`, `queue`, or `telemetry`.
    pub aud: String,

    /// The signing key's RFC 7638 thumbprint. Set by [`encode`] from the key
    /// it signs with, so a caller never has to fill it in.
    #[serde(default)]
    pub kid: String,

    /// Issuer identity (SPIFFE ID of the control plane).
    pub iss: String,

    /// Unique capability identifier (UUIDv7).
    pub jti: String,

    /// Expiration timestamp (Unix seconds).
    pub exp: u64,

    /// Issued-at timestamp (Unix seconds).
    pub iat: u64,

    /// Monotonic epoch for mass revocation. A verifier can reject any token
    /// whose `auth_epoch` is below a configured floor.
    pub auth_epoch: u64,

    /// Subject identity (SPIFFE ID of the node).
    pub sub: String,

    /// The set of (tenant, binding) pairs this capability authorizes.
    pub resources: Vec<CapabilityResource>,
}

impl CapabilityClaims {
    /// Build a new set of claims with the current timestamp.
    pub fn new(
        aud: impl Into<String>,
        iss: impl Into<String>,
        sub: impl Into<String>,
        auth_epoch: u64,
        lifetime: Duration,
        resources: Vec<CapabilityResource>,
    ) -> Self {
        let now = now_secs();
        Self {
            aud: aud.into(),
            kid: String::new(),
            iss: iss.into(),
            jti: uuid::Uuid::now_v7().to_string(),
            exp: now.saturating_add(lifetime.as_secs()),
            iat: now,
            auth_epoch,
            sub: sub.into(),
            resources,
        }
    }

    /// Check whether this capability is still valid at the current time,
    /// allowing for `CLOCK_SKEW_SECS` of clock drift.
    pub fn is_expired(&self) -> bool {
        let now = now_secs();
        now.saturating_sub(CLOCK_SKEW_SECS) > self.exp
    }

    /// Check whether this capability's iat is too far in the future.
    pub fn is_not_yet_valid(&self) -> bool {
        let now = now_secs();
        self.iat.saturating_sub(CLOCK_SKEW_SECS) > now
    }

    /// Verify that this capability authorizes the given `tenant`.
    pub fn authorizes_tenant(&self, tenant: &str) -> bool {
        self.resources.iter().any(|r| r.tenant == tenant)
    }

    /// Verify that this capability authorizes the given `tenant` and `binding`.
    pub fn authorizes_binding(&self, tenant: &str, binding: &str) -> bool {
        self.resources
            .iter()
            .any(|r| r.tenant == tenant && r.binding == binding)
    }

    /// Returns all tenants authorized by this capability.
    pub fn tenants(&self) -> impl Iterator<Item = &str> {
        self.resources.iter().map(|r| r.tenant.as_str())
    }
}

// ---- signing and encoding -------------------------------------------------

/// Encode a capability token from claims and a signing key.
///
/// Returns the wire-format string: `v1.<base64url(payload)>.<base64url(sig)>`.
/// The `kid` claim is overwritten with the thumbprint of `signing_key`.
pub fn encode(claims: &CapabilityClaims, signing_key: &ed25519_dalek::SigningKey) -> String {
    let claims = CapabilityClaims {
        kid: key_id(&signing_key.verifying_key()),
        ..claims.clone()
    };
    let payload = serialize_claims(&claims);
    let signature = signing_key.sign(&payload);
    let payload_b64 = base64url(&payload);
    let sig_b64 = base64url(signature.to_bytes().as_ref());
    format!("{VERSION_PREFIX}{payload_b64}{TOKEN_SEPARATOR}{sig_b64}")
}

/// Decode and verify a capability token signed by `verifying_key`.
///
/// Returns the deserialized claims on success, or a [`CapabilityError`]
/// describing why verification failed. A token whose `kid` names another key
/// is refused with [`CapabilityError::UnknownKey`].
pub fn decode(
    token: &str,
    verifying_key: &ed25519_dalek::VerifyingKey,
) -> Result<CapabilityClaims, CapabilityError> {
    decode_with(token, &KeyRing::from_keys([*verifying_key]))
}

/// Decode and verify a capability token against whichever key in `keys` its
/// `kid` names.
pub fn decode_with(token: &str, keys: &KeyRing) -> Result<CapabilityClaims, CapabilityError> {
    let (payload_bytes, sig_bytes) = parse_token(token)?;

    // The kid is read before the signature is checked: it only selects the
    // key, and the signature over these same bytes then proves it.
    #[derive(Deserialize)]
    struct KeyHint {
        #[serde(default)]
        kid: String,
    }
    let hint: KeyHint = serde_json::from_slice(&payload_bytes).map_err(|e| {
        CapabilityError::InvalidClaims(format!("claims deserialization failed: {e}"))
    })?;
    let verifying_key = keys.lookup(&hint.kid)?;

    let signature = ed25519_dalek::Signature::from_slice(&sig_bytes)
        .map_err(|e| CapabilityError::ParseError(format!("invalid signature bytes: {e}")))?;

    verifying_key
        .verify_strict(&payload_bytes, &signature)
        .map_err(|e| CapabilityError::BadSignature(e.to_string()))?;

    let claims: CapabilityClaims = serde_json::from_slice(&payload_bytes).map_err(|e| {
        CapabilityError::InvalidClaims(format!("claims deserialization failed: {e}"))
    })?;

    Ok(claims)
}

/// Parse a token string into its payload and signature byte components
/// without verifying the signature. Useful for inspecting the claims before
/// a full verification.
pub fn parse_token(token: &str) -> Result<(Vec<u8>, Vec<u8>), CapabilityError> {
    let body = token.strip_prefix(VERSION_PREFIX).ok_or_else(|| {
        CapabilityError::ParseError(format!("token must start with {VERSION_PREFIX:?}"))
    })?;

    let (payload_b64, sig_b64) = body.split_once(TOKEN_SEPARATOR).ok_or_else(|| {
        CapabilityError::ParseError(
            "token must contain exactly one '.' separator between payload and signature"
                .to_string(),
        )
    })?;

    if payload_b64.is_empty() {
        return Err(CapabilityError::ParseError(
            "token payload is empty".to_string(),
        ));
    }
    if sig_b64.is_empty() {
        return Err(CapabilityError::ParseError(
            "token signature is empty".to_string(),
        ));
    }

    let payload = base64url_decode(payload_b64)
        .map_err(|e| CapabilityError::ParseError(format!("payload base64 decode failed: {e}")))?;
    let signature = base64url_decode(sig_b64)
        .map_err(|e| CapabilityError::ParseError(format!("signature base64 decode failed: {e}")))?;

    Ok((payload, signature))
}

// ---- verification convenience ---------------------------------------------

/// The result of verifying a capability against a specific request.
#[derive(Debug, Clone)]
pub struct VerifiedCapability {
    /// The node's SPIFFE ID that presented this capability.
    pub node_id: String,
    /// The canonical tenant id authorized for this call.
    pub canonical_tenant_id: String,
    /// The binding type authorized.
    pub binding: String,
    /// `true` when the token was accepted under T8 degraded-mode grace rules
    /// (it was expired but within the grace window).
    pub grace_accepted: bool,
    /// Token expiry as Unix seconds, for observability and alerting.
    pub exp: u64,
}

/// Fully verify a capability token: signature, expiry, audience, and
/// that at least one resource matches the expected tenant and binding.
pub fn verify_for_request(
    token: &str,
    verifying_key: &ed25519_dalek::VerifyingKey,
    expected_aud: &str,
    tenant: &str,
    binding: &str,
    min_auth_epoch: Option<u64>,
    revoked_jtis: Option<&[String]>,
) -> Result<VerifiedCapability, CapabilityError> {
    let claims = decode(token, verifying_key)?;
    check_claims(
        claims,
        false,
        expected_aud,
        tenant,
        binding,
        min_auth_epoch.unwrap_or(0),
        revoked_jtis.unwrap_or_default(),
    )
}

/// Every check after the signature: the ones in [`check_common`], then the
/// tenant + binding.
fn check_claims(
    claims: CapabilityClaims,
    grace_accepted: bool,
    expected_aud: &str,
    tenant: &str,
    binding: &str,
    min_auth_epoch: u64,
    revoked_jtis: &[String],
) -> Result<VerifiedCapability, CapabilityError> {
    check_common(
        &claims,
        grace_accepted,
        expected_aud,
        min_auth_epoch,
        revoked_jtis,
    )?;

    if !claims.authorizes_binding(tenant, binding) {
        if !claims.authorizes_tenant(tenant) {
            return Err(CapabilityError::TenantNotAuthorized {
                tenant: tenant.to_string(),
            });
        }
        return Err(CapabilityError::BindingNotAuthorized {
            tenant: tenant.to_string(),
            binding: binding.to_string(),
        });
    }

    Ok(VerifiedCapability {
        node_id: claims.sub,
        canonical_tenant_id: tenant.to_string(),
        binding: binding.to_string(),
        grace_accepted,
        exp: claims.exp,
    })
}

/// Every check after the signature that does not look at the resources:
/// lifetime, expiry (skipped when `grace_accepted`), issue time, audience,
/// auth epoch and revocation.
fn check_common(
    claims: &CapabilityClaims,
    grace_accepted: bool,
    expected_aud: &str,
    min_auth_epoch: u64,
    revoked_jtis: &[String],
) -> Result<(), CapabilityError> {
    let now = now_secs();

    let lifetime = claims.exp.saturating_sub(claims.iat);
    if lifetime > MAX_CAPABILITY_LIFETIME_SECS {
        return Err(CapabilityError::InvalidClaims(format!(
            "lifetime {lifetime}s exceeds the maximum of {MAX_CAPABILITY_LIFETIME_SECS}s"
        )));
    }

    if claims.is_expired() && !grace_accepted {
        return Err(CapabilityError::Expired {
            exp: claims.exp,
            now,
        });
    }

    if !grace_accepted && claims.is_not_yet_valid() {
        return Err(CapabilityError::NotYetValid {
            iat: claims.iat,
            now,
        });
    }

    if claims.aud != expected_aud {
        return Err(CapabilityError::WrongAudience {
            expected: expected_aud.to_string(),
            got: claims.aud.clone(),
        });
    }

    if claims.auth_epoch < min_auth_epoch {
        return Err(CapabilityError::AuthEpochTooLow {
            got: claims.auth_epoch,
            minimum: min_auth_epoch,
        });
    }

    if revoked_jtis.iter().any(|jti| *jti == claims.jti) {
        return Err(CapabilityError::Revoked {
            jti: claims.jti.clone(),
        });
    }

    Ok(())
}

/// A capability that passed every check but the resource one, handed back
/// whole so a service whose calls are not about exactly one tenant can decide
/// per record what the resources allow.
#[derive(Debug, Clone)]
pub struct VerifiedGrant {
    /// The principal the capability was issued to.
    pub sub: String,
    /// Every (tenant, binding) pair it authorizes.
    pub resources: Vec<CapabilityResource>,
    /// `true` when accepted under T8 degraded-mode grace rules.
    pub grace_accepted: bool,
    /// Token expiry as Unix seconds.
    pub exp: u64,
}

impl VerifiedGrant {
    /// The tenants this grant names with `binding`.
    pub fn tenants_with<'a>(&'a self, binding: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.resources
            .iter()
            .filter(move |r| r.binding == binding)
            .map(|r| r.tenant.as_str())
    }
}

// ---- verifier -------------------------------------------------------------

/// A reusable verifier for capability tokens.
///
/// Bundles the key ring with the two revocation-state inputs
/// (`min_auth_epoch` and `revoked_jtis`) and the T8 degraded-mode state
/// (`last_sync`, `grace_max_secs`) so a service can construct one
/// verifier at startup and reuse it for every request.
#[derive(Debug, Clone)]
pub struct CapabilityVerifier {
    /// The control plane's published keys, picked by the token's `kid`.
    /// Shared with whatever refreshes it, so a new key is picked up without
    /// touching the verifier.
    keys: KeyRing,
    /// Minimum auth_epoch accepted (loaded from revocation feed).
    min_auth_epoch: u64,
    /// List of revoked JTIs (loaded from revocation feed).
    revoked_jtis: Vec<String>,
    /// Last successful revocation-list pull, as a monotonic instant.
    /// `None` means never synced → degraded from the start.
    last_sync: Option<Instant>,
    /// Maximum grace period in seconds, capped at [`MAX_GRACE_PERIOD_SECS`].
    grace_max_secs: u64,
    /// Clock-skew tolerance in seconds for grace-condition
    /// `exp ≥ last_sync − skew`.
    skew_secs: u64,
    /// Revocation-list pull interval in seconds.
    pull_interval_secs: u64,
    /// Total number of expired tokens accepted under T8 degraded-mode grace rules.
    grace_accepted_total: Arc<AtomicU64>,
    /// The trust domain a node subject must be in to get T8 grace, when the
    /// verifier knows its own; `None` accepts a node of any trust domain.
    trust_domain: Option<String>,
    /// `issued_at` of the last revocation list accepted, so an older one -
    /// a replay - is refused.
    last_list_issued_at: u64,
    /// When this verifier was made: a verifier that has never synced has
    /// been degraded since then.
    created_at: Instant,
}

impl CapabilityVerifier {
    /// A verifier trusting exactly one key, with `min_auth_epoch = 0`, an
    /// empty revoked list, and T8 defaults (grace period = 24 h,
    /// skew = 30 s, pull interval = 30 s).
    pub fn new(verifying_key: ed25519_dalek::VerifyingKey) -> Self {
        Self::with_key_ring(KeyRing::from_keys([verifying_key]))
    }

    /// A verifier trusting whatever `keys` holds at the time of each call,
    /// with the same defaults as [`Self::new`].
    pub fn with_key_ring(keys: KeyRing) -> Self {
        Self {
            keys,
            min_auth_epoch: 0,
            revoked_jtis: Vec::new(),
            last_sync: None,
            grace_max_secs: DEFAULT_GRACE_PERIOD_SECS,
            skew_secs: DEFAULT_DEGRADED_SKEW_SECS,
            pull_interval_secs: DEFAULT_PULL_INTERVAL_SECS,
            grace_accepted_total: Arc::new(AtomicU64::new(0)),
            trust_domain: None,
            last_list_issued_at: 0,
            created_at: Instant::now(),
        }
    }

    /// Wrap this verifier for sharing between guards and the poller.
    pub fn shared(self) -> SharedVerifier {
        Arc::new(std::sync::Mutex::new(self))
    }

    /// Adopt a verified revocation list (see [`revocation::verify`]).
    ///
    /// Refuses a list that is stale - older than
    /// [`revocation::MAX_LIST_AGE_SECS`], from the future, or older than the
    /// last one accepted - and leaves everything as it was, including the
    /// degraded-mode clock: a replayed list must not make a verifier cut off
    /// from the control plane look in sync. The epoch floor only ever rises.
    pub fn apply_revocation_list(
        &mut self,
        list: RevocationList,
    ) -> Result<(), RevocationListError> {
        revocation::check_fresh(&list, now_secs(), self.last_list_issued_at)?;
        self.min_auth_epoch = self.min_auth_epoch.max(list.auth_epoch);
        self.revoked_jtis = list.revoked_jtis;
        self.last_list_issued_at = list.issued_at;
        self.record_sync();
        Ok(())
    }

    /// The minimum auth epoch currently enforced.
    pub fn min_auth_epoch(&self) -> u64 {
        self.min_auth_epoch
    }

    /// When degraded mode began, as Unix seconds, or `None` when not
    /// degraded. A verifier that has never synced has been degraded since it
    /// was made.
    pub fn degraded_since_unix(&self) -> Option<u64> {
        if !self.is_degraded() {
            return None;
        }
        let began = match self.last_sync {
            Some(last) => last + Duration::from_secs(self.pull_interval_secs * DEGRADED_PULL_INTERVAL_FACTOR),
            None => self.created_at,
        };
        Some(now_secs().saturating_sub(began.elapsed().as_secs()))
    }

    /// The T8 metrics in Prometheus text form, labelled with `service`, for
    /// the process's own metrics endpoint.
    pub fn render_metrics(&self, service: &str) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(768);
        let label = format!("{{service=\"{service}\"}}");
        let mut family = |name: &str, kind: &str, help: &str, value: u64| {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} {kind}");
            let _ = writeln!(out, "{name}{label} {value}");
        };
        family(
            "deis_auth_degraded",
            "gauge",
            "1 while this verifier has not fetched the control plane's revocation list for two poll intervals (T8 degraded mode)",
            u64::from(self.is_degraded()),
        );
        family(
            "deis_auth_degraded_since_seconds",
            "gauge",
            "Unix time degraded mode began, 0 when not degraded",
            self.degraded_since_unix().unwrap_or(0),
        );
        family(
            "deis_auth_grace_accepted_total",
            "counter",
            "Expired node capabilities accepted under T8 degraded-mode grace",
            self.grace_accepted_total(),
        );
        family(
            "deis_auth_revocation_last_sync_seconds",
            "gauge",
            "Unix time of the last revocation list accepted, 0 if none yet",
            self.last_sync_unix().unwrap_or(0),
        );
        out
    }

    /// The key ring this verifier checks signatures against.
    pub fn key_ring(&self) -> &KeyRing {
        &self.keys
    }

    /// Update the minimum accepted auth epoch.
    pub fn set_min_auth_epoch(&mut self, epoch: u64) {
        self.min_auth_epoch = epoch;
    }

    /// Replace the revoked JTI list.
    pub fn set_revoked_jtis(&mut self, jtis: Vec<String>) {
        self.revoked_jtis = jtis;
    }

    /// Set the maximum grace period in seconds, silently capped at
    /// [`MAX_GRACE_PERIOD_SECS`] (24 hours).
    pub fn set_grace_max_secs(&mut self, secs: u64) {
        self.grace_max_secs = secs.min(MAX_GRACE_PERIOD_SECS);
    }

    /// Only grant T8 grace to nodes of `trust_domain` - this installation's.
    pub fn set_trust_domain(&mut self, trust_domain: impl Into<String>) {
        self.trust_domain = Some(trust_domain.into());
    }

    /// Set the revocation-list pull interval in seconds.
    pub fn set_pull_interval_secs(&mut self, secs: u64) {
        self.pull_interval_secs = secs;
    }

    /// Record a successful revocation-list pull, resetting the degraded-mode
    /// clock.
    pub fn record_sync(&mut self) {
        self.last_sync = Some(Instant::now());
    }

    /// Total number of expired tokens accepted under T8 degraded-mode grace
    /// rules since this verifier was created.
    pub fn grace_accepted_total(&self) -> u64 {
        self.grace_accepted_total.load(Ordering::Relaxed)
    }

    /// Seconds since the last successful revocation-list pull, or `None` if a
    /// pull has never completed.
    pub fn degraded_since_seconds(&self) -> Option<u64> {
        self.last_sync.map(|last| last.elapsed().as_secs())
    }

    /// Whether the verifier is in degraded mode.
    ///
    /// Returns `true` when:
    /// - `last_sync` has never been set (the verifier has never synced), or
    /// - the elapsed time since `last_sync` exceeds
    ///   `DEGRADED_PULL_INTERVAL_FACTOR × pull_interval_secs`.
    pub fn is_degraded(&self) -> bool {
        match self.last_sync {
            None => true,
            Some(last) => {
                last.elapsed()
                    > Duration::from_secs(
                        self.pull_interval_secs * DEGRADED_PULL_INTERVAL_FACTOR,
                    )
            }
        }
    }

    /// The approximate Unix time of the last successful sync, or `None` if
    /// the verifier has never synced.
    pub fn last_sync_unix(&self) -> Option<u64> {
        self.last_sync.map(|last| {
            let elapsed = last.elapsed().as_secs();
            now_secs().saturating_sub(elapsed)
        })
    }

    /// Verify a token against an expected audience, tenant, and binding.
    pub fn verify(
        &self,
        token: &str,
        expected_aud: &str,
        tenant: &str,
        binding: &str,
    ) -> Result<VerifiedCapability, CapabilityError> {
        let claims = decode_with(token, &self.keys)?;
        check_claims(
            claims,
            false,
            expected_aud,
            tenant,
            binding,
            self.min_auth_epoch,
            &self.revoked_jtis,
        )
    }

    /// Verify with T8 degraded-mode grace period.
    ///
    /// When the token's `sub` is a runtime node's SPIFFE name
    /// (`spiffe://<trust domain>/deislet/node/<id>`, see [`is_node_subject`])
    /// and the verifier is in degraded mode, expired tokens are accepted if:
    /// 1. `exp ≥ last_sync_unix − skew_secs` (the token was still valid when
    ///    the control plane was last reachable), and
    /// 2. `now ≤ exp + grace_max_secs` (within the grace window).
    ///
    /// Revocation (`jti` and `auth_epoch`, from the last list fetched) and
    /// every other check still apply. Human / CLI tokens are never eligible
    /// for grace: their subject is not a node's name.
    pub fn verify_with_grace(
        &self,
        token: &str,
        expected_aud: &str,
        tenant: &str,
        binding: &str,
    ) -> Result<VerifiedCapability, CapabilityError> {
        let claims = decode_with(token, &self.keys)?;
        let now = now_secs();
        let grace_accepted = claims.is_expired()
            && is_node_subject(&claims.sub, self.trust_domain.as_deref())
            && self.is_degraded()
            && self.grace_applies(&claims, now);

        let verified = check_claims(
            claims,
            grace_accepted,
            expected_aud,
            tenant,
            binding,
            self.min_auth_epoch,
            &self.revoked_jtis,
        )?;

        if verified.grace_accepted {
            self.grace_accepted_total.fetch_add(1, Ordering::Relaxed);
        }
        Ok(verified)
    }

    /// Verify a token against an expected audience only, with the same T8
    /// grace as [`Self::verify_with_grace`], and hand back its resources for
    /// the caller to check.
    ///
    /// For a service whose one call can touch several tenants - a
    /// deis-telemetry push carries every application on a node - and which
    /// therefore cannot name the one tenant [`Self::verify_with_grace`]
    /// needs. The caller must then refuse whatever the resources do not
    /// name; an empty resource list authorizes nothing.
    pub fn verify_grant_with_grace(
        &self,
        token: &str,
        expected_aud: &str,
    ) -> Result<VerifiedGrant, CapabilityError> {
        let claims = decode_with(token, &self.keys)?;
        let now = now_secs();
        let grace_accepted = claims.is_expired()
            && is_node_subject(&claims.sub, self.trust_domain.as_deref())
            && self.is_degraded()
            && self.grace_applies(&claims, now);

        check_common(
            &claims,
            grace_accepted,
            expected_aud,
            self.min_auth_epoch,
            &self.revoked_jtis,
        )?;

        if grace_accepted {
            self.grace_accepted_total.fetch_add(1, Ordering::Relaxed);
        }
        Ok(VerifiedGrant {
            sub: claims.sub,
            resources: claims.resources,
            grace_accepted,
            exp: claims.exp,
        })
    }

    /// Check whether the two grace-period conditions are satisfied.
    fn grace_applies(&self, claims: &CapabilityClaims, now: u64) -> bool {
        let last_unix = match self.last_sync_unix() {
            Some(t) => t,
            None => return false,
        };
        let last_allowed = last_unix.saturating_sub(self.skew_secs);
        if claims.exp < last_allowed {
            return false;
        }
        if now > claims.exp.saturating_add(self.grace_max_secs) {
            return false;
        }
        true
    }
}

// ---- helpers --------------------------------------------------------------

/// Returns `true` if the subject is a runtime node's SPIFFE name,
/// `spiffe://<trust domain>/deislet/node/<id>` - and, when `trust_domain` is
/// given, one in that trust domain.
///
/// Parsed with the same code every machine handshake uses, so what counts as
/// a node here is exactly what the machine channel admits as one. Anything
/// else - a human capability's subject, the control plane's own name, a name
/// that does not parse - is not a node and gets no T8 grace.
fn is_node_subject(sub: &str, trust_domain: Option<&str>) -> bool {
    let Ok(id) = deis_identity::SpiffeId::parse(sub) else {
        return false;
    };
    if trust_domain.is_some_and(|domain| id.trust_domain() != domain) {
        return false;
    }
    deis_identity::names::service_kind(&id) == Some(deis_identity::ServiceKind::Node)
}

/// JCS-canonicalize the claims to deterministic bytes for signing.
fn serialize_claims(claims: &CapabilityClaims) -> Vec<u8> {
    serde_jcs::to_vec(claims).expect("JCS serialization of CapabilityClaims is infallible")
}

/// Base64url encode without padding, as used in the token wire format.
fn base64url(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode a base64url-encoded string without padding.
fn base64url_decode(input: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    URL_SAFE_NO_PAD.decode(input)
}

/// Current time in Unix seconds.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---- tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn make_keypair() -> (SigningKey, ed25519_dalek::VerifyingKey) {
        let mut seed = [0u8; 32];
        // Deterministic seed for tests
        seed[0..6].copy_from_slice(b"deis-c");
        let signing = SigningKey::from_bytes(&seed);
        let verifying = signing.verifying_key();
        (signing, verifying)
    }

    fn make_claims(aud: &str) -> CapabilityClaims {
        CapabilityClaims::new(
            aud,
            "spiffe://deislet/control/main",
            "spiffe://deislet/node/n1",
            1,
            Duration::from_secs(900),
            vec![CapabilityResource::new(
                &"v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002"
                    .parse()
                    .unwrap(),
                "kv",
            )],
        )
    }

    #[test]
    fn round_trip_encode_decode() {
        let (signing, verifying) = make_keypair();
        let claims = make_claims("store");
        let token = encode(&claims, &signing);

        // Token has the right prefix
        assert!(token.starts_with("v1."));

        let decoded = decode(&token, &verifying).unwrap();
        assert_eq!(
            decoded,
            CapabilityClaims {
                kid: key_id(&verifying),
                ..claims
            }
        );
    }

    #[test]
    fn token_format_has_correct_separators() {
        let (signing, _) = make_keypair();
        let claims = make_claims("store");
        let token = encode(&claims, &signing);

        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "token must have 3 dot-separated parts");
        assert_eq!(parts[0], "v1");
        // Payload and sig must be non-empty base64url
        assert!(!parts[1].is_empty());
        assert!(!parts[2].is_empty());
        // No padding chars in base64url
        assert!(!parts[1].contains('='));
        assert!(!parts[2].contains('='));
    }

    #[test]
    fn bad_signature_is_detected() {
        let (signing, verifying) = make_keypair();
        let claims = make_claims("store");
        let token = encode(&claims, &signing);

        // Mutate a character in the payload
        let mut tampered = token.clone();
        let idx = tampered.rfind('.').unwrap() - 5;
        unsafe {
            tampered.as_bytes_mut()[idx] ^= 1;
        }

        let result = decode(&tampered, &verifying);
        assert!(
            matches!(result, Err(CapabilityError::BadSignature(_))),
            "expected BadSignature, got {result:?}"
        );
    }

    #[test]
    fn wrong_key_is_detected() {
        let (signing, _) = make_keypair();
        let claims = make_claims("store");
        let token = encode(&claims, &signing);

        // Different keypair
        let mut seed = [0u8; 32];
        seed[0..6].copy_from_slice(b"other-");
        let other_signing = SigningKey::from_bytes(&seed);
        let other_verifying = other_signing.verifying_key();

        // The token names its own key, which this verifier does not hold.
        let result = decode(&token, &other_verifying);
        assert!(
            matches!(result, Err(CapabilityError::UnknownKey { .. })),
            "expected UnknownKey, got {result:?}"
        );
    }

    /// A token that names a trusted key but was signed by another one is a
    /// forgery, and the signature check is what catches it.
    #[test]
    fn a_kid_naming_a_trusted_key_does_not_excuse_a_foreign_signature() {
        let (_, trusted) = make_keypair();
        let forger = SigningKey::from_bytes(&[9u8; 32]);
        let mut claims = make_claims("store");
        claims.kid = key_id(&trusted);
        // Signed by hand so `encode` cannot correct the kid.
        let payload = serialize_claims(&claims);
        let signature = forger.sign(&payload);
        let token = format!(
            "{VERSION_PREFIX}{}{TOKEN_SEPARATOR}{}",
            base64url(&payload),
            base64url(&signature.to_bytes())
        );
        assert!(matches!(
            decode(&token, &trusted),
            Err(CapabilityError::BadSignature(_))
        ));
    }

    #[test]
    fn encode_names_the_signing_key() {
        let (signing, verifying) = make_keypair();
        let token = encode(&make_claims("store"), &signing);
        let claims = decode(&token, &verifying).unwrap();
        assert_eq!(claims.kid, key_id(&verifying));
    }

    /// Rotation as a verifier sees it: tokens from the old and the new key
    /// both verify while both are published, and the old key's tokens stop
    /// the moment it is dropped from the ring.
    #[test]
    fn a_verifier_accepts_every_published_key_and_only_those() {
        let old = SigningKey::from_bytes(&[1u8; 32]);
        let new = SigningKey::from_bytes(&[2u8; 32]);
        let ring = KeyRing::from_keys([old.verifying_key(), new.verifying_key()]);
        let verifier = CapabilityVerifier::with_key_ring(ring.clone());
        let tenant = "app.production";
        let claims = CapabilityClaims::new(
            AUD_STORE,
            "issuer",
            "subject",
            1,
            Duration::from_secs(900),
            resources_for_audience(tenant, AUD_STORE),
        );
        let old_token = encode(&claims, &old);
        let new_token = encode(&claims, &new);
        assert!(verifier.verify(&old_token, AUD_STORE, tenant, "kv").is_ok());
        assert!(verifier.verify(&new_token, AUD_STORE, tenant, "kv").is_ok());

        ring.replace([new.verifying_key()]);
        assert!(matches!(
            verifier.verify(&old_token, AUD_STORE, tenant, "kv"),
            Err(CapabilityError::UnknownKey { .. })
        ));
        assert!(verifier.verify(&new_token, AUD_STORE, tenant, "kv").is_ok());
    }

    /// What an issuer builds for an audience is exactly what each verifier
    /// behind it checks.
    #[test]
    fn resources_for_an_audience_satisfy_every_service_behind_it() {
        let (signing, verifying) = make_keypair();
        let verifier = CapabilityVerifier::new(verifying);
        let tenant = "app.production";
        for (aud, bindings) in [
            (AUD_STORE, STORE_BINDINGS),
            (AUD_QUEUE, QUEUE_BINDINGS),
            (AUD_TELEMETRY, TELEMETRY_BINDINGS),
        ] {
            let claims = CapabilityClaims::new(
                aud,
                "issuer",
                "subject",
                1,
                Duration::from_secs(900),
                resources_for_audience(tenant, aud),
            );
            let token = encode(&claims, &signing);
            for binding in bindings {
                verifier
                    .verify(&token, aud, tenant, binding)
                    .unwrap_or_else(|e| panic!("{aud}/{binding}: {e}"));
            }
            assert!(matches!(
                verifier.verify(&token, aud, "other.production", bindings[0]),
                Err(CapabilityError::TenantNotAuthorized { .. })
            ));
        }
        assert!(resources_for_audience(tenant, "telemetry.read").is_empty());
    }

    #[test]
    fn expiry_is_checked() {
        let (signing, verifying) = make_keypair();
        let mut claims = make_claims("store");
        // Set expiry to 60 seconds ago
        claims.exp = now_secs().saturating_sub(60);
        claims.iat = claims.exp.saturating_sub(900);
        let token = encode(&claims, &signing);

        let result = verify_for_request(
            &token,
            &verifying,
            "store",
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "kv",
            None,
            None,
        );
        assert!(
            matches!(result, Err(CapabilityError::Expired { .. })),
            "expected Expired, got {result:?}"
        );
    }

    #[test]
    fn clock_skew_is_tolerated() {
        let (signing, verifying) = make_keypair();
        let mut claims = make_claims("store");
        // Set expiry to just now (within clock skew)
        claims.exp = now_secs();
        claims.iat = claims.exp.saturating_sub(900);
        let token = encode(&claims, &signing);

        let result = verify_for_request(
            &token,
            &verifying,
            "store",
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "kv",
            None,
            None,
        );
        assert!(
            result.is_ok(),
            "capability at the boundary should pass with clock skew tolerance: {result:?}"
        );
    }

    #[test]
    fn a_lifetime_beyond_the_maximum_is_refused() {
        let (signing, verifying) = make_keypair();
        let mut claims = make_claims("store");
        claims.exp = claims.iat + 365 * 24 * 3600;
        let token = encode(&claims, &signing);
        let verifier = CapabilityVerifier::new(verifying);

        let result = verifier.verify(
            &token,
            "store",
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "kv",
        );
        assert!(
            matches!(result, Err(CapabilityError::InvalidClaims(ref m)) if m.contains("lifetime")),
            "a year-long token must be refused, got {result:?}"
        );
        let grant = verifier.verify_grant_with_grace(&token, "store");
        assert!(
            matches!(grant, Err(CapabilityError::InvalidClaims(_))),
            "the grant path applies the same ceiling, got {grant:?}"
        );

        // Exactly the ceiling is what every issuer mints, and passes.
        claims.exp = claims.iat + MAX_CAPABILITY_LIFETIME_SECS;
        let token = encode(&claims, &signing);
        assert!(verifier.verify_grant_with_grace(&token, "store").is_ok());
    }

    #[test]
    fn a_grant_carries_every_resource_and_checks_everything_else() {
        let (signing, verifying) = make_keypair();
        let claims = CapabilityClaims::new(
            AUD_TELEMETRY,
            "spiffe://deislet/control/main",
            "spiffe://deis.internal/deislet/node/n1",
            1,
            Duration::from_secs(900),
            vec![
                CapabilityResource {
                    tenant: "shop.production".into(),
                    binding: TELEMETRY_INGEST.into(),
                    generation: None,
                },
                CapabilityResource {
                    tenant: TELEMETRY_PLATFORM_TENANT.into(),
                    binding: TELEMETRY_INGEST.into(),
                    generation: None,
                },
            ],
        );
        let token = encode(&claims, &signing);
        let mut verifier = CapabilityVerifier::new(verifying);

        let grant = verifier
            .verify_grant_with_grace(&token, AUD_TELEMETRY)
            .expect("a well-formed telemetry capability");
        assert_eq!(grant.sub, "spiffe://deis.internal/deislet/node/n1");
        assert_eq!(
            grant.tenants_with(TELEMETRY_INGEST).collect::<Vec<_>>(),
            ["shop.production", TELEMETRY_PLATFORM_TENANT]
        );
        assert_eq!(grant.tenants_with(TELEMETRY_READ).count(), 0);

        assert!(matches!(
            verifier.verify_grant_with_grace(&token, AUD_STORE),
            Err(CapabilityError::WrongAudience { .. })
        ));
        verifier.set_revoked_jtis(vec![claims.jti.clone()]);
        assert!(matches!(
            verifier.verify_grant_with_grace(&token, AUD_TELEMETRY),
            Err(CapabilityError::Revoked { .. })
        ));
    }

    #[test]
    fn wrong_audience_is_refused() {
        let (signing, verifying) = make_keypair();
        let claims = make_claims("queue");
        let token = encode(&claims, &signing);

        let result = verify_for_request(
            &token,
            &verifying,
            "store", // expecting "store" but token has "queue"
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "kv",
            None,
            None,
        );
        assert!(
            matches!(result, Err(CapabilityError::WrongAudience { .. })),
            "expected WrongAudience, got {result:?}"
        );
    }

    #[test]
    fn wrong_tenant_is_refused() {
        let (signing, verifying) = make_keypair();
        let claims = make_claims("store");
        let token = encode(&claims, &signing);

        let result = verify_for_request(
            &token,
            &verifying,
            "store",
            "v1/018f2518-7e38-7f80-8000-999999999999/018f2518-7e38-7f80-8000-999999999999",
            "kv",
            None,
            None,
        );
        assert!(
            matches!(result, Err(CapabilityError::TenantNotAuthorized { .. })),
            "expected TenantNotAuthorized, got {result:?}"
        );
    }

    #[test]
    fn wrong_binding_is_refused() {
        let (signing, verifying) = make_keypair();
        let claims = make_claims("store");
        let token = encode(&claims, &signing);

        let result = verify_for_request(
            &token,
            &verifying,
            "store",
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "blob", // token has "kv", not "blob"
            None,
            None,
        );
        assert!(
            matches!(result, Err(CapabilityError::BindingNotAuthorized { .. })),
            "expected BindingNotAuthorized, got {result:?}"
        );
    }

    #[test]
    fn auth_epoch_check_rejects_old_tokens() {
        let (signing, verifying) = make_keypair();
        let claims = make_claims("store");
        let token = encode(&claims, &signing);

        let result = verify_for_request(
            &token,
            &verifying,
            "store",
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "kv",
            Some(5), // minimum epoch is 5, token has 1
            None,
        );
        assert!(
            matches!(result, Err(CapabilityError::AuthEpochTooLow { .. })),
            "expected AuthEpochTooLow, got {result:?}"
        );
    }

    #[test]
    fn revoked_jti_is_refused() {
        let (signing, verifying) = make_keypair();
        let claims = make_claims("store");
        let jti = claims.jti.clone();
        let token = encode(&claims, &signing);

        let result = verify_for_request(
            &token,
            &verifying,
            "store",
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "kv",
            None,
            Some(&[jti]),
        );
        assert!(
            matches!(result, Err(CapabilityError::Revoked { .. })),
            "expected Revoked, got {result:?}"
        );
    }

    #[test]
    fn bad_format_is_rejected() {
        let (_, verifying) = make_keypair();

        for bad in [
            "",
            "not-a-token",
            "v2.something.sig",
            "v1.noseparator",
            "v1..",
            "v1.payload.",
            "v1..signature",
        ] {
            let result = decode(bad, &verifying);
            assert!(
                matches!(result, Err(CapabilityError::ParseError(_))),
                "expected ParseError for {bad:?}, got {result:?}"
            );
        }
    }

    #[test]
    fn multiple_resources_are_all_checked() {
        let (signing, verifying) = make_keypair();
        let claims = CapabilityClaims::new(
            "store",
            "spiffe://deislet/control/main",
            "spiffe://deislet/node/n1",
            1,
            Duration::from_secs(900),
            vec![
                CapabilityResource::new(
                    &"v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002"
                        .parse()
                        .unwrap(),
                    "kv",
                ),
                CapabilityResource::new(
                    &"v1/018f2518-7e38-7f80-8000-000000000003/018f2518-7e38-7f80-8000-000000000004"
                        .parse()
                        .unwrap(),
                    "sql",
                ),
            ],
        );
        let token = encode(&claims, &signing);

        // authorized for first resource
        assert!(
            verify_for_request(
                &token,
                &verifying,
                "store",
                "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
                "kv",
                None,
                None,
            )
            .is_ok()
        );

        // authorized for second resource
        assert!(
            verify_for_request(
                &token,
                &verifying,
                "store",
                "v1/018f2518-7e38-7f80-8000-000000000003/018f2518-7e38-7f80-8000-000000000004",
                "sql",
                None,
                None,
            )
            .is_ok()
        );

        // wrong binding on first tenant
        assert!(matches!(
            verify_for_request(
                &token,
                &verifying,
                "store",
                "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
                "sql",
                None,
                None,
            ),
            Err(CapabilityError::BindingNotAuthorized { .. })
        ));
    }

    #[test]
    fn future_iat_is_rejected() {
        let (signing, verifying) = make_keypair();
        let mut claims = make_claims("store");
        // Set iat 2 minutes in the future (beyond clock skew)
        claims.iat = now_secs().saturating_add(120);
        claims.exp = claims.iat.saturating_add(900);
        let token = encode(&claims, &signing);

        let result = verify_for_request(
            &token,
            &verifying,
            "store",
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "kv",
            None,
            None,
        );
        assert!(
            matches!(result, Err(CapabilityError::NotYetValid { .. })),
            "expected NotYetValid, got {result:?}"
        );
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // Future extensions must not break deserialization
        let json = serde_json::json!({
            "aud": "store",
            "iss": "issuer",
            "jti": "00000000-0000-7000-8000-000000000001",
            "exp": 9999999999u64,
            "iat": 9999990000u64,
            "auth_epoch": 1,
            "sub": "subject",
            "resources": [],
            "future_field": "some_value",
            "another_extension": 42
        });
        let claims: CapabilityClaims = serde_json::from_value(json).unwrap();
        assert_eq!(claims.aud, "store");
        // Extra fields are silently dropped
    }

    #[test]
    fn canonicalization_is_stable() {
        // JCS must produce identical bytes for identical claims.
        // Use manual construction to avoid auto-generated jti.
        let claims1 = CapabilityClaims {
            aud: "store".into(),
            kid: String::new(),
            iss: "spiffe://deislet/control/main".into(),
            jti: "00000000-0000-7000-8000-000000000001".into(),
            exp: 1790478752,
            iat: 1790477852,
            auth_epoch: 1,
            sub: "spiffe://deislet/node/n1".into(),
            resources: vec![CapabilityResource::new(
                &"v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002"
                    .parse()
                    .unwrap(),
                "kv",
            )],
        };
        let claims2 = claims1.clone();

        let buf1 = serialize_claims(&claims1);
        let buf2 = serialize_claims(&claims2);

        assert_eq!(buf1, buf2, "JCS must produce stable output");
    }

    #[test]
    fn settlement_edges() {
        let (signing, verifying) = make_keypair();

        // Token with no resources
        let claims = CapabilityClaims::new(
            "store",
            "issuer",
            "subject",
            1,
            Duration::from_secs(900),
            vec![],
        );
        let token = encode(&claims, &signing);

        // Any tenant should be rejected
        let result = verify_for_request(
            &token,
            &verifying,
            "store",
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "kv",
            None,
            None,
        );
        assert!(matches!(
            result,
            Err(CapabilityError::TenantNotAuthorized { .. })
        ));

        // Token with empty string tenant in resource should still parse
        let claims = CapabilityClaims {
            aud: "store".into(),
            kid: String::new(),
            iss: "issuer".into(),
            sub: "subject".into(),
            jti: uuid::Uuid::now_v7().to_string(),
            exp: now_secs().saturating_add(900),
            iat: now_secs(),
            auth_epoch: 1,
            resources: vec![CapabilityResource {
                tenant: String::new(),
                binding: "kv".into(),
                generation: None,
            }],
        };
        let token = encode(&claims, &signing);

        // Empty tenant only matches empty tenant
        assert!(verify_for_request(&token, &verifying, "store", "", "kv", None, None,).is_ok());

        assert!(matches!(
            verify_for_request(&token, &verifying, "store", "some-tenant", "kv", None, None,),
            Err(CapabilityError::TenantNotAuthorized { .. })
        ));
    }

    // ---- T8 degraded-mode / grace-period tests ---------------------------

    /// A node's name exactly as `deis_identity::names::node` spells it and
    /// as `IssueCapability` puts it in `sub`: the name the node proved.
    const NODE_SUBJECT: &str = "spiffe://deis.internal/deislet/node/n1";

    /// What this check used to look for, and no node's name ever matched.
    const LEGACY_SHAPE: &str = "spiffe://deislet/node/n1";

    /// Make a token for a node principal that expires in `offset_secs`
    /// (negative = already expired).
    fn make_node_token(
        signing: &SigningKey,
        offset_secs: i64,
    ) -> String {
        let now = now_secs();
        let exp = (now as i64 + offset_secs) as u64;
        let iat = exp.saturating_sub(900);
        let claims = CapabilityClaims {
            aud: "store".into(),
            kid: String::new(),
            iss: "spiffe://deislet/control/main".into(),
            sub: NODE_SUBJECT.into(),
            jti: uuid::Uuid::now_v7().to_string(),
            exp,
            iat,
            auth_epoch: 1,
            resources: vec![CapabilityResource::new(
                &"v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002"
                    .parse()
                    .unwrap(),
                "kv",
            )],
        };
        encode(&claims, signing)
    }

    /// Make a token for a human principal that expires in `offset_secs`
    /// (negative = already expired).
    fn make_human_token(
        signing: &SigningKey,
        offset_secs: i64,
    ) -> String {
        let now = now_secs();
        let exp = (now as i64 + offset_secs) as u64;
        let iat = exp.saturating_sub(900);
        let claims = CapabilityClaims {
            aud: "store".into(),
            kid: String::new(),
            iss: "spiffe://deislet/control/main".into(),
            sub: "spiffe://deislet/human/alice".into(),
            jti: uuid::Uuid::now_v7().to_string(),
            exp,
            iat,
            auth_epoch: 1,
            resources: vec![CapabilityResource::new(
                &"v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002"
                    .parse()
                    .unwrap(),
                "kv",
            )],
        };
        encode(&claims, signing)
    }

    #[test]
    fn is_node_subject_identifies_the_names_nodes_really_carry() {
        let issued = deis_identity::names::node("deis.internal", "runtime-42").unwrap();
        assert!(is_node_subject(issued.as_str(), None));
        assert!(is_node_subject(NODE_SUBJECT, None));
        assert!(is_node_subject("spiffe://prod.example/deislet/node/n1", None));
    }

    #[test]
    fn is_node_subject_rejects_human_spiffe() {
        assert!(!is_node_subject("spiffe://deislet/human/root", None));
        assert!(!is_node_subject("spiffe://deislet/human/alice", None));
    }

    #[test]
    fn is_node_subject_rejects_arbitrary_sub() {
        assert!(!is_node_subject("admin", None));
        // The shape this check used to look for, which no node carries.
        assert!(!is_node_subject(LEGACY_SHAPE, None));
        assert!(!is_node_subject("spiffe://deis.internal/deislet/control", None));
        assert!(!is_node_subject("spiffe://deis.internal/deislet/proxy/p1", None));
        assert!(!is_node_subject("spiffe://deis.internal/deislet/node/n1/extra", None));
    }

    #[test]
    fn is_node_subject_holds_a_node_to_the_verifiers_trust_domain() {
        assert!(is_node_subject(NODE_SUBJECT, Some("deis.internal")));
        assert!(!is_node_subject(NODE_SUBJECT, Some("other.internal")));
    }

    /// The acceptance line for T8: a capability minted for a node as the
    /// control plane really mints it gets grace in degraded mode, and one
    /// from another trust domain does not once the verifier knows its own.
    #[test]
    fn grace_accepts_a_real_format_node_subject_and_only_in_its_trust_domain() {
        let (signing, verifying) = make_keypair();
        let token = make_node_token(&signing, -60);
        let mut verifier = CapabilityVerifier::new(verifying);
        verifier.last_sync = Some(Instant::now() - Duration::from_secs(3600));
        let tenant = "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002";
        let accepted = verifier
            .verify_with_grace(&token, "store", tenant, "kv")
            .expect("an expired node capability in degraded mode");
        assert!(accepted.grace_accepted);

        verifier.set_trust_domain("other.internal");
        assert!(matches!(
            verifier.verify_with_grace(&token, "store", tenant, "kv"),
            Err(CapabilityError::Expired { .. })
        ));
    }

    #[test]
    fn degraded_when_never_synced() {
        let (_, verifying) = make_keypair();
        let verifier = CapabilityVerifier::new(verifying);
        assert!(verifier.is_degraded());
    }

    #[test]
    fn not_degraded_after_sync() {
        let (_, verifying) = make_keypair();
        let mut verifier = CapabilityVerifier::new(verifying);
        verifier.record_sync();
        assert!(!verifier.is_degraded());
    }

    #[test]
    fn verify_with_grace_accepts_valid_token_normally() {
        let (signing, verifying) = make_keypair();
        let token = make_node_token(&signing, 600); // expires in 10 min
        let mut verifier = CapabilityVerifier::new(verifying);
        verifier.record_sync();

        let result = verifier.verify_with_grace(
            &token,
            "store",
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "kv",
        );
        match result {
            Ok(cap) => {
                assert!(!cap.grace_accepted, "valid token must not be grace_accepted");
                assert_eq!(cap.node_id, NODE_SUBJECT);
            }
            Err(e) => panic!("valid token should pass: {e}"),
        }
    }

    #[test]
    fn verify_with_grace_accepts_expired_node_token_in_degraded_mode() {
        let (signing, verifying) = make_keypair();
        let token = make_node_token(&signing, -60); // expired 60s ago
        let mut verifier = CapabilityVerifier::new(verifying);
        // Record sync, then simulate disconnection by not syncing.
        verifier.record_sync();
        // The verifier is NOT degraded (just synced), so the only way to
        // trigger grace is to skip record_sync and let time pass.  We
        // override last_sync to the distant past via manual field
        // manipulation.
        verifier.last_sync = Some(Instant::now() - Duration::from_secs(3600));
        assert!(verifier.is_degraded());

        let result = verifier.verify_with_grace(
            &token,
            "store",
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "kv",
        );
        match result {
            Ok(cap) => {
                assert!(cap.grace_accepted, "expired node token in degraded mode must be grace_accepted");
                assert_eq!(cap.node_id, NODE_SUBJECT);
            }
            Err(e) => panic!("grace should accept recently-expired node token: {e}"),
        }
    }

    #[test]
    fn grace_accepted_counter_increments() {
        let (signing, verifying) = make_keypair();
        let token = make_node_token(&signing, -60);
        let mut verifier = CapabilityVerifier::new(verifying);
        verifier.last_sync = Some(Instant::now() - Duration::from_secs(3600));
        assert!(verifier.is_degraded());
        assert_eq!(verifier.grace_accepted_total(), 0);

        // First grace acceptance.
        let _ = verifier.verify_with_grace(
            &token, "store",
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "kv",
        ).unwrap();
        assert_eq!(verifier.grace_accepted_total(), 1);

        // Second grace acceptance — counter accumulates.
        let _ = verifier.verify_with_grace(
            &token, "store",
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "kv",
        ).unwrap();
        assert_eq!(verifier.grace_accepted_total(), 2);
    }

    #[test]
    fn verify_with_grace_rejects_expired_human_token_even_in_degraded() {
        let (signing, verifying) = make_keypair();
        let token = make_human_token(&signing, -60); // expired 60s ago
        let mut verifier = CapabilityVerifier::new(verifying);
        verifier.last_sync = Some(Instant::now() - Duration::from_secs(3600));
        assert!(verifier.is_degraded());

        let result = verifier.verify_with_grace(
            &token,
            "store",
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "kv",
        );
        assert!(
            matches!(result, Err(CapabilityError::Expired { .. })),
            "human tokens must not receive grace"
        );
    }

    #[test]
    fn verify_with_grace_rejects_expired_node_when_not_degraded() {
        let (signing, verifying) = make_keypair();
        let token = make_node_token(&signing, -60); // expired 60s ago
        let mut verifier = CapabilityVerifier::new(verifying);
        verifier.record_sync(); // just synced = not degraded
        assert!(!verifier.is_degraded());

        let result = verifier.verify_with_grace(
            &token,
            "store",
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "kv",
        );
        assert!(
            matches!(result, Err(CapabilityError::Expired { .. })),
            "expired tokens must be refused when not degraded"
        );
    }

    #[test]
    fn verify_with_grace_rejects_token_expired_before_disconnection() {
        let (signing, verifying) = make_keypair();
        let token = make_node_token(&signing, -7200); // expired 2h ago
        let mut verifier = CapabilityVerifier::new(verifying);
        // last_sync was 1 min ago, but token expired 2h ago.
        verifier.last_sync = Some(Instant::now() - Duration::from_secs(60));
        assert!(verifier.is_degraded());

        let result = verifier.verify_with_grace(
            &token,
            "store",
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "kv",
        );
        assert!(
            matches!(result, Err(CapabilityError::Expired { .. })),
            "token expired before control-plane disconnection must not be revived"
        );
    }

    #[test]
    fn verify_with_grace_rejects_token_past_grace_window() {
        let (signing, verifying) = make_keypair();
        // Token expired 25h ago (past 24h grace window).
        let token = make_node_token(&signing, -90_000);
        let mut verifier = CapabilityVerifier::new(verifying);
        verifier.last_sync = Some(Instant::now() - Duration::from_secs(7200));
        assert!(verifier.is_degraded());

        let result = verifier.verify_with_grace(
            &token,
            "store",
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "kv",
        );
        assert!(
            matches!(result, Err(CapabilityError::Expired { .. })),
            "token past grace window must be refused"
        );
    }

    fn signed_list(signing: &SigningKey, epoch: u64, jtis: Vec<String>) -> RevocationList {
        let signed = revocation::sign(&RevocationList::new("iss", epoch, jtis), signing);
        revocation::verify(&signed, &KeyRing::from_keys([signing.verifying_key()])).unwrap()
    }

    const TENANT: &str =
        "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002";

    /// A jti on the list is refused from the moment the list is adopted -
    /// in normal mode and under T8 grace alike.
    #[test]
    fn a_jti_on_an_adopted_list_is_refused_even_under_grace() {
        let (signing, verifying) = make_keypair();
        let claims = make_claims("store");
        let token = encode(&claims, &signing);
        let mut verifier = CapabilityVerifier::new(verifying);
        assert!(verifier.verify_with_grace(&token, "store", TENANT, "kv").is_ok());

        verifier
            .apply_revocation_list(signed_list(&signing, 1, vec![claims.jti.clone()]))
            .unwrap();
        assert!(matches!(
            verifier.verify_with_grace(&token, "store", TENANT, "kv"),
            Err(CapabilityError::Revoked { .. })
        ));

        let expired = make_node_token(&signing, -60);
        let expired_jti = decode(&expired, &signing.verifying_key()).unwrap().jti;
        verifier
            .apply_revocation_list(signed_list(&signing, 1, vec![expired_jti]))
            .unwrap();
        verifier.last_sync = Some(Instant::now() - Duration::from_secs(30));
        verifier.set_pull_interval_secs(10);
        assert!(verifier.is_degraded());
        assert!(matches!(
            verifier.verify_with_grace(&expired, "store", TENANT, "kv"),
            Err(CapabilityError::Revoked { .. })
        ));
    }

    /// Mass revocation: a list carrying a higher epoch refuses every token
    /// minted under a lower one, and a later list cannot lower it again.
    #[test]
    fn an_epoch_bump_refuses_older_tokens_and_never_goes_back() {
        let (signing, verifying) = make_keypair();
        let old = encode(&make_claims("store"), &signing);
        let mut verifier = CapabilityVerifier::new(verifying);
        verifier.apply_revocation_list(signed_list(&signing, 2, vec![])).unwrap();
        assert!(matches!(
            verifier.verify_with_grace(&old, "store", TENANT, "kv"),
            Err(CapabilityError::AuthEpochTooLow { got: 1, minimum: 2 })
        ));
        verifier.apply_revocation_list(signed_list(&signing, 1, vec![])).unwrap();
        assert_eq!(verifier.min_auth_epoch(), 2);
    }

    /// A replayed list is refused and does not count as a sync, so it can
    /// neither hide newer revocations nor mask an outage.
    #[test]
    fn a_stale_list_changes_nothing_and_does_not_count_as_a_sync() {
        let (signing, verifying) = make_keypair();
        let mut verifier = CapabilityVerifier::new(verifying);
        let mut old = signed_list(&signing, 1, vec![]);
        old.issued_at -= revocation::MAX_LIST_AGE_SECS + 10;
        assert!(matches!(
            verifier.apply_revocation_list(old),
            Err(RevocationListError::Stale(_))
        ));
        assert!(verifier.last_sync.is_none());

        let fresh = signed_list(&signing, 1, vec!["x".into()]);
        let mut earlier = fresh.clone();
        earlier.issued_at -= 5;
        earlier.revoked_jtis.clear();
        verifier.apply_revocation_list(fresh).unwrap();
        assert!(verifier.apply_revocation_list(earlier).is_err());
        assert_eq!(verifier.revoked_jtis, vec!["x".to_string()]);
    }

    #[test]
    fn the_metrics_say_whether_the_verifier_is_degraded() {
        let (signing, verifying) = make_keypair();
        let mut verifier = CapabilityVerifier::new(verifying);
        let text = verifier.render_metrics("store");
        assert!(text.contains("deis_auth_degraded{service=\"store\"} 1"), "{text}");
        assert!(text.contains("# TYPE deis_auth_grace_accepted_total counter"));
        verifier.apply_revocation_list(signed_list(&signing, 1, vec![])).unwrap();
        let text = verifier.render_metrics("store");
        assert!(text.contains("deis_auth_degraded{service=\"store\"} 0"), "{text}");
        assert!(text.contains("deis_auth_degraded_since_seconds{service=\"store\"} 0"));
        assert!(text.contains("deis_auth_grace_accepted_total{service=\"store\"} 0"));
    }

    #[test]
    fn grace_max_is_capped_at_24_hours() {
        let (_, verifying) = make_keypair();
        let mut verifier = CapabilityVerifier::new(verifying);
        verifier.set_grace_max_secs(172_800); // 48h — should be capped
        assert_eq!(verifier.grace_max_secs, MAX_GRACE_PERIOD_SECS);
    }

    #[test]
    fn verify_with_grace_checks_binding_even_in_grace() {
        let (signing, verifying) = make_keypair();
        let token = make_node_token(&signing, -60);
        let mut verifier = CapabilityVerifier::new(verifying);
        verifier.last_sync = Some(Instant::now() - Duration::from_secs(3600));
        assert!(verifier.is_degraded());

        // Token has binding "kv", but we ask for "sql"
        let result = verifier.verify_with_grace(
            &token,
            "store",
            "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002",
            "sql",
        );
        assert!(
            matches!(result, Err(CapabilityError::BindingNotAuthorized { .. })),
            "binding must be checked even in grace mode"
        );
    }
}
