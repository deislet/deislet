//! The node's management plane must not serve a caller with no credential.
//!
//! Two independent reviews reached `GetStatus`, `UnloadApp` and
//! `StartInspect` on a running node over its public data port with no
//! credential at all: the two gRPC services were dispatched straight into
//! their handlers, so anything that could reach an application on the node
//! could also list every tenant on it, take one offline, or hand
//! `ApplyDeployment` a bundle URL of its own.
//!
//! These tests drive `dispatcher::start_server` itself - the same listener,
//! the same router, the same dispatch - because the hole was in how the two
//! services were mounted, not in either service.

use deis_runtime::app::ApplicationManager;
use deis_runtime::config::RuntimeConfig;
use deis_runtime::deployment::DeploymentManager;
use deis_runtime::dispatcher::{self, ServerOptions};
use deis_runtime::health::NodeHealth;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// A gRPC request frame carrying an empty message.
///
/// One byte saying the payload is uncompressed, then its big-endian length.
/// `GetStatusRequest { include_apps: false }` and a default `ScheduledEvent`
/// both encode to nothing at all under proto3, so this is a complete, valid
/// call - which is the point: the refusal has to happen before the message is
/// looked at.
const EMPTY_GRPC_FRAME: [u8; 5] = [0, 0, 0, 0, 0];

const GET_STATUS: &str = "/deis.management.RuntimeManagement/GetStatus";
const ON_SCHEDULED: &str = "/deis.trigger.Trigger/OnScheduled";

