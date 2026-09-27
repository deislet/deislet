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
pub fn encode(claims: &CapabilityClaims, signing_key: &ed25519_dalek::SigningKey) -> String {
    let payload = serialize_claims(claims);
    let signature = signing_key.sign(&payload);
    let payload_b64 = base64url(&payload);
    let sig_b64 = base64url(signature.to_bytes().as_ref());
    format!("{VERSION_PREFIX}{payload_b64}{TOKEN_SEPARATOR}{sig_b64}")
}

/// Decode and verify a capability token.
///
/// Returns the deserialized claims on success, or a [`CapabilityError`]
/// describing why verification failed.
pub fn decode(
    token: &str,
    verifying_key: &ed25519_dalek::VerifyingKey,
) -> Result<CapabilityClaims, CapabilityError> {
    let (payload_bytes, sig_bytes) = parse_token(token)?;

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

    // Check expiry
    if claims.is_expired() {
        return Err(CapabilityError::Expired {
            exp: claims.exp,
            now: now_secs(),
        });
    }

    // Check iat not in the future
    if claims.is_not_yet_valid() {
        return Err(CapabilityError::NotYetValid {
            iat: claims.iat,
            now: now_secs(),
        });
    }

    // Check audience
    if claims.aud != expected_aud {
        return Err(CapabilityError::WrongAudience {
            expected: expected_aud.to_string(),
            got: claims.aud,
        });
    }

    // Check auth epoch
    if let Some(minimum) = min_auth_epoch {
        if claims.auth_epoch < minimum {
            return Err(CapabilityError::AuthEpochTooLow {
                got: claims.auth_epoch,
                minimum,
            });
        }
    }

    // Check revocation
    if let Some(revoked) = revoked_jtis {
        if revoked.iter().any(|jti| jti == &claims.jti) {
            return Err(CapabilityError::Revoked {
                jti: claims.jti.clone(),
            });
        }
    }

    // Check tenant + binding
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
        node_id: claims.sub.clone(),
        canonical_tenant_id: tenant.to_string(),
        binding: binding.to_string(),
        grace_accepted: false,
        exp: claims.exp,
    })
}

// ---- verifier -------------------------------------------------------------

/// A reusable verifier for capability tokens.
///
/// Bundles the verifying key with the two revocation-state inputs
/// (`min_auth_epoch` and `revoked_jtis`) and the T8 degraded-mode state
/// (`last_sync`, `grace_max_secs`) so a service can construct one
/// verifier at startup and reuse it for every request.
#[derive(Debug, Clone)]
pub struct CapabilityVerifier {
    /// The Ed25519 public key for verifying signatures.
    verifying_key: ed25519_dalek::VerifyingKey,
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
}

impl CapabilityVerifier {
    /// Create a verifier with `min_auth_epoch = 0`, an empty revoked list,
    /// and T8 defaults (grace period = 24 h, skew = 30 s,
    /// pull interval = 30 s).
    pub fn new(verifying_key: ed25519_dalek::VerifyingKey) -> Self {
        Self {
            verifying_key,
            min_auth_epoch: 0,
            revoked_jtis: Vec::new(),
            last_sync: None,
            grace_max_secs: DEFAULT_GRACE_PERIOD_SECS,
            skew_secs: DEFAULT_DEGRADED_SKEW_SECS,
            pull_interval_secs: DEFAULT_PULL_INTERVAL_SECS,
            grace_accepted_total: Arc::new(AtomicU64::new(0)),
        }
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
        verify_for_request(
            token,
            &self.verifying_key,
            expected_aud,
            tenant,
            binding,
            Some(self.min_auth_epoch),
            Some(&self.revoked_jtis),
        )
    }

    /// Verify with T8 degraded-mode grace period.
    ///
    /// When the token's `sub` identifies a node SPIFFE identity
    /// (`spiffe://deislet/node/…`) and the verifier is in degraded mode,
    /// expired tokens are accepted if:
    /// 1. `exp ≥ last_sync_unix − skew_secs` (the token was still valid when
    ///    the control plane was last reachable), and
    /// 2. `now ≤ exp + grace_max_secs` (within the grace window).
    ///
    /// Human / CLI tokens are never eligible for grace — their `sub` starts
    /// with `spiffe://deislet/human/`.
    pub fn verify_with_grace(
        &self,
        token: &str,
        expected_aud: &str,
        tenant: &str,
        binding: &str,
    ) -> Result<VerifiedCapability, CapabilityError> {
        let claims = decode(token, &self.verifying_key)?;
        let now = now_secs();
        let is_node = is_node_subject(&claims.sub);
        let grace_accepted = claims.is_expired()
            && is_node
            && self.is_degraded()
            && self.grace_applies(&claims, now);

        // Check expiry with grace.
        if claims.is_expired() && !grace_accepted {
            return Err(CapabilityError::Expired {
                exp: claims.exp,
                now,
            });
        }

        // Iat must not be in the future (checked even in grace).
        if !grace_accepted && claims.is_not_yet_valid() {
            return Err(CapabilityError::NotYetValid {
                iat: claims.iat,
                now,
            });
        }

        // Check audience.
        if claims.aud != expected_aud {
            return Err(CapabilityError::WrongAudience {
                expected: expected_aud.to_string(),
                got: claims.aud,
            });
        }

        // Check auth epoch.
        if claims.auth_epoch < self.min_auth_epoch {
            return Err(CapabilityError::AuthEpochTooLow {
                got: claims.auth_epoch,
                minimum: self.min_auth_epoch,
            });
        }

        // Check revocation.
        if self.revoked_jtis.iter().any(|jti| *jti == claims.jti) {
            return Err(CapabilityError::Revoked {
                jti: claims.jti.clone(),
            });
        }

        // Check tenant + binding.
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

        if grace_accepted {
            self.grace_accepted_total
                .fetch_add(1, Ordering::Relaxed);
        }

        Ok(VerifiedCapability {
            node_id: claims.sub.clone(),
            canonical_tenant_id: tenant.to_string(),
            binding: binding.to_string(),
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

/// Returns `true` if the subject identifies a node principal
/// (SPIFFE `spiffe://deislet/node/…`).  Human and CLI tokens use
/// `spiffe://deislet/human/…` and are excluded from T8 grace.
fn is_node_subject(sub: &str) -> bool {
    sub.starts_with("spiffe://deislet/node/")
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
        assert_eq!(decoded, claims);
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

        let result = decode(&token, &other_verifying);
        assert!(
            matches!(result, Err(CapabilityError::BadSignature(_))),
            "expected BadSignature, got {result:?}"
        );
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
            iss: "spiffe://deislet/control/main".into(),
            sub: "spiffe://deislet/node/n1".into(),
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
    fn is_node_subject_identifies_node_spiffe() {
        assert!(is_node_subject("spiffe://deislet/node/n1"));
        assert!(is_node_subject("spiffe://deislet/node/runtime-42"));
    }

    #[test]
    fn is_node_subject_rejects_human_spiffe() {
        assert!(!is_node_subject("spiffe://deislet/human/root"));
        assert!(!is_node_subject("spiffe://deislet/human/alice"));
    }

    #[test]
    fn is_node_subject_rejects_arbitrary_sub() {
        assert!(!is_node_subject("admin"));
        assert!(!is_node_subject("spiffe://other/node/n1"));
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
                assert_eq!(cap.node_id, "spiffe://deislet/node/n1");
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
                assert_eq!(cap.node_id, "spiffe://deislet/node/n1");
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
