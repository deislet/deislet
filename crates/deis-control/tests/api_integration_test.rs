use deis_control::*;
use std::sync::Arc;
use tokio::time::Duration;

/// Poll the given TCP port until the API server accepts connections,
/// so tests do not rely on fixed delays for startup synchronization.
/// Panics if the server is not ready within the attempt limit.
async fn wait_for_server_ready(port: u16) {
    const MAX_ATTEMPTS: u32 = 100;
    let mut ticker = tokio::time::interval(Duration::from_millis(50));
    // The first tick completes immediately, so the first probe has no delay.
    for _ in 0..MAX_ATTEMPTS {
        ticker.tick().await;
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
    }
    panic!("API server on port {port} did not become ready in time");
}

#[tokio::test]
async fn test_api_server_health() {
    // Create test configuration
    let config = ControlConfig::default();
    let database = Arc::new(Database::init(":memory:").await.unwrap());

    // Create server components
    let sessions = Arc::new(SessionManager::new());
    let broadcaster = Arc::new(EventBroadcaster::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));

    // Start the API server in the background
    let api_config = Arc::new(config.clone());
    tokio::spawn(async move {
        let _ = deis_control::api::start_api_server(
            sessions,
            database,
            broadcaster,
            config_manager,
            command_executor,
            metrics_collector,
            api_config,
            18081,
        )
        .await;
    });

    // Wait until the server is ready
    wait_for_server_ready(18081).await;

    // Test the health endpoint
    let client = reqwest::Client::new();
    let response = client
        .get("http://localhost:18081/health")
        .send()
        .await
        .expect("Failed to call health endpoint");

    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert_eq!(body, "OK");
}

#[tokio::test]
async fn test_api_nodes_endpoint() {
    let config = ControlConfig::default();
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let sessions = Arc::new(SessionManager::new());
    let broadcaster = Arc::new(EventBroadcaster::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));

    let api_config = Arc::new(config.clone());
    tokio::spawn(async move {
        let _ = deis_control::api::start_api_server(
            sessions,
            database,
            broadcaster,
            config_manager,
            command_executor,
            metrics_collector,
            api_config,
            18082,
        )
        .await;
    });

    wait_for_server_ready(18082).await;

    // Test the nodes endpoint
    let client = reqwest::Client::new();
    let response = client
        .get("http://localhost:18082/api/nodes")
        .send()
        .await
        .expect("Failed to call nodes endpoint");

    assert_eq!(response.status(), 200);

    let json: serde_json::Value = response.json().await.unwrap();
    assert!(json.get("nodes").is_some());
    assert!(json.get("total").is_some());
}

#[tokio::test]
async fn test_api_metrics_endpoint() {
    let config = ControlConfig::default();
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let sessions = Arc::new(SessionManager::new());
    let broadcaster = Arc::new(EventBroadcaster::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));

    let api_config = Arc::new(config.clone());
    tokio::spawn(async move {
        let _ = deis_control::api::start_api_server(
            sessions,
            database,
            broadcaster,
            config_manager,
            command_executor,
            metrics_collector,
            api_config,
            18083,
        )
        .await;
    });

    wait_for_server_ready(18083).await;

    // Test the metrics endpoint
    let client = reqwest::Client::new();
    let response = client
        .get("http://localhost:18083/api/metrics/summary")
        .send()
        .await
        .expect("Failed to call metrics endpoint");

    assert_eq!(response.status(), 200);

    let json: serde_json::Value = response.json().await.unwrap();
    assert!(json.get("metrics").is_some());
    assert!(json["metrics"].get("nodes").is_some());
    assert!(json["metrics"].get("system").is_some());
}

#[tokio::test]
async fn test_api_events_endpoint() {
    let config = ControlConfig::default();
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let sessions = Arc::new(SessionManager::new());
    let broadcaster = Arc::new(EventBroadcaster::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));

    let api_config = Arc::new(config.clone());
    tokio::spawn(async move {
        let _ = deis_control::api::start_api_server(
            sessions,
            database,
            broadcaster,
            config_manager,
            command_executor,
            metrics_collector,
            api_config,
            18084,
        )
        .await;
    });

    wait_for_server_ready(18084).await;

    // Test the events endpoint
    let client = reqwest::Client::new();
    let response = client
        .get("http://localhost:18084/api/events")
        .send()
        .await
        .expect("Failed to call events endpoint");

    assert_eq!(response.status(), 200);

    let json: serde_json::Value = response.json().await.unwrap();
    assert!(json.get("events").is_some());
}

/// Bounded probe for the opposite of `wait_for_server_ready`: the accept
/// loop has stopped when a fresh connection is refused.
async fn wait_for_port_closed(addr: std::net::SocketAddr) -> bool {
    const MAX_ATTEMPTS: u32 = 100;
    let mut ticker = tokio::time::interval(Duration::from_millis(20));
    for _ in 0..MAX_ATTEMPTS {
        ticker.tick().await;
        if tokio::net::TcpStream::connect(addr).await.is_err() {
            return true;
        }
    }
    false
}

