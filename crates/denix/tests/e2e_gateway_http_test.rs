use axum::routing::any;
use axum::Router;
use denix::app::ApplicationManager;
use denix::dispatcher::{handlers, AppState};
use std::sync::Arc;

#[tokio::test]
async fn test_gateway_http_e2e() {
    let app_manager = Arc::new(ApplicationManager::new());
    let state = AppState {
        app_manager: app_manager.clone(),
    };

    let code = r#"
        globalThis.default = {
            fetch(request) {
                const url = new URL(request.url);
                const name = url.searchParams.get("name") || "world";
                return new Response("Hello " + name);
            }
        }
    "#;

    app_manager
        .load_app("e2e_app".to_string(), code.to_string(), Default::default())
        .await
        .expect("Failed to load app");

    let app = Router::new()
        .route("/*path", any(handlers::handle_request))
        .route("/", any(handlers::handle_request))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind listener");
    let addr = listener.local_addr().expect("Failed to get addr");

    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("Server failed");
    });

    tokio::task::yield_now().await;

    let client = reqwest::Client::new();
    let url = format!("http://{}/hello?name=denix", addr);
    let response = client
        .get(url)
        .header("x-app-id", "e2e_app")
        .send()
        .await
        .expect("Request failed");

    assert!(response.status().is_success());
    let body = response.text().await.expect("Read body failed");
    assert_eq!(body, "Hello denix");

    server.abort();
    let _ = server.await;
}
