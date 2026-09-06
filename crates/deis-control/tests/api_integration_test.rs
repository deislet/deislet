use deis_control::*;
use std::sync::Arc;
use tokio::time::Duration;

/// The caller address the real server puts on every request it accepts, and
/// which a `oneshot` against a bare router has to supply for itself.
///
/// It is not decoration. `api_auth_enabled` defaults to off, and a control
/// plane that requires no credential serves the human API only to callers on
/// its own machine, so a request that carries no address is refused with 401
/// rather than admitted as anonymous.
fn local_peer() -> axum::extract::ConnectInfo<std::net::SocketAddr> {
    axum::extract::ConnectInfo("127.0.0.1:54321".parse().expect("a peer address"))
}

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
    tokio::spawn(async move {
        let _ = deis_control::api::start_api_server(
            sessions,
            database,
            config_manager,
            command_executor,
            metrics_collector,
            config,
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
async fn test_api_sessions_and_fleet_are_two_different_lists() {
    let config = ControlConfig::default();
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let sessions = Arc::new(SessionManager::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));

    tokio::spawn(async move {
        let _ = deis_control::api::start_api_server(
            sessions,
            database,
            config_manager,
            command_executor,
            metrics_collector,
            config,
            18082,
        )
        .await;
    });

    wait_for_server_ready(18082).await;

    // The control channels open to this process - not the runtime fleet,
    // which is a different list at /api/runtime.
    let client = reqwest::Client::new();
    let response = client
        .get("http://localhost:18082/api/sessions")
        .send()
        .await
        .expect("Failed to call the sessions endpoint");

    assert_eq!(response.status(), 200);

    let json: serde_json::Value = response.json().await.unwrap();
    assert!(json.get("sessions").is_some());
    assert!(json.get("total").is_some());

    let fleet = client
        .get("http://localhost:18082/api/runtime")
        .send()
        .await
        .expect("Failed to call the runtime fleet endpoint");
    assert_eq!(fleet.status(), 200);
    let fleet: serde_json::Value = fleet.json().await.unwrap();
    assert!(
        fleet.get("nodes").is_some(),
        "the fleet is a separate list from the open control channels"
    );
}

#[tokio::test]
async fn test_api_metrics_endpoint() {
    let config = ControlConfig::default();
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let sessions = Arc::new(SessionManager::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));

    tokio::spawn(async move {
        let _ = deis_control::api::start_api_server(
            sessions,
            database,
            config_manager,
            command_executor,
            metrics_collector,
            config,
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
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));

    tokio::spawn(async move {
        let _ = deis_control::api::start_api_server(
            sessions,
            database,
            config_manager,
            command_executor,
            metrics_collector,
            config,
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

    // The event log answers in the unified list envelope.
    let json: serde_json::Value = response.json().await.unwrap();
    assert_eq!(json["schemaVersion"], 1, "{json}");
    assert!(json["items"].is_array(), "{json}");
    assert_eq!(json["page"]["total"], 0, "{json}");
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

/// The two credentials of the assembled router, checked where they are
/// actually composed rather than one middleware at a time.
///
/// `api_auth_enabled` governs the dashboard and the human API. It must not
/// reach the endpoints that serve nodes: one of them answers with every
/// environment's decrypted secrets, so it authenticates with
/// `node_preshared_keys` whatever that flag says - and it does not accept a
/// dashboard key, because the dashboard is deliberately unable to read a
/// secret back.
#[tokio::test]
async fn the_node_endpoints_take_the_node_key_and_the_api_key_takes_everything_else() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let config = ControlConfig {
        api_auth_enabled: true,
        api_keys: vec!["dashboard-key".to_string()],
        node_preshared_keys: vec!["node-psk".to_string()],
        ..Default::default()
    };
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let sessions = Arc::new(SessionManager::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));
    let api = deis_control::api::build_api_router(
        sessions,
        database,
        config_manager,
        command_executor,
        metrics_collector,
        Arc::new(RuntimeNodeManager::new(Vec::new(), 60)),
        Arc::new(config),
    )
    .await;

    let status_of = |uri: &str, credential: Option<&str>| {
        let router = api.router.clone();
        let mut builder = Request::builder().method("GET").uri(uri);
        if let Some(credential) = credential {
            builder = builder.header("Authorization", format!("Bearer {credential}"));
        }
        let request = builder.body(Body::empty()).unwrap();
        async move { router.oneshot(request).await.unwrap().status() }
    };

    const ASSIGNMENTS: &str = "/api/nodes/node-a/assignments";
    assert_eq!(status_of(ASSIGNMENTS, None).await, StatusCode::UNAUTHORIZED);
    assert_eq!(
        status_of(ASSIGNMENTS, Some("dashboard-key")).await,
        StatusCode::UNAUTHORIZED,
        "an API key must not be able to read the fleet's secrets"
    );
    assert_eq!(
        status_of(ASSIGNMENTS, Some("node-psk")).await,
        StatusCode::OK,
        "a node must reach its assignments while the API key requirement is on"
    );

    // And the other way round: the human API is the API key's, not the
    // node's.
    assert_eq!(
        status_of("/api/apps", Some("dashboard-key")).await,
        StatusCode::OK
    );
    assert_eq!(
        status_of("/api/apps", Some("node-psk")).await,
        StatusCode::UNAUTHORIZED
    );

    // Registration is on the same credential, on the assembled router: a
    // caller that can put a node on the fleet list has given itself a
    // delivery target, and a delivery target is handed every environment's
    // decrypted secrets on its first assignment fetch.
    let register = |credential: Option<&str>| {
        let router = api.router.clone();
        let mut builder = Request::builder()
            .method("PUT")
            .uri("/api/nodes/intruder/registration")
            .header("content-type", "application/json");
        if let Some(credential) = credential {
            builder = builder.header("Authorization", format!("Bearer {credential}"));
        }
        let request = builder
            .body(Body::from(r#"{"address":"http://10.0.0.7:8086"}"#))
            .unwrap();
        async move { router.oneshot(request).await.unwrap().status() }
    };
    assert_eq!(register(None).await, StatusCode::UNAUTHORIZED);
    assert_eq!(
        register(Some("dashboard-key")).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(register(Some("node-psk")).await, StatusCode::OK);
}

/// Accounts, tokens and the audit log on the router the binary actually
/// assembles - not on a test router that mounts the same handlers.
///
/// What this is here to catch is the defect this repository keeps producing:
/// a feature written, tested and registered, and then reachable from nowhere.
/// Every claim below is made through `build_api_router`, so a route that was
/// declared in the wrong place, or an authorisation gate that was layered
/// around the wrong half of the router, fails here rather than in production.
#[tokio::test]
async fn an_operator_can_mint_a_scoped_token_use_it_revoke_it_and_read_it_back() {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    // The shape of an installation that existed before accounts did: the
    // requirement is on, and one flat key in the configuration file.
    let config = ControlConfig {
        api_auth_enabled: true,
        api_keys: vec!["the-operators-old-key".to_string()],
        node_preshared_keys: vec!["node-psk".to_string()],
        ..Default::default()
    };
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let sessions = Arc::new(SessionManager::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));
    let api = deis_control::api::build_api_router(
        sessions,
        Arc::clone(&database),
        config_manager,
        command_executor,
        metrics_collector,
        Arc::new(RuntimeNodeManager::new(Vec::new(), 60)),
        Arc::new(config),
    )
    .await;
    // The same startup step the binary runs: it is what files the flat key
    // where `deis token list` can show it.
    deis_control::api::announce_authorization(&api.authz).await;

    let call = |method: &str, uri: &str, credential: Option<&str>, body: &str| {
        let router = api.router.clone();
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(credential) = credential {
            builder = builder.header("Authorization", format!("Bearer {credential}"));
        }
        let request = builder.body(Body::from(body.to_string())).unwrap();
        async move {
            let response = router.oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            (status, value)
        }
    };

    const OLD_KEY: &str = "the-operators-old-key";

    // The flat key is listed, and listed as unscoped: that count is what an
    // operator drives to zero, and it is the total of the `scope=unscoped`
    // listing in the unified envelope.
    let (status, listed) = call("GET", "/api/tokens", Some(OLD_KEY), "").await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["schemaVersion"], 1);
    assert_eq!(listed["page"]["total"], 1);
    assert_eq!(listed["items"][0]["origin"], "config");
    let (status, unscoped) = call(
        "GET",
        "/api/tokens?scope=unscoped&limit=1",
        Some(OLD_KEY),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{unscoped}");
    assert_eq!(unscoped["page"]["total"], 1);

    // An account and a token for one environment of one application.
    let (status, account) = call(
        "POST",
        "/api/accounts",
        Some(OLD_KEY),
        r#"{"name":"ci","role":"deployer"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{account}");
    assert_eq!(account["role"], "deployer");

    let (status, minted) = call(
        "POST",
        "/api/accounts/ci/tokens",
        Some(OLD_KEY),
        r#"{"name":"preview-pipeline","scopes":["deploy:demo/preview"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{minted}");
    let secret = minted["secret"].as_str().unwrap().to_string();
    let token_id = minted["token"]["id"].as_str().unwrap().to_string();
    assert_eq!(minted["token"]["scopes"], "deploy:demo/preview");

    // The secret exists in that one response and nowhere else: a listing
    // afterwards must not carry it.
    let (_, listed) = call("GET", "/api/tokens", Some(OLD_KEY), "").await;
    assert!(
        !listed.to_string().contains(&secret),
        "a token listing must never carry a credential"
    );

    // What the scoped token may do, and what it may not, on the real router.
    let (status, _) = call("POST", "/api/apps", Some(&secret), r#"{"id":"demo"}"#).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a CI credential creates its own app"
    );

    let (status, refused) = call(
        "POST",
        "/api/apps",
        Some(&secret),
        r#"{"id":"someone-elses"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        refused["details"]
            .as_str()
            .unwrap()
            .contains("deploy:someone-elses"),
        "{refused}"
    );

    for uri in ["/api/audit", "/api/tokens", "/api/accounts"] {
        let (status, _) = call("GET", uri, Some(&secret), "").await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{uri} is an administrator's, and this token is a deployer's"
        );
    }

    // Revoked, and refused from the very next request - with no cache to
    // expire and nothing to restart.
    let (status, _) = call(
        "DELETE",
        &format!("/api/tokens/{token_id}"),
        Some(OLD_KEY),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call("GET", "/api/apps/demo", Some(&secret), "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // And all of it is on the record, readable through the route the CLI
    // reads: the application that was created, the refusal, and the mint.
    let (status, log) = call("GET", "/api/audit?limit=100", Some(OLD_KEY), "").await;
    assert_eq!(status, StatusCode::OK, "{log}");
    let actions: Vec<&str> = log["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["action"].as_str().unwrap())
        .collect();
    for expected in ["account.create", "token.mint", "token.revoke", "app.create"] {
        assert!(
            actions.contains(&expected),
            "{expected} is missing: {actions:?}"
        );
    }

    let refusal = log["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["outcome"] == "refused" && entry["action"] == "app.create")
        .expect("the refused app.create has to be on the record");
    assert_eq!(refusal["app_id"], "someone-elses");
    assert_eq!(refusal["actor_account"], "ci");
    assert_eq!(refusal["status"], 403);

    // There is no way to take a line back out. Not for the unscoped key
    // either, which is the most powerful credential this control plane has.
    let before = log["items"].as_array().unwrap().len();
    let (status, _) = call("DELETE", "/api/audit", Some(OLD_KEY), "").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    let (_, after) = call("GET", "/api/audit?limit=100", Some(OLD_KEY), "").await;
    assert!(after["items"].as_array().unwrap().len() >= before);
}

/// The Restart button's whole round trip, in the shape the dashboard sends
/// it. It used to post `{command_type: 'custom', command_data: ...}`, which
/// the control plane could not deserialize at all: the command never reached
/// a node and the caller got a 422 about a field name.
///
/// And the answer for a node that is not connected has to say so: a command
/// can only go down a control channel the node is holding open, so naming a
/// node that has none is the caller's mistake, not a control plane failure.
#[tokio::test]
async fn the_dashboards_restart_body_is_the_one_the_control_plane_accepts() {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let sessions = Arc::new(SessionManager::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));
    let api = deis_control::api::build_api_router(
        sessions,
        database,
        config_manager,
        command_executor,
        metrics_collector,
        Arc::new(RuntimeNodeManager::new(Vec::new(), 60)),
        Arc::new(ControlConfig::default()),
    )
    .await;

    let post = |body: &'static str| {
        let router = api.router.clone();
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/commands/execute/deis-proxy-1")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        // The address the real server always attaches
        // (`into_make_service_with_connect_info`). This control plane demands
        // no credential, and a control plane that demands none serves the
        // human API only to its own machine - so a request with no address on
        // it at all is one the gate has no reason to trust.
        request.extensions_mut().insert(local_peer());
        async move {
            let response = router.oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            (status, String::from_utf8(bytes.to_vec()).unwrap())
        }
    };

    // What the button used to send: the server could not read it, so this
    // never became a command at all.
    let (status, _) = post(r#"{"command_type":"custom","command_data":"graceful_restart"}"#).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the old body must still be rejected, so nothing goes back to sending it"
    );

    // What it sends now. No node is connected in this test, so the answer is
    // about the fleet rather than about the control plane.
    let (status, body) = post(r#"{"command_type":"restart"}"#).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body.contains("control channel"),
        "the refusal must say why nothing could be sent: {body}"
    );

    // A free-form command travels as the tagged form, and gets the same
    // answer rather than a deserialization failure.
    let (status, _) = post(r#"{"command_type":{"custom":"graceful_restart"},"data":null}"#).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The blocker in its own right: a control plane assembled from a
/// configuration that names no node key must not serve the node endpoints to
/// anyone. "Unconfigured" used to mean "unguarded" here, so anyone who could
/// reach the port could inject a node into the fleet.
#[tokio::test]
async fn a_router_built_without_a_node_key_registers_nobody() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let config = ControlConfig::default();
    assert!(
        config.node_preshared_keys.is_empty(),
        "the default is the case this test is about"
    );
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let sessions = Arc::new(SessionManager::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));
    let fleet = Arc::new(RuntimeNodeManager::new(Vec::new(), 60));
    let api = deis_control::api::build_api_router(
        sessions,
        database,
        config_manager,
        command_executor,
        metrics_collector,
        Arc::clone(&fleet),
        Arc::new(config),
    )
    .await;

    let request = Request::builder()
        .method("PUT")
        .uri("/api/nodes/intruder/registration")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"address":"http://10.0.0.7:8086"}"#))
        .unwrap();
    let status = api.router.clone().oneshot(request).await.unwrap().status();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        fleet.is_empty(),
        "nothing may join the fleet through an unauthenticated call"
    );
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
        config_manager,
        command_executor,
        metrics_collector,
        runtime_manager,
        api_config,
    )
    .await;

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

    let config = ControlConfig {
        node_preshared_keys: vec!["node-psk".to_string()],
        ..Default::default()
    };
    let database = Arc::new(Database::init(":memory:").await.unwrap());

    let server = ControlServer::new(config.clone()).with_database(Arc::clone(&database));
    let sessions = server.sessions();
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
        config_manager,
        command_executor,
        metrics_collector,
        runtime_manager,
        Arc::new(config),
    )
    .await;
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
                secret: "node-psk".to_string(),
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

/// Pushing configuration to a node that is not connected is a fact about the
/// fleet, not a fault in the control plane.
///
/// Both push endpoints mapped every failure to 500, so "this node is not
/// here" and "this process is broken" were the same answer - the same defect
/// the command path had, one module over. A `deis-runtime` node never holds a
/// control channel at all, so this is the answer an operator gets whenever
/// they aim a push at one.
#[tokio::test]
async fn pushing_config_to_an_unconnected_node_is_a_404_not_a_500() {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let sessions = Arc::new(SessionManager::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));

    // Store something to push, so the push fails on "this node is not
    // connected" rather than on "there is nothing stored for it" - both are
    // 404, and only the first is the case under test.
    config_manager
        .update_config(
            "deis-runtime-1",
            std::collections::HashMap::from([(
                "app.max_instances".to_string(),
                serde_json::json!(20),
            )]),
        )
        .await
        .expect("the configuration is stored");

    let api = deis_control::api::build_api_router(
        sessions,
        database,
        Arc::clone(&config_manager),
        command_executor,
        metrics_collector,
        Arc::new(RuntimeNodeManager::new(Vec::new(), 60)),
        Arc::new(ControlConfig::default()),
    )
    .await;

    let post = |uri: &'static str| {
        let router = api.router.clone();
        let mut request = Request::builder()
            .method("POST")
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(local_peer());
        async move {
            let response = router.oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            (status, String::from_utf8(bytes.to_vec()).unwrap())
        }
    };

    let (status, body) = post("/api/configs/push/deis-runtime-1").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body.contains("control channel"),
        "the refusal must say why nothing could be pushed: {body}"
    );

    let (status, body) = post("/api/configs/push/deis-runtime-1/app.max_instances").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.contains("control channel"), "{body}");
}

/// The takeover, end to end, and what stops it now.
///
/// Three things that each read as reasonable put it together:
/// `api_auth_enabled` defaults to off; an unauthenticated caller is admitted
/// when the peer is on this machine; and the human API was wrapped in
/// `CorsLayer::permissive()`. A browser on the operator's box is a loopback
/// peer, so any page they visited could `fetch()` the control plane with no
/// credential, create an owner account, mint an unscoped root token, deploy,
/// and read the audit log - and read every answer, because
/// `access-control-allow-origin: *` is exactly the permission to read a
/// response.
#[tokio::test]
async fn a_web_page_on_another_origin_cannot_drive_the_control_plane() {
    let config = ControlConfig::default();
    assert!(
        !config.api_auth_enabled,
        "this test is about the default install, so it must be the default that is exercised"
    );
    assert!(
        config.api_cors_allowed_origins.is_empty(),
        "cross-origin access has to be off unless an operator names the origins"
    );
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let sessions = Arc::new(SessionManager::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));

    tokio::spawn(async move {
        let _ = deis_control::api::start_api_server(
            sessions,
            database,
            config_manager,
            command_executor,
            metrics_collector,
            config,
            18391,
        )
        .await;
    });
    wait_for_server_ready(18391).await;

    let client = reqwest::Client::new();
    let base = "http://127.0.0.1:18391";

    // 1. The preflight. Without an `access-control-allow-origin` naming the
    //    page's origin, the browser never sends the call at all, so a JSON
    //    body or any method past POST does not leave it.
    for (method, path) in [
        ("POST", "/api/accounts"),
        ("POST", "/api/accounts/attacker/tokens"),
        ("DELETE", "/api/runtime/node-1/apps/demo"),
    ] {
        let preflight = client
            .request(reqwest::Method::OPTIONS, format!("{base}{path}"))
            .header("Origin", "https://evil.example")
            .header("Access-Control-Request-Method", method)
            .header("Access-Control-Request-Headers", "content-type")
            .send()
            .await
            .expect("the preflight itself is answered, one way or another");
        assert!(
            preflight
                .headers()
                .get("access-control-allow-origin")
                .is_none(),
            "the preflight for {method} {path} told the page it may make the call"
        );
    }

    // 2. The call itself, made anyway - a page cannot do this, but nothing
    //    about the answer may depend on that. It is refused, and the refusal
    //    says what a console hosted elsewhere does instead.
    let created = client
        .post(format!("{base}/api/accounts"))
        .header("Origin", "https://evil.example")
        .header("Sec-Fetch-Site", "cross-site")
        .json(&serde_json::json!({"name": "attacker", "role": "owner"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 401);
    assert!(
        created
            .headers()
            .get("access-control-allow-origin")
            .is_none(),
        "even the refusal must not be readable by the page that caused it"
    );
    assert!(
        created
            .text()
            .await
            .unwrap()
            .contains("api_cors_allowed_origins"),
        "the refusal has to name what an operator changes"
    );

    // 3. And no account was created, so nothing downstream of it exists.
    let accounts = client
        .get(format!("{base}/api/accounts"))
        .send()
        .await
        .unwrap();
    assert_eq!(accounts.status(), 200, "the local operator is unaffected");
    let listed: serde_json::Value = accounts.json().await.unwrap();
    let names = serde_json::to_string(&listed).unwrap();
    assert!(!names.contains("attacker"), "{names}");

    // 4. The console this port serves is same-origin, so it is untouched:
    //    same call, marked the way a browser marks its own page.
    let same_origin = client
        .post(format!("{base}/api/accounts"))
        .header("Origin", base)
        .header("Sec-Fetch-Site", "same-origin")
        .json(&serde_json::json!({"name": "operator", "role": "owner"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        same_origin.status(),
        200,
        "the console served from this port stopped working"
    );
}

/// Reading the cluster's size, node count, uptime and event totals is a read
/// like any other. It used to be exempt from `api_auth_enabled` and from the
/// loopback rule at once, so any host that could route to the port had it.
#[tokio::test]
async fn a_metrics_scrape_needs_the_credential_the_rest_of_the_api_needs() {
    let config = ControlConfig {
        api_auth_enabled: true,
        api_keys: vec!["the-only-key".to_string()],
        ..Default::default()
    };
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let sessions = Arc::new(SessionManager::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));

    tokio::spawn(async move {
        let _ = deis_control::api::start_api_server(
            sessions,
            database,
            config_manager,
            command_executor,
            metrics_collector,
            config,
            18392,
        )
        .await;
    });
    wait_for_server_ready(18392).await;

    let client = reqwest::Client::new();
    let base = "http://127.0.0.1:18392";

    for path in ["/api/metrics", "/api/metrics/summary"] {
        let anonymous = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(
            anonymous.status(),
            401,
            "{path} was served with no credential by a control plane that demands one"
        );

        let scrape = client
            .get(format!("{base}{path}"))
            .header("Authorization", "Bearer the-only-key")
            .send()
            .await
            .unwrap();
        assert_eq!(scrape.status(), 200, "{path} refused a valid credential");
    }

    // The probe is still the one thing in front of the gate: it answers a
    // constant, and the orchestrator asking has nothing to present.
    let health = client.get(format!("{base}/health")).send().await.unwrap();
    assert_eq!(health.status(), 200);
    assert_eq!(health.text().await.unwrap(), "OK");
}

/// The way back for an operator who does host a console somewhere else: name
/// the origin, and mint it a credential. Naming it is what lets the browser
/// read the answers; the credential is what gets past the gate, because the
/// admission that needs none is for this machine's own tools.
#[tokio::test]
async fn a_named_origin_is_answered_and_every_other_origin_is_not() {
    let config = ControlConfig {
        api_cors_allowed_origins: vec![
            "https://console.example.com".to_string(),
            // The two spellings that cannot work, next to the one that can:
            // neither may take the good entry down with it.
            "*".to_string(),
            "https://trailing.example.com/".to_string(),
        ],
        api_keys: vec!["console-key".to_string()],
        api_auth_enabled: true,
        ..Default::default()
    };
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let sessions = Arc::new(SessionManager::new());
    let config_manager = Arc::new(ConfigManager::new(
        Arc::clone(&database),
        Arc::clone(&sessions),
    ));
    let command_executor = Arc::new(CommandExecutor::new(
        Arc::clone(&sessions),
        Arc::clone(&database),
    ));
    let metrics_collector = Arc::new(MetricsCollector::new(Arc::clone(&sessions)));

    tokio::spawn(async move {
        let _ = deis_control::api::start_api_server(
            sessions,
            database,
            config_manager,
            command_executor,
            metrics_collector,
            config,
            18393,
        )
        .await;
    });
    wait_for_server_ready(18393).await;

    let client = reqwest::Client::new();
    let base = "http://127.0.0.1:18393";

    // The named origin is preflighted for itself, by name and not by `*`.
    let preflight = client
        .request(reqwest::Method::OPTIONS, format!("{base}/api/apps"))
        .header("Origin", "https://console.example.com")
        .header("Access-Control-Request-Method", "GET")
        .header("Access-Control-Request-Headers", "authorization")
        .send()
        .await
        .unwrap();
    assert_eq!(
        preflight
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://console.example.com")
    );
    // `Vary: origin`, or a cache in front of this could hand one origin the
    // answer that was written for another.
    assert!(
        preflight
            .headers()
            .get_all("vary")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| v.to_ascii_lowercase().contains("origin"))
    );

    // And its calls are readable, with the credential the API requires.
    let listed = client
        .get(format!("{base}/api/apps"))
        .header("Origin", "https://console.example.com")
        .header("Sec-Fetch-Site", "cross-site")
        .header("Authorization", "Bearer console-key")
        .send()
        .await
        .unwrap();
    assert_eq!(listed.status(), 200);
    assert_eq!(
        listed
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://console.example.com")
    );

    // Nobody else, including the two entries that could never work.
    for other in [
        "https://evil.example",
        "https://trailing.example.com",
        "http://console.example.com",
    ] {
        let preflight = client
            .request(reqwest::Method::OPTIONS, format!("{base}/api/apps"))
            .header("Origin", other)
            .header("Access-Control-Request-Method", "GET")
            .send()
            .await
            .unwrap();
        assert!(
            preflight
                .headers()
                .get("access-control-allow-origin")
                .is_none(),
            "{other} was allowed to read this control plane's answers"
        );
    }
}
