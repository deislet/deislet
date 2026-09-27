//! The credential this node presents to deis-telemetry.
//!
//! # Why there is one
//!
//! deis-telemetry holds every application's logs, every invocation trace and
//! every metric series on the installation. It guards all of that with one
//! capability token: with a `capability_public_key` configured it refuses any
//! call that does not present a valid Ed25519-signed capability token, and
//! with none configured it refuses to start at all unless both of its
//! listeners are on loopback. So on any installation where the nodes are not
//! the same machine as the collector, the collector has a key, and a node that
//! cannot present the matching token has every log line, every trace and every
//! metric sample it ever produces refused.
//!
//! What made that worth a module rather than a line in each shipper is how it
//! failed: a node with no credential does not stop. The log shipper keeps the
//! batch queued and tries again on the next tick, the metric shipper pushes
//! cumulative totals that are refused and pushed again, and the trace shipper
//! puts its records back at the front of the queue. Each of them is doing
//! exactly what it should when telemetry is *down* -- and none of them can be
//! fixed by waiting, because the calls are not failing, they are being refused.
//! The operator sees a collector that is up, nodes that are up, and nothing in
//! `deis logs` or `deis trace` for any application.
//!
//! # What an operator configures
//!
//! `logging.auth_secret`, or `DEIS_RUNTIME_LOGGING_AUTH_SECRET`, set to the
//! same string as deis-telemetry's own `auth_secret`. The secret is hashed
//! with SHA-256 to derive a 32-byte Ed25519 signing key, which in turn signs
//! a capability token. The secret itself is never sent, never logged and never
//! written to the configuration.
//!
//! # Why the subject is a channel and not a tenant
//!
//! deis-store and deis-queue derive a token per tenant, so a token that leaked
//! out of one application authorises that application's data and nothing else.
//! A telemetry call is not about one tenant: one push carries lines from every
//! application on the node, and one query reads across all of them. So the
//! subject is the service itself, and the token says "the holder is part of
//! this platform" and nothing more. deis-telemetry's `auth` module has the
//! whole of that reasoning; this side only has to derive the same value.

use deis_capability::{encode, CapabilityClaims, CapabilityResource};
use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::Request;

/// The audience the runtime stamps into every telemetry capability token.
///
/// deis-telemetry's gRPC interceptor tries `"telemetry"` first, then
/// `"telemetry.read"` — see `deis_telemetry::auth`. A push-side credential
/// uses `"telemetry"` so the interceptor matches on the first attempt.
const TELEMETRY_AUDIENCE: &str = "telemetry";

/// The lifetime of a telemetry capability token in seconds.
///
/// The token is derived from the configured secret at node startup and reused
/// for the lifetime of the process. A generous lifetime avoids the token
/// expiring while the node is running, but it is ultimately bounded by
/// `auth_epoch` revocation on the collector side.
const TELEMETRY_TOKEN_LIFETIME_SECS: u64 = 365 * 24 * 3600;

/// The token this node presents on every telemetry call, or nothing when no
/// secret is configured.
///
/// Cheap to clone, and deliberately shared by the three channels: logs,
/// metrics and traces go to one service over one credential, and three
/// derivations would be three places for the audience to be spelled wrong.
#[derive(Clone, Default)]
pub struct TelemetryCredential {
    /// Pre-formatted `Bearer <token>` value, parsed once at startup so a
    /// secret that cannot be sent as gRPC metadata is a startup complaint
    /// rather than a failure on every push for the life of the process.
    bearer: Option<MetadataValue<Ascii>>,
}

impl TelemetryCredential {
    /// Derive the credential from the configured secret.
    ///
    /// Hashes the secret with SHA-256 to derive a 32-byte Ed25519 seed, signs
    /// a capability token with audience `"telemetry"`, and pre-formats it as
    /// an `Authorization: Bearer <token>` header value.
    ///
    /// A blank secret counts as none: an empty header would be presented as a
    /// token and refused, where sending nothing at least works against a
    /// collector that requires no proof -- which is the single-box default and
    /// has to keep working untouched.
    pub fn from_secret(secret: Option<&str>) -> Self {
        let Some(secret) = secret.map(str::trim).filter(|secret| !secret.is_empty()) else {
            return Self { bearer: None };
        };

        // Derive a deterministic 32-byte Ed25519 seed from the shared secret.
        let hash = Sha256::digest(secret.as_bytes());
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&hash);
        let signing_key = SigningKey::from_bytes(&seed);

        let claims = CapabilityClaims::new(
            TELEMETRY_AUDIENCE,
            "deis-runtime",
            "deis-runtime",
            1,
            Duration::from_secs(TELEMETRY_TOKEN_LIFETIME_SECS),
            vec![CapabilityResource {
                tenant: String::new(),
                binding: String::new(),
                generation: None,
            }],
        );
        let token = encode(&claims, &signing_key);

