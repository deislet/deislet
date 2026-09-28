use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use deis_identity::{names, MachineIdentity, Protocol, SpiffeId};
use deis_identity_test_pki::{write_files, TestCa};
use deis_wire::machine::machine_channel_server::{MachineChannel, MachineChannelServer};
use deis_wire::machine::{
    DeregisterNodeRequest, DeregisterNodeResponse, FetchAssignmentsRequest,
    FetchAssignmentsResponse, FetchRoutingTableRequest, FetchRoutingTableResponse,
    IssueCapabilityRequest, IssueCapabilityResponse, RegisterNodeRequest, RegisterNodeResponse,
};
use tonic::service::Routes;
use tonic::transport::Endpoint;
use tonic::Status;

use super::*;
use crate::app::{AppKey, ApplicationManager};
use crate::config::RuntimeConfig;
use crate::deployment::sha256_hex;
use crate::deployment::NodeRegistration;

const HANDLER: &str = "export default function handler() { return new Response('ok'); }";

/// The trust domain every certificate minted in this file is issued into.
/// Arbitrary - nothing here talks to a real fleet - but shared by every
/// stand-in control plane and every node identity a test mints, because a
/// certificate naming the wrong trust domain is refused at the handshake
/// exactly the way one naming the wrong machine is.
const TRUST_DOMAIN: &str = "deis.internal";

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

/// A node identity `ca` signed, so a client built from it completes a real
/// handshake against a listener the same authority signed.
///
/// Nothing keeps the directory the files were written to: an identity reads
/// its files once at load and only again on an explicit reload, which
/// nothing in this module asks for.
fn node_identity(ca: &TestCa, node_id: &str) -> Arc<MachineIdentity> {
    let directory = tempfile::tempdir().unwrap();
    let name = names::node(TRUST_DOMAIN, node_id).unwrap();
    let files = write_files(directory.path(), "node", &ca.issue(&name), &ca.root_pem);
    let identity = Arc::new(MachineIdentity::load(files.as_identity_files(), &[]).unwrap());
    drop(directory);
    identity
}

/// Serve `service` as `deis.machine.MachineChannel` over mutual TLS on a
/// fresh loopback port, presenting a certificate `ca` issues for this
/// fleet's control name, and hand back the endpoint to dial and the name it
/// proves.
///
/// Every stand-in control plane in this file needs this same wiring, so that
/// driving a test through [`AssignmentClient::over_machine_channel`] or
/// [`NodeRegistrar::over_machine_channel`] completes the same connector and
/// the same mutual TLS handshake a real node uses. What
/// `node_preshared_keys` used to prove - that the caller is a node - is now
/// this handshake, and a stand-in that skipped it would no longer be
/// exercising the thing that replaced the fleet key.
fn serve_machine_channel(ca: &TestCa, service: impl MachineChannel) -> (String, SpiffeId) {
    let control_name = names::control(TRUST_DOMAIN).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let files = write_files(
        directory.path(),
        "control",
        &ca.issue(&control_name),
        &ca.root_pem,
    );
    let identity =
        Arc::new(MachineIdentity::load(files.as_identity_files(), &[Protocol::Http2]).unwrap());
    drop(directory);

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let tls = RustlsConfig::from_config(identity.server_config());
    let acceptor = RustlsAcceptor::new(tls);
    let app = Routes::default()
        .add_service(MachineChannelServer::new(service))
        .into_axum_router();
    tokio::spawn(async move {
        let _ = axum_server::from_tcp(listener)
            .acceptor(acceptor)
            .serve(app.into_make_service())
            .await;
    });

    (format!("https://{address}"), control_name)
}

/// What `FetchAssignments` answers with, shared by every dynamic case below.
///
/// Behind locks so a test can change what the control plane says between two
/// reconciliations - the whole point of reconciling - and behind a watch
/// channel so a test can hold an answer open and make something happen on
/// the node while the request is in flight.
struct AssignmentsService {
    assignments: Arc<parking_lot::Mutex<serde_json::Value>>,
    /// Whether this plane claims its answer is the whole truth, and how many
    /// assignments it says it left out. Real deis-control sends both on
    /// every answer.
    completeness: Arc<parking_lot::Mutex<(bool, u32)>>,
    /// Set instead of an answer for the one case a node has to tell apart
    /// from an empty fleet: the control plane refusing the call outright.
    refusal: Arc<parking_lot::Mutex<Option<Status>>>,
    /// One permit per time the route has been entered, so a test can wait
    /// for the request to be in flight without a timer.
    asked: Arc<tokio::sync::Semaphore>,
    /// The route answers only while this is true. Starts true, so a test
    /// that does not care about the timing never notices it.
    open: tokio::sync::watch::Receiver<bool>,
}

#[tonic::async_trait]
impl MachineChannel for AssignmentsService {
    async fn fetch_assignments(
        &self,
        _request: tonic::Request<FetchAssignmentsRequest>,
    ) -> Result<tonic::Response<FetchAssignmentsResponse>, Status> {
        if let Some(status) = self.refusal.lock().clone() {
            return Err(status);
        }

        self.asked.add_permits(1);
        let mut open = self.open.clone();
        while !*open.borrow_and_update() {
            if open.changed().await.is_err() {
                break;
            }
        }

        let (complete, withheld) = *self.completeness.lock();
        Ok(tonic::Response::new(FetchAssignmentsResponse {
            assignments_json: self.assignments.lock().to_string(),
            complete,
            withheld,
        }))
    }

    // Registration and deregistration are on this channel too, and nothing
    // under test through this stand-in makes either call: these cases cover
    // reconciliation, which only ever fetches assignments. Answered rather
    // than left out because the trait requires every RPC of the service, and
    // a stand-in that answered one of them with something plausible would
    // let a test start depending on an answer no control plane gave.
    async fn register(
        &self,
        _request: tonic::Request<RegisterNodeRequest>,
    ) -> Result<tonic::Response<RegisterNodeResponse>, Status> {
        Err(Status::unimplemented(
            "this stand-in only serves FetchAssignments",
        ))
    }

    async fn deregister(
        &self,
        _request: tonic::Request<DeregisterNodeRequest>,
    ) -> Result<tonic::Response<DeregisterNodeResponse>, Status> {
        Err(Status::unimplemented(
            "this stand-in only serves FetchAssignments",
        ))
    }

