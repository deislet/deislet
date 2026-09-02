//! Which of its node keys the control plane presents to which node.
//!
//! The rotation in `docs/trust-boundaries.md` configures two values at once -
//! `node_preshared_keys = [NEW, OLD]` - and moves nodes from one to the other.
//! During that window an address that registered by proving OLD must be
//! driven with OLD: handing it NEW would give whoever holds the retiring
//! value the one that replaces it, and the rotation would then evict nothing.
//!
//! The node in these tests is a stand-in built from the real management
//! proto. It answers `HealthCheck` and `GetStatus` and records the credential
//! each call carried, which is the only fact under test. The control plane
//! side is the real one: the node router with its authentication layer, the
//! fleet manager, and the probe the poll loop runs.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use deis_control::api::middleware::NodeAuthConfig;
use deis_control::api::registration::{RegistrationState, node_router};
use deis_control::config::RuntimeNodeConfig;
use deis_control::runtime_client::proto::runtime_management_server::{
    RuntimeManagement, RuntimeManagementServer,
};
use deis_control::runtime_client::proto::{
    ApplyDeploymentRequest, ApplyDeploymentResponse, ExecuteCommandRequest, ExecuteCommandResponse,
    GetStatusRequest, GetStatusResponse, HealthCheckRequest, HealthCheckResponse, NodeInfo,
    PushConfigRequest, PushConfigResponse, StartInspectRequest, StartInspectResponse,
    UnloadAppRequest, UnloadAppResponse, UpdateAppRequest, UpdateAppResponse,
};
use deis_control::runtime_client::{NodeError, NodeKeyStatus, RuntimeNodeManager};
use deis_wire::conventions::{NODE_AUTH_METADATA, node_auth_secret};
use tonic::{Request, Response, Status};
use tower::ServiceExt;

/// A node that records the credential of every management call it receives
/// and answers healthy. It accepts anything: what it was handed is the
/// question, not whether it would have let the caller in.
struct RecordingNode {
    /// The id this node gives for itself on `GetStatus`.
    node_id: String,
    presented: Arc<Mutex<Vec<Option<String>>>>,
}

impl RecordingNode {
    fn record<T>(&self, request: &Request<T>) {
        let credential = request
            .metadata()
            .get(NODE_AUTH_METADATA)
            .and_then(|value| value.to_str().ok())
            .map(|value| node_auth_secret(value).to_string());
        self.presented.lock().unwrap().push(credential);
    }
}

#[tonic::async_trait]
impl RuntimeManagement for RecordingNode {
    async fn push_config(
        &self,
        _: Request<PushConfigRequest>,
    ) -> Result<Response<PushConfigResponse>, Status> {
        Err(Status::unimplemented("not part of this test"))
    }
    async fn apply_deployment(
        &self,
        _: Request<ApplyDeploymentRequest>,
    ) -> Result<Response<ApplyDeploymentResponse>, Status> {
        Err(Status::unimplemented("not part of this test"))
    }
    async fn unload_app(
        &self,
        _: Request<UnloadAppRequest>,
    ) -> Result<Response<UnloadAppResponse>, Status> {
        Err(Status::unimplemented("not part of this test"))
    }
    async fn update_app(
        &self,
        _: Request<UpdateAppRequest>,
    ) -> Result<Response<UpdateAppResponse>, Status> {
        Err(Status::unimplemented("not part of this test"))
    }
    async fn get_status(
        &self,
        request: Request<GetStatusRequest>,
    ) -> Result<Response<GetStatusResponse>, Status> {
        self.record(&request);
        Ok(Response::new(GetStatusResponse {
            node_info: Some(NodeInfo {
                node_id: self.node_id.clone(),
                version: "test".to_string(),
                uptime_seconds: 42,
                hostname: "stand-in".to_string(),
            }),
            ..GetStatusResponse::default()
        }))
    }
    async fn execute_command(
        &self,
        _: Request<ExecuteCommandRequest>,
    ) -> Result<Response<ExecuteCommandResponse>, Status> {
        Err(Status::unimplemented("not part of this test"))
    }
    async fn health_check(
        &self,
        request: Request<HealthCheckRequest>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        self.record(&request);
        Ok(Response::new(HealthCheckResponse {
            healthy: true,
            version: "test".to_string(),
            details: Default::default(),
        }))
    }
    async fn start_inspect(
        &self,
        _: Request<StartInspectRequest>,
    ) -> Result<Response<StartInspectResponse>, Status> {
        Err(Status::unimplemented("not part of this test"))
    }
}

