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
use crate::app::{AppKey, ApplicationManager};
use crate::config::RuntimeConfig;
use crate::deployment::sha256_hex;
use crate::deployment::NodeRegistration;

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
    /// The list this plane serves. Behind a lock so a test can change what
    /// the control plane says between two reconciliations, which is the whole
    /// point of reconciling.
    assignments: Arc<parking_lot::Mutex<serde_json::Value>>,
    /// Whether the plane claims its answer is the whole truth, and how many
    /// assignments it says it left out. Real deis-control sends both headers
    /// on every answer.
    completeness: Arc<parking_lot::Mutex<(bool, u32)>>,
    /// One permit per time the assignments route has been entered, so a test
    /// can wait for the request to be in flight without a timer.
    asked: Arc<tokio::sync::Semaphore>,
    /// The assignments route answers only while this is true. Starts true, so
    /// a test that does not care about the timing never notices it.
    open: tokio::sync::watch::Sender<bool>,
    _task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct PlaneState {
    assignments: Arc<parking_lot::Mutex<serde_json::Value>>,
    completeness: Arc<parking_lot::Mutex<(bool, u32)>>,
    bundle: Arc<Vec<u8>>,
    status: StatusCode,
    /// When set, both routes answer 401 unless the request presents it, the
    /// way deis-control does once `node_preshared_keys` is configured.
    required_key: Option<String>,
    asked: Arc<tokio::sync::Semaphore>,
    open: tokio::sync::watch::Receiver<bool>,
}

impl PlaneState {
    fn authorised(&self, headers: &axum::http::HeaderMap) -> bool {
        let Some(required) = &self.required_key else {
            return true;
        };
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == format!("Bearer {required}"))
    }
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
        Self::guarded(entries, bundle, bundle_sha, status, None).await
    }

    /// The same, but demanding a node key on both routes.
    async fn guarded(
        entries: &[(&str, &str, &str)],
        bundle: Vec<u8>,
        bundle_sha: &str,
        status: StatusCode,
        required_key: Option<&str>,
    ) -> Self {
        let required_key = required_key.map(str::to_string);
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

        let assignments = Arc::new(parking_lot::Mutex::new(assignments));
        let completeness = Arc::new(parking_lot::Mutex::new((true, 0u32)));
        let asked = Arc::new(tokio::sync::Semaphore::new(0));
        let (open, open_rx) = tokio::sync::watch::channel(true);
        let state = PlaneState {
            assignments: assignments.clone(),
            completeness: completeness.clone(),
            bundle: Arc::new(bundle),
            status,
            required_key,
            asked: asked.clone(),
            open: open_rx,
        };
        let router = Router::new()
            .route(
                "/api/nodes/:id/assignments",
                get(
                    |State(state): State<PlaneState>,
                     AxumPath(_id): AxumPath<String>,
                     headers: axum::http::HeaderMap| async move {
                        if !state.authorised(&headers) {
                            return StatusCode::UNAUTHORIZED.into_response();
                        }
                        state.asked.add_permits(1);
                        let mut open = state.open.clone();
                        while !*open.borrow_and_update() {
                            if open.changed().await.is_err() {
                                break;
                            }
                        }
                        if state.status != StatusCode::OK {
                            return state.status.into_response();
                        }
                        let (complete, withheld) = *state.completeness.lock();
                        let mut response = Json(state.assignments.lock().clone()).into_response();
                        let headers = response.headers_mut();
                        headers.insert(
                            "x-deis-assignments-complete",
                            if complete { "true" } else { "false" }.parse().unwrap(),
                        );
                        headers.insert(
                            "x-deis-assignments-withheld",
                            withheld.to_string().parse().unwrap(),
                        );
                        response
                    },
                ),
            )
            .route(
                "/bundle",
                get(
                    |State(state): State<PlaneState>, headers: axum::http::HeaderMap| async move {
                        if !state.authorised(&headers) {
                            return StatusCode::UNAUTHORIZED.into_response();
                        }
                        Response::builder()
                            .status(StatusCode::OK)
                            .body(Body::from(state.bundle.as_ref().clone()))
                            .unwrap()
                            .into_response()
                    },
                ),
            )
            .with_state(state);

        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            base,
            assignments,
            completeness,
            asked,
            open,
            _task: task,
        }
    }

    fn base(&self) -> &str {
        &self.base
    }

    /// Replace the list this plane serves, as a deploy, a rollback or a
    /// deletion in the catalog would.
    fn now_assigns(&self, assignments: serde_json::Value) {
        *self.assignments.lock() = assignments;
    }

    /// Say the next answer is missing `withheld` assignments this control
    /// plane holds but could not describe - a secret that will not decrypt,
    /// say.
    fn answers_incompletely(&self, withheld: u32) {
        *self.completeness.lock() = (false, withheld);
    }

    /// Answer completely again, as a control plane does once whatever stopped
    /// it describing an assignment is fixed.
    fn answers_completely(&self) {
        *self.completeness.lock() = (true, 0);
    }

    /// Hold the assignments answer open until [`Self::answers_now`], so a test
    /// can make something happen on the node while the request is in flight.
    fn holds_its_answer(&self) {
        self.open.send(false).unwrap();
    }

    /// Wait until the assignments route has been entered. No timer: the route
    /// hands out a permit as it is entered.
    async fn was_asked(&self) {
        self.asked.acquire().await.unwrap().forget();
    }

    /// Release an answer held by [`Self::holds_its_answer`].
    fn answers_now(&self) {
        self.open.send(true).unwrap();
    }

    /// One assignment line pointing at this plane's own bundle route.
    fn line(
        &self,
        app_id: &str,
        environment: &str,
        version_id: &str,
        sha: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "app_id": app_id,
            "version_id": version_id,
            "environment": environment,
            "bundle_url": format!("{}/bundle", self.base),
            "bundle_sha": sha,
        })
    }
}