    async fn fetch_routing_table(
        &self,
        _request: tonic::Request<FetchRoutingTableRequest>,
    ) -> Result<tonic::Response<FetchRoutingTableResponse>, Status> {
        Err(Status::unimplemented(
            "this stand-in only serves FetchAssignments",
        ))
    }

    async fn fetch_secret_values(
        &self,
        _request: tonic::Request<deis_wire::machine::FetchSecretValuesRequest>,
    ) -> std::result::Result<
        tonic::Response<deis_wire::machine::FetchSecretValuesResponse>,
        tonic::Status,
    > {
        Err(tonic::Status::unimplemented("fetch_secret_values"))
    }

    async fn issue_capability(
        &self,
        _request: tonic::Request<IssueCapabilityRequest>,
    ) -> std::result::Result<tonic::Response<IssueCapabilityResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("issue_capability"))
    }
}

/// A stand-in control plane that serves an assignment list over the machine
/// channel, under mutual TLS, and the bundle it points at over plain HTTP -
/// exactly as deis-control does: one door for the list, mutually
/// authenticated because it carries every environment's decrypted secrets,
/// and one for the bytes, addressed by whatever URL the list names and never
/// guarded by how that list arrived (see `deis_runtime::deployment::fetch`).
///
/// Both listeners are bound before either router is built, so the assignment
/// list can name this server's own bundle address, which is what the node
/// has to download from.
struct ControlPlane {
    /// Where this plane serves `/bundle`, over plain HTTP.
    base: String,
    /// Where this plane serves `deis.machine.MachineChannel`, over mutual
    /// TLS.
    machine_endpoint: String,
    /// The authority that signed this plane's own certificate and every node
    /// identity [`Self::node_identity`] hands out - the same one, so a node
    /// built from it completes a real handshake against this plane's
    /// listener.
    ca: TestCa,
    /// The name this plane's own machine listener proves.
    control_identity: SpiffeId,
    assignments: Arc<parking_lot::Mutex<serde_json::Value>>,
    completeness: Arc<parking_lot::Mutex<(bool, u32)>>,
    refusal: Arc<parking_lot::Mutex<Option<Status>>>,
    asked: Arc<tokio::sync::Semaphore>,
    open: tokio::sync::watch::Sender<bool>,
    _bundle_task: tokio::task::JoinHandle<()>,
}

impl ControlPlane {
    /// `entries` are `(app_id, version_id, bundle_path)`; every one of them
    /// is pointed at this server's `bundle_path`, so a path other than
    /// `/bundle` stands for a URL that does not resolve.
    async fn serving(entries: &[(&str, &str, &str)], bundle: Vec<u8>, bundle_sha: &str) -> Self {
        let bundle_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", bundle_listener.local_addr().unwrap());
        let bundle = Arc::new(bundle);
        let bundle_router = Router::new().route(
            "/bundle",
            get(move || {
                let bundle = Arc::clone(&bundle);
                async move {
                    Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(bundle.as_ref().clone()))
                        .unwrap()
                }
            }),
        );
        let bundle_task = tokio::spawn(async move {
            let _ = axum::serve(bundle_listener, bundle_router).await;
        });

        let assignments = Arc::new(parking_lot::Mutex::new(serde_json::Value::Array(
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
        )));
        let completeness = Arc::new(parking_lot::Mutex::new((true, 0u32)));
        let refusal: Arc<parking_lot::Mutex<Option<Status>>> =
            Arc::new(parking_lot::Mutex::new(None));
        let asked = Arc::new(tokio::sync::Semaphore::new(0));
        let (open, open_rx) = tokio::sync::watch::channel(true);

        let ca = TestCa::new();
        let (machine_endpoint, control_identity) = serve_machine_channel(
            &ca,
            AssignmentsService {
                assignments: assignments.clone(),
                completeness: completeness.clone(),
                refusal: refusal.clone(),
                asked: asked.clone(),
                open: open_rx,
            },
        );

        Self {
            base,
            machine_endpoint,
            ca,
            control_identity,
            assignments,
            completeness,
            refusal,
            asked,
            open,
            _bundle_task: bundle_task,
        }
    }

    /// A plane that refuses every `FetchAssignments` call with `status`,
    /// instead of answering with a list - for the one case a node has to
    /// tell apart from an empty fleet: the control plane refusing outright.
    async fn refusing(status: Status) -> Self {
        let plane = Self::serving(&[], Vec::new(), "").await;
        *plane.refusal.lock() = Some(status);
        plane
    }

    fn base(&self) -> &str {
        &self.base
    }

    fn machine_endpoint(&self) -> &str {
        &self.machine_endpoint
    }

    /// A node identity this plane's own authority signed, so a client built
    /// from it completes a real handshake against this plane's listener.
    fn node_identity(&self, node_id: &str) -> Arc<MachineIdentity> {
        node_identity(&self.ca, node_id)
    }

    fn control_identity(&self) -> SpiffeId {
        self.control_identity.clone()
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

    /// Answer completely again, as a control plane does once whatever
    /// stopped it describing an assignment is fixed.
    fn answers_completely(&self) {
        *self.completeness.lock() = (true, 0);
    }

    /// Hold the assignments answer open until [`Self::answers_now`], so a
    /// test can make something happen on the node while the request is in
    /// flight.
    fn holds_its_answer(&self) {
        self.open.send(false).unwrap();
    }

    /// Wait until the assignments route has been entered. No timer: the
    /// route hands out a permit as it is entered.
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
    let mut config = RuntimeConfig {
        data_dir: dir.to_path_buf(),
        ..Default::default()
    };
    config.bundles.dir = Some(dir.join("bundles"));
    Arc::new(
        DeploymentManager::from_config(apps, &config, None)
            .await
            .unwrap(),
    )
}

fn client(plane: &ControlPlane, node_id: &str) -> AssignmentClient {
    AssignmentClient::over_machine_channel(
        plane.machine_endpoint(),
        node_id,
        Duration::from_secs(10),
        plane.node_identity(node_id),
        plane.control_identity(),
    )
    .unwrap()
}

