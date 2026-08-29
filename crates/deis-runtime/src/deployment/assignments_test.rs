use std::io::Write;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use axum::Router;

use super::*;
use crate::app::ApplicationManager;
use crate::config::RuntimeConfig;
use crate::deployment::sha256_hex;

const HANDLER: &str = "export default function handler() { return new Response('ok'); }";

fn bundle(app: &str) -> (Vec<u8>, String) {
    let manifest = serde_json::json!({
        "schema": 1,
        "app": app,
        "entry": "worker/index.js",
        "assets": [{
            "path": "index.html",
            "sha256": sha256_hex(b"<h1>hi</h1>"),
            "size": 11,
        }],
    });

    let mut buf = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::FileOptions::default();
        writer
            .start_file(deis_bundle::MANIFEST_ENTRY, options)
            .unwrap();
        writer.write_all(manifest.to_string().as_bytes()).unwrap();
        writer.start_file("worker/index.js", options).unwrap();
        writer.write_all(HANDLER.as_bytes()).unwrap();
        writer.start_file("assets/index.html", options).unwrap();
        writer.write_all(b"<h1>hi</h1>").unwrap();
        writer.finish().unwrap();
    }

    let sha = sha256_hex(&buf);
    (buf, sha)
}

/// A stand-in control plane that serves an assignment list and the bundle it
/// points at, exactly as deis-control does.
///
/// The listener is bound before the router is built so that the assignment
/// list can name this server's own address, which is what the node has to
/// download from.
struct ControlPlane {
    base: String,
    _task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct PlaneState {
    assignments: Arc<serde_json::Value>,
    bundle: Arc<Vec<u8>>,
    status: StatusCode,
}

impl ControlPlane {
    /// `entries` are `(app_id, version_id, bundle_path)`; every one of them is
    /// pointed at this server's `bundle_path`, so a path other than `/bundle`
    /// stands for a URL that does not resolve.
    async fn serving(
        entries: &[(&str, &str, &str)],
        bundle: Vec<u8>,
        bundle_sha: &str,
        status: StatusCode,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");

        let assignments = serde_json::Value::Array(
            entries
                .iter()
                .map(|(app_id, version_id, path)| {
                    serde_json::json!({
                        "app_id": app_id,
                        "version_id": version_id,
                        "environment": "production",
                        "bundle_url": format!("{base}{path}"),
                        "bundle_sha": bundle_sha,
                    })
                })
                .collect(),
        );

        let state = PlaneState {
            assignments: Arc::new(assignments),
            bundle: Arc::new(bundle),
            status,
        };
        let router = Router::new()
            .route(
                "/api/nodes/:id/assignments",
                get(
                    |State(state): State<PlaneState>, AxumPath(_id): AxumPath<String>| async move {
                        if state.status != StatusCode::OK {
                            return state.status.into_response();
                        }
                        Json(state.assignments.as_ref().clone()).into_response()
                    },
                ),
            )
            .route(
                "/bundle",
                get(|State(state): State<PlaneState>| async move {
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(state.bundle.as_ref().clone()))
                        .unwrap()
                        .into_response()
                }),
            )
            .with_state(state);

        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { base, _task: task }
    }

    fn base(&self) -> &str {
        &self.base
    }
}

async fn deployer(dir: &std::path::Path, apps: Arc<ApplicationManager>) -> Arc<DeploymentManager> {
    let mut config = RuntimeConfig {
        data_dir: dir.to_path_buf(),
        ..Default::default()
    };
    config.bundles.dir = Some(dir.join("bundles"));
    Arc::new(DeploymentManager::from_config(apps, &config).await.unwrap())
}

fn client(base: &str, node_id: &str) -> AssignmentClient {
    AssignmentClient::new(base, node_id, Duration::from_secs(10)).unwrap()
}

#[tokio::test]
async fn a_node_loads_everything_the_control_plane_assigned_it() {
    let dir = tempfile::tempdir().unwrap();
    let (bytes, sha) = bundle("demo");
    let plane =
        ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha, StatusCode::OK).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let outcome = sync_once(&client(plane.base(), "node-a"), &deployer)
        .await
        .unwrap();

    assert_eq!(outcome.assigned, 1);
    assert_eq!(outcome.installed, 1);
    assert_eq!(outcome.failed, 0);
    assert_eq!(apps.installed_version("demo").unwrap().version_id, "v1");
    assert_eq!(apps.assets("demo").unwrap().len(), 1);
}