async fn deployer(dir: &std::path::Path, apps: Arc<ApplicationManager>) -> Arc<DeploymentManager> {
    deployer_for(dir, apps, None, None).await
}

/// A deployment manager configured the way a node's TOML would configure it,
/// so the bundle download presents the same credential the real one does.
async fn deployer_for(
    dir: &std::path::Path,
    apps: Arc<ApplicationManager>,
    endpoint: Option<&str>,
    secret: Option<&str>,
) -> Arc<DeploymentManager> {
    let mut config = RuntimeConfig {
        data_dir: dir.to_path_buf(),
        ..Default::default()
    };
    config.bundles.dir = Some(dir.join("bundles"));
    config.control.endpoint = endpoint.map(str::to_string);
    config.control.secret = secret.map(str::to_string);
    Arc::new(DeploymentManager::from_config(apps, &config).await.unwrap())
}

fn client(base: &str, node_id: &str) -> AssignmentClient {
    AssignmentClient::new(base, node_id, Duration::from_secs(10), None).unwrap()
}

#[tokio::test]
async fn a_node_loads_everything_the_control_plane_assigned_it() {
    let dir = tempfile::tempdir().unwrap();
    let (bytes, sha) = bundle("demo");
    let plane =
        ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha, StatusCode::OK).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(plane.base(), "node-a");
    let outcome = Reconciler::new(&client, &deployer).once().await.unwrap();

    assert_eq!(outcome.assigned, 1);
    assert_eq!(outcome.installed, 1);
    assert_eq!(outcome.failed, 0);
    let demo = AppKey::production("demo");
    assert_eq!(apps.installed_version(&demo).unwrap().version_id, "v1");
    assert_eq!(apps.assets(&demo).unwrap().len(), 1);
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
    let client = client(plane.base(), "node-a");
    let outcome = Reconciler::new(&client, &deployer).once().await.unwrap();

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
    let client = client(plane.base(), "node-a");
    let outcome = Reconciler::new(&client, &deployer).once().await.unwrap();

    assert_eq!(outcome.failed, 1);
    assert_eq!(outcome.installed, 1);
    assert!(apps
        .installed_version(&AppKey::production("demo"))
        .is_some());
    assert!(apps
        .installed_version(&AppKey::production("broken"))
        .is_none());
}

#[tokio::test]
async fn a_control_plane_that_refuses_is_reported_rather_than_treated_as_empty() {
    let dir = tempfile::tempdir().unwrap();
    let plane = ControlPlane::serving(&[], Vec::new(), "", StatusCode::INTERNAL_SERVER_ERROR).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps).await;
    let client = client(plane.base(), "node-a");
    let err = Reconciler::new(&client, &deployer)
        .once()
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

    let unreachable = client("http://127.0.0.1:0", "node-a");
    let err = Reconciler::new(&unreachable, &deployer)
        .once()
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

    run_control_loop(
        None,
        None,
        deployer,
        health.clone(),
        Duration::from_secs(1),
        Duration::from_secs(60),
    )
    .await;

    assert!(health.is_healthy());
}

