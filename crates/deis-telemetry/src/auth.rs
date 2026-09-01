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
//! What a caller now presents is a capability token,
//! `base64(HMAC-SHA256(auth_secret, "deis-telemetry"))`, where the subject is
//! [`TELEMETRY_CHANNEL`]. It travels in
//! [`STORAGE_TOKEN_HEADER`](deis_wire::conventions::STORAGE_TOKEN_HEADER) on
//! a gRPC call, or as `Authorization: Bearer <token>` on a scrape - the form
//! a Prometheus scrape configuration can already produce.
//!
//! It is a capability, not a session: it does not expire and it carries no
//! identity beyond "the holder may use this service". Rotating `auth_secret`
//! invalidates it at once, which is the lever an operator has.
//!
//! # Why one token and not one per tenant
//!
//! deis-store and deis-queue derive the token from *the tenant the request
//! itself claims*, so a token that leaked out of one application authorises
//! nothing for another. That works there because every storage call is about
//! exactly one tenant. Here it would not describe anything real:
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
//! So this token authenticates *the channel*, not the tenant: it says the
//! caller is part of this platform. It closes the hole above - a stranger who
//! cannot reach the secret can neither read nor forge - and it deliberately
//! does not partition one application's telemetry from another's. Anything
//! already inside the platform can still name any `app_id`. Making that
//! boundary real needs a per-application credential on every producer and a
//! scoped read path, which is a change to the wire contract of every
//! component that ships telemetry, not a check that can be added here.
//!
//! The derivation is [`storage_token`] and the header is the platform's
//! existing one, because a second HMAC scheme and a second header name would
//! be two more things for six binaries to disagree about, and a disagreement
//! here shows up as every call from every node being refused.
//!
//! # What an unconfigured service does
//!
//! Two cases, and they are deliberately not the same:
//!
//! * **Both ports on loopback with no secret** keeps working, untouched. That
//!   is the default single-box install, where deis-runtime and deis-telemetry
//!   share a host and neither port is reachable from anywhere else. It is not
//!   silent: [`TelemetryAuth::for_listeners`] logs one warning at startup
//!   naming exactly what is and is not protecting the data.
//! * **Either port on any other address with no secret** refuses to start.
//!   Unconfigured must not quietly mean unguarded, and a non-loopback bind is
//!   the moment the containment that stood in for authentication stops
//!   existing. This is the same shape as deis-store and deis-queue, and as
//!   the node channel on deis-control where an empty key list shuts the
//!   channel rather than opening it. Both ports count, because both hand out
//!   other people's data: the gRPC port serves logs and traces, and the
//!   Prometheus port serves every application's metric series.
//!
//! # Where the check runs
//!
//! Once per surface, outside the handlers. On gRPC it is a tonic interceptor
//! applied in [`crate::server`] to all three services at once, so no ingest
//! or query path can forget it and a fourth service added later cannot become
//! a fourth unguarded door - see `server::guarded`. On the scrape port it is
//! one layer over the whole router in [`crate::http`], for the same reason.
//!
//! The gRPC health service is deliberately left open: it reports whether this
//! process can answer, names no application and reads no data, and a liveness
//! probe that needs a credential is a probe the control plane cannot make.

use std::net::SocketAddr;
use std::sync::Arc;

use deis_wire::conventions::{STORAGE_TOKEN_HEADER, storage_token, storage_token_matches};
use tonic::{Request, Status};

use crate::config::Config;

/// What the capability token for this service is derived over.
///
/// A fixed string rather than a tenant, because a telemetry call is not about
/// one tenant - see the module documentation. It names the channel so that a
/// token minted for deis-telemetry is not also a token for deis-store: both
/// are derived from the same `auth_secret`, and the subject is the only thing
/// telling them apart.
pub const TELEMETRY_CHANNEL: &str = "deis-telemetry";

/// The header a scrape may carry its token in instead of
/// [`STORAGE_TOKEN_HEADER`], because that is the one a Prometheus
/// `bearer_token` setting produces.
const AUTHORIZATION_HEADER: &str = "authorization";

/// The scheme prefix on an `Authorization` value, compared case-insensitively
/// the way every HTTP implementation does.
const BEARER_PREFIX: &str = "bearer ";

/// What the process checks on every call.
///
/// Cheap to clone: the secret is shared, so one instance guards the three
/// gRPC services and the scrape endpoint.
#[derive(Clone)]
pub struct TelemetryAuth {
    /// The expected token, derived once at startup. `None` means no secret is
    /// configured, which is only reachable with both ports on loopback -
    /// [`TelemetryAuth::for_listeners`] refuses to build one otherwise.
    ///
    /// The expected token is kept rather than the secret: it is all the check
    /// needs, and a process that never holds the secret after startup cannot
    /// leak it into a log line or a core dump.
    expected: Option<Arc<String>>,
}

