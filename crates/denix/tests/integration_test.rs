// 集成测试
// 测试完整的请求流程

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use denix::app::ApplicationManager;
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt; // for `oneshot` and `ready`

#[tokio::test]
async fn test_health_endpoint() {
    let app_manager = Arc::new(ApplicationManager::new());
    let app = create_test_app(app_manager).await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["status"], "healthy");
}

#[tokio::test]
async fn test_create_and_execute_app() {
    let app_manager = Arc::new(ApplicationManager::new());
    let app = create_test_app(app_manager).await;

    // Create app
    let create_request = Request::builder()
        .method("POST")
        .uri("/api/apps")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "id": "test_app",
                "code": r#"
                    globalThis.default = {
                        fetch(request) {
                            return new Response("Integration test response");
                        }
                    }
                "#
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.clone().oneshot(create_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // Execute app
    let exec_request = Request::builder()
        .uri("/apps/test_app")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(exec_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body_str = String::from_utf8(body.to_vec()).unwrap();

    assert_eq!(body_str, "Integration test response");
}

#[tokio::test]
async fn test_list_apps() {
    let app_manager = Arc::new(ApplicationManager::new());

    // Load some apps
    let code = r#"
        globalThis.default = {
            fetch(request) {
                return new Response("test");
            }
        }
    "#;

    app_manager
        .load_app("app1".to_string(), code.to_string(), Default::default())
        .await
        .unwrap();

    app_manager
        .load_app("app2".to_string(), code.to_string(), Default::default())
        .await
        .unwrap();

    let app = create_test_app(app_manager).await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/apps")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["apps"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn test_update_app() {
    let app_manager = Arc::new(ApplicationManager::new());

    // Create initial app
    let code1 = r#"
        globalThis.default = {
            fetch(request) {
                return new Response("version 1");
            }
        }
    "#;

    app_manager
        .load_app("update_test".to_string(), code1.to_string(), Default::default())
        .await
        .unwrap();

    let app = create_test_app(app_manager).await;

    // Update app
    let update_request = Request::builder()
        .method("PUT")
        .uri("/api/apps/update_test")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "code": r#"
                    globalThis.default = {
                        fetch(request) {
                            return new Response("version 2");
                        }
                    }
                "#
            })
            .to_string(),
        ))
        .unwrap();

    let response = app.clone().oneshot(update_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Execute updated app
    let exec_request = Request::builder()
        .uri("/apps/update_test")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(exec_request).await.unwrap();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body_str = String::from_utf8(body.to_vec()).unwrap();

    assert_eq!(body_str, "version 2");
}

#[tokio::test]
async fn test_delete_app() {
    let app_manager = Arc::new(ApplicationManager::new());

    let code = r#"
        globalThis.default = {
            fetch(request) {
                return new Response("test");
            }
        }
    "#;

    app_manager
        .load_app("del_test".to_string(), code.to_string(), Default::default())
        .await
        .unwrap();

    let app = create_test_app(app_manager).await;

    // Delete app
    let delete_request = Request::builder()
        .method("DELETE")
        .uri("/api/apps/del_test")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(delete_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Try to execute deleted app
    let exec_request = Request::builder()
        .uri("/apps/del_test")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(exec_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_app_with_path() {
    let app_manager = Arc::new(ApplicationManager::new());
    let app = create_test_app(app_manager.clone()).await;

    // Create app that uses path
    let create_request = Request::builder()
        .method("POST")
        .uri("/api/apps")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "id": "path_test",
                "code": r#"
                    globalThis.default = {
                        fetch(request) {
                            const url = new URL(request.url);
                            return new Response(`Path: ${url.pathname}`);
                        }
                    }
                "#
            })
            .to_string(),
        ))
        .unwrap();

    app.clone().oneshot(create_request).await.unwrap();

    // Execute with path
    let exec_request = Request::builder()
        .uri("/apps/path_test/some/nested/path")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(exec_request).await.unwrap();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body_str = String::from_utf8(body.to_vec()).unwrap();

    assert!(body_str.contains("/some/nested/path"));
}

// Helper function to create test app
async fn create_test_app(
    app_manager: Arc<ApplicationManager>,
) -> axum::Router {
    use axum::{
        routing::{delete, get, post, put},
        Router,
    };
    use denix::gateway::{handlers, AppState};

    let state = AppState { app_manager };

    Router::new()
        .route("/", get(handlers::handle_root))
        .route("/health", get(handlers::handle_health))
        .route("/api/apps", post(handlers::handle_create_app))
        .route("/api/apps", get(handlers::handle_list_apps))
        .route("/api/apps/:id", get(handlers::handle_get_app))
        .route("/api/apps/:id", put(handlers::handle_update_app))
        .route("/api/apps/:id", delete(handlers::handle_delete_app))
        .route("/apps/:id", get(handlers::handle_app_request))
        .route("/apps/:id/*path", get(handlers::handle_app_request))
        .with_state(state)
}
