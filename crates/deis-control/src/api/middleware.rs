//! Who is allowed to call what.
//!
//! Two separate credentials, deliberately never merged:
//!
//! * [`AuthConfig`] is the dashboard and human API key. It is governed by
//!   `api_auth_enabled`, which defaults to off.
//! * [`NodeAuthConfig`] is the credential a runtime node already shares with
//!   the control plane to register (`node_preshared_keys`). It guards the
//!   endpoints that exist for nodes, and `api_auth_enabled` has no say over
//!   it: one of those endpoints hands out decrypted secrets, and a flag whose
//!   purpose is to make a dashboard convenient must not be able to switch
//!   authentication off on that channel.
//!
//! Neither credential has an "unconfigured means open" state. For the node
//! channel that is the whole point: an empty key list refuses every caller,
//! and [`crate::config::ControlConfig::ensure_node_credential`] mints a key
//! before the server starts so the refusal is not what an operator meets on
//! a first run.

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

/// API authentication middleware state
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
    keys: Arc<Vec<String>>,
}

impl NodeAuthConfig {
    pub fn from_control_config(config: &ControlConfig) -> Self {
        Self::new(config.node_preshared_keys.clone())
    }

    pub fn new(keys: Vec<String>) -> Self {
        Self {
            keys: Arc::new(keys.into_iter().filter(|k| !k.trim().is_empty()).collect()),
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
        self.keys.iter().any(|key| secret_eq(key, presented))
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
fn presented_credentials(headers: &HeaderMap) -> Vec<&str> {
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

/// How an audit line names the caller of a request.
///
/// This platform has no accounts, so there is no name to print. What there is
/// are two facts worth keeping: which credential was presented and where the
/// request came from. The credential is reduced to the first bytes of its
/// SHA-256 rather than printed - enough to tell two keys apart and to match a
/// line against a key an operator still holds, useless to anyone who reads
/// the log looking for a way in. Replace this with the account once accounts
/// exist; until then it is what an audit line has.
///
/// `peer` is `None` when the server was built without connect info, which is
/// why the caller passes an `Option` rather than this reaching for it: an
/// audit line that failed to render would be worse than one missing an
/// address.
pub fn caller_description(headers: &HeaderMap, peer: Option<std::net::SocketAddr>) -> String {
    let credential = match presented_credentials(headers).first() {
        Some(presented) => format!("key:{}", credential_fingerprint(presented)),
        None => "anonymous".to_string(),
    };
    match peer {
        Some(addr) => format!("{credential} from {addr}"),
        None => format!("{credential} from an unknown address"),
    }
}

/// A short, stable, non-reversible name for one credential.
fn credential_fingerprint(credential: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(credential.as_bytes());
    hex::encode(&digest[..4])
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
pub async fn node_auth(
    node_auth: axum::extract::State<NodeAuthConfig>,
    headers: HeaderMap,
    request: Request,
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

    if presented_credentials(&headers)
        .into_iter()
        .any(|credential| node_auth.accepts(credential))
    {
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

/// API Key authentication middleware
pub async fn api_key_auth(
    auth_config: axum::extract::State<AuthConfig>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    // Bypass if authentication is disabled
    if !auth_config.enabled {
        return next.run(request).await;
    }

    // Skip endpoints that don't require authentication
    // - /health: Health check
    // - /api/metrics: Prometheus Metrics (Prometheus usually doesn't support auth)
    let path = request.uri().path();
    if path == "/health" || path == "/api/metrics" {
        return next.run(request).await;
    }

    // "Authorization: Bearer <key>", a bare "Authorization: <key>", or
    // "X-API-Key: <key>" - all three are accepted.
    if presented_credentials(&headers)
        .into_iter()
        .any(|presented| {
            auth_config
                .api_keys
                .iter()
                .any(|key| secret_eq(key, presented))
        })
    {
        return next.run(request).await;
    }

    // Authentication failed
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "Unauthorized",
            "details": "Valid API key required. Provide it via 'Authorization: Bearer <key>' or 'X-API-Key: <key>' header."
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

    #[tokio::test]
    async fn test_auth_middleware_disabled() {
        use axum::Router;
        use axum::routing::get;

        // Create a simple route
        async fn test_handler() -> &'static str {
            "OK"
        }

        let auth_config = AuthConfig {
            enabled: false,
            api_keys: Arc::new(vec![]),
        };

        let app = Router::new().route("/test", get(test_handler)).layer(
            axum::middleware::from_fn_with_state(auth_config, api_key_auth),
        );

        // Send request without authentication
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should succeed (auth disabled)
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_auth_middleware_no_key() {
        use axum::Router;
        use axum::routing::get;

        async fn test_handler() -> &'static str {
            "OK"
        }

        let auth_config = AuthConfig {
            enabled: true,
            api_keys: Arc::new(vec!["valid-key-123".to_string()]),
        };

        let app = Router::new().route("/test", get(test_handler)).layer(
            axum::middleware::from_fn_with_state(auth_config, api_key_auth),
        );

        // Send request without API key
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should return 401
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_middleware_bearer_token() {
        use axum::Router;
        use axum::routing::get;

        async fn test_handler() -> &'static str {
            "OK"
        }

        let auth_config = AuthConfig {
            enabled: true,
            api_keys: Arc::new(vec!["valid-key-123".to_string()]),
        };

        let app = Router::new().route("/test", get(test_handler)).layer(
            axum::middleware::from_fn_with_state(auth_config, api_key_auth),
        );

        // Use Bearer token
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/test")
                    .header("Authorization", "Bearer valid-key-123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should succeed
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_auth_middleware_x_api_key() {
        use axum::Router;
        use axum::routing::get;

        async fn test_handler() -> &'static str {
            "OK"
        }

        let auth_config = AuthConfig {
            enabled: true,
            api_keys: Arc::new(vec!["valid-key-123".to_string()]),
        };

        let app = Router::new().route("/test", get(test_handler)).layer(
            axum::middleware::from_fn_with_state(auth_config, api_key_auth),
        );

        // Use X-API-Key header
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/test")
                    .header("X-API-Key", "valid-key-123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should succeed
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_auth_middleware_invalid_key() {
        use axum::Router;
        use axum::routing::get;

        async fn test_handler() -> &'static str {
            "OK"
        }

        let auth_config = AuthConfig {
            enabled: true,
            api_keys: Arc::new(vec!["valid-key-123".to_string()]),
        };

        let app = Router::new().route("/test", get(test_handler)).layer(
            axum::middleware::from_fn_with_state(auth_config, api_key_auth),
        );

        // Use invalid API key
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/test")
                    .header("Authorization", "Bearer wrong-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should return 401
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_health_endpoint_bypass() {
        use axum::Router;
        use axum::routing::get;

        async fn health_handler() -> &'static str {
            "OK"
        }

        let auth_config = AuthConfig {
            enabled: true,
            api_keys: Arc::new(vec!["valid-key-123".to_string()]),
        };

        let app = Router::new().route("/health", get(health_handler)).layer(
            axum::middleware::from_fn_with_state(auth_config, api_key_auth),
        );

        // /health endpoint does not require authentication
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should succeed (even without API key)
        assert_eq!(response.status(), StatusCode::OK);
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

    #[test]
    fn a_credential_comparison_rejects_a_prefix_and_a_longer_value() {
        assert!(secret_eq("abc", "abc"));
        assert!(!secret_eq("abc", "ab"));
        assert!(!secret_eq("abc", "abcd"));
        assert!(!secret_eq("abc", "abd"));
    }

    #[tokio::test]
    async fn test_metrics_endpoint_bypass() {
        use axum::Router;
        use axum::routing::get;

        async fn metrics_handler() -> &'static str {
            "OK"
        }

        let auth_config = AuthConfig {
            enabled: true,
            api_keys: Arc::new(vec!["valid-key-123".to_string()]),
        };

        let app = Router::new()
            .route("/api/metrics", get(metrics_handler))
            .layer(axum::middleware::from_fn_with_state(
                auth_config,
                api_key_auth,
            ));

        // /api/metrics endpoint does not require authentication (needed for Prometheus)
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Should succeed (even without API key)
        assert_eq!(response.status(), StatusCode::OK);
    }
}