/// A node with no control plane still has to notice when it refuses a push.
///
/// This is the worst version of the fault: with no control plane configured
/// there is no reconcile pass at all after the first, so the readiness the
/// loop computed on its way out was the last word for the life of the
/// process. A node that then refused a pushed deployment answered the edge
/// `ready`, and kept its full share of that application's traffic,
/// indefinitely.
///
/// It also pins the one line that joins the two: `run_control_loop` handing
/// the deployment manager the same readiness it reports its own passes
/// through. Without it every hook in `DeploymentManager` is dead code.
#[tokio::test]
async fn a_refused_push_degrades_a_node_that_has_no_next_reconcile_pass() {
    let dir = tempfile::tempdir().unwrap();
    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps).await;
    let health = crate::health::NodeHealth::starting();

    run_control_loop(
        None,
        None,
        Arc::clone(&deployer),
        health.clone(),
        Duration::from_secs(1),
        Duration::from_secs(60),
    )
    .await;
    assert!(
        health.is_healthy(),
        "a node with nothing to be missing is ready: {:?}",
        health.snapshot()
    );

    // A bundle URL nothing answers on: the push is refused after the control
    // plane has already been told this node would run the version.
    let refused = DeploymentOrder {
        app_id: "demo".to_string(),
        version_id: "v1".to_string(),
        environment: "production".to_string(),
        bundle_url: "http://127.0.0.1:1/bundle".to_string(),
        bundle_sha: "0".repeat(64),
        env: std::collections::HashMap::new(),
        limits: None,
        compatibility: None,
    };
    let err = deployer
        .apply(refused)
        .await
        .expect_err("nothing serves that bundle");

    match health.snapshot() {
        crate::health::Readiness::Degraded { reason, apps } => {
            assert!(
                reason.contains("demo") && reason.contains("did not take it"),
                "the reason has to name what this node refused, or the answer is one more \
                 guess: {reason}"
            );
            assert!(
                reason.contains("could not be downloaded"),
                "and which step refused, which is what sends an operator to the right place: \
                 {reason}"
            );
            assert!(
                !reason.contains("127.0.0.1:1"),
                "but not the error's own message: this reason is answered unauthenticated on \
                 the node's data port, and a load error is text the application wrote: {reason}"
            );
            assert_eq!(apps, 0, "nothing was installed, so nothing is being served");
        }
        other => panic!(
            "a node that has just refused a push must not go on answering the edge that it is \
             running what it was told to; it answered {other:?} after {err}"
        ),
    }
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

/// The control plane will not hand a node its assignment list - which carries
/// every environment's decrypted secrets - to a caller that cannot prove it
/// is a node. A node that sends no credential gets 401 and installs nothing;
/// with `control.secret` set it gets the list and the bundle behind it.
#[tokio::test]
async fn a_node_presents_its_key_to_the_control_plane() {
    let dir = tempfile::tempdir().unwrap();
    let (bytes, sha) = bundle("demo");
    let plane = ControlPlane::guarded(
        &[("demo", "v1", "/bundle")],
        bytes,
        &sha,
        StatusCode::OK,
        Some("node-key"),
    )
    .await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer_for(
        dir.path(),
        apps.clone(),
        Some(plane.base()),
        Some("node-key"),
    )
    .await;

    let anonymous = AssignmentClient::new(plane.base(), "node-a", Duration::from_secs(10), None)
        .unwrap()
        .fetch()
        .await;
    assert!(
        matches!(anonymous, Err(AssignmentError::Status { code: 401, .. })),
        "expected a refusal without a credential, got {anonymous:?}"
    );

    let authenticated = AssignmentClient::new(
        plane.base(),
        "node-a",
        Duration::from_secs(10),
        Some(NodeCredential::new("node-key", Some(plane.base()))),
    )
    .unwrap();
    let outcome = Reconciler::new(&authenticated, &deployer)
        .once()
        .await
        .unwrap();

    assert_eq!(outcome.assigned, 1);
    assert_eq!(
        outcome.installed, 1,
        "the bundle download has to carry the key too, or the version never installs"
    );
    assert_eq!(
        apps.installed_version(&AppKey::production("demo"))
            .unwrap()
            .version_id,
        "v1"
    );
}

