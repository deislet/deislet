//! Who is allowed to call what.
//!
//! Two separate credentials, deliberately never merged:
//!
//! * [`AuthConfig`] is the dashboard and human API key. It is governed by
//!   `api_auth_enabled`, which defaults to off.
//! * [`NodeAuthConfig`] is the credential a runtime node already shares with
//!   the control plane to register (`node_preshared_keys`). It guards the two
//!   endpoints that exist for nodes, and `api_auth_enabled` has no say over
//!   it: one of those endpoints hands out decrypted secrets, and a flag whose
//!   purpose is to make a dashboard convenient must not be able to switch
//!   authentication off on that channel.

use axum::{
    Json,
    extract::Request,
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::sync::Arc;
use tracing::warn;

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
/// `GET /api/nodes/:id/assignments` and the bundle download.
///
/// It is `node_preshared_keys` - the same list a node authenticates against
/// when it registers over gRPC (see [`crate::handler::MessageHandler`]), so a
/// node has one credential and not two.
///
/// An empty list means this control plane has no node credential at all. The
/// node endpoints then still answer, because a fleet configured that way is
/// how the platform has always run, but the assignment list refuses to carry
/// decrypted secrets over a channel where nobody can prove anything - see
/// [`Self::is_configured`] and `api::catalog::assignments`.
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

    /// Whether a caller of a node endpoint can prove it is a node.
    ///
    /// False means every caller on that channel is anonymous, which is what
    /// decides that no secret value may travel over it.
    pub fn is_configured(&self) -> bool {
        !self.keys.is_empty()
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
/// these two values are the whole authorisation of the node channel.
fn secret_eq(a: &str, b: &str) -> bool {
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

/// Authentication for the endpoints that exist for runtime nodes.
///
/// Runs whatever `api_auth_enabled` says, because the assignment list carries
/// every environment's decrypted secrets: an operator turning the dashboard's
/// key requirement off must not thereby publish the platform's secrets to
/// anyone who can reach this port.
///
/// With no `node_preshared_keys` configured there is nothing to check, so the
/// request passes and the handler withholds secret values instead; refusing
/// outright would take down every node of a fleet that never had a key.
pub async fn node_auth(
    node_auth: axum::extract::State<NodeAuthConfig>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    if !node_auth.is_configured() {
        return next.run(request).await;
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
        assert!(node_auth_config.is_configured());

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

    /// With no node key configured the endpoint still answers - the
    /// handler is what withholds the secrets - or every node of a fleet
    /// that never had a key would stop being able to load anything.
    #[tokio::test]
    async fn a_control_plane_without_a_node_key_still_serves_the_node_endpoints() {
        use axum::Router;
        use axum::routing::get;

        async fn assignments() -> &'static str {
            "OK"
        }

        let node_auth_config = NodeAuthConfig::new(vec!["   ".to_string()]);
        assert!(
            !node_auth_config.is_configured(),
            "a blank entry in the file is not a credential"
        );

        let app = Router::new()
            .route("/api/nodes/a/assignments", get(assignments))
            .layer(axum::middleware::from_fn_with_state(
                node_auth_config,
                node_auth,
            ));

        let response = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/nodes/a/assignments")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
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
