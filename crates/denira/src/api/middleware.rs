use axum::{
    extract::Request,
    http::{StatusCode, HeaderMap},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::sync::Arc;

use crate::config::SupervisorConfig;

/// API authentication middleware state
#[derive(Clone)]
pub struct AuthConfig {
    pub enabled: bool,
    pub api_keys: Arc<Vec<String>>,
}

impl AuthConfig {
    pub fn from_supervisor_config(config: &SupervisorConfig) -> Self {
        Self {
            enabled: config.api_auth_enabled,
            api_keys: Arc::new(config.api_keys.clone()),
        }
    }
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

    // Check Authorization header
    if let Some(auth_header) = headers.get("Authorization") {
        if let Ok(auth_str) = auth_header.to_str() {
            // Support two formats:
            // 1. "Bearer <api_key>"
            // 2. "<api_key>" (direct)
            let api_key = if let Some(key) = auth_str.strip_prefix("Bearer ") {
                key
            } else {
                auth_str
            };

            // Verify API Key
            if auth_config.api_keys.contains(&api_key.to_string()) {
                return next.run(request).await;
            }
        }
    }

    // Check X-API-Key header (fallback)
    if let Some(api_key_header) = headers.get("X-API-Key") {
        if let Ok(api_key) = api_key_header.to_str() {
            if auth_config.api_keys.contains(&api_key.to_string()) {
                return next.run(request).await;
            }
        }
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
    use axum::{body::Body, http::{Request as HttpRequest, StatusCode}};
    use tower::ServiceExt;

    #[test]
    fn test_auth_config_from_supervisor_config() {
        let mut config = crate::config::SupervisorConfig::default();
        config.api_auth_enabled = true;
        config.api_keys = vec!["test-key-123".to_string()];

        let auth_config = AuthConfig::from_supervisor_config(&config);
        assert!(auth_config.enabled);
        assert_eq!(auth_config.api_keys.len(), 1);
        assert_eq!(auth_config.api_keys[0], "test-key-123");
    }

    #[test]
    fn test_auth_config_disabled_by_default() {
        let config = crate::config::SupervisorConfig::default();
        let auth_config = AuthConfig::from_supervisor_config(&config);
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

        let app = Router::new()
            .route("/test", get(test_handler))
            .layer(axum::middleware::from_fn_with_state(
                auth_config,
                api_key_auth,
            ));

        // Send request without authentication
        let response = app
            .oneshot(HttpRequest::builder()
                .uri("/test")
                .body(Body::empty())
                .unwrap())
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

        let app = Router::new()
            .route("/test", get(test_handler))
            .layer(axum::middleware::from_fn_with_state(
                auth_config,
                api_key_auth,
            ));

        // Send request without API key
        let response = app
            .oneshot(HttpRequest::builder()
                .uri("/test")
                .body(Body::empty())
                .unwrap())
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

        let app = Router::new()
            .route("/test", get(test_handler))
            .layer(axum::middleware::from_fn_with_state(
                auth_config,
                api_key_auth,
            ));

        // Use Bearer token
        let response = app
            .oneshot(HttpRequest::builder()
                .uri("/test")
                .header("Authorization", "Bearer valid-key-123")
                .body(Body::empty())
                .unwrap())
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

        let app = Router::new()
            .route("/test", get(test_handler))
            .layer(axum::middleware::from_fn_with_state(
                auth_config,
                api_key_auth,
            ));

        // Use X-API-Key header
        let response = app
            .oneshot(HttpRequest::builder()
                .uri("/test")
                .header("X-API-Key", "valid-key-123")
                .body(Body::empty())
                .unwrap())
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

        let app = Router::new()
            .route("/test", get(test_handler))
            .layer(axum::middleware::from_fn_with_state(
                auth_config,
                api_key_auth,
            ));

        // Use invalid API key
        let response = app
            .oneshot(HttpRequest::builder()
                .uri("/test")
                .header("Authorization", "Bearer wrong-key")
                .body(Body::empty())
                .unwrap())
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

        let app = Router::new()
            .route("/health", get(health_handler))
            .layer(axum::middleware::from_fn_with_state(
                auth_config,
                api_key_auth,
            ));

        // /health endpoint does not require authentication
        let response = app
            .oneshot(HttpRequest::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap())
            .await
            .unwrap();

        // Should succeed (even without API key)
        assert_eq!(response.status(), StatusCode::OK);
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
            .oneshot(HttpRequest::builder()
                .uri("/api/metrics")
                .body(Body::empty())
                .unwrap())
            .await
            .unwrap();

        // Should succeed (even without API key)
        assert_eq!(response.status(), StatusCode::OK);
    }
}
