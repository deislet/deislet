//! Who is allowed to call what.
//!
//! Two separate credentials, deliberately never merged:
//!
//! * The human API - dashboard and CLI - is guarded by
//!   [`crate::api::authz::authorize`], which resolves an account, checks the
//!   scope the route needs and writes the audit line. `api_auth_enabled`
//!   decides one thing about it - whether a caller has to present a
//!   credential at all - and it still defaults to off; [`AuthConfig`] is what
//!   the startup banner prints about it.
//! * [`NodeAuthConfig`] is the credential a runtime node already shares with
//!   the control plane to register (`node_preshared_keys`). It guards the
//!   endpoints that exist for nodes, and `api_auth_enabled` has no say over
//!   it: one of those endpoints hands out decrypted secrets, and a flag whose
//!   purpose is to make a dashboard convenient must not be able to switch
//!   authentication off on that channel.
//!
//! Neither credential has an "unconfigured means open" state, and each
//! reaches that by the means its channel allows.
//!
//! For the node channel it is the whole point: an empty key list refuses
//! every caller, and [`crate::config::ControlConfig::ensure_node_credential`]
//! mints a key before the server starts so the refusal is not what an
//! operator meets on a first run.
//!
//! For the human API, `api_auth_enabled = false` means no credential is
//! required - not that the surface is published. A caller who presents
//! nothing is admitted only over a direct connection from the machine the
//! control plane runs on. A caller from any other host, through a reverse
//! proxy, or from a web page on another origin is refused; a credential that
//! *is* presented is held to its scopes whatever the flag says. The default
//! `bind_addr` is `0.0.0.0` and cannot be narrowed the way deis-queue narrows
//! its own - this listener is where every node fetches its assignments and its
//! bundles - so the peer and proxy markers carry the rule. See
//! [`crate::api::authz::authorize`].

use axum::{
    Json,
    extract::Request,
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::sync::Arc;
use tracing::{error, warn};

use crate::config::ControlConfig;
use crate::node_key::{NodeKeyFingerprint, ProvedNodeKey};

/// What the configuration file says about the human API's credential.
///
/// It no longer decides anything by itself: [`crate::api::authz::authorize`]
/// is the gate, and it resolves an account and a scope list. This is the
/// summary the startup banner prints - whether authentication is demanded at
/// all, and how many flat `api_keys` entries the file still carries.
#[derive(Clone)]
pub struct AuthConfig {
    pub enabled: bool,
    pub api_keys: Arc<Vec<String>>,
}

impl AuthConfig {
    pub fn from_control_config(config: &ControlConfig) -> Self {
        Self {
            enabled: config.api_auth_enabled,
            api_keys: Arc::new(config.api_keys.clone()),
        }
    }
}

/// The credential a runtime node presents on the endpoints that serve nodes:
/// `PUT /api/nodes/:id/registration`, `GET /api/nodes/:id/assignments` and
/// the bundle download.
///
/// It is `node_preshared_keys` - the same list a node authenticates against
/// when it opens its gRPC control channel - so a node has one credential and
/// not two.
///
/// An empty list is not a mode. It means this process has no way to tell a
/// node from anyone else, so [`node_auth`] answers every caller with 503 and
/// nothing behind it runs. The binary never gets there:
/// [`crate::config::ControlConfig::ensure_node_credential`] mints a key at
/// startup when the file names none.
#[derive(Clone)]
pub struct NodeAuthConfig {
    /// Each configured key with its fingerprint, computed once: the
    /// fingerprint is what a registration records about the key it proved,
    /// so that the control plane later drives that node with the same value
    /// and not with whichever one happens to be first in the list.
    keys: Arc<Vec<(String, NodeKeyFingerprint)>>,
}

impl NodeAuthConfig {
    pub fn from_control_config(config: &ControlConfig) -> Self {
        Self::new(config.node_preshared_keys.clone())
    }

    /// Blank entries are dropped and the rest are trimmed, which is the same
    /// reading [`crate::runtime_client::RuntimeNodeManager::with_node_keys`]
    /// gives the list, so a key proved here is a key that can be presented
    /// back.
    pub fn new(keys: Vec<String>) -> Self {
        Self {
            keys: Arc::new(
                keys.iter()
                    .map(|k| k.trim())
                    .filter(|k| !k.is_empty())
                    .map(|k| (k.to_string(), NodeKeyFingerprint::of(k)))
                    .collect(),
            ),
        }
    }

    /// How many keys this channel accepts. Zero means the channel is shut,
    /// not that it is open; it is reported at startup so an operator sees
    /// which of the two credentials is which.
    pub fn key_count(&self) -> usize {
        self.keys.len()
    }

    /// Whether this credential is one of the configured node keys.
    pub fn accepts(&self, presented: &str) -> bool {
        self.accepted(presented).is_some()
    }

    /// Which configured key this credential is, if any.
    ///
    /// The whole list is walked whatever matches, so the time taken says
    /// nothing about which entry, if any, was the right one.
    pub fn accepted(&self, presented: &str) -> Option<NodeKeyFingerprint> {
        let presented = presented.trim();
        let mut found = None;
        for (key, fingerprint) in self.keys.iter() {
            if secret_eq(key, presented) && found.is_none() {
                found = Some(fingerprint.clone());
            }
        }
        found
    }
}

/// Compare a presented credential with a configured one in time that does
/// not depend on how many leading bytes matched.
///
/// The difference is small over a LAN, but it costs nothing to remove, and
/// these two values are the whole authorisation of the node channel - the
/// gRPC handshake included, which is why this is not private to this module.
pub(crate) fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The credentials a request carries, in the two forms this API accepts:
/// `Authorization: Bearer <key>` (or the bare key) and `X-API-Key: <key>`.
///
/// Spelled once so the node channel and the human API read a request the
/// same way; only the list they check against differs.
pub(crate) fn presented_credentials(headers: &HeaderMap) -> Vec<&str> {
    let mut found = Vec::with_capacity(2);
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        found.push(value.strip_prefix("Bearer ").unwrap_or(value));
    }
    if let Some(value) = headers.get("X-API-Key").and_then(|v| v.to_str().ok()) {
        found.push(value);
    }
    found
}