        let bearer = format!("Bearer {token}");
        match bearer.parse::<MetadataValue<Ascii>>() {
            Ok(value) => Self {
                bearer: Some(value),
            },
            // The token is base64url of JSON + Ed25519 sig, so this cannot
            // happen from a secret of any shape; it is said out loud rather
            // than silently dropped because the consequence of shipping
            // without the token is that everything this node observes is
            // refused.
            Err(_) => {
                tracing::error!(
                    "the telemetry capability token derived from logging.auth_secret cannot be \
                     sent as gRPC metadata, so this node will ship without one. Against a \
                     deis-telemetry that has a capability_public_key, every log line, trace \
                     and metric sample from this node is refused"
                );
                Self { bearer: None }
            }
        }
    }

    /// Whether this node has a credential at all.
    ///
    /// Reported once when a shipper starts, so the startup log says which of
    /// the two regimes is in force rather than leaving it to be worked out
    /// from an empty `deis logs`.
    pub fn is_present(&self) -> bool {
        self.bearer.is_some()
    }

    /// Wrap one message in a request carrying the token.
    ///
    /// Stamped here rather than in each call so a push that forgot it would
    /// be refused, retried, and refused again for as long as the node ran.
    pub fn stamp<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        if let Some(bearer) = &self.bearer {
            request
                .metadata_mut()
                .insert("authorization", bearer.clone());
        }
        request
    }
}

/// Redacted on purpose: a derived `Debug` would put the token into any line
/// that formats a shipper, and the token is the whole of the authority to read
/// and to forge every application's telemetry on this installation.
impl std::fmt::Debug for TelemetryCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryCredential")
            .field(
                "bearer",
                &if self.bearer.is_some() {
                    "<present>"
                } else {
                    "<absent>"
                },
            )
            .finish()
    }
}

/// What a shipper says when it starts, so an operator can tell the two regimes
/// apart from the first lines of a node's log.
///
/// Not a refusal to start. A node that cannot ship telemetry must still serve
/// requests -- log delivery is never allowed to become a reason a request fails
/// -- so the mismatch is loud and the node carries on.
pub fn report_posture(credential: &TelemetryCredential, endpoint: &str) {
    if credential.is_present() {
        tracing::info!(
            %endpoint,
            "This node presents an Ed25519 capability token on every log, trace and metric \
             push. If deis-telemetry answers unauthenticated, the two no longer share the same \
             capability signing key"
        );
    } else {
        tracing::warn!(
            %endpoint,
            "No logging.auth_secret is configured, so this node ships to deis-telemetry without \
             a capability token. That works only against a collector that has no \
             capability_public_key either -- which it may only have when both of its \
             listeners are on loopback. Against any other collector every log line, every \
             invocation trace and every metric sample from this node is refused, the node \
             retries them for as long as it runs, and `deis logs` and `deis trace` stay \
             empty for every application it serves"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_without_a_secret_carries_no_token() {
        assert!(!TelemetryCredential::from_secret(None).is_present());
        assert!(!TelemetryCredential::from_secret(Some("")).is_present());
        assert!(
            !TelemetryCredential::from_secret(Some("   ")).is_present(),
            "whitespace is what a half-filled config file holds, and an empty header is refused \
             where no header at all is not"
        );
    }

    #[test]
    fn the_token_is_an_ed25519_capability() {
        let credential = TelemetryCredential::from_secret(Some("shared-secret"));
        assert!(credential.is_present());

        let stamped = credential.stamp(());
        let header_value = stamped
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .expect("the token travels in an Authorization: Bearer header");

        assert!(
            header_value.starts_with("Bearer "),
            "the header value must be 'Bearer <token>', got: {header_value:?}"
        );

        // The token itself must be a valid v1 capability token.
        let token = header_value.strip_prefix("Bearer ").unwrap();
        assert!(
            token.starts_with("v1."),
            "the capability token must start with 'v1.', got prefix: {:?}",
            &token[..token.len().min(10)]
        );

        // The same secret must produce a valid capability token each time.
        // Each call generates a fresh jti (JWT ID), so the tokens are not
        // byte-identical, but both must be well-formed Bearer tokens signed
        // by the same key.
        let credential2 = TelemetryCredential::from_secret(Some("shared-secret"));
        let stamped2 = credential2.stamp(());
        let header_value2 = stamped2
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap();
        assert!(
            header_value2.starts_with("Bearer v1."),
            "the second token from the same secret must also be a capability token"
        );

        // A different secret must produce a different token.
        let credential3 = TelemetryCredential::from_secret(Some("other-secret"));
        let stamped3 = credential3.stamp(());
        let header_value3 = stamped3
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap();
        assert_ne!(
            header_value, header_value3,
            "different secrets must produce different Ed25519 keys and different tokens"
        );
    }

    #[test]
    fn a_node_without_a_secret_stamps_nothing() {
        let stamped = TelemetryCredential::from_secret(None).stamp(());
        assert!(
            stamped.metadata().get("authorization").is_none(),
            "an empty token is presented and refused; no token at all is served by a collector \
             that requires none"
        );
    }
}