/// A stop signal must drain rather than kill: the listener stops taking new
/// connections at once, but a request already inside a handler runs to
/// completion and its caller gets the real answer. Before there was any
/// signal handling this could not happen - SIGTERM ended the process where
/// it stood, so a bundle upload or a deployment write in flight was lost.
#[tokio::test]
async fn a_stop_signal_drains_in_flight_requests_before_the_server_stops() {
    let config = ControlConfig::default();
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let sessions = Arc::new(SessionManager::new());
    let broadcaster = Arc::new(EventBroadcaster::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));
    let runtime_manager = Arc::new(RuntimeNodeManager::new(Vec::new(), 60));
    let api_config = Arc::new(config.clone());

    let api = deis_control::api::build_api_router(
        sessions,
        database,
        broadcaster,
        config_manager,
        command_executor,
        metrics_collector,
        runtime_manager,
        api_config,
    );

    // A request that stays inside its handler until the test lets it go,
    // which is what makes "in flight" observable.
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let router = api.router.route(
        "/test/held-open",
        axum::routing::get({
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            move || {
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                async move {
                    entered.notify_one();
                    release.notified().await;
                    "finished"
                }
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(deis_control::api::serve(
        listener,
        router,
        None,
        async move {
            let _ = stopped.await;
        },
    ));

    let client = reqwest::Client::new();
    let in_flight = tokio::spawn({
        let client = client.clone();
        let url = format!("http://{addr}/test/held-open");
        async move { client.get(url).send().await }
    });

    // Only once the handler is running is the request truly in flight.
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("the request never reached its handler");

    stop.send(()).unwrap();

    assert!(
        wait_for_port_closed(addr).await,
        "the listener kept accepting connections after the stop signal"
    );

    // The drain is under way and this request is still open: it must get
    // its answer, not a severed connection.
    release.notify_one();
    let response = tokio::time::timeout(Duration::from_secs(5), in_flight)
        .await
        .expect("the in-flight request never came back")
        .unwrap()
        .expect("the in-flight request was cut instead of drained");
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "finished");

    // And the server itself is finished, rather than left running.
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the server did not stop after draining")
        .unwrap()
        .expect("the server reported an error while stopping");
}

/// The REST API and the node gRPC channel share one port, and the node
/// side speaks HTTP/2 without TLS. This is the round trip that proves the
/// serving path still carries both: a REST call and a gRPC handshake
/// against the same address, with the drain wired in.
#[tokio::test]
async fn one_port_serves_both_rest_and_the_node_grpc_channel() {
    use deis_wire::pb::{
        AuthenticateRequest, ControlMessage, control_message::Payload,
        control_service_client::ControlServiceClient,
    };

    let config = ControlConfig::default();
    let database = Arc::new(Database::init(":memory:").await.unwrap());

    let server = ControlServer::new(config.clone()).with_database(Arc::clone(&database));
    let sessions = server.sessions();
    let broadcaster = server.broadcaster();
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));
    let runtime_manager = Arc::new(RuntimeNodeManager::new(Vec::new(), 60));

    let api = deis_control::api::build_api_router(
        Arc::clone(&sessions),
        Arc::clone(&database),
        broadcaster,
        config_manager,
        command_executor,
        metrics_collector,
        runtime_manager,
        Arc::new(config),
    );
    // Assembled the same way the binary assembles it.
    let app = tonic::service::Routes::default()
        .add_service(server.build_grpc_service())
        .into_axum_router()
        .merge(api.router);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(deis_control::api::serve(listener, app, None, async move {
        let _ = stopped.await;
    }));

    let health = reqwest::Client::new()
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .expect("REST call failed");
    assert_eq!(health.status(), 200);

    let mut client = ControlServiceClient::connect(format!("http://{addr}"))
        .await
        .expect("gRPC client could not connect to the shared port");
    let (outbound, rx) = tokio::sync::mpsc::channel(4);
    outbound
        .send(ControlMessage {
            payload: Some(Payload::Authenticate(AuthenticateRequest {
                instance_id: "test-node".to_string(),
                secret: String::new(),
            })),
        })
        .await
        .unwrap();
    let mut inbound = client
        .establish_channel(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .expect("the node channel was refused")
        .into_inner();

    let reply = tokio::time::timeout(Duration::from_secs(5), inbound.message())
        .await
        .expect("no answer on the node channel")
        .expect("the node channel failed")
        .expect("the node channel closed without answering");
    match reply.payload {
        Some(Payload::AuthResponse(response)) => assert!(response.success),
        other => panic!("expected an auth response, got {other:?}"),
    }

    stop.send(()).unwrap();
    // The node keeps its channel open, which is exactly the case the
    // bounded grace period exists for; dropping it lets the drain finish.
    drop(outbound);
    drop(inbound);
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .expect("the server did not stop while a node channel was open")
        .unwrap()
        .expect("the server reported an error while stopping");
}