impl TelemetryAuth {
    /// Decide what guards this service, or refuse to serve at all.
    ///
    /// Called once, from [`crate::server::start`], with the addresses actually
    /// bound rather than the ones in the file: `listen` may name a hostname,
    /// and it is what the sockets resolved to that decides who can reach them.
    ///
    /// This is also where the security posture of the process is written to
    /// the log, once, so that the first lines of a startup log say which of
    /// the two regimes is in force.
    pub fn for_listeners(
        config: &Config,
        grpc_addr: SocketAddr,
        metrics_addr: SocketAddr,
    ) -> anyhow::Result<Self> {
        let secret = config
            .auth_secret
            .as_deref()
            .map(str::trim)
            .filter(|secret| !secret.is_empty());

        let reachable: Vec<SocketAddr> = [grpc_addr, metrics_addr]
            .into_iter()
            .filter(|addr| !addr.ip().is_loopback())
            .collect();

        match (secret, reachable.as_slice()) {
            (Some(secret), reachable) => {
                tracing::info!(
                    %grpc_addr,
                    %metrics_addr,
                    header = STORAGE_TOKEN_HEADER,
                    subject = TELEMETRY_CHANNEL,
                    reachable_from_other_hosts = reachable.len(),
                    "every log, trace and metric call, and every scrape, must present the \
                     capability token for this service; a call without it is refused before any \
                     data is read or written. The token proves the caller is part of this \
                     platform, not which application it is: anything holding it may still name \
                     any app_id"
                );
                Ok(Self {
                    expected: Some(Arc::new(storage_token(secret, TELEMETRY_CHANNEL))),
                })
            }
            (None, []) => {
                // The default single-box install. Kept working on purpose,
                // but never silently: this line is the whole of the trust
                // model, and an operator has to be able to find it.
                tracing::warn!(
                    %grpc_addr,
                    %metrics_addr,
                    "no auth_secret is configured, so callers are not authenticated at all: \
                     anything that can reach these ports may read every application's logs, \
                     traces and metric series, and may push entries carrying any app_id it \
                     likes. What protects the data is only that both addresses are unreachable \
                     from another host. Set auth_secret here before either is bound anywhere else"
                );
                Ok(Self { expected: None })
            }
            (None, reachable) => {
                let named: Vec<String> = reachable.iter().map(SocketAddr::to_string).collect();
                let named = named.join(", ");
                tracing::error!(
                    exposed = %named,
                    %grpc_addr,
                    %metrics_addr,
                    "refusing to start: {} address(es) here are reachable from other hosts and \
                     no auth_secret is configured, so anything that can route to them could read \
                     every application's logs, traces and metrics and forge entries against any \
                     app_id. Set auth_secret, or bind loopback addresses",
                    reachable.len()
                );
                Err(anyhow::anyhow!(
                    "{named} is not a loopback address and no auth_secret is configured; set \
                     auth_secret or bind a loopback address"
                ))
            }
        }
    }

    /// A service that requires tokens derived from `secret`.
    pub fn with_secret(secret: impl AsRef<str>) -> Self {
        Self {
            expected: Some(Arc::new(storage_token(secret.as_ref(), TELEMETRY_CHANNEL))),
        }
    }

    /// Whether a token is required. Reported at startup so the log says which
    /// regime is in force rather than leaving it to be inferred.
    pub fn is_enforcing(&self) -> bool {
        self.expected.is_some()
    }

    /// Whether `presented` is the token this service accepts.
    ///
    /// The token itself is never logged by any caller of this: it is the
    /// credential, and a log that carries it is a log that grants access to
    /// whoever reads it.
    fn accepts(&self, presented: Option<&str>) -> bool {
        let Some(expected) = self.expected.as_deref() else {
            return true;
        };
        presented.is_some_and(|presented| storage_token_matches(expected, presented))
    }

    /// Refuse a gRPC call that does not carry the token.
    ///
    /// `Status` is a large error variant, which clippy flags. It is the type
    /// `tonic::service::Interceptor` is defined to return, so introducing an
    /// error enum here would only add a conversion on the way back out to the
    /// same `Status`.
    #[allow(clippy::result_large_err)]
    pub fn check<T>(&self, request: &Request<T>) -> Result<(), Status> {
        if !self.is_enforcing() {
            return Ok(());
        }
        let presented = request
            .metadata()
            .get(STORAGE_TOKEN_HEADER)
            .and_then(|value| value.to_str().ok());
        if self.accepts(presented) {
            return Ok(());
        }
        tracing::warn!(
            header = STORAGE_TOKEN_HEADER,
            carried_a_token = presented.is_some(),
            "refused a telemetry call that did not present this service's capability token; \
             either the caller is not part of this platform, or it and deis-telemetry no longer \
             share the same auth_secret - in which case every call from every node is being \
             refused and nothing is being recorded"
        );
        Err(Status::unauthenticated(format!(
            "a valid {STORAGE_TOKEN_HEADER} is required"
        )))
    }

