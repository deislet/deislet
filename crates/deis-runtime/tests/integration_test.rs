// Minimal Integration Test for Runtime Gateway
// Tests the primary execution path via handle_request

use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::any,
    Router,
};
use deis_runtime::app::{AppKey, ApplicationManager};
use deis_runtime::dispatcher::AppState;
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::ServiceExt; // for oneshot

#[tokio::test]
async fn test_gateway_execution() {
    // Setup
    let app_manager = Arc::new(ApplicationManager::new());
    let state = AppState {
        app_manager: app_manager.clone(),
    };

    // Create a test app
    let code = r#"
        export default function handler(context) {
            return new Response("Hello from Gateway!");
        }
    "#;

    app_manager
        .load_app(
            AppKey::production("test_app"),
            code.to_string(),
            Default::default(),
            None,
        )
        .await
        .expect("Failed to load app");

    // Create Router with the actual handler
    let app = Router::new()
        .route("/", any(deis_runtime::dispatcher::handlers::handle_request))
        .with_state(state);

    // 1. Test missing x-deis-app-id header (Should fail)
    let req_missing_header = Request::builder().uri("/").body(Body::empty()).unwrap();

    let response = app.clone().oneshot(req_missing_header).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // 2. Test valid execution
    let req_valid = Request::builder()
        .uri("/")
        .header("x-deis-app-id", "test_app")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req_valid).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = std::str::from_utf8(&body_bytes).unwrap();
    assert_eq!(body, "Hello from Gateway!");
}

/// A request says which application it is for and, optionally, which
/// environment of it. Two environments of one application are two running
/// copies, so the pair has to decide which one answers - and a request that
/// names no environment must still reach production.
#[tokio::test]
async fn the_environment_header_picks_which_copy_of_an_app_answers() {
    let app_manager = Arc::new(ApplicationManager::new());
    let state = AppState {
        app_manager: app_manager.clone(),
    };

    for (environment, body) in [
        ("production", "from production"),
        ("preview-9", "from preview"),
    ] {
        let code = format!(
            r#"
            export default function handler(context) {{
                return new Response("{body}");
            }}
            "#
        );
        app_manager
            .load_app(
                AppKey::new("routed", environment),
                code,
                Default::default(),
                None,
            )
            .await
            .expect("Failed to load app");
    }

    let app = Router::new()
        .route("/", any(deis_runtime::dispatcher::handlers::handle_request))
        .with_state(state);

    let answer = |environment: Option<&'static str>| {
        let app = app.clone();
        async move {
            let mut builder = Request::builder()
                .uri("/")
                .header("x-deis-app-id", "routed");
            if let Some(environment) = environment {
                builder = builder.header("x-deis-environment", environment);
            }
            let response = app
                .oneshot(builder.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            (status, String::from_utf8(body.to_vec()).unwrap())
        }
    };

    let (status, body) = answer(Some("preview-9")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "from preview");

    let (status, body) = answer(Some("production")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "from production");

    // No environment header at all: an older caller reaches production rather
    // than being refused.
    let (status, body) = answer(None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "from production");

    // A name that could climb out of a directory - the pair becomes a path on
    // this node and a storage tenant - is refused before anything uses it.
    let (status, _body) = answer(Some("../production")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