/// Start a recording node calling itself `node_id` on a free loopback port;
/// returns its address and the log of credentials it has been shown.
async fn recording_node(node_id: &str) -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    let node = RecordingNode {
        node_id: node_id.to_string(),
        presented: Arc::default(),
    };
    let presented = Arc::clone(&node.presented);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(RuntimeManagementServer::new(node))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    (addr, presented)
}

/// The control plane as the binary wires it: one key list feeding both the
/// node endpoints' authentication and the fleet's outbound credential.
fn control_plane(keys: &[&str]) -> (axum::Router, Arc<RuntimeNodeManager>) {
    let keys: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
    let fleet = Arc::new(
        RuntimeNodeManager::new_with_ttl(Vec::<RuntimeNodeConfig>::new(), 15, 90)
            .with_node_keys(&keys),
    );
    let router = node_router(RegistrationState {
        nodes: Arc::clone(&fleet),
        node_auth: NodeAuthConfig::new(keys),
    });
    (router, fleet)
}

/// `PUT /api/nodes/{id}/registration` as a node would send it.
async fn register(router: &axum::Router, id: &str, addr: &str, key: &str) -> StatusCode {
    router
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri(format!("/api/nodes/{id}/registration"))
                .header("content-type", "application/json")
                .header("Authorization", format!("Bearer {key}"))
                .body(Body::from(format!("{{\"address\":\"{addr}\"}}")))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// The judge's reproduction (r9 tenancy-fleet F1), through the real
/// registration route: keys `[NEW, OLD]`, a registration that proved OLD,
/// then the probe the poll loop runs and the probe a contested claim runs.
/// Both must carry OLD and neither may carry NEW.
#[tokio::test]
async fn a_node_that_registered_with_the_old_key_is_driven_with_the_old_key() {
    let (router, fleet) = control_plane(&["new-key", "old-key"]);
    let (addr, presented) = recording_node("intruder").await;

    assert_eq!(
        register(&router, "intruder", &addr, "old-key").await,
        StatusCode::OK
    );

    // The poll loop's probe.
    let health = fleet
        .check_node("intruder")
        .await
        .expect("the node answers");
    assert!(health.healthy);
    assert_eq!(
        presented.lock().unwrap().clone(),
        vec![Some("old-key".to_string())],
        "an address that proved only the retiring value must be driven with that value and never see the new one"
    );

    // The identity probe a contested re-registration runs against the
    // address on file goes out with the holder's key, too.
    let (other_addr, _) = recording_node("intruder").await;
    assert_eq!(
        register(&router, "intruder", &other_addr, "new-key").await,
        StatusCode::CONFLICT,
        "the address on file answered, so the id stays with it"
    );
    let seen = presented.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "health probe, then identity probe: {seen:?}");
    assert!(
        seen.iter().all(|key| key.as_deref() == Some("old-key")),
        "{seen:?}"
    );

    // The fleet listing says which key the node is on, without saying the
    // key.
    let view = fleet
        .list()
        .into_iter()
        .find(|v| v.id == "intruder")
        .unwrap();
    assert_eq!(view.key_status, NodeKeyStatus::Proved);
    let rendered = serde_json::to_string(&view).unwrap();
    assert!(
        !rendered.contains("old-key") && !rendered.contains("new-key"),
        "{rendered}"
    );
}

/// Step 5 of the rotation: the retiring value is dropped and the control
/// plane restarted. A holder of that value cannot register, so its address
/// is not on the fleet list and receives nothing; a node that moved to the
/// new value carries on.
#[tokio::test]
async fn after_the_old_key_is_dropped_a_holder_of_it_receives_nothing() {
    let (router, fleet) = control_plane(&["new-key"]);
    let (old_holder, presented_to_old) = recording_node("still-on-old").await;
    let (new_holder, presented_to_new) = recording_node("moved").await;

    assert_eq!(
        register(&router, "still-on-old", &old_holder, "old-key").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        register(&router, "moved", &new_holder, "new-key").await,
        StatusCode::OK
    );

    assert_eq!(fleet.ids(), vec!["moved".to_string()]);
    assert!(matches!(
        fleet.check_node("still-on-old").await,
        Err(NodeError::UnknownNode)
    ));
    fleet
        .check_node("moved")
        .await
        .expect("the moved node answers");

    assert!(
        presented_to_old.lock().unwrap().is_empty(),
        "nothing may be sent to an address that could not prove a current key"
    );
    assert_eq!(
        presented_to_new.lock().unwrap().clone(),
        vec![Some("new-key".to_string())]
    );
}
