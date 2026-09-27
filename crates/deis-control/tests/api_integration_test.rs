use deis_control::capability_issuer::CapabilityIssuer;
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

/// Mint the credential a test presents to the assembled router: an unscoped
/// owner token, which is what the operator ends up holding after a fresh
/// login. The bootstrap account is the one every installation has, so the
/// token hangs off that.
async fn mint_owner_credential(database: &Arc<Database>) -> String {
    let identity = IdentityStore::new(Arc::clone(database));
    identity
        .create_account("bootstrap", Role::Owner)
        .await
        .expect("the bootstrap account was created");
    let (_token, secret) = identity
        .create_token("bootstrap", "test-operator", Authority::Unscoped)
        .await
        .expect("the operator credential was minted");
    secret
}

/// The `Authorization` header value for a credential.
fn bearer(secret: &str) -> String {
    format!("Bearer {secret}")
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
    let credential = mint_owner_credential(&database).await;
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
    // which is a different list at /api/platform/runtime.
    let client = reqwest::Client::new();
    let response = client
        .get("http://localhost:18082/api/platform/sessions")
        .header("Authorization", bearer(&credential))
        .send()
        .await
        .expect("Failed to call the sessions endpoint");

    assert_eq!(response.status(), 200);

    let json: serde_json::Value = response.json().await.unwrap();
    assert!(json.get("sessions").is_some());
    assert!(json.get("total").is_some());

    let fleet = client
        .get("http://localhost:18082/api/platform/runtime")
        .header("Authorization", bearer(&credential))
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
    let credential = mint_owner_credential(&database).await;
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
        .get("http://localhost:18083/api/platform/metrics/summary")
        .header("Authorization", bearer(&credential))
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
    let credential = mint_owner_credential(&database).await;
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
        .get("http://localhost:18084/api/platform/events")
        .header("Authorization", bearer(&credential))
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
/// The value this installation's fleet key used to be. Nothing configures it
/// any more; it is passed where a node credential used to be passed, so that
/// the tests say "whatever string a caller brings" rather than leaving the
/// case untested.
const RETIRED_FLEET_KEY: &str = "node-psk";

/// The human listener serves the human API and nothing a node used to call.
/// The paths a node used to call are not mounted here at all, so the question
/// "does some other credential reach them" has no subject: there is nothing
/// on this port for any credential to open.
///
/// The string below is the value this installation's fleet key used to be. It
/// is passed deliberately, because what is being asserted is that it is now
/// just a string - one more credential this listener has never heard of.
#[tokio::test]
async fn the_human_listener_serves_the_api_key_and_nothing_a_node_used_to_call() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    let config = ControlConfig::default();
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let credential = mint_owner_credential(&database).await;
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
    CapabilityIssuer::ephemeral("spiffe://deislet/control/test").await,
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

    // The assignment list is not on this listener at all any more. It carries
    // every environment's decrypted secrets, and it is served on the machine
    // channel, where a caller proves a name and may only ask for its own.
    //
    // 404 and not 401 is the assertion that matters: 401 would say a
    // credential could open it, and none can - there is nothing here to open.
    // That holds for the dashboard key, for the fleet key that used to work,
    // and for no credential at all.
    const ASSIGNMENTS: &str = "/api/nodes/node-a/assignments";

    // Authenticated as an operator, and the path is simply not here: 404 from
    // the fallback. That is the assertion that matters, and it is stronger
    // than the 401 this used to make - a 401 says a credential could open it,
    // and this says there is nothing to open. The list carries every
    // environment's decrypted secrets and is served on the machine channel,
    // where a caller proves a name and may only ask for its own.
    assert_eq!(
        status_of(ASSIGNMENTS, Some(&credential)).await,
        StatusCode::NOT_FOUND,
        "the fleet's secrets are still reachable from the listener a browser can reach"
    );

    // The other two never reach the routing table to find that out: this
    // listener authenticates before it looks, so no credential and a
    // credential that is not an operator's are both refused at the gate.
    // `RETIRED_FLEET_KEY` is in that second class now, which is the point.
    for credential in [None, Some(RETIRED_FLEET_KEY)] {
        assert_eq!(
            status_of(ASSIGNMENTS, credential).await,
            StatusCode::UNAUTHORIZED,
            "with credential {credential:?}"
        );
    }

    // And the other way round: the human API is the API key's, not the
    // node's.
    assert_eq!(
        status_of("/api/v1/apps", Some(&credential)).await,
        StatusCode::OK
    );
    assert_eq!(
        status_of("/api/v1/apps", Some(RETIRED_FLEET_KEY)).await,
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
    // Registration is what makes a machine a delivery target, and it is not
    // on this listener either. An operator's own credential gets 404 - there
    // is nothing here to be authorised for - and the two that are not an
    // operator's are refused at the gate before that question is reached.
    // What used to be the interesting line, the fleet key answering 200, is
    // gone with the route.
    assert_eq!(register(Some(&credential)).await, StatusCode::NOT_FOUND);
    for credential in [None, Some(RETIRED_FLEET_KEY)] {
        assert_eq!(
            register(credential).await,
            StatusCode::UNAUTHORIZED,
            "with credential {credential:?}"
        );
    }
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

    // The shape of a fresh install: a data directory, and no live credential
    // until the startup step mints the one-time bootstrap credential.
    let data_dir = tempfile::tempdir().expect("the data directory was created");
    let config = ControlConfig {
        data_dir: data_dir.path().to_path_buf(),
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
    CapabilityIssuer::ephemeral("spiffe://deislet/control/test").await,
    )
    .await;
    // The same startup step the binary runs: it mints the one-time bootstrap
    // credential and writes it into the data directory.
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

    // The bootstrap credential is on the record: read it out of the file the
    // startup step wrote, the way `deis login --bootstrap` does.
    let bootstrap_file = data_dir.path().join("bootstrap.token");
    let bootstrap_secret = std::fs::read_to_string(&bootstrap_file)
        .expect("the bootstrap credential file was written")
        .trim()
        .to_string();

    // Its one successful use: mint the operator's standing token off the
    // bootstrap account, unscoped, the way a first login does. The gate then
    // revokes the bootstrap credential and deletes its file.
    let (status, minted_owner) = call(
        "POST",
        "/api/v1/accounts/bootstrap/tokens",
        Some(&bootstrap_secret),
        r#"{"name":"owner","unscoped":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{minted_owner}");
    let owner_secret = minted_owner["secret"].as_str().unwrap().to_string();

    // One-time means one-time: the file is gone, and the credential itself
    // answers nothing from its very next use.
    assert!(
        !bootstrap_file.exists(),
        "the bootstrap file must be deleted after the first successful use"
    );
    let (status, refused) = call("GET", "/api/v1/tokens", Some(&bootstrap_secret), "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{refused}");

    // The operator's credential is listed, and listed as unscoped: that count
    // is what an operator drives to zero, and it is the total of the
    // `scope=unscoped` listing in the unified envelope.
    let (status, listed) = call("GET", "/api/v1/tokens", Some(&owner_secret), "").await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["schemaVersion"], 1);
    assert_eq!(listed["page"]["total"], 1, "{listed}");
    assert_eq!(listed["items"][0]["origin"], "minted");
    let (status, unscoped) = call(
        "GET",
        "/api/v1/tokens?scope=unscoped&limit=1",
        Some(&owner_secret),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{unscoped}");
    assert_eq!(unscoped["page"]["total"], 1);

    // An account and a token for one environment of one application.
    let (status, account) = call(
        "POST",
        "/api/v1/accounts",
        Some(&owner_secret),
        r#"{"name":"ci","role":"deployer"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{account}");
    assert_eq!(account["role"], "deployer");

    let (status, minted) = call(
        "POST",
        "/api/v1/accounts/ci/tokens",
        Some(&owner_secret),
        r#"{"name":"preview-pipeline","scopes":["deploy:demo/preview"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{minted}");
    let secret = minted["secret"].as_str().unwrap().to_string();
    let token_id = minted["token"]["id"].as_str().unwrap().to_string();
    assert_eq!(minted["token"]["scopes"], "deploy:demo/preview");

    // The secret exists in that one response and nowhere else: a listing
    // afterwards must not carry it.
    let (_, listed) = call("GET", "/api/v1/tokens", Some(&owner_secret), "").await;
    assert!(
        !listed.to_string().contains(&secret),
        "a token listing must never carry a credential"
    );

    // What the scoped token may do, and what it may not, on the real router.
    let (status, _) = call("POST", "/api/v1/apps", Some(&secret), r#"{"id":"demo"}"#).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a CI credential creates its own app"
    );

    let (status, refused) = call(
        "POST",
        "/api/v1/apps",
        Some(&secret),
        r#"{"id":"someone-elses"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(refused["error"]["code"], "insufficient-scope");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("deploy:someone-elses"),
        "{refused}"
    );

    for uri in ["/api/v1/audit", "/api/v1/tokens", "/api/v1/accounts"] {
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
        &format!("/api/v1/tokens/{token_id}"),
        Some(&owner_secret),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call("GET", "/api/v1/apps/demo", Some(&secret), "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // And all of it is on the record, readable through the route the CLI
    // reads: the application that was created, the refusal, and the mint.
    let (status, log) = call("GET", "/api/v1/audit?limit=100", Some(&owner_secret), "").await;
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
    let (status, _) = call("DELETE", "/api/v1/audit", Some(&owner_secret), "").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    let (_, after) = call("GET", "/api/v1/audit?limit=100", Some(&owner_secret), "").await;
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
    let credential = mint_owner_credential(&database).await;
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
    CapabilityIssuer::ephemeral("spiffe://deislet/control/test").await,
    )
    .await;

    let post = |body: &'static str| {
        let router = api.router.clone();
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/platform/commands/execute/deis-proxy-1")
            .header("content-type", "application/json")
            .header("authorization", bearer(&credential))
            .body(Body::from(body))
            .unwrap();
        // The address the real server always attaches
        // (`into_make_service_with_connect_info`), and the operator credential
        // the dashboard presents.
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

/// The listener a browser reaches must not be a way onto the fleet, whatever
/// it answers.
///
/// This began as a narrower claim - that a control plane naming no node key
/// must not serve the node endpoints, because "unconfigured" used to mean
/// "unguarded" and anyone who could reach the port could inject a node. Both
/// the key and those endpoints are gone, so the narrow claim has no subject.
/// The wide one is what mattered and is asserted below: a machine gets onto
/// the fleet by proving a certificate on the machine channel, and there is no
/// second door.
#[tokio::test]
async fn nothing_joins_the_fleet_through_the_human_listener() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

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
    let fleet = Arc::new(RuntimeNodeManager::new(Vec::new(), 60));
    let api = deis_control::api::build_api_router(
        sessions,
        database,
        config_manager,
        command_executor,
        metrics_collector,
        Arc::clone(&fleet),
        Arc::new(config),
    CapabilityIssuer::ephemeral("spiffe://deislet/control/test").await,
    )
    .await;

    let request = Request::builder()
        .method("PUT")
        .uri("/api/nodes/intruder/registration")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"address":"http://10.0.0.7:8086"}"#))
        .unwrap();
    let status = api.router.clone().oneshot(request).await.unwrap().status();
    // Refused at the gate, before any question of which route this is: the
    // registration endpoint is not on this listener at all now, and an
    // unauthenticated caller does not get far enough to be told so. What the
    // test is really about is the line below - whatever this listener
    // answers, nothing joined the fleet through it.
    assert_eq!(status, StatusCode::UNAUTHORIZED);
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
    CapabilityIssuer::ephemeral("spiffe://deislet/control/test").await,
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

/// One port carries two protocols, and admits a machine on neither.
///
/// The REST API and the control gRPC service are mounted on the same
/// listener, and hyper tells them apart by sniffing the HTTP/2 preface rather
/// than by anything configured. This is the round trip that proves the
/// serving path still carries both - a REST call and a full gRPC handshake
/// against one address, with the drain wired in.
///
/// It used to end by asserting the handshake succeeded, because a fleet key
/// in that message was what admitted a node. What admits one now is a
/// certificate proved on the connection, and this connection is plaintext on
/// the listener a browser reaches, so the handshake is answered and refused.
/// Asserted here as well as in `api::grpc`'s own tests because this is the
/// assembled router - the shape the binary serves - and a shared port is
/// exactly where a second way in would appear without anyone deciding to add
/// one.
#[tokio::test]
async fn one_port_serves_both_rest_and_the_node_grpc_channel() {
    use deis_wire::pb::{
        AuthenticateRequest, ControlMessage, control_message::Payload,
        control_service_client::ControlServiceClient,
    };

    let config = ControlConfig {
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
    CapabilityIssuer::ephemeral("spiffe://deislet/control/test").await,
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
                secret: RETIRED_FLEET_KEY.to_string(),
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
        Some(Payload::AuthResponse(response)) => assert!(
            !response.success,
            "the shared port carries the protocol but admits nobody on it: {}",
            response.message
        ),
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
    let credential = mint_owner_credential(&database).await;
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
    CapabilityIssuer::ephemeral("spiffe://deislet/control/test").await,
    )
    .await;

    let post = |uri: &'static str| {
        let router = api.router.clone();
        let mut request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("authorization", bearer(&credential))
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

    let (status, body) = post("/api/platform/configs/push/deis-runtime-1").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body.contains("control channel"),
        "the refusal must say why nothing could be pushed: {body}"
    );

    let (status, body) = post("/api/platform/configs/push/deis-runtime-1/app.max_instances").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.contains("control channel"), "{body}");
}

/// The takeover, end to end, and what stops it now.
///
/// Two things that each read as reasonable put the old one together: an
/// unauthenticated caller was admitted when the peer was on this machine, and
/// the human API was wrapped in `CorsLayer::permissive()`. Any page the
/// operator visited could `fetch()` the control plane, create an owner
/// account, mint an unscoped root token, deploy, and read the audit log - and
/// read every answer, because `access-control-allow-origin: *` is exactly the
/// permission to read a response. Now the gate demands a credential from
/// every caller and CORS names no origin unless the operator names one.
#[tokio::test]
async fn a_web_page_on_another_origin_cannot_drive_the_control_plane() {
    let config = ControlConfig::default();
    assert!(
        config.api_cors_allowed_origins.is_empty(),
        "cross-origin access has to be off unless an operator names the origins"
    );
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    // The operator's own credential, minted the way a first login does.
    let credential = mint_owner_credential(&database).await;
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
        ("POST", "/api/v1/accounts"),
        ("POST", "/api/v1/accounts/attacker/tokens"),
        ("DELETE", "/api/platform/runtime/node-1/apps/demo"),
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
        .post(format!("{base}/api/v1/accounts"))
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
            .contains("A valid credential is required"),
        "the refusal has to say what is missing"
    );

    // 3. And no account was created, so nothing downstream of it exists. The
    //    operator, presenting the credential, sees the same accounts as
    //    before; without one, even this machine's own tools are refused.
    let accounts = client
        .get(format!("{base}/api/v1/accounts"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        accounts.status(),
        401,
        "a caller with no credential is refused, from this machine or anywhere"
    );
    let accounts = client
        .get(format!("{base}/api/v1/accounts"))
        .header("Authorization", bearer(&credential))
        .send()
        .await
        .unwrap();
    assert_eq!(accounts.status(), 200, "the local operator is unaffected");
    let listed: serde_json::Value = accounts.json().await.unwrap();
    let names = serde_json::to_string(&listed).unwrap();
    assert!(!names.contains("attacker"), "{names}");

    // 4. The console this port serves is same-origin, but same-origin is no
    //    longer an admission: the page the console is still needs the
    //    operator's credential, and a page that forges the browser's own
    //    marking without one gets the same refusal as the attacker's page.
    let same_origin = client
        .post(format!("{base}/api/v1/accounts"))
        .header("Origin", base)
        .header("Sec-Fetch-Site", "same-origin")
        .json(&serde_json::json!({"name": "operator", "role": "owner"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        same_origin.status(),
        401,
        "same-origin without a credential is refused like anything else"
    );
}

/// Reading the cluster's size, node count, uptime and event totals is a read
/// like any other. It used to be exempt from `api_auth_enabled` and from the
/// loopback rule at once, so any host that could route to the port had it.
#[tokio::test]
async fn a_metrics_scrape_needs_the_credential_the_rest_of_the_api_needs() {
    let config = ControlConfig::default();
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let credential = mint_owner_credential(&database).await;
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

    for path in ["/api/metrics", "/api/platform/metrics/summary"] {
        let anonymous = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(
            anonymous.status(),
            401,
            "{path} was served with no credential by a control plane that demands one"
        );

        let scrape = client
            .get(format!("{base}{path}"))
            .header("Authorization", bearer(&credential))
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
        ..Default::default()
    };
    let database = Arc::new(Database::init(":memory:").await.unwrap());
    let credential = mint_owner_credential(&database).await;
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
        .request(reqwest::Method::OPTIONS, format!("{base}/api/v1/apps"))
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
        .get(format!("{base}/api/v1/apps"))
        .header("Origin", "https://console.example.com")
        .header("Sec-Fetch-Site", "cross-site")
        .header("Authorization", bearer(&credential))
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
            .request(reqwest::Method::OPTIONS, format!("{base}/api/v1/apps"))
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
