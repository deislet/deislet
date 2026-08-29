//! What a node tells the control plane about itself, and what it does when the
//! control plane will not hear it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::put;
use axum::{Json, Router};

use super::*;

/// A stand-in control plane that records what it was told.
struct Plane {
    base: String,
    seen: Arc<parking_lot::Mutex<Vec<serde_json::Value>>>,
    calls: Arc<AtomicUsize>,
    _task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct PlaneState {
    seen: Arc<parking_lot::Mutex<Vec<serde_json::Value>>>,
    calls: Arc<AtomicUsize>,
    status: StatusCode,
    required_key: Option<String>,
}

impl Plane {
    async fn answering(status: StatusCode, required_key: Option<&str>) -> Self {
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let state = PlaneState {
            seen: seen.clone(),
            calls: calls.clone(),
            status,
            required_key: required_key.map(str::to_string),
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        // The route, the method and the answer are deis-control's, taken from
        // the shared constant rather than written out here. A stand-in shaped
        // like this crate's own assumption is how the two halves came to
        // disagree in the first place: the node called `POST /register` while
        // the control plane served `PUT /registration`, both sides compiled,
        // both sides' tests passed, and no node ever registered.
        let router = Router::new()
            .route(
                deis_wire::conventions::NODE_REGISTRATION_ROUTE,
                put(
                    |State(state): State<PlaneState>,
                     headers: HeaderMap,
                     body: Json<serde_json::Value>| async move {
                        state.calls.fetch_add(1, Ordering::SeqCst);
                        if let Some(required) = &state.required_key {
                            let presented = headers
                                .get(axum::http::header::AUTHORIZATION)
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_string();
                            if presented != format!("Bearer {required}") {
                                return StatusCode::UNAUTHORIZED.into_response();
                            }
                        }
                        state.seen.lock().push(body.0);
                        if state.status.is_success() {
                            return (
                                state.status,
                                Json(serde_json::json!({
                                    "node_id": "node-a",
                                    "address": "http://example:8123",
                                    "outcome": "new",
                                    "source": "registered",
                                    "refresh_after_seconds": 30,
                                    "registration_ttl_seconds": 90,
                                    "assignment_refresh_seconds": 30,
                                })),
                            )
                                .into_response();
                        }
                        state.status.into_response()
                    },
                ),
            )
            .with_state(state);

        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            base,
            seen,
            calls,
            _task: task,
        }
    }
}

fn registrar(base: &str, node_id: &str, secret: Option<&str>) -> NodeRegistrar {
    NodeRegistrar::new(
        base,
        NodeRegistration::of_this_node(node_id, 8123),
        Duration::from_secs(5),
        secret.map(|secret| NodeCredential::new(secret, Some(base))),
    )
    .expect("the registrar is built")
}

#[tokio::test]
async fn a_node_tells_the_control_plane_where_to_dial_it() {
    let plane = Plane::answering(StatusCode::OK, None).await;

    let outcome = registrar(&plane.base, "node-a", None)
        .await_registered()
        .await;
    assert_eq!(
        outcome,
        Registered::Accepted {
            refresh_after: Duration::from_secs(30)
        },
        "the refresh interval is the control plane's to set, and the node has to carry it"
    );

    let seen = plane.seen.lock();
    assert_eq!(seen.len(), 1);
    let body = &seen[0];
    // deis-control reads exactly two fields, and `address` has to be dialable
    // as it stands: it is what every deployment push and cron fire goes to.
    let address = body["address"]
        .as_str()
        .unwrap_or_else(|| panic!("the control plane cannot dial this node back: {body}"));
    assert!(
        address.starts_with("http://") && address.ends_with(":8123"),
        "deis-control refuses an address without a scheme and a host: {address}"
    );
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn the_registration_url_is_the_one_deis_control_serves() {
    // The path and the method are the whole contract here, and neither is
    // checked by the compiler. This asserts the URL a node actually calls, so
    // that changing either side alone fails here instead of in a fleet that
    // quietly never learns about its nodes.
    let node = registrar("http://control.example", "node-a", None);
    assert_eq!(
        node.url(),
        format!(
            "http://control.example{}",
            deis_wire::conventions::node_registration_path("node-a")
        )
    );
    assert!(
        node.url().ends_with("/api/nodes/node-a/registration"),
        "deis-control serves /registration, not /register: {}",
        node.url()
    );
}

#[tokio::test]
async fn an_answer_without_a_refresh_interval_still_gets_one() {
    // A control plane that answers 200 with nothing useful must not leave the
    // node believing it is registered for ever: the registration is a lease,
    // and never refreshing it drops the node out of the fleet silently.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new().route(
        deis_wire::conventions::NODE_REGISTRATION_ROUTE,
        put(|| async { Json(serde_json::json!({})) }),
    );
    let _task = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    match registrar(&base, "node-a", None).await_registered().await {
        Registered::Accepted { refresh_after } => {
            assert!(
                refresh_after > Duration::ZERO && refresh_after < Duration::from_secs(90),
                "the fallback has to sit inside deis-control's default 90s TTL, got {refresh_after:?}"
            );
        }
        other => panic!("expected an accepted registration, got {other:?}"),
    }
}

#[tokio::test]
async fn registration_presents_the_node_credential() {
    // The node endpoints are guarded by node_preshared_keys, independent of
    // the human API's api_auth_enabled. A registration that went out
    // anonymous would be refused and the node would never appear in the
    // fleet.
    let plane = Plane::answering(StatusCode::OK, Some("node-key")).await;

    let outcome = registrar(&plane.base, "node-a", Some("node-key"))
        .await_registered()
        .await;
    assert!(matches!(outcome, Registered::Accepted { .. }));
    assert_eq!(plane.seen.lock().len(), 1);
}

#[tokio::test]
async fn a_refused_credential_is_an_error_the_caller_retries() {
    let plane = Plane::answering(StatusCode::OK, Some("node-key")).await;

    let error = registrar(&plane.base, "node-a", Some("wrong-key"))
        .register()
        .await
        .expect_err("the control plane refuses the wrong key");
    assert!(
        matches!(error, RegistrationError::Status { code: 401, .. }),
        "{error}"
    );
    assert!(plane.seen.lock().is_empty());
}

#[tokio::test]
async fn a_control_plane_without_the_endpoint_is_a_permanent_answer() {
    // Retrying this every reconcile interval would fill the log of every node
    // in the fleet with a line about a feature the other side does not have.
    let plane = Plane::answering(StatusCode::OK, None).await;
    // Nothing is routed at this path, so axum answers 404.
    let registrar = NodeRegistrar::new(
        &format!("{}/nowhere", plane.base),
        NodeRegistration::of_this_node("node-a", 8123),
        Duration::from_secs(5),
        None,
    )
    .expect("the registrar is built");

    assert_eq!(
        registrar.register().await.expect("404 is not an error"),
        Registered::NotSupported
    );
    assert_eq!(plane.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_control_plane_that_is_not_there_is_a_transport_failure() {
    // Nothing listens on port 1.
    let registrar = registrar("http://127.0.0.1:1", "node-a", None);
    let error = registrar
        .register()
        .await
        .expect_err("nothing is listening");
    assert!(
        matches!(error, RegistrationError::Transport { .. }),
        "{error}"
    );
}

/// Small helper so the happy-path tests read as one statement.
impl NodeRegistrar {
    async fn await_registered(&self) -> Registered {
        self.register().await.expect("the control plane answers")
    }
}