#[tokio::test]
async fn an_environment_the_control_plane_no_longer_assigns_stops_serving() {
    // The failure this closes: an application deleted from the catalog, or a
    // node taken out of its placement, reaches a node as *nothing at all* -
    // the row that would have said so no longer exists. A node that only ever
    // added kept serving it, with its secrets, until somebody restarted the
    // process.
    let dir = tempfile::tempdir().unwrap();
    let (bytes, sha) = bundle("demo");
    let plane =
        ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha, StatusCode::OK).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(plane.base(), "node-a");
    let mut reconciler = Reconciler::new(&client, &deployer);

    let outcome = reconciler.once().await.unwrap();
    assert_eq!(outcome.installed, 1);
    assert!(apps
        .installed_version(&AppKey::production("demo"))
        .is_some());

    plane.now_assigns(serde_json::json!([]));

    // The first complete answer that leaves it out is not enough on its own:
    // a control plane that read a broken catalog produces exactly this answer.
    let outcome = reconciler.once().await.unwrap();
    assert_eq!(outcome.assigned, 0);
    assert_eq!(outcome.removal_pending, 1);
    assert_eq!(outcome.unloaded, 0);
    assert!(apps
        .installed_version(&AppKey::production("demo"))
        .is_some());

    // The second one says the same thing, so this is not a blip.
    let outcome = reconciler.once().await.unwrap();
    assert_eq!(outcome.assigned, 0);
    assert_eq!(outcome.unloaded, 1);
    assert_eq!(outcome.unload_failed, 0);
    assert!(
        apps.installed_version(&AppKey::production("demo"))
            .is_none(),
        "the node kept serving an application the control plane no longer assigns it"
    );
}

#[tokio::test]
async fn one_short_answer_alone_never_stops_an_application() {
    // `complete` says only that the control plane dropped nothing while it
    // assembled the answer. It is not a check that the catalog behind it was
    // readable, so a control plane that read an empty catalog sends an empty
    // list marked complete - and acting on that one answer takes every
    // application off this node at once. The second answer is what tells the
    // two apart, and a fault that has passed disagrees with the first.
    let dir = tempfile::tempdir().unwrap();
    let (bytes, sha) = bundle("demo");
    let plane =
        ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha, StatusCode::OK).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(plane.base(), "node-a");
    let mut reconciler = Reconciler::new(&client, &deployer);

    reconciler.once().await.unwrap();
    let assigned = plane.assignments.lock().clone();

    plane.now_assigns(serde_json::json!([]));
    let outcome = reconciler.once().await.unwrap();
    assert_eq!(outcome.removal_pending, 1);
    assert_eq!(outcome.unloaded, 0);

    plane.now_assigns(assigned);
    let outcome = reconciler.once().await.unwrap();
    assert_eq!(
        outcome.unloaded, 0,
        "the fault passed, so nothing should ever have been stopped for it"
    );
    assert_eq!(outcome.unchanged, 1);
    assert!(
        apps.installed_version(&AppKey::production("demo"))
            .is_some(),
        "one bad answer from the control plane became an outage on this node"
    );
}

#[tokio::test]
async fn removing_a_preview_leaves_production_serving() {
    // Reconciliation is per (app, environment), because that is what runs.
    // Taking a preview down must not touch the production copy beside it.
    let dir = tempfile::tempdir().unwrap();
    let (bytes, sha) = bundle("demo");
    let plane = ControlPlane::serving(&[], bytes, &sha, StatusCode::OK).await;
    plane.now_assigns(serde_json::json!([
        plane.line("demo", "production", "v1", &sha),
        plane.line("demo", "preview-7", "v1", &sha),
    ]));

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(plane.base(), "node-a");
    let mut reconciler = Reconciler::new(&client, &deployer);

    let outcome = reconciler.once().await.unwrap();
    assert_eq!(outcome.installed, 2, "both environments should have loaded");

    plane.now_assigns(serde_json::json!([plane.line(
        "demo",
        "production",
        "v1",
        &sha
    )]));

    reconciler.once().await.unwrap();
    let outcome = reconciler.once().await.unwrap();
    assert_eq!(outcome.unloaded, 1);
    assert!(
        apps.installed_version(&AppKey::new("demo", "preview-7"))
            .is_none(),
        "the preview is no longer assigned and must have stopped"
    );
    assert!(
        apps.installed_version(&AppKey::production("demo"))
            .is_some(),
        "production was still assigned and must not have been touched"
    );
}

