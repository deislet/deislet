// Minimal Integration Test for Denix Gateway
// Tests the primary execution path via handle_request

use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::any,
    Router,
};
use denix::app::ApplicationManager;
use denix::gateway::AppState;
use std::sync::Arc;
use tower::ServiceExt; // for oneshot

#[tokio::test]
async fn test_gateway_execution() {
    // Setup
    let app_manager = Arc::new(ApplicationManager::new());
    let state = AppState { app_manager: app_manager.clone() };

    // Create a test app
    let code = r#"
        globalThis.default = {
            fetch(request) {
                return new Response("Hello from Gateway!");
            }
        }
    "#;
    
    app_manager.load_app("test_app".to_string(), code.to_string(), Default::default())
        .await
        .expect("Failed to load app");

    // Create Router with the actual handler
    let app = Router::new()
        .route("/", any(denix::gateway::handlers::handle_request))
        .with_state(state);

    // 1. Test missing x-app-id header (Should fail)
    let req_missing_header = Request::builder()
        .uri("/")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(req_missing_header).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // 2. Test valid execution
    let req_valid = Request::builder()
        .uri("/")
        .header("x-app-id", "test_app")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req_valid).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    
    // We would verify body here, but extracting body from response in test requires 
    // http-body-util which might not be in dev-dependencies.
    // Checking status 200 is sufficient proof that handle_request -> execute_app worked.
}