#[test]
fn an_assignment_carries_the_environment_a_push_would_have() {
    // Before the list carried this, a node that restarted came back running
    // the right code with no secrets and no storage addresses, and stayed
    // that way until somebody deployed again.
    let assignment: Assignment = serde_json::from_value(serde_json::json!({
        "app_id": "demo",
        "version_id": "v1",
        "environment": "production",
        "bundle_url": "http://control/bundle",
        "bundle_sha": "abc",
        "env": { "API_KEY": "s3cr3t", "DEIS_STORE_ENDPOINT": "http://127.0.0.1:8090" },
    }))
    .unwrap();

    let order = assignment.into_order();
    assert_eq!(order.env.get("API_KEY").unwrap(), "s3cr3t");
    assert_eq!(
        order.env.get(crate::runtime::STORE_ENDPOINT_KEY).unwrap(),
        "http://127.0.0.1:8090"
    );
}

#[test]
fn an_assignment_without_an_environment_still_loads() {
    // A control plane that predates the field must not stop a node dead; the
    // app runs on its manifest's own variables until the next push.
    let assignment: Assignment = serde_json::from_value(serde_json::json!({
        "app_id": "demo",
        "version_id": "v1",
        "environment": "production",
        "bundle_url": "http://control/bundle",
        "bundle_sha": "abc",
    }))
    .unwrap();

    assert!(assignment.into_order().env.is_empty());
}

#[test]
fn debugging_an_assignment_never_prints_a_secret() {
    let assignment: Assignment = serde_json::from_value(serde_json::json!({
        "app_id": "demo",
        "version_id": "v1",
        "environment": "production",
        "bundle_url": "http://control/bundle",
        "bundle_sha": "abc",
        "env": { "API_KEY": "s3cr3t-value" },
    }))
    .unwrap();

    let rendered = format!("{assignment:?}");
    assert!(rendered.contains("API_KEY"), "{rendered}");
    assert!(!rendered.contains("s3cr3t-value"), "{rendered}");
}

#[tokio::test]
async fn an_empty_assignment_list_is_a_valid_answer() {
    let dir = tempfile::tempdir().unwrap();
    let plane = ControlPlane::serving(&[], Vec::new(), "", StatusCode::OK).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps).await;
    let outcome = sync_once(&client(plane.base(), "node-a"), &deployer)
        .await
        .unwrap();

    assert_eq!(outcome, SyncOutcome::default());
}

#[tokio::test]
async fn one_unusable_assignment_does_not_stop_the_others() {
    let dir = tempfile::tempdir().unwrap();
    let (bytes, sha) = bundle("demo");
    let plane = ControlPlane::serving(
        &[("broken", "v1", "/nowhere"), ("demo", "v1", "/bundle")],
        bytes,
        &sha,
        StatusCode::OK,
    )
    .await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let outcome = sync_once(&client(plane.base(), "node-a"), &deployer)
        .await
        .unwrap();

    assert_eq!(outcome.failed, 1);
    assert_eq!(outcome.installed, 1);
    assert!(apps.installed_version("demo").is_some());
    assert!(apps.installed_version("broken").is_none());
}

#[tokio::test]
async fn a_control_plane_that_refuses_is_reported_rather_than_treated_as_empty() {
    let dir = tempfile::tempdir().unwrap();
    let plane = ControlPlane::serving(&[], Vec::new(), "", StatusCode::INTERNAL_SERVER_ERROR).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps).await;
    let err = sync_once(&client(plane.base(), "node-a"), &deployer)
        .await
        .unwrap_err();

    assert!(
        matches!(err, AssignmentError::Status { code: 500, .. }),
        "{err}"
    );
}

#[tokio::test]
async fn a_control_plane_that_is_not_there_is_a_transport_failure() {
    let dir = tempfile::tempdir().unwrap();
    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps).await;

    let err = sync_once(&client("http://127.0.0.1:0", "node-a"), &deployer)
        .await
        .unwrap_err();

    assert!(matches!(err, AssignmentError::Transport { .. }), "{err}");
}

#[test]
fn the_node_id_is_escaped_into_the_path_it_addresses() {
    let client = client("http://control:8080/", "node/../other");
    assert_eq!(
        client.url(),
        "http://control:8080/api/nodes/node%2F..%2Fother/assignments"
    );
}

#[tokio::test]
async fn without_a_control_plane_the_node_is_ready_on_what_it_has() {
    let dir = tempfile::tempdir().unwrap();
    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps).await;
    let health = crate::health::NodeHealth::starting();

    run_startup_sync(None, deployer, health.clone(), Duration::from_secs(1)).await;

    assert!(health.is_healthy());
}

#[test]
fn retries_back_off_but_never_stop() {
    let base = Duration::from_secs(15);
    assert_eq!(retry_delay(base, 1), base);
    assert_eq!(retry_delay(base, 2), Duration::from_secs(30));
    assert_eq!(retry_delay(base, 5), Duration::from_secs(240));
    // Capped, so a long outage still gets a retry every few minutes rather
    // than drifting into hours.
    assert_eq!(retry_delay(base, 40), Duration::from_secs(300));
}