/// Take a port from the OS and give it straight back, so the server under
/// test can bind it. Nothing else in this process is listening, and the
/// alternative - a hard-coded port - collides with whatever else the machine
/// happens to be running.
async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0")
        .await
        .expect("the OS should hand out a port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// A node with a data listener, and optionally a management plane of its own.
async fn start_node(
    port: u16,
    management_keys: Vec<String>,
    management_listen: Option<SocketAddr>,
) -> tokio::task::JoinHandle<()> {
    let app_manager = Arc::new(ApplicationManager::new());
    let data_dir = std::env::temp_dir().join(format!("deis-management-auth-{port}"));
    let config = RuntimeConfig {
        data_dir: data_dir.clone(),
        ..RuntimeConfig::default()
    };
    let deployer = Arc::new(
        DeploymentManager::from_config(app_manager.clone(), &config)
            .await
            .expect("preparing the bundle directory"),
    );

    tokio::spawn(async move {
        let result = dispatcher::start_server(ServerOptions {
            app_manager,
            deployer,
            health: NodeHealth::starting(),
            node_id: "auth-test-node".to_string(),
            port,
            started_at: std::time::Instant::now(),
            shutdown_grace: Duration::from_secs(1),
            max_request_body_bytes: deis_runtime::config::default_max_request_body_bytes(),
            management_keys,
            management_listen,
        })
        .await;
        // Only reached when the node is torn down at the end of a test, or
        // when it refused to serve at all - which is a failure worth seeing.
        if let Err(e) = result {
            eprintln!("node stopped: {e:#}");
        }
    })
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        // The control plane speaks h2c to a node, so the test does too:
        // a refusal that only worked over HTTP/1.1 would not be the refusal a
        // real caller meets.
        .http2_prior_knowledge()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("building the test client")
}

/// One gRPC call, retried until the node is listening.
///
/// The node binds its listener, loads its system application and only then
/// starts serving, so the first attempts can be refused. Retrying the real
/// call is the condition worth waiting on - no fixed pause would be right on
/// both a warm and a cold machine.
async fn grpc_call(
    client: &reqwest::Client,
    addr: &str,
    path: &str,
    credential: Option<&str>,
) -> reqwest::Response {
    for attempt in 0..200 {
        let mut request = client
            .post(format!("http://{addr}{path}"))
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .body(EMPTY_GRPC_FRAME.to_vec());
        if let Some(credential) = credential {
            request = request.header("authorization", format!("Bearer {credential}"));
        }
        match request.send().await {
            Ok(response) => return response,
            Err(e) if attempt == 199 => panic!("{path} never answered: {e}"),
            Err(_) => tokio::task::yield_now().await,
        }
    }
    unreachable!("the loop above either returns or panics")
}

/// The gRPC status a response carries in its headers.
///
/// A refusal is a trailers-only response - HTTP 200 with `grpc-status` in the
/// headers and no body - which is exactly how a served call differs from it
/// here: a served call puts its status in the trailers instead and answers
/// with a message.
fn grpc_status(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get("grpc-status")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

#[tokio::test]
async fn an_unauthenticated_caller_gets_nothing_from_the_management_plane() {
    let port = free_port().await;
    let node = start_node(port, vec!["node-psk".to_string()], None).await;
    let addr = format!("127.0.0.1:{port}");
    let client = client();

    // GetStatus listed every tenant's applications, environments and op
    // metrics to a caller with no credential.
    let response = grpc_call(&client, &addr, GET_STATUS, None).await;
    assert_eq!(
        grpc_status(&response).as_deref(),
        Some("16"),
        "GetStatus must be refused as UNAUTHENTICATED, got {:?}",
        response.headers()
    );
    assert!(
        response.bytes().await.unwrap().is_empty(),
        "a refused call must not carry a message"
    );

    // The trigger service is the same management plane and was equally open:
    // OnScheduled runs an application's handler with that environment's
    // secrets.
    let response = grpc_call(&client, &addr, ON_SCHEDULED, None).await;
    assert_eq!(
        grpc_status(&response).as_deref(),
        Some("16"),
        "OnScheduled must be refused as UNAUTHENTICATED, got {:?}",
        response.headers()
    );

    // A wrong key is refused the same way a missing one is.
    let response = grpc_call(&client, &addr, GET_STATUS, Some("not-the-key")).await;
    assert_eq!(grpc_status(&response).as_deref(), Some("16"));

    node.abort();
}

#[tokio::test]
async fn a_node_with_no_key_on_the_public_port_serves_no_management_call() {
    // An empty key list is not a mode. A node that cannot tell its control
    // plane from anyone else serves neither, and says which setting is
    // missing - the same answer deis-control's node endpoints give when
    // `node_preshared_keys` is empty. Its public traffic is untouched,
    // because that is not what the missing key guards.
    let port = free_port().await;
    let node = start_node(port, Vec::new(), None).await;
    let addr = format!("127.0.0.1:{port}");
    let client = client();

    for path in [GET_STATUS, ON_SCHEDULED] {
        for credential in [None, Some("guessed-key")] {
            let response = grpc_call(&client, &addr, path, credential).await;
            assert_eq!(
                grpc_status(&response).as_deref(),
                Some("14"),
                "{path} must be refused as UNAVAILABLE with no key configured"
            );
        }
    }

    node.abort();
}

#[tokio::test]
async fn the_control_plane_is_still_served() {
    // The other half of the fix: a node that refuses everything is not a
    // node. The credential the control plane presents has to get through, or
    // no deployment lands and no cron fires.
    let port = free_port().await;
    let node = start_node(port, vec!["node-psk".to_string()], None).await;
    let addr = format!("127.0.0.1:{port}");
    let client = client();

    let response = grpc_call(&client, &addr, GET_STATUS, Some("node-psk")).await;
    assert_eq!(
        grpc_status(&response),
        None,
        "a served call reports its status in the trailers, not the headers"
    );
    let body = response.bytes().await.unwrap();
    assert!(
        !body.is_empty(),
        "an authenticated GetStatus must answer with a message"
    );
    assert!(
        String::from_utf8_lossy(&body).contains("auth-test-node"),
        "the answer should be this node's status"
    );

    // And public application traffic is untouched: the credential guards the
    // two gRPC prefixes and nothing else on the port.
    let public = reqwest::Client::new()
        .get(format!("http://{addr}/health"))
        .header("x-deis-app-id", "_system")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .expect("the data path must still answer");
    assert!(
        public.status().is_success(),
        "public traffic must not need a node key: {}",
        public.status()
    );

    node.abort();
}

#[tokio::test]
async fn a_separate_management_listener_takes_the_plane_off_the_public_port() {
    // What an operator gets for setting `[management] listen`: the public
    // port no longer carries the management plane at all, so a caller that
    // can reach an application cannot even address it.
    let port = free_port().await;
    let management_port = free_port().await;
    let management_addr = SocketAddr::from(([127, 0, 0, 1], management_port));
    let node = start_node(port, vec!["node-psk".to_string()], Some(management_addr)).await;
    let client = client();

    let response = grpc_call(&client, &format!("127.0.0.1:{port}"), GET_STATUS, None).await;
    // The gRPC prefixes are no longer mounted here, so the call falls through
    // to the application router and is answered as what it now is: a request
    // for a path on an application nobody named. Whatever the code, what
    // matters is that it is not a gRPC answer and carries no status.
    assert!(
        response.status().is_client_error(),
        "the public port must not serve the management plane: {}",
        response.status()
    );
    assert_eq!(
        grpc_status(&response),
        None,
        "the public port must not answer as gRPC at all"
    );
    let body = response.bytes().await.unwrap();
    assert!(
        !String::from_utf8_lossy(&body).contains("auth-test-node"),
        "the public port must not report this node's status"
    );

    // It is served on its own listener, and still only to a caller with the
    // key: separating the two is defence in depth, not a substitute for the
    // credential.
    let addr = management_addr.to_string();
    let refused = grpc_call(&client, &addr, GET_STATUS, None).await;
    assert_eq!(grpc_status(&refused).as_deref(), Some("16"));

    let served = grpc_call(&client, &addr, GET_STATUS, Some("node-psk")).await;
    assert_eq!(grpc_status(&served), None);
    assert!(!served.bytes().await.unwrap().is_empty());

    node.abort();
}