/// Authentication for the endpoints that exist for runtime nodes.
///
/// Runs whatever `api_auth_enabled` says, because the assignment list carries
/// every environment's decrypted secrets: an operator turning the dashboard's
/// key requirement off must not thereby publish the platform's secrets to
/// anyone who can reach this port.
///
/// With no key configured there is nothing a caller could present, so the
/// channel is shut rather than opened. Passing the request through instead
/// would mean anyone who can reach this port can register a node - and a node
/// on the fleet list is a delivery target that is handed every environment's
/// decrypted secrets on its first assignment fetch.
///
/// The key the caller proved travels on to the handler as a
/// [`ProvedNodeKey`] extension - its fingerprint, never the value. The
/// registration handler records it, so that this control plane afterwards
/// drives the registered address with the credential it actually held: during
/// a rotation the list carries two values, and an address that proved the
/// retiring one must not be handed its replacement.
pub async fn node_auth(
    node_auth: axum::extract::State<NodeAuthConfig>,
    headers: HeaderMap,
    mut request: Request,
    next: Next,
) -> Response {
    if node_auth.key_count() == 0 {
        error!(
            path = %request.uri().path(),
            "Refused a call to a node endpoint because this control plane has no node credential at all; no node can register, fetch assignments or download a bundle until node_preshared_keys names one"
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "No node credential configured",
                "details": "This control plane has no node_preshared_keys, so it cannot tell a node from any other caller and serves none of them. Set node_preshared_keys on the control plane and on every node."
            })),
        )
            .into_response();
    }

    if let Some(proved) = presented_credentials(&headers)
        .into_iter()
        .find_map(|credential| node_auth.accepted(credential))
    {
        request.extensions_mut().insert(ProvedNodeKey(proved));
        return next.run(request).await;
    }

    warn!(
        path = %request.uri().path(),
        "Refused a call to a node endpoint that presented no valid node key; the caller gets nothing, and a node that cannot authenticate keeps serving whatever it already had instead of picking up new versions"
    );
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "Unauthorized",
            "details": "This endpoint serves runtime nodes. Provide one of the control plane's node_preshared_keys via 'Authorization: Bearer <key>' or 'X-API-Key: <key>'."
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request as HttpRequest, StatusCode},
    };
    use tower::ServiceExt;

    #[test]
    fn test_auth_config_from_control_config() {
        let config = crate::config::ControlConfig {
            api_auth_enabled: true,
            api_keys: vec!["test-key-123".to_string()],
            ..Default::default()
        };

        let auth_config = AuthConfig::from_control_config(&config);
        assert!(auth_config.enabled);
        assert_eq!(auth_config.api_keys.len(), 1);
        assert_eq!(auth_config.api_keys[0], "test-key-123");
    }

    #[test]
    fn test_auth_config_disabled_by_default() {
        let config = crate::config::ControlConfig::default();
        let auth_config = AuthConfig::from_control_config(&config);
        assert!(!auth_config.enabled);
    }

    /// The node channel must not follow `api_auth_enabled`: that flag is
    /// about the dashboard, and this channel hands out decrypted secrets.
    #[tokio::test]
    async fn a_node_endpoint_stays_authenticated_when_the_api_key_flag_is_off() {
        use axum::Router;
        use axum::routing::get;

        async fn assignments() -> &'static str {
            "OK"
        }

        // Exactly the shape of a default control plane: no API key
        // requirement, but a node key configured.
        let config = crate::config::ControlConfig {
            api_auth_enabled: false,
            node_preshared_keys: vec!["node-psk".to_string()],
            ..Default::default()
        };
        let node_auth_config = NodeAuthConfig::from_control_config(&config);
        assert_eq!(node_auth_config.key_count(), 1);

        let app = Router::new()
            .route("/api/nodes/a/assignments", get(assignments))
            .layer(axum::middleware::from_fn_with_state(
                node_auth_config,
                node_auth,
            ));

        let anonymous = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/nodes/a/assignments")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

        // A human API key is not a node credential: the dashboard is not
        // allowed to read secrets back (architecture section 8).
        let wrong = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/nodes/a/assignments")
                    .header("Authorization", "Bearer some-dashboard-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        for header in ["Authorization", "X-API-Key"] {
            let value = if header == "Authorization" {
                "Bearer node-psk"
            } else {
                "node-psk"
            };
            let response = app
                .clone()
                .oneshot(
                    HttpRequest::builder()
                        .uri("/api/nodes/a/assignments")
                        .header(header, value)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{header} was refused");
        }
    }

    /// "Unconfigured" must not mean "unguarded". With no node key the
    /// channel is shut, not open: registering a node this way would put a
    /// machine of the caller's choosing on the fleet list, and the fleet
    /// list is what receives deployments, cron fires and decrypted secrets.
    #[tokio::test]
    async fn a_control_plane_without_a_node_key_serves_no_node_endpoint_at_all() {
        use axum::Router;
        use axum::routing::{get, put};

        async fn ok() -> &'static str {
            "OK"
        }

        let node_auth_config = NodeAuthConfig::new(vec!["   ".to_string()]);
        assert_eq!(
            node_auth_config.key_count(),
            0,
            "a blank entry in the file is not a credential"
        );

        let app = Router::new()
            .route("/api/nodes/a/assignments", get(ok))
            .route("/api/nodes/a/registration", put(ok))
            .layer(axum::middleware::from_fn_with_state(
                node_auth_config,
                node_auth,
            ));

        for (method, uri) in [
            ("GET", "/api/nodes/a/assignments"),
            ("PUT", "/api/nodes/a/registration"),
        ] {
            // Anonymous, and with a guess at a credential: neither gets in,
            // and the answer says the control plane is the thing that is
            // misconfigured rather than the caller.
            for credential in [None, Some("anything")] {
                let mut builder = HttpRequest::builder().method(method).uri(uri);
                if let Some(credential) = credential {
                    builder = builder.header("Authorization", format!("Bearer {credential}"));
                }
                let response = app
                    .clone()
                    .oneshot(builder.body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::SERVICE_UNAVAILABLE,
                    "{method} {uri} was served without any node credential being configured"
                );
            }
        }
    }

    /// The handler behind this layer records which key the caller proved,
    /// so the layer has to say which one it was - by fingerprint, and the
    /// right one when the list carries two values at once.
    #[test]
    fn the_accepted_key_is_named_by_its_fingerprint() {
        let config = NodeAuthConfig::new(vec!["new-key".to_string(), " old-key ".to_string()]);
        assert_eq!(config.key_count(), 2);
        assert_eq!(
            config.accepted("old-key"),
            Some(NodeKeyFingerprint::of("old-key")),
            "the second value is proved as itself, not as the first"
        );
        assert_eq!(
            config.accepted("new-key"),
            Some(NodeKeyFingerprint::of("new-key"))
        );
        assert_eq!(config.accepted("other"), None);
        assert!(!config.accepts(""));
    }

    /// The proved key reaches the handler as an extension, and only when
    /// something was proved: a refused request runs no handler at all.
    #[tokio::test]
    async fn the_proved_key_travels_to_the_handler() {
        use axum::{Extension, Router, routing::get};

        async fn which(Extension(proved): Extension<ProvedNodeKey>) -> String {
            proved.0.short()
        }

        let app = Router::new()
            .route("/api/nodes/a/registration", get(which))
            .layer(axum::middleware::from_fn_with_state(
                NodeAuthConfig::new(vec!["new-key".to_string(), "old-key".to_string()]),
                node_auth,
            ));

        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/nodes/a/registration")
                    .header("Authorization", "Bearer old-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&body),
            NodeKeyFingerprint::of("old-key").short()
        );
    }

    #[test]
    fn a_credential_comparison_rejects_a_prefix_and_a_longer_value() {
        assert!(secret_eq("abc", "abc"));
        assert!(!secret_eq("abc", "ab"));
        assert!(!secret_eq("abc", "abcd"));
        assert!(!secret_eq("abc", "abd"));
    }
}