/// A client pointed at a machine endpoint nothing answers on, for the tests
/// that check what a node does when it cannot reach its control plane at
/// all. The identity is real but signed by its own throwaway authority: the
/// connection never gets far enough for that to matter, because nothing is
/// listening at all.
fn unreachable_client(machine_endpoint: &str, node_id: &str) -> AssignmentClient {
    AssignmentClient::over_machine_channel(
        machine_endpoint,
        node_id,
        Duration::from_secs(10),
        test_identity(node_id),
        names::control(TRUST_DOMAIN).unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn a_node_loads_everything_the_control_plane_assigned_it() {
    let dir = tempfile::tempdir().unwrap();
    let (bytes, sha) = bundle("demo");
    let plane = ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(&plane, "node-a");
    let outcome = Reconciler::new(&client, &deployer).once().await.unwrap();

    assert_eq!(outcome.assigned, 1);
    assert_eq!(outcome.installed, 1);
    assert_eq!(outcome.failed, 0);
    let demo = AppKey::production("demo");
    assert_eq!(apps.installed_version(&demo).unwrap().version_id, "v1");
    assert_eq!(apps.assets(&demo).unwrap().len(), 1);
}

#[tokio::test]
async fn an_inflight_assignment_prepares_without_starting_the_current_version() {
    const DEPLOYMENT: &str = "018f2518-7e38-7f80-8000-000000000021";
    const GENERATION: &str = "018f2518-7e38-7f80-8000-000000000022";
    let dir = tempfile::tempdir().unwrap();
    let (bytes, sha) = bundle("demo");
    let plane = ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha).await;
    plane.now_assigns(serde_json::json!([{
        "app_id": "demo",
        "version_id": "v1",
        "deployment_id": DEPLOYMENT,
        "routing_generation": GENERATION,
        "deployment_action": "prepare",
        "environment": "production",
        "bundle_url": format!("{}/bundle", plane.base()),
        "bundle_sha": sha,
    }]));

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(&plane, "node-a");
    let mut reconciler = Reconciler::new(&client, &deployer);
    let prepared = reconciler.once().await.unwrap();
    let key = AppKey::production("demo");
    assert_eq!(prepared.installed, 1);
    assert!(apps.installed_version(&key).is_none());
    assert!(apps.serves_generation(&key, Some(GENERATION)).await);

    plane.now_assigns(serde_json::json!([{
        "app_id": "demo",
        "version_id": "v1",
        "deployment_id": DEPLOYMENT,
        "routing_generation": GENERATION,
        "deployment_action": "activate",
        "environment": "production",
        "bundle_url": format!("{}/bundle", plane.base()),
        "bundle_sha": sha,
    }]));
    reconciler.once().await.unwrap();
    assert_eq!(
        apps.installed_version(&key)
            .unwrap()
            .routing_generation
            .as_deref(),
        Some(GENERATION)
    );
}

#[test]
fn an_assignment_carries_the_environment_a_push_would_have() {
    // Before the list carried this, a node that restarted came back running
    // the right code with no secrets and no storage addresses, and stayed
    // that way until somebody deployed again.
    let assignment: Assignment = serde_json::from_value(serde_json::json!({
        "app_id": "demo",
        "canonical_application_id": "018f2518-7e38-7f80-8000-000000000001",
        "version_id": "v1",
        "environment": "production",
        "canonical_environment_id": "018f2518-7e38-7f80-8000-000000000002",
        "bundle_url": "http://control/bundle",
        "bundle_sha": "abc",
        "env": { "API_KEY": "s3cr3t", "DEIS_STORE_ENDPOINT": "http://127.0.0.1:8090" },
    }))
    .unwrap();

    let order = assignment.into_order().unwrap();
    assert_eq!(
        order.canonical_tenant.unwrap().to_string(),
        "v1/018f2518-7e38-7f80-8000-000000000001/018f2518-7e38-7f80-8000-000000000002"
    );
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

    let order = assignment.into_order().unwrap();
    assert!(order.env.is_empty());
    assert!(order.canonical_tenant.is_none());
}

#[test]
fn an_assignment_cannot_supply_only_half_of_canonical_identity() {
    let assignment: Assignment = serde_json::from_value(serde_json::json!({
        "app_id": "demo",
        "canonical_application_id": "018f2518-7e38-7f80-8000-000000000001",
        "version_id": "v1",
        "environment": "production",
        "bundle_url": "http://control/bundle",
        "bundle_sha": "abc"
    }))
    .unwrap();

    assert!(assignment
        .into_order()
        .unwrap_err()
        .contains("canonical_environment_id"));
}

#[test]
fn an_assignment_cannot_smuggle_a_non_v7_identity() {
    let error = serde_json::from_value::<Assignment>(serde_json::json!({
        "app_id": "demo",
        "canonical_application_id": "550e8400-e29b-41d4-a716-446655440000",
        "version_id": "v1",
        "environment": "production",
        "canonical_environment_id": "018f2518-7e38-7f80-8000-000000000002",
        "bundle_url": "http://control/bundle",
        "bundle_sha": "abc"
    }))
    .unwrap_err();

    assert!(error.to_string().contains("UUIDv7"), "{error}");
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
    let plane = ControlPlane::serving(&[], Vec::new(), "").await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps).await;
    let client = client(&plane, "node-a");
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
    )
    .await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(&plane, "node-a");
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
    let plane = ControlPlane::refusing(Status::internal("the catalog is down")).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps).await;
    let client = client(&plane, "node-a");
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

    let unreachable = unreachable_client("https://127.0.0.1:0", "node-a");
    let err = Reconciler::new(&unreachable, &deployer)
        .once()
        .await
        .unwrap_err();

    assert!(matches!(err, AssignmentError::Transport { .. }), "{err}");
}