#[tokio::test]
async fn reconciling_a_node_that_already_matches_changes_nothing() {
    // Run on a timer, so the normal case has to be free of side effects: no
    // reload, no re-download, no restarted isolate.
    let dir = tempfile::tempdir().unwrap();
    let (bytes, sha) = bundle("demo");
    let plane =
        ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha, StatusCode::OK).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(plane.base(), "node-a");

    let mut reconciler = Reconciler::new(&client, &deployer);
    reconciler.once().await.unwrap();
    let outcome = reconciler.once().await.unwrap();

    assert_eq!(
        outcome,
        SyncOutcome {
            assigned: 1,
            unchanged: 1,
            ..Default::default()
        },
        "a second pass over an unchanged list must be a no-op"
    );
}

#[tokio::test]
async fn a_new_version_in_the_list_replaces_the_running_one() {
    // A push this node missed - it was restarting, or unreachable - only ever
    // reaches it through a later reconciliation.
    let dir = tempfile::tempdir().unwrap();
    let (bytes, sha) = bundle("demo");
    let plane =
        ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha, StatusCode::OK).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(plane.base(), "node-a");

    let mut reconciler = Reconciler::new(&client, &deployer);
    reconciler.once().await.unwrap();
    assert_eq!(
        apps.installed_version(&AppKey::production("demo"))
            .unwrap()
            .version_id,
        "v1"
    );

    plane.now_assigns(serde_json::json!([plane.line(
        "demo",
        "production",
        "v2",
        &sha
    )]));

    let outcome = reconciler.once().await.unwrap();
    assert_eq!(outcome.installed, 1);
    assert_eq!(outcome.unloaded, 0, "a version change is not an unload");
    assert_eq!(
        apps.installed_version(&AppKey::production("demo"))
            .unwrap()
            .version_id,
        "v2"
    );
}

#[tokio::test]
async fn an_incomplete_answer_loads_but_never_removes() {
    // The dangerous case: a control plane that holds an assignment it cannot
    // describe - a secret that will not decrypt, say - answers with a short
    // list. Absence would otherwise read as "deleted", and a node acting on
    // it would take down an application that is running perfectly well for a
    // problem on the other side.
    let dir = tempfile::tempdir().unwrap();
    let (bytes, sha) = bundle("demo");
    let plane =
        ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha, StatusCode::OK).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(plane.base(), "node-a");

    let mut reconciler = Reconciler::new(&client, &deployer);
    reconciler.once().await.unwrap();
    assert!(apps
        .installed_version(&AppKey::production("demo"))
        .is_some());

    plane.now_assigns(serde_json::json!([]));
    plane.answers_incompletely(1);

    for _ in 0..3 {
        let outcome = reconciler.once().await.unwrap();
        assert_eq!(outcome.unloaded, 0);
        assert_eq!(
            outcome.removal_pending, 0,
            "an incomplete answer says nothing about absence, so it must not count"
        );
    }
    assert!(
        apps.installed_version(&AppKey::production("demo"))
            .is_some(),
        "an incomplete answer must not be treated as an instruction to unload"
    );
}

#[tokio::test]
async fn an_incomplete_answer_neither_counts_towards_a_removal_nor_undoes_one() {
    // An incomplete answer is not evidence about absence in either direction,
    // so it must not be the second of the two answers that stops something,
    // and it must not reset a count that a complete answer already started.
    let dir = tempfile::tempdir().unwrap();
    let (bytes, sha) = bundle("demo");
    let plane =
        ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha, StatusCode::OK).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(plane.base(), "node-a");
    let mut reconciler = Reconciler::new(&client, &deployer);

    reconciler.once().await.unwrap();

    // One complete answer leaves it out: the count has started.
    plane.now_assigns(serde_json::json!([]));
    assert_eq!(reconciler.once().await.unwrap().removal_pending, 1);

    // An incomplete answer in between must not be the second one.
    plane.answers_incompletely(1);
    let outcome = reconciler.once().await.unwrap();
    assert_eq!(outcome.unloaded, 0);
    assert!(apps
        .installed_version(&AppKey::production("demo"))
        .is_some());

    // The next complete answer is, and the count was not thrown away.
    plane.answers_completely();
    assert_eq!(reconciler.once().await.unwrap().unloaded, 1);
    assert!(apps
        .installed_version(&AppKey::production("demo"))
        .is_none());
}