    /// Whether a scrape may be served.
    ///
    /// Two accepted forms because two kinds of caller ask: a platform
    /// component already speaks [`STORAGE_TOKEN_HEADER`], while a Prometheus
    /// server can only be configured to send `Authorization: Bearer`.
    pub fn check_scrape(&self, headers: &axum::http::HeaderMap) -> bool {
        if !self.is_enforcing() {
            return true;
        }
        let direct = headers
            .get(STORAGE_TOKEN_HEADER)
            .and_then(|value| value.to_str().ok());
        if self.accepts(direct) {
            return true;
        }
        let bearer = headers
            .get(AUTHORIZATION_HEADER)
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() > BEARER_PREFIX.len())
            .filter(|value| value[..BEARER_PREFIX.len()].eq_ignore_ascii_case(BEARER_PREFIX))
            .map(|value| value[BEARER_PREFIX.len()..].trim());
        if self.accepts(bearer) {
            return true;
        }
        tracing::warn!(
            carried_a_token = direct.is_some() || bearer.is_some(),
            "refused a Prometheus scrape that did not present this service's capability token; \
             until it does, this endpoint reports nothing and every series on it will show a gap"
        );
        false
    }
}

/// Redacted on purpose. A derived `Debug` would put the expected token into
/// every panic message and every log line that formats a service, which is
/// exactly the place a credential must never appear.
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
        self.check(&request)?;
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};
    use tonic::service::Interceptor;

    fn addr(text: &str) -> SocketAddr {
        text.parse().expect("test address must parse")
    }

    fn call(token: Option<&str>) -> Request<()> {
        let mut request = Request::new(());
        if let Some(token) = token {
            request.metadata_mut().insert(
                STORAGE_TOKEN_HEADER,
                token.parse().expect("a token is valid metadata"),
            );
        }
        request
    }

    fn scrape(header: &'static str, value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header,
            HeaderValue::from_str(value).expect("a token is a valid header value"),
        );
        headers
    }

    #[test]
    fn a_non_loopback_bind_with_no_secret_refuses_to_start() {
        // The whole point of failing closed: an operator who widens `listen`
        // and nothing else must not end up publishing every application's
        // logs, traces and metrics.
        for text in ["0.0.0.0:8092", "192.168.1.10:8092", "[::]:8092"] {
            let error = match TelemetryAuth::for_listeners(
                &Config::default(),
                addr(text),
                addr("127.0.0.1:8093"),
            ) {
                Ok(_) => panic!("{text} must not be served unauthenticated"),
                Err(e) => e,
            };
            assert!(
                format!("{error:#}").contains("auth_secret"),
                "the refusal must name the missing setting: {error:#}"
            );
        }
    }

    #[test]
    fn a_public_scrape_port_with_no_secret_also_refuses_to_start() {
        // The scrape port hands out every application's metric series, so a
        // guard that only looked at the gRPC port would leave half the data
        // published and call the service protected.
        let error = TelemetryAuth::for_listeners(
            &Config::default(),
            addr("127.0.0.1:8092"),
            addr("0.0.0.0:8093"),
        )
        .expect_err("a public scrape port with no secret must not be served");
        assert!(format!("{error:#}").contains("0.0.0.0:8093"), "{error:#}");
    }

    #[test]
    fn a_non_loopback_bind_with_a_secret_serves() {
        let config = Config {
            auth_secret: Some("shared".to_string()),
            ..Config::default()
        };
        let auth =
            TelemetryAuth::for_listeners(&config, addr("0.0.0.0:8092"), addr("0.0.0.0:8093"))
                .expect("a configured secret is what makes a public bind serveable");
        assert!(auth.is_enforcing());
    }

    #[test]
    fn loopback_with_no_secret_keeps_working() {
        // The default single-box install. It stays unauthenticated on
        // purpose; the startup warning is what makes that visible.
        for text in ["127.0.0.1:8092", "[::1]:8092"] {
            let auth =
                TelemetryAuth::for_listeners(&Config::default(), addr(text), addr("127.0.0.1:0"))
                    .expect("loopback with no secret is the supported default");
            assert!(!auth.is_enforcing());
            assert!(auth.check(&call(None)).is_ok());
            assert!(auth.check_scrape(&HeaderMap::new()));
        }
    }

    #[test]
    fn a_blank_secret_is_not_a_secret() {
        // A key left as an empty string in a config file is an operator who
        // meant to fill it in, not one who chose to run without it.
        for blank in ["", "   "] {
            let config = Config {
                auth_secret: Some(blank.to_string()),
                ..Config::default()
            };
            assert!(
                TelemetryAuth::for_listeners(&config, addr("0.0.0.0:8092"), addr("0.0.0.0:8093"))
                    .is_err()
            );
            let auth = TelemetryAuth::for_listeners(
                &config,
                addr("127.0.0.1:8092"),
                addr("127.0.0.1:8093"),
            )
            .expect("loopback stays serveable");
            assert!(!auth.is_enforcing());
        }
    }

    #[test]
    fn a_matching_token_is_accepted_and_anything_else_is_not() {
        let auth = TelemetryAuth::with_secret("shared");
        let token = storage_token("shared", TELEMETRY_CHANNEL);
        assert!(auth.check(&call(Some(&token))).is_ok());

        for wrong in [
            String::new(),
            "not-a-token".to_string(),
            format!("{token}x"),
            token[..10].to_string(),
        ] {
            assert!(
                auth.check(&call(Some(&wrong))).is_err(),
                "{wrong:?} must not be accepted"
            );
        }
        assert!(auth.check(&call(None)).is_err());
    }

    #[test]
    fn a_token_minted_under_another_secret_is_refused() {
        // What a half-finished rotation looks like: this service has the new
        // secret, whatever mints tokens is still handing out the old one.
        let auth = TelemetryAuth::with_secret("new-secret");
        let stale = storage_token("old-secret", TELEMETRY_CHANNEL);
        assert!(auth.check(&call(Some(&stale))).is_err());
    }

    #[test]
    fn a_storage_token_for_a_tenant_does_not_open_this_service() {
        // Both are derived from one auth_secret, so the subject is the only
        // thing keeping them apart: a token handed to an application for its
        // own storage must not also let it read the whole platform's logs.
        let auth = TelemetryAuth::with_secret("shared");
        let tenants = storage_token("shared", "shop.production");
        assert!(auth.check(&call(Some(&tenants))).is_err());
    }

    #[test]
    fn a_scrape_may_present_its_token_either_way() {
        let auth = TelemetryAuth::with_secret("shared");
        let token = storage_token("shared", TELEMETRY_CHANNEL);
        assert!(auth.check_scrape(&scrape(STORAGE_TOKEN_HEADER, &token)));
        assert!(auth.check_scrape(&scrape(AUTHORIZATION_HEADER, &format!("Bearer {token}"))));
        // The scheme is compared the way HTTP compares it.
        assert!(auth.check_scrape(&scrape(AUTHORIZATION_HEADER, &format!("bearer {token}"))));

        assert!(!auth.check_scrape(&HeaderMap::new()));
        assert!(!auth.check_scrape(&scrape(AUTHORIZATION_HEADER, &token)));
        assert!(!auth.check_scrape(&scrape(AUTHORIZATION_HEADER, "Bearer ")));
        assert!(!auth.check_scrape(&scrape(AUTHORIZATION_HEADER, "Basic aGk6dGhlcmU=")));
        assert!(!auth.check_scrape(&scrape(STORAGE_TOKEN_HEADER, "not-a-token")));
    }

    #[test]
    fn the_interceptor_refuses_with_an_unauthenticated_status() {
        // This is the form the server actually applies, so the status a
        // caller sees is checked here rather than inferred from `check`.
        let mut auth = TelemetryAuth::with_secret("shared");
        let status = match Interceptor::call(&mut auth, call(None)) {
            Ok(_) => panic!("an unsigned call must be refused"),
            Err(status) => status,
        };
        assert_eq!(status.code(), tonic::Code::Unauthenticated);

        let token = storage_token("shared", TELEMETRY_CHANNEL);
        assert!(
            Interceptor::call(&mut auth, call(Some(&token))).is_ok(),
            "a correctly signed call must pass through"
        );
    }

    #[test]
    fn the_credential_never_reaches_a_debug_line() {
        let auth = TelemetryAuth::with_secret("shared");
        let rendered = format!("{auth:?}");
        assert!(!rendered.contains(&storage_token("shared", TELEMETRY_CHANNEL)));
        assert!(!rendered.contains("shared"));
        assert!(rendered.contains("enforcing: true"));
    }
}