/// A node that cannot reach its control plane degrades, and the reason it then
/// answers on its public data port names the class only -- never the control
/// plane's own address out of the transport error.
///
/// `x-deis-node-reason` is answered unauthenticated to anyone who reaches the
/// node's data port (see `crate::health::REASON_HEADER`), and the transport
/// error the reconcile pass fails with carries the internal URL this node was
/// configured to call. The reason used to be `format!("control plane
/// unreachable: {e}")`, so that URL left the node to whoever asked. It stays
/// in the node's own log; the header carries the class.
#[tokio::test]
async fn the_unreachable_reason_names_the_class_not_the_control_plane_address() {
    let dir = tempfile::tempdir().unwrap();
    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps).await;
    let health = crate::health::NodeHealth::starting();

    // A control plane at a port nothing answers on, so every reconcile pass is
    // a transport failure carrying this address.
    let unreachable = unreachable_client("https://127.0.0.1:1", "node-a");
    let health_for_loop = health.clone();
    let loop_task = tokio::spawn(async move {
        run_control_loop(
            Some(unreachable),
            None,
            deployer,
            health_for_loop,
            Duration::from_secs(1),
            Duration::from_secs(60),
            None,
        )
        .await;
    });

    // The first pass runs with no delay before it, so the node degrades within
    // milliseconds. Polled rather than slept on: yield until the loop has
    // published the degraded state, bounded so a regression that never
    // degrades fails the test rather than hanging it.
    let degraded = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let crate::health::Readiness::Degraded { reason, .. } = health.snapshot() {
                return reason;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the node has to degrade when its control plane cannot be reached");
    loop_task.abort();

    assert!(
        degraded.contains("control plane unreachable"),
        "the reason has to name the class so an operator can tell this cause from the others: \
         {degraded}"
    );
    assert!(
        !degraded.contains("127.0.0.1:1") && !degraded.contains("https://"),
        "the control plane's own address must not travel on the header the node answers \
         unauthenticated on its data port; it belongs in the node's log: {degraded}"
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
        None,
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
        None,
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
        canonical_tenant: None,
        version_id: "v1".to_string(),
        environment: "production".to_string(),
        bundle_url: "http://127.0.0.1:1/bundle".to_string(),
        bundle_sha: "0".repeat(64),
        env: std::collections::HashMap::new(),
        standard_environment: None,
        limits: None,
        compatibility: None,
        deployment_id: None,
        routing_generation: None,
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

#[tokio::test]
async fn an_environment_the_control_plane_no_longer_assigns_stops_serving() {
    // The failure this closes: an application deleted from the catalog, or a
    // node taken out of its placement, reaches a node as *nothing at all* -
    // the row that would have said so no longer exists. A node that only ever
    // added kept serving it, with its secrets, until somebody restarted the
    // process.
    let dir = tempfile::tempdir().unwrap();
    let (bytes, sha) = bundle("demo");
    let plane = ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(&plane, "node-a");
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
    let plane = ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(&plane, "node-a");
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
    let plane = ControlPlane::serving(&[], bytes, &sha).await;
    plane.now_assigns(serde_json::json!([
        plane.line("demo", "production", "v1", &sha),
        plane.line("demo", "preview-7", "v1", &sha),
    ]));

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(&plane, "node-a");
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
    let plane = ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(&plane, "node-a");

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
    let plane = ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(&plane, "node-a");

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
    let plane = ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(&plane, "node-a");

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
    let plane = ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(&plane, "node-a");
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

/// A stand-in control plane that only counts registrations, so a test can ask
/// how many times a node said it was there.
async fn counting_registrar() -> (NodeRegistrar, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingService {
        calls: Arc<AtomicUsize>,
    }

    #[tonic::async_trait]
    impl MachineChannel for CountingService {
        async fn register(
            &self,
            _request: tonic::Request<RegisterNodeRequest>,
        ) -> Result<tonic::Response<RegisterNodeResponse>, Status> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(tonic::Response::new(RegisterNodeResponse {
                registration_json: serde_json::json!({ "refresh_after_seconds": 30 }).to_string(),
            }))
        }

        async fn deregister(
            &self,
            _request: tonic::Request<DeregisterNodeRequest>,
        ) -> Result<tonic::Response<DeregisterNodeResponse>, Status> {
            Err(Status::unimplemented(
                "this stand-in only counts registrations",
            ))
        }

        async fn fetch_assignments(
            &self,
            _request: tonic::Request<FetchAssignmentsRequest>,
        ) -> Result<tonic::Response<FetchAssignmentsResponse>, Status> {
            Err(Status::unimplemented(
                "this stand-in only counts registrations",
            ))
        }

        async fn fetch_routing_table(
            &self,
            _request: tonic::Request<FetchRoutingTableRequest>,
        ) -> Result<tonic::Response<FetchRoutingTableResponse>, Status> {
            Err(Status::unimplemented(
                "this stand-in only counts registrations",
            ))
        }

        async fn fetch_secret_values(
            &self,
            _request: tonic::Request<deis_wire::machine::FetchSecretValuesRequest>,
        ) -> std::result::Result<
            tonic::Response<deis_wire::machine::FetchSecretValuesResponse>,
            tonic::Status,
        > {
            Err(tonic::Status::unimplemented("fetch_secret_values"))
        }

        async fn issue_capability(
            &self,
            _request: tonic::Request<IssueCapabilityRequest>,
        ) -> std::result::Result<tonic::Response<IssueCapabilityResponse>, tonic::Status> {
            Err(tonic::Status::unimplemented("issue_capability"))
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let ca = TestCa::new();
    let (machine_endpoint, control_identity) = serve_machine_channel(
        &ca,
        CountingService {
            calls: calls.clone(),
        },
    );

    let registrar = NodeRegistrar::over_machine_channel(
        &machine_endpoint,
        NodeRegistration::of_this_node("node-a", 8123),
        Duration::from_secs(5),
        node_identity(&ca, "node-a"),
        control_identity,
        None,
    )
    .expect("the registrar is built");
    (registrar, calls)
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
    let plane = ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(&plane, "node-a");
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
                canonical_tenant: None,
                version_id: "v2".to_string(),
                environment: "production".to_string(),
                bundle_url: format!("{}/bundle", plane.base()),
                bundle_sha: sha.clone(),
                env: HashMap::new(),
                standard_environment: None,
                limits: None,
                compatibility: None,
                deployment_id: None,
                routing_generation: None,
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
    let plane = ControlPlane::serving(&[("demo", "v1", "/bundle")], bytes, &sha).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir.path(), apps.clone()).await;
    let client = client(&plane, "node-a");
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
                canonical_tenant: None,
                version_id: "v2".to_string(),
                environment: "production".to_string(),
                bundle_url: format!("{}/bundle", plane.base()),
                bundle_sha: sha.clone(),
                env: HashMap::new(),
                standard_environment: None,
                limits: None,
                compatibility: None,
                deployment_id: None,
                routing_generation: None,
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

// ---- the machine channel -------------------------------------------------
//
// The same question, asked at the other door. What these cover is everything
// between the RPC and an `AssignmentList`: the decode, `complete` read as an
// explicit field, and a failed call put into the taxonomy the control loop and
// the runbooks are written around. The handshake itself is not here - it is
// covered against the control plane's own listener, in deis-control, which is
// the only place both ends of it exist.

/// A stand-in machine channel that answers `FetchAssignments` with whatever it
/// was built with.
struct MachineControlPlane {
    answer: Result<FetchAssignmentsResponse, Status>,
}

#[tonic::async_trait]
impl MachineChannel for MachineControlPlane {
    async fn fetch_assignments(
        &self,
        _request: tonic::Request<FetchAssignmentsRequest>,
    ) -> Result<tonic::Response<FetchAssignmentsResponse>, Status> {
        match &self.answer {
            Ok(answer) => Ok(tonic::Response::new(answer.clone())),
            Err(status) => Err(Status::new(status.code(), status.message())),
        }
    }

    async fn fetch_routing_table(
        &self,
        _request: tonic::Request<FetchRoutingTableRequest>,
    ) -> Result<tonic::Response<FetchRoutingTableResponse>, Status> {
        Err(Status::unimplemented(
            "this node never asks for the routing table",
        ))
    }

    // Registration and deregistration are on this channel too, and nothing
    // under test here makes either call: what these cases cover is the path
    // between `FetchAssignments` and an `AssignmentList`. They are answered
    // rather than left out because the trait requires every RPC of the
    // service, and a stand-in that answered one of them with something
    // plausible would let a test start depending on an answer no control
    // plane gave.
    async fn register(
        &self,
        _request: tonic::Request<RegisterNodeRequest>,
    ) -> Result<tonic::Response<RegisterNodeResponse>, Status> {
        Err(Status::unimplemented(
            "these cases do not register this node",
        ))
    }

    async fn deregister(
        &self,
        _request: tonic::Request<DeregisterNodeRequest>,
    ) -> Result<tonic::Response<DeregisterNodeResponse>, Status> {
        Err(Status::unimplemented(
            "these cases do not deregister this node",
        ))
    }

    async fn fetch_secret_values(
        &self,
        _request: tonic::Request<deis_wire::machine::FetchSecretValuesRequest>,
    ) -> std::result::Result<
        tonic::Response<deis_wire::machine::FetchSecretValuesResponse>,
        tonic::Status,
    > {
        Err(tonic::Status::unimplemented("fetch_secret_values"))
    }

    async fn issue_capability(
        &self,
        _request: tonic::Request<IssueCapabilityRequest>,
    ) -> std::result::Result<tonic::Response<IssueCapabilityResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("issue_capability"))
    }
}

/// Serve that channel on a loopback port and point a client at it.
///
/// Plain TCP on purpose: mutual TLS is what `Dialler` does and what
/// deis-control's own listener test proves end to end, and repeating it here
/// would test that crate again rather than this one. What is exercised here is
/// everything this crate owns above the connection.
async fn machine_client(answer: Result<FetchAssignmentsResponse, Status>) -> AssignmentClient {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let incoming = tonic::transport::server::TcpIncoming::from_listener(listener, true, None)
        .expect("a listener tonic can serve on");
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(MachineChannelServer::new(MachineControlPlane { answer }))
            .serve_with_incoming(incoming)
            .await;
    });

    let channel = Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect_lazy();
    AssignmentClient::over_channel(
        channel,
        "node-a",
        test_identity("node-a"),
        format!("https://{address}/deis.machine.MachineChannel/FetchAssignments"),
    )
}

