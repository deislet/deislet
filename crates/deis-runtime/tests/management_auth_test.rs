//! The node's public data port must not carry its management plane at all.
//!
//! Two independent reviews reached `GetStatus`, `UnloadApp` and
//! `StartInspect` on a running node over its public data port with no
//! credential at all: the two gRPC services were dispatched straight into
//! their handlers, so anything that could reach an application on the node
//! could also list every tenant on it, take one offline, or hand
//! `ApplyDeployment` a bundle URL of its own.
//!
//! The first answer to that was a credential in front of the dispatch - a
//! value the whole fleet shared. The answer now is that those services are
//! not on this listener, so there is no credential to get wrong and no
//! request to refuse: the paths fall through to the application router and
//! are answered as any request naming no application is. What does serve them
//! is `[machine_identity] management_listen`, where the caller proved a
//! certificate before the connection existed; `dispatcher::machine_test`
//! covers that side.
//!
//! These tests drive `dispatcher::start_server` itself - the same listener,
//! the same router, the same dispatch - because the hole was in how the two
//! services were mounted, not in either service.

use deis_runtime::app::ApplicationManager;
use deis_runtime::config::RuntimeConfig;
use deis_runtime::deployment::DeploymentManager;
use deis_runtime::dispatcher::{self, ServerOptions};
use deis_runtime::health::NodeHealth;
use reqwest::StatusCode;
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
async fn start_node(port: u16) -> tokio::task::JoinHandle<()> {
    let app_manager = Arc::new(ApplicationManager::new());
    let data_dir = std::env::temp_dir().join(format!("deis-management-auth-{port}"));
    let config = RuntimeConfig {
        data_dir: data_dir.clone(),
        ..RuntimeConfig::default()
    };
    let deployer = Arc::new(
        DeploymentManager::from_config(app_manager.clone(), &config, None)
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
            // None on purpose: this harness is about what the *data* port
            // does, and a node with no machine management plane is the
            // sharpest version of that question - there is nowhere else on
            // this process for a management call to have gone.
            machine_management: None,
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

/// Ask the data port until it answers.
///
/// The node builds a V8 snapshot before it binds, which takes seconds, so a
/// request made the instant `start_node` returns is refused by nothing having
/// opened the socket yet. `grpc_call` above carries the same retry for the
/// same reason; this is its plain-HTTP twin.
///
/// A retry and not a sleep: the wait is over when the node answers, which is
/// the condition actually being waited for, and a fixed pause would be either
/// too short on a loaded machine or wasted on an idle one.
async fn get_until_answered(addr: &str, path: &str) -> reqwest::Response {
    let client = reqwest::Client::new();
    for attempt in 0..200 {
        match client
            .get(format!("http://{addr}{path}"))
            .header("x-deis-app-id", "_system")
            .timeout(Duration::from_secs(20))
            .send()
            .await
        {
            Ok(response) => return response,
            Err(e) if attempt == 199 => panic!("{path} never answered: {e}"),
            Err(_) => tokio::task::yield_now().await,
        }
    }
    unreachable!("the loop above either answers or panics")
}

#[tokio::test]
async fn the_data_port_does_not_dispatch_a_management_call_at_all() {
    let port = free_port().await;
    let node = start_node(port).await;
    let addr = format!("127.0.0.1:{port}");
    let client = client();

    // GetStatus listed every tenant's applications, environments and op
    // metrics to a caller with no credential. OnScheduled is the same plane:
    // it runs an application's handler with that environment's secrets.
    //
    // Neither is refused now - which is a stronger statement than being
    // refused. They are not here: the request reaches the application router,
    // which answers a request that names no application, so there is no
    // credential a caller could bring that would change the answer.
    for path in [GET_STATUS, ON_SCHEDULED] {
        for credential in [None, Some("any-string-at-all")] {
            let response = grpc_call(&client, &addr, path, credential).await;
            assert!(
                response.status().is_client_error(),
                "{path} must fall through to the application router, got {}",
                response.status()
            );
            assert_eq!(
                grpc_status(&response),
                None,
                "{path} must not be answered as gRPC on the data port at all"
            );
            let body = response.bytes().await.unwrap();
            assert!(
                !String::from_utf8_lossy(&body).contains("auth-test-node"),
                "{path} must not report this node's status"
            );
        }
    }

    node.abort();
}

#[tokio::test]
async fn public_traffic_is_untouched_by_the_management_plane_moving() {
    // The other half of the change: a node that serves nothing is not a node.
    // Moving the management services off this listener must not have taken
    // application traffic with them.
    let port = free_port().await;
    let node = start_node(port).await;
    let addr = format!("127.0.0.1:{port}");

    let public = get_until_answered(&addr, "/health").await;
    assert!(
        public.status() != StatusCode::UNAUTHORIZED && public.status() != StatusCode::FORBIDDEN,
        "public traffic must not be asked for a credential: {}",
        public.status()
    );
    // 503 and not 200, and that is the answer being asserted: this node has no
    // control plane to ask, so it never left `Readiness::Starting`, and
    // `_system`'s `/health` reports what the node is rather than that it is
    // listening. A 200 here would mean the probe had gone back to answering
    // from a constant - see `deis_runtime::health`.
    assert_eq!(
        public.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a node that has not been told what to run must not pass the edge's probe"
    );
    assert_eq!(
        public
            .headers()
            .get(deis_runtime::health::READINESS_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("starting"),
        "the edge reads its rotation decision off this header"
    );

    node.abort();
}