#[tokio::test]
async fn a_control_plane_that_cannot_say_is_treated_as_unable_to_say() {
    // No header at all is a control plane older than the completeness
    // contract. It cannot promise its list is the whole truth, so this node
    // must not remove on the strength of it - the same fail-safe direction as
    // an explicit "false".
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            Router::new().route(
                "/api/nodes/:id/assignments",
                get(|| async { Json(serde_json::json!([])) }),
            ),
        )
        .await;
    });

    let list = client(&base, "node-a").fetch().await.unwrap();
    assert!(
        !list.complete,
        "an answer with no completeness header cannot be acted on for removal"
    );
    assert_eq!(list.withheld, 0);
    task.abort();
}

/// A stand-in control plane that only counts registrations, so a test can ask
/// how many times a node said it was there.
async fn counting_registrar() -> (NodeRegistrar, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::AtomicUsize;

    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route(
            deis_wire::conventions::NODE_REGISTRATION_ROUTE,
            axum::routing::put(|State(calls): State<Arc<AtomicUsize>>| async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Json(serde_json::json!({ "refresh_after_seconds": 30 }))
            }),
        )
        .with_state(calls);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let registrar = NodeRegistrar::new(
        &base,
        NodeRegistration::of_this_node("node-a", 8123),
        Duration::from_secs(5),
        None,
    )
    .expect("the registrar is built");
    (registrar, seen)
}

#[tokio::test]
async fn a_registration_is_a_lease_the_node_keeps_renewing() {
    // deis-control drops a node that has not registered again within its TTL,
    // and a dropped node keeps serving every request perfectly while silently
    // receiving no deployment pushes and no cron fires. So "already registered"
    // must not mean "never say it again".
    let (registrar, calls) = counting_registrar().await;

    let first = announce(&registrar, RegistrationState::Pending).await;
    let refresh_at = match first {
        RegistrationState::Done { refresh_at } => refresh_at,
        other => panic!("expected the registration to land, got {other:?}"),
    };
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // Still inside the lease: saying it again would be pure noise.
    let again = announce(&registrar, first).await;
    assert_eq!(again, first);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a node inside its lease must not re-register on every pass"
    );

    // Past it: the lease has to be renewed or the control plane forgets this
    // node, and nothing about the node itself would look wrong.
    let expired = RegistrationState::Done {
        refresh_at: Instant::now() - Duration::from_secs(1),
    };
    let renewed = announce(&registrar, expired).await;
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "an expired lease has to be renewed, or this node drops out of the fleet"
    );
    match renewed {
        RegistrationState::Done { refresh_at: next } => {
            assert!(next > refresh_at, "the deadline has to move forward");
        }
        other => panic!("expected the renewal to land, got {other:?}"),
    }
}

#[test]
fn the_loop_never_sleeps_past_the_moment_it_must_re_register() {
    // reconcile_interval is configurable and the control plane's TTL is not
    // this node's to choose. A 5 minute reconcile against a 90 second TTL
    // would drop the node out of the fleet between passes.
    let state = RegistrationState::Done {
        refresh_at: Instant::now() + Duration::from_secs(30),
    };
    let until = state
        .time_until_refresh(Instant::now())
        .expect("a registered node has a deadline");
    assert!(until <= Duration::from_secs(30));
    assert!(state.cap(Duration::from_secs(300)) <= Duration::from_secs(30));

    // A backoff is capped by the same deadline. This is the one that used to
    // get through: a node that is merely degraded climbs a retry ladder, and
    // at the ceiling that ladder is minutes long - longer than the lease.
    assert!(
        state.cap(retry_delay(Duration::from_secs(15), 40)) <= Duration::from_secs(30),
        "a degraded node must not back off past its own registration lease"
    );

    // A deadline already passed shortens the sleep to nothing, so the next
    // pass re-registers immediately rather than sleeping through the lapse.
    let lapsed = RegistrationState::Done {
        refresh_at: Instant::now() - Duration::from_secs(1),
    };
    assert_eq!(lapsed.cap(Duration::from_secs(300)), Duration::ZERO);

    // Nothing to refresh yet, and nothing to refresh ever: neither shortens
    // the reconcile interval.
    assert_eq!(
        RegistrationState::Pending.time_until_refresh(Instant::now()),
        None
    );
    assert_eq!(
        RegistrationState::NotSupported.time_until_refresh(Instant::now()),
        None
    );
    assert_eq!(
        RegistrationState::Pending.cap(Duration::from_secs(300)),
        Duration::from_secs(300)
    );
    assert_eq!(
        RegistrationState::NotSupported.cap(Duration::from_secs(300)),
        Duration::from_secs(300)
    );
}