/// A real identity for a node of this name, signed by its own throwaway
/// authority. Nothing dials with it in most of these tests; it is only what a
/// refusal names, or - in [`unreachable_client`] - an identity for a
/// connection that never gets far enough for its authority to matter.
fn test_identity(node_id: &str) -> Arc<MachineIdentity> {
    node_identity(&TestCa::new(), node_id)
}

fn one_assignment_json() -> String {
    serde_json::json!([{
        "app_id": "demo",
        "version_id": "v1",
        "environment": "production",
        "bundle_url": "https://control.internal:8443/bundle",
        "bundle_sha": "abc",
    }])
    .to_string()
}

#[tokio::test]
async fn an_rpc_answer_becomes_the_same_assignment_list_the_http_route_produced() {
    let client = machine_client(Ok(FetchAssignmentsResponse {
        assignments_json: one_assignment_json(),
        complete: true,
        withheld: 0,
    }))
    .await;

    let list = client.fetch().await.expect("a well-formed answer");
    assert_eq!(list.assignments.len(), 1);
    assert_eq!(list.assignments[0].app_id, "demo");
    assert_eq!(list.assignments[0].version_id, "v1");
    assert!(list.complete);
    assert_eq!(list.withheld, 0);
}

/// `complete` is a field here and was a header there, and it has to mean the
/// same thing: false is "remove nothing on the strength of this". The HTTP
/// path reads an absent header as false; this path reads the field straight,
/// and a node must not be able to tell the difference.
#[tokio::test]
async fn an_incomplete_rpc_answer_is_incomplete_the_same_way_an_http_one_is() {
    let client = machine_client(Ok(FetchAssignmentsResponse {
        assignments_json: one_assignment_json(),
        complete: false,
        withheld: 3,
    }))
    .await;

    let list = client
        .fetch()
        .await
        .expect("an incomplete answer still parses");
    assert_eq!(list.assignments.len(), 1);
    assert!(!list.complete, "a node would have unloaded on this answer");
    assert_eq!(list.withheld, 3);
}

#[tokio::test]
async fn an_rpc_answer_this_node_cannot_read_is_malformed_rather_than_empty() {
    let client = machine_client(Ok(FetchAssignmentsResponse {
        assignments_json: "{ this is not a list }".to_string(),
        complete: true,
        withheld: 0,
    }))
    .await;

    let err = client.fetch().await.unwrap_err();
    assert!(matches!(err, AssignmentError::Malformed { .. }), "{err}");
}

/// The refusal that means a configuration mistake and not a broken network:
/// the connection proved one machine and the request asked about another.
/// Reported as a refusal - the same class an HTTP 403 lands in - so the
/// control loop and an operator's runbook read it the way they always have.
#[tokio::test]
async fn asking_for_another_node_is_a_refusal_and_not_a_transport_failure() {
    let client = machine_client(Err(Status::permission_denied(
        "node-a may not fetch assignments for node-b",
    )))
    .await;

    let err = client.fetch().await.unwrap_err();
    assert!(
        matches!(err, AssignmentError::Status { code: 403, .. }),
        "{err}"
    );
}

