//! The credential this node presents to deis-telemetry.
//!
//! # Why there is one
//!
//! deis-telemetry holds every application's logs, every invocation trace and
//! every metric series on the installation. It guards all of that with one
//! capability token: with an `auth_secret` configured it refuses any call that
//! does not present the token, and with none configured it refuses to start at
//! all unless both of its listeners are on loopback. So on any installation
//! where the nodes are not the same machine as the collector, the collector has
//! a secret, and a node that cannot present the matching token has every log
//! line, every trace and every metric sample it ever produces refused.
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
//! same string as deis-telemetry's own `auth_secret`. The token is derived
//! from it, never sent, never logged and never written to the configuration:
//! the node holds the secret and computes the token, which is the same shape
//! the storage clients use for their own tokens (see
//! `runtime::bindings::StorageIdentity`) -- the difference being only what the
//! token is derived *over*.
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

use deis_wire::conventions::{storage_token, STORAGE_TOKEN_HEADER};
use tonic::metadata::{Ascii, MetadataValue};
use tonic::Request;

/// What the token for deis-telemetry is derived over.
///
/// The subject is the only thing separating this token from a storage token
/// minted from the same `auth_secret`, so it has to be the string
/// deis-telemetry uses: `deis_telemetry::auth::TELEMETRY_CHANNEL`. The two
/// copies exist because this crate does not depend on that one -- a node would
/// otherwise link the collector's whole server, its database and its HTTP
/// stack -- and `tests/telemetry_auth_test.rs`, which does depend on it as a
/// test-only dependency, is what stops them drifting apart: it pushes through
/// this credential to a real deis-telemetry that requires one.
const TELEMETRY_CHANNEL: &str = "deis-telemetry";

/// The token this node presents on every telemetry call, or nothing when no
/// secret is configured.
///
/// Cheap to clone, and deliberately shared by the three channels: logs,
/// metrics and traces go to one service over one credential, and three
/// derivations would be three places for the subject to be spelled wrong.
#[derive(Clone, Default)]
pub struct TelemetryCredential {
    /// Parsed once, when the shipper starts, so a secret that cannot be sent
    /// as gRPC metadata is a startup complaint rather than a failure on every
    /// push for the life of the process.
    token: Option<MetadataValue<Ascii>>,
}

impl TelemetryCredential {
    /// Derive the credential from the configured secret.
    ///
    /// A blank secret counts as none: an empty header would be presented as a
    /// token and refused, where sending nothing at least works against a
    /// collector that requires no proof -- which is the single-box default and
    /// has to keep working untouched.
    pub fn from_secret(secret: Option<&str>) -> Self {
        let Some(secret) = secret.map(str::trim).filter(|secret| !secret.is_empty()) else {
            return Self { token: None };
        };
        let derived = storage_token(secret, TELEMETRY_CHANNEL);
        match derived.parse::<MetadataValue<Ascii>>() {
            Ok(token) => Self { token: Some(token) },
            // The derivation is base64 of an HMAC, so this cannot happen from
            // a secret of any shape; it is said out loud rather than silently
            // dropped because the consequence of shipping without the token is
            // that everything this node observes is refused.
            Err(_) => {
                tracing::error!(
                    header = STORAGE_TOKEN_HEADER,
                    "the telemetry capability token derived from logging.auth_secret cannot be \
                     sent as gRPC metadata, so this node will ship without one. Against a \
                     deis-telemetry that has an auth_secret, every log line, trace and metric \
                     sample from this node is refused"
                );
                Self { token: None }
            }
        }
    }

    /// Whether this node has a credential at all.
    ///
    /// Reported once when a shipper starts, so the startup log says which of
    /// the two regimes is in force rather than leaving it to be worked out
    /// from an empty `deis logs`.
    pub fn is_present(&self) -> bool {
        self.token.is_some()
    }

    /// Wrap one message in a request carrying the token.
    ///
    /// The same shape `runtime::bindings::StorageIdentity::stamp` has, and
    /// stamped here rather than in each call for the same reason: a push that
    /// forgot it would be refused, retried, and refused again for as long as
    /// the node ran.
    pub fn stamp<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        if let Some(token) = &self.token {
            request
                .metadata_mut()
                .insert(STORAGE_TOKEN_HEADER, token.clone());
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
                "token",
                &if self.token.is_some() {
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
            header = STORAGE_TOKEN_HEADER,
            "This node presents deis-telemetry's capability token on every log, trace and metric \
             push. If deis-telemetry answers unauthenticated, the two no longer share the same \
             auth_secret"
        );
    } else {
        tracing::warn!(
            %endpoint,
            "No logging.auth_secret is configured, so this node ships to deis-telemetry without \
             a capability token. That works only against a collector that has no auth_secret \
             either -- which it may only have when both of its listeners are on loopback. \
             Against any other collector every log line, every invocation trace and every metric \
             sample from this node is refused, the node retries them for as long as it runs, and \
             `deis logs` and `deis trace` stay empty for every application it serves"
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
    fn the_token_is_the_channels_and_not_a_tenants() {
        let credential = TelemetryCredential::from_secret(Some("shared-secret"));
        assert!(credential.is_present());

        let stamped = credential.stamp(());
        let presented = stamped
            .metadata()
            .get(STORAGE_TOKEN_HEADER)
            .and_then(|value| value.to_str().ok())
            .expect("the token travels in the platform's own credential header");

        assert_eq!(
            presented,
            storage_token("shared-secret", TELEMETRY_CHANNEL),
            "the collector recomputes the token over its own subject; anything else is refused"
        );
        assert_ne!(
            presented,
            storage_token("shared-secret", "demo.production"),
            "a token minted for a tenant must not also be one for the telemetry channel, or a \
             token that leaked out of one application would read every application's logs"
        );
    }

    #[test]
    fn a_node_without_a_secret_stamps_nothing() {
        let stamped = TelemetryCredential::from_secret(None).stamp(());
        assert!(
            stamped.metadata().get(STORAGE_TOKEN_HEADER).is_none(),
            "an empty token is presented and refused; no token at all is served by a collector \
             that requires none"
        );
    }
}