#[tokio::test]
async fn a_push_that_lands_while_the_list_is_in_flight_is_not_rolled_back() {
    // The list describes what the control plane wanted at the moment it was
    // asked. A push that lands while the request is in flight is the later
    // instruction about the same environment, and the node cannot tell which
    // is newer by looking at the version ids: they are opaque, and a rollback
    // deliberately points an environment at an older one. So the node dates
    // the list instead, and refuses to apply it over anything it did to
    // itself since.
    let dir = tempfile::tempdir().unwrap();
    let (bytes, sha) = bundle("demo");
    let plane =
        ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha, StatusCode::OK).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(plane.base(), "node-a");
    let mut reconciler = Reconciler::new(&client, &deployer);

    reconciler.once().await.unwrap();
    assert_eq!(
        apps.installed_version(&AppKey::production("demo"))
            .unwrap()
            .version_id,
        "v1"
    );

    // The control plane still answers "v1"; the push says "v2" and lands
    // while that answer is being held open.
    plane.holds_its_answer();
    let (outcome, ()) = tokio::join!(async { reconciler.once().await.unwrap() }, async {
        plane.was_asked().await;
        deployer
            .apply(DeploymentOrder {
                app_id: "demo".to_string(),
                version_id: "v2".to_string(),
                environment: "production".to_string(),
                bundle_url: format!("{}/bundle", plane.base()),
                bundle_sha: sha.clone(),
                env: HashMap::new(),
                limits: None,
                compatibility: None,
            })
            .await
            .unwrap();
        plane.answers_now();
    });

    assert_eq!(outcome.superseded, 1);
    assert_eq!(outcome.installed, 0);
    assert_eq!(
        apps.installed_version(&AppKey::production("demo"))
            .unwrap()
            .version_id,
        "v2",
        "the reconcile undid a push that landed after the list it acted on"
    );
}

#[tokio::test]
async fn a_push_that_lands_while_the_list_is_in_flight_is_not_unloaded_either() {
    // The same race on the other half of a reconciliation, which is the half
    // that cannot be taken back. This environment is already a confirmed
    // removal candidate, so the pass about to run would stop it - and a push
    // lands on it while the answer is in flight. The list predates that push,
    // so it has nothing to say about the environment any more.
    let dir = tempfile::tempdir().unwrap();
    let (bytes, sha) = bundle("demo");
    let plane =
        ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha, StatusCode::OK).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(plane.base(), "node-a");
    let mut reconciler = Reconciler::new(&client, &deployer);

    reconciler.once().await.unwrap();

    // One complete answer leaving it out: the next one would stop it.
    plane.now_assigns(serde_json::json!([]));
    assert_eq!(reconciler.once().await.unwrap().removal_pending, 1);

    plane.holds_its_answer();
    let (outcome, ()) = tokio::join!(async { reconciler.once().await.unwrap() }, async {
        plane.was_asked().await;
        deployer
            .apply(DeploymentOrder {
                app_id: "demo".to_string(),
                version_id: "v2".to_string(),
                environment: "production".to_string(),
                bundle_url: format!("{}/bundle", plane.base()),
                bundle_sha: sha.clone(),
                env: HashMap::new(),
                limits: None,
                compatibility: None,
            })
            .await
            .unwrap();
        plane.answers_now();
    });

    assert_eq!(outcome.unloaded, 0);
    assert_eq!(outcome.superseded, 1);
    assert_eq!(
        apps.installed_version(&AppKey::production("demo"))
            .unwrap()
            .version_id,
        "v2",
        "a reconcile stopped an application a push had just installed"
    );
}