#[tokio::test]
async fn a_control_plane_that_cannot_authenticate_this_node_is_a_refusal_too() {
    let client = machine_client(Err(Status::unauthenticated("no name was proved"))).await;

    let err = client.fetch().await.unwrap_err();
    assert!(
        matches!(err, AssignmentError::Status { code: 401, .. }),
        "{err}"
    );
}

/// A machine channel that is not listening has to look like a control plane
/// that is not there, because it is one: the node keeps serving what it has
/// and retries, rather than treating the failure as an answer.
#[tokio::test]
async fn a_machine_channel_that_is_not_there_is_a_transport_failure() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);

    let channel = Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect_lazy();
    let client = AssignmentClient::over_channel(
        channel,
        "node-a",
        test_identity("node-a"),
        format!("https://{address}/deis.machine.MachineChannel/FetchAssignments"),
    );

    let err = client.fetch().await.unwrap_err();
    assert!(matches!(err, AssignmentError::Transport { .. }), "{err}");
}

// ---- which transport a configuration asks for ----------------------------

fn machine_config() -> (RuntimeConfig, Arc<MachineIdentity>) {
    let mut config = RuntimeConfig::default();
    config.control.endpoint = Some("http://127.0.0.1:8080".to_string());
    config.machine_identity = Some(crate::config::MachineIdentityConfig {
        control_endpoint: Some("https://127.0.0.1:8443".to_string()),
        certificate: Some(PathBuf::from("/etc/deislet/node.crt")),
        private_key: Some(PathBuf::from("/etc/deislet/node.key")),
        trust_root: Some(PathBuf::from("/etc/deislet/root.pem")),
        trust_domain: Some(TRUST_DOMAIN.to_string()),
        ..Default::default()
    });
    (config, test_identity("node-a"))
}

#[tokio::test]
async fn a_node_with_an_identity_asks_over_the_machine_channel() {
    let (config, identity) = machine_config();
    let client = AssignmentClient::from_config(&config, "node-a", Some(identity))
        .expect("a complete machine configuration must build a client")
        .expect("a configured node has a client");

    assert!(
        client.transport_name().contains("machine channel"),
        "{}",
        client.transport_name()
    );
    // The endpoint and the RPC's own path, which is what an operator matches
    // against the control plane's log.
    assert_eq!(
        client.url(),
        "https://127.0.0.1:8443/deis.machine.MachineChannel/FetchAssignments"
    );
}

#[tokio::test]
async fn a_node_told_about_no_control_plane_at_all_asks_nobody() {
    let config = RuntimeConfig::default();
    assert!(AssignmentClient::from_config(&config, "node-a", None)
        .expect("no endpoint is not an error")
        .is_none());
}

/// The two halves of a machine channel mean different things on their own,
/// and the difference is the whole of this test.
///
/// An identity with no address is a node that serves its management plane and
/// asks nobody - `deis dev` runs exactly that, and `RuntimeConfig::validate`
/// accepts it alongside a `management_listen`. It builds no client, which is
/// the same answer a node with no identity at all gets.
///
/// An address with no identity is not a configuration: the address and the
/// files are one table, so it can only come from a caller that loaded no
/// identity for a table that has one. That is refused rather than quietly
/// returning `None`, because a `None` there would be a node that fetches
/// nothing while every other part of it looks configured.
#[tokio::test]
async fn an_address_without_an_identity_is_refused_and_an_identity_without_one_asks_nobody() {
    let (mut config, identity) = machine_config();
    config
        .machine_identity
        .as_mut()
        .expect("the fixture configures one")
        .control_endpoint = None;
    assert!(
        AssignmentClient::from_config(&config, "node-a", Some(Arc::clone(&identity)))
            .expect("an identity with nowhere to ask is a configuration, not an error")
            .is_none()
    );

    let (config, _) = machine_config();
    assert!(AssignmentClient::from_config(&config, "node-a", None).is_err());
}

/// A node configured to ask nobody builds no client at all, and that is a
/// supported way to run one rather than a misconfiguration.
///
/// It serves whatever its local store already holds and nothing changes it.
/// The arm exists because `from_config` has three refusals around it - a
/// machine endpoint with no identity, an identity with no endpoint, and a
/// `[control] endpoint` with no `[machine_identity]`, which
/// `RuntimeConfig::validate` catches first - and "nothing configured" has to
/// stay distinguishable from all of them.
#[tokio::test]
async fn a_node_that_names_no_control_plane_builds_no_client() {
    let config = RuntimeConfig::default();
    assert!(config.control.endpoint.is_none());
    assert!(config.machine_identity.is_none());

    let client = AssignmentClient::from_config(&config, "node-a", None)
        .expect("asking nobody is a configuration, not an error");
    assert!(
        client.is_none(),
        "a node with no control plane must not be handed a client that would dial one"
    );
}

/// The node's half of `FetchSecretValues`: a schema-2 deployment that binds a
/// secret starts only once its value has been fetched over the machine
/// channel, and the isolate sees it.
mod secret_fetch {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use deis_wire::machine::{FetchSecretValuesRequest, FetchSecretValuesResponse};

    use super::*;
    use crate::deployment::SecretSource;

    const SECRET_HANDLER: &str =
        "export default function handler(context) { return new Response(context.env.TOKEN); }";

    /// A stand-in control plane that answers `FetchSecretValues` by the same
    /// rules deis-control does: only for the node the deployments are
    /// assigned to, only deployments on its list, only revisions they bind,
    /// and exactly the revisions asked for.
    struct SecretPlane {
        assignments: serde_json::Value,
        /// `(deployment_id, revision)` to plaintext - the catalog.
        secrets: HashMap<(String, String), String>,
        /// The node these deployments are assigned to. A lock so a test can
        /// reassign them between two passes.
        assigned_to: Arc<parking_lot::Mutex<String>>,
        fetches: Arc<AtomicUsize>,
    }

    #[tonic::async_trait]
    impl MachineChannel for SecretPlane {
        async fn fetch_assignments(
            &self,
            _request: tonic::Request<FetchAssignmentsRequest>,
        ) -> Result<tonic::Response<FetchAssignmentsResponse>, Status> {
            Ok(tonic::Response::new(FetchAssignmentsResponse {
                assignments_json: self.assignments.to_string(),
                complete: true,
                withheld: 0,
            }))
        }

