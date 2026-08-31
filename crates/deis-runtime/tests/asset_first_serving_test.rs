//! Asset-first serving, end to end over HTTP.
//!
//! A version is taken delivery of the way the control plane delivers one - a
//! URL and a digest, fetched by the node - and then the same requests a
//! browser would make are sent through the real HTTP entry. What this proves
//! that a unit test cannot: a path that names a bundled file is answered from
//! the bundle without the isolate ever running, and a path that does not still
//! reaches the isolate.

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use deis_runtime::app::ApplicationManager;
use deis_runtime::config::RuntimeConfig;
use deis_runtime::deployment::{sha256_hex, DeploymentManager, DeploymentOrder};
use deis_runtime::dispatcher::{handlers, AppState};

/// The handler is only reached on an asset miss, and says so.
const HANDLER: &str = r#"
export default function handler(context) {
  return new Response("the isolate ran", { status: 200 });
}
"#;

const INDEX_HTML: &str = "<!doctype html><h1>from the bundle</h1>";
const APP_CSS: &str = "body { margin: 0 }";

fn bundle() -> (Vec<u8>, String) {
    let manifest = serde_json::json!({
        "schema": 1,
        "app": "site",
        "entry": "worker/index.js",
        "runtime": "standard-v1",
        "routes": [{ "pattern": "/api/hello", "methods": [] }],
        "assets": [
            {
                "path": "index.html",
                "sha256": sha256_hex(INDEX_HTML.as_bytes()),
                "content_type": "text/html; charset=utf-8",
                "size": INDEX_HTML.len(),
            },
            {
                "path": "css/app.css",
                "sha256": sha256_hex(APP_CSS.as_bytes()),
                "size": APP_CSS.len(),
            },
        ],
    });

    let mut buf = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::FileOptions::default();
        writer.start_file("manifest.json", options).unwrap();
        writer.write_all(manifest.to_string().as_bytes()).unwrap();
        writer.start_file("worker/index.js", options).unwrap();
        writer.write_all(HANDLER.as_bytes()).unwrap();
        writer.start_file("assets/index.html", options).unwrap();
        writer.write_all(INDEX_HTML.as_bytes()).unwrap();
        writer.start_file("assets/css/app.css", options).unwrap();
        writer.write_all(APP_CSS.as_bytes()).unwrap();
        writer.finish().unwrap();
    }

    let sha = sha256_hex(&buf);
    (buf, sha)
}

/// Serves the bundle, standing in for the control plane's bundle endpoint.
async fn serve_bundle(bytes: Vec<u8>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let state = Arc::new(bytes);
    let router = Router::new()
        .route(
            "/bundle",
            get(|State(state): State<Arc<Vec<u8>>>| async move {
                Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from(state.as_ref().clone()))
                    .unwrap()
                    .into_response()
            }),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (addr, task)
}

#[tokio::test]
async fn a_bundled_site_is_served_from_its_assets_and_falls_through_on_a_miss() {
    let dir = tempfile::tempdir().unwrap();
    let (bytes, sha) = bundle();
    let (bundle_addr, _bundle_task) = serve_bundle(bytes).await;

    let app_manager = Arc::new(ApplicationManager::new());
    let config = RuntimeConfig {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    };
    let deployer = DeploymentManager::from_config(app_manager.clone(), &config)
        .await
        .unwrap();

    deployer
        .apply(DeploymentOrder {
            app_id: "site".to_string(),
            version_id: "v1".to_string(),
            environment: "production".to_string(),
            bundle_url: format!("http://{bundle_addr}/bundle"),
            bundle_sha: sha,
            env: HashMap::new(),
            limits: None,
        })
        .await
        .expect("the deployment should have been applied");

    let router = Router::new()
        .route("/*path", any(handlers::handle_request))
        .route("/", any(handlers::handle_request))
        .with_state(AppState::new(
            app_manager.clone(),
            deis_runtime::config::default_max_request_body_bytes(),
        ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let client = reqwest::Client::new();
    let get = |path: &str| {
        client
            .get(format!("http://{addr}{path}"))
            .header("x-deis-app-id", "site")
    };

    // The root resolves to index.html, with a strong ETag and the node's
    // cache policy.
    let response = get("/").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/html; charset=utf-8"
    );
    assert_eq!(
        response.headers()["cache-control"],
        "public, max-age=0, must-revalidate"
    );
    let etag = response.headers()["etag"].to_str().unwrap().to_string();
    assert_eq!(etag, format!("\"{}\"", sha256_hex(INDEX_HTML.as_bytes())));
    assert_eq!(response.text().await.unwrap(), INDEX_HTML);

    // A second visit with the tag the browser kept costs no body.
    let response = get("/")
        .header("if-none-match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert!(response.text().await.unwrap().is_empty());

    // A nested file is served with the type guessed from its extension,
    // because the manifest declared none for it.
    let response = get("/css/app.css").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/css; charset=utf-8"
    );
    assert_eq!(response.text().await.unwrap(), APP_CSS);

    // A path with no asset behind it is the isolate's job.
    let response = get("/api/hello").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "the isolate ran");

    // A POST to a path that happens to name a static file is a route the app
    // may implement, so it must not be swallowed by the asset index.
    let response = client
        .post(format!("http://{addr}/index.html"))
        .header("x-deis-app-id", "site")
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "the isolate ran");
}