        async fn fetch_secret_values(
            &self,
            request: tonic::Request<FetchSecretValuesRequest>,
        ) -> Result<tonic::Response<FetchSecretValuesResponse>, Status> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let request = request.into_inner();
            if request.node_id != *self.assigned_to.lock() {
                return Err(Status::permission_denied("not assigned to this node"));
            }
            let entries: Vec<deis_wire::conventions::SecretFetchEntry> =
                serde_json::from_str(&request.entries_json).unwrap();
            let mut values = HashMap::new();
            for entry in entries {
                if entry.revisions.is_empty() {
                    return Err(Status::invalid_argument("no revisions"));
                }
                for revision in entry.revisions {
                    let key = (entry.deployment_id.clone(), revision.clone());
                    let value = self
                        .secrets
                        .get(&key)
                        .ok_or_else(|| Status::permission_denied("not bound"))?;
                    values.insert(
                        format!("{}\x00{revision}", entry.deployment_id),
                        value.clone(),
                    );
                }
            }
            Ok(tonic::Response::new(FetchSecretValuesResponse {
                values_json: serde_json::to_string(&values).unwrap(),
            }))
        }

        async fn register(
            &self,
            _request: tonic::Request<RegisterNodeRequest>,
        ) -> Result<tonic::Response<RegisterNodeResponse>, Status> {
            Err(Status::unimplemented("not used"))
        }

        async fn deregister(
            &self,
            _request: tonic::Request<DeregisterNodeRequest>,
        ) -> Result<tonic::Response<DeregisterNodeResponse>, Status> {
            Err(Status::unimplemented("not used"))
        }

        async fn fetch_routing_table(
            &self,
            _request: tonic::Request<FetchRoutingTableRequest>,
        ) -> Result<tonic::Response<FetchRoutingTableResponse>, Status> {
            Err(Status::unimplemented("not used"))
        }

        async fn issue_capability(
            &self,
            _request: tonic::Request<IssueCapabilityRequest>,
        ) -> Result<tonic::Response<IssueCapabilityResponse>, Status> {
            Err(Status::unimplemented("not used"))
        }
    }

    fn declarations() -> deis_wire::EnvironmentDeclarations {
        deis_wire::EnvironmentDeclarations::new(
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
            vec![deis_wire::EnvironmentDeclaration {
                name: "TOKEN".to_string(),
                kind: deis_wire::EnvironmentKind::Secret,
                value_type: deis_wire::EnvironmentValueType::String,
                required: true,
            }],
        )
    }

    /// A schema-2 bundle declaring `TOKEN`, laid out as `deis build` does.
    fn standard_bundle(declarations: &deis_wire::EnvironmentDeclarations) -> (Vec<u8>, String) {
        let manifest = serde_json::json!({
            "schema": 2,
            "app": "demo",
            "entry": "worker/index.js",
            "runtime": "standard-v1",
            "environment": declarations,
            "assets": [],
        });
        let document = declarations.canonical_json().unwrap();
        let mut buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let options = zip::write::FileOptions::default();
            writer
                .start_file(deis_bundle::MANIFEST_ENTRY, options)
                .unwrap();
            writer.write_all(manifest.to_string().as_bytes()).unwrap();
            writer.start_file("worker/index.js", options).unwrap();
            writer.write_all(SECRET_HANDLER.as_bytes()).unwrap();
            writer
                .start_file("documents/environment-secrets.json", options)
                .unwrap();
            writer.write_all(&document).unwrap();
            writer.finish().unwrap();
        }
        let sha = sha256_hex(&buf);
        (buf, sha)
    }

    async fn serve_bundle(bytes: Vec<u8>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let bytes = Arc::new(bytes);
        let router = Router::new().route(
            "/bundle",
            get(move || {
                let bytes = Arc::clone(&bytes);
                async move { Response::new(Body::from(bytes.as_ref().clone())) }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("{base}/bundle")
    }

    async fn served_token(apps: &ApplicationManager) -> String {
        let (_status, _headers, body) = apps
            .execute_app(
                &AppKey::production("demo"),
                "http://localhost/".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await
            .unwrap();
        String::from_utf8(body.into_bytes().unwrap().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn a_deployment_that_binds_a_secret_starts_with_the_fetched_value() {
        let declarations = declarations();
        let (bundle, sha) = standard_bundle(&declarations);
        let bundle_url = serve_bundle(bundle).await;

        let application_id = deis_wire::CanonicalId::new_v7();
        let environment_id = deis_wire::CanonicalId::new_v7();
        let deployment_id = deis_wire::CanonicalId::new_v7().to_string();
        let revision = "secret-revision-1";
        let snapshot = deis_wire::EnvironmentBindingSnapshot::new(
            declarations.standard_version.clone(),
            "v1",
            environment_id.to_string(),
            declarations.identity().unwrap(),
            vec![deis_wire::EnvironmentBinding::Secret(
                deis_wire::EnvironmentSecretBinding::new("TOKEN", revision),
            )],
        );
        let line = serde_json::json!({
            "app_id": "demo",
            "canonical_application_id": application_id,
            "version_id": "v1",
            "deployment_id": deployment_id,
            "environment": "production",
            "canonical_environment_id": environment_id,
            "bundle_url": bundle_url,
            "bundle_sha": sha,
            "environment_declarations": declarations,
            "environment_snapshot_id": snapshot.identity().unwrap(),
            "environment_snapshot": snapshot,
        });
        // The line itself carries no value; only the RPC does.
        assert!(!line.to_string().contains("s3cret-value"));

        let ca = TestCa::new();
        let assigned_to = Arc::new(parking_lot::Mutex::new("node-other".to_string()));
        let fetches = Arc::new(AtomicUsize::new(0));
        let (endpoint, control_identity) = serve_machine_channel(
            &ca,
            SecretPlane {
                assignments: serde_json::json!([line]),
                secrets: HashMap::from([(
                    (deployment_id.clone(), revision.to_string()),
                    "s3cret-value".to_string(),
                )]),
                assigned_to: assigned_to.clone(),
                fetches: fetches.clone(),
            },
        );
        let client = AssignmentClient::over_machine_channel(
            &endpoint,
            "node-1",
            Duration::from_secs(10),
            node_identity(&ca, "node-1"),
            control_identity,
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let apps = Arc::new(ApplicationManager::new());
        let deployer = deployer(dir.path(), apps.clone()).await;
        deployer.fetch_secrets_through(SecretSource::new(
            client.machine_channel_client().unwrap(),
            "node-1",
        ));
        let mut reconciler = Reconciler::new(&client, &deployer);

        // Assigned to another node: the control plane refuses this node the
        // values, so the deployment is refused rather than started without
        // them.
        let outcome = reconciler.once().await.unwrap();
        assert_eq!(outcome.failed, 1, "{outcome:?}");
        assert_eq!(deployer.installed_app_count(), 0);
        assert_eq!(fetches.load(Ordering::SeqCst), 1);

        // Now this node's: the next pass fetches and starts it.
        *assigned_to.lock() = "node-1".to_string();
        let outcome = reconciler.once().await.unwrap();
        assert_eq!(outcome.installed, 1, "{outcome:?}");
        assert_eq!(served_token(&apps).await, "s3cret-value");
        assert_eq!(fetches.load(Ordering::SeqCst), 2);

        // Already running: nothing is fetched again.
        let outcome = reconciler.once().await.unwrap();
        assert_eq!(outcome.unchanged, 1, "{outcome:?}");
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
        assert_eq!(
            deployer.secrets.get().unwrap().cached_deployments(),
            1,
            "the running deployment keeps its values cached"
        );
    }

    #[tokio::test]
    async fn a_complete_list_evicts_values_of_deployments_no_longer_assigned() {
        let ca = TestCa::new();
        let (endpoint, control_identity) = serve_machine_channel(
            &ca,
            SecretPlane {
                assignments: serde_json::json!([]),
                secrets: HashMap::new(),
                assigned_to: Arc::new(parking_lot::Mutex::new("node-1".to_string())),
                fetches: Arc::new(AtomicUsize::new(0)),
            },
        );
        let client = AssignmentClient::over_machine_channel(
            &endpoint,
            "node-1",
            Duration::from_secs(10),
            node_identity(&ca, "node-1"),
            control_identity,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let deployer = deployer(dir.path(), Arc::new(ApplicationManager::new())).await;
        deployer.fetch_secrets_through(SecretSource::new(
            client.machine_channel_client().unwrap(),
            "node-1",
        ));
        deployer
            .secrets
            .get()
            .unwrap()
            .insert_for_test("gone-deployment", "r1", "stale");
        assert_eq!(deployer.secrets.get().unwrap().cached_deployments(), 1);

        Reconciler::new(&client, &deployer).once().await.unwrap();
        assert_eq!(deployer.secrets.get().unwrap().cached_deployments(), 0);
    }
}

/// Run the control loop until `until` holds, then stop it.
async fn run_loop_until(
    client: AssignmentClient,
    deployer: Arc<DeploymentManager>,
    health: Arc<crate::health::NodeHealth>,
    offline: Arc<super::super::offline_cache::OfflineCache>,
    until: impl Fn() -> bool,
) {
    let task = tokio::spawn(run_control_loop(
        Some(client),
        None,
        deployer,
        health,
        Duration::from_secs(1),
        Duration::from_secs(60),
        Some(offline),
    ));
    tokio::time::timeout(Duration::from_secs(30), async {
        while !until() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the control loop never got there");
    task.abort();
    let _ = task.await;
}

/// A node that restarts while its control plane is down comes back from its
/// offline cache: the deployment as the last sync handed it over, not the
/// bare record in its bundle store. With the cache tampered with, it falls
/// back to the bundle store alone.
#[tokio::test]
async fn a_restart_with_the_control_plane_down_keeps_serving_from_the_offline_cache() {
    use super::super::offline_cache::OfflineCache;

    let dir = tempfile::tempdir().unwrap();
    let app = "offline-restart";
    let key = AppKey::new(app, "production");
    let (bytes, sha) = bundle(app);
    let plane = ControlPlane::serving(&[(app, "v1", "/bundle")], bytes, &sha).await;
    let mut line = plane.line(app, "production", "v1", &sha);
    line["env"] = serde_json::json!({ "GREETING": "hello from the last sync" });
    plane.now_assigns(serde_json::json!([line]));

    let cache = || Arc::new(OfflineCache::new(dir.path().join("offline-cache"), [9; 32], "node-a"));

    // First life: the control plane answers, and the sync is sealed.
    {
        let apps = Arc::new(ApplicationManager::new());
        let deployer = deployer(dir.path(), apps.clone()).await;
        let health = crate::health::NodeHealth::starting();
        let offline = cache();
        let written = offline.path();
        run_loop_until(
            client(&plane, "node-a"),
            deployer,
            health.clone(),
            offline,
            || written.exists() && health.is_healthy(),
        )
        .await;
    }
    let sealed = cache().load().unwrap().expect("the sync was written");
    assert!(sealed.assignments_json.contains("hello from the last sync"));

    // Second life: nothing answers.
    let (installed, hosted) = restart_with_control_down(dir.path(), &key, false).await;
    let installed = installed.expect("the assigned version is installed again");
    assert_eq!(installed.version_id, "v1");
    assert!(hosted, "an isolate serves it");
    assert!(
        installed.env_confirmed,
        "restored from the offline cache, with the environment the last sync confirmed"
    );

    let (installed, _) = restart_with_control_down(dir.path(), &key, true).await;
    let installed = installed.expect("the bundle store still brings the code back");
    assert!(
        !installed.env_confirmed,
        "a tampered cache is not believed; the node falls back to its bundle store alone"
    );
}

/// Start a node on `dir` whose control plane answers nothing, with the
/// offline cache the test above sealed - flipped a bit first when `tamper` -
/// and report what it ended up running for `key`.
async fn restart_with_control_down(
    dir: &std::path::Path,
    key: &AppKey,
    tamper: bool,
) -> (Option<crate::app::InstalledVersion>, bool) {
    use super::super::offline_cache::OfflineCache;
    let cache = || Arc::new(OfflineCache::new(dir.join("offline-cache"), [9; 32], "node-a"));
    if tamper {
        let path = cache().path();
        let mut raw = std::fs::read(&path).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        std::fs::write(&path, raw).unwrap();
    }
    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer(dir, apps.clone()).await;
    let health = crate::health::NodeHealth::starting();
    let watching = health.clone();
    run_loop_until(
        unreachable_client("https://127.0.0.1:1", "node-a"),
        deployer,
        health,
        cache(),
        move || {
            matches!(
                watching.snapshot(),
                crate::health::Readiness::Degraded { .. }
            )
        },
    )
    .await;
    (apps.installed_version(key), apps.is_hosted(key).await)
}
