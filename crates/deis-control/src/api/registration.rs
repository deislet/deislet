//! Nodes announcing themselves.
//!
//! Discovery used to run backwards: the control plane learned which nodes
//! existed only from its own configuration file, so adding a machine meant
//! editing that file and restarting, while the machine itself already knew
//! everything needed - who it is and where its management port is. The pull
//! half of the design has worked for a while (a node asks for its assignments
//! and fetches its own bundles, architecture principle 8); this is the other
//! half, and it uses the credential that channel already uses.
//!
//! What registering buys the node: it becomes a delivery target, so a deploy
//! reaches it as a push instead of only on its next reconcile, and it becomes
//! eligible to run cron occurrences. What it costs: the node has to keep
//! saying it is there. A registration expires after
//! `node_registration_ttl_secs` without a refresh, and the answer to a
//! registration says how often to come back.
//!
//! Registration is *not* how a node gets work. Nothing here writes to the
//! catalog, and a node that never registers still serves every application it
//! pulls; it is only invisible to pushes and to cron.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};

use crate::api::middleware::{NodeAuthConfig, node_auth};
use crate::runtime_client::{
    Deregistered, NodeSource, Registered, RegistrationError, RuntimeNodeManager,
};

#[derive(Clone)]
pub struct RegistrationState {
    pub nodes: Arc<RuntimeNodeManager>,
    /// The credential this route is mounted behind, kept here so
    /// [`node_router`] can apply the layer itself and no caller can mount
    /// the route without it.
    pub node_auth: NodeAuthConfig,
}

/// What a node sends about itself.
#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    /// gRPC address of this node's management plane, as the control plane has
    /// to dial it: `http://host:port`.
    ///
    /// The node reports it rather than the control plane inferring it from
    /// the connection, because the port that answers management calls is not
    /// the port the registration arrived on, and a node behind a forwarder
    /// knows the address that reaches it while the socket does not.
    pub address: String,
    /// Version of the node process. Only used for display until the first
    /// health probe confirms it.
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub node_id: String,
    pub address: String,
    /// `new`, `address_changed` or `refreshed`.
    pub outcome: &'static str,
    /// `configured` or `registered`: whether this id is also in the control
    /// plane's file, which is what decides that it never expires.
    pub source: NodeSource,
    /// Seconds after which the node should register again.
    ///
    /// The same timer can carry the assignment reconcile - see
    /// `assignment_refresh_seconds`, which is deliberately the same number -
    /// so a node needs one loop and not two.
    pub refresh_after_seconds: u64,
    /// Seconds without a refresh after which this registration is dropped.
    pub registration_ttl_seconds: u64,
    /// How often the control plane expects this node to reconcile against
    /// `GET /api/nodes/{id}/assignments`.
    pub assignment_refresh_seconds: u64,
}

/// PUT /api/nodes/:id/registration
///
/// Authenticated as a node, like the assignment list and the bundle download:
/// it is the same fleet credential, and an unauthenticated caller must not be
/// able to add a delivery target that would then be handed every environment's
/// decrypted secrets on its first assignment fetch.
pub async fn register(
    State(state): State<RegistrationState>,
    Path(node_id): Path<String>,
    Json(request): Json<RegisterRequest>,
) -> impl IntoResponse {
    let outcome = match state
        .nodes
        .register_claim(
            &node_id,
            &request.address,
            request.version.clone(),
            Utc::now(),
        )
        .await
    {
        Ok(outcome) => outcome,
        Err(e) => {
            warn!(
                node = %node_id,
                address = %request.address,
                error = %e,
                "Refused a node registration; this node is not a delivery target and takes no cron fire, though it can still pull its own assignments"
            );
            let status = match e {
                RegistrationError::EmptyId | RegistrationError::InvalidId => {
                    StatusCode::BAD_REQUEST
                }
                RegistrationError::InvalidAddress { .. } => StatusCode::BAD_REQUEST,
                // Not the caller's request being malformed: the request is
                // fine and the name in it belongs to somebody else. 409 is
                // what a node's own log needs to show, because the operator
                // reading it has to change a node id on one of two machines.
                RegistrationError::IdHeldByAnotherMachine { .. } => StatusCode::CONFLICT,
            };
            return (status, Json(json!({ "error": e.to_string() }))).into_response();
        }
    };

    let source = state
        .nodes
        .list()
        .into_iter()
        .find(|view| view.id == node_id)
        .map(|view| view.source)
        // Only a deregistration racing this call gets here; the node was
        // registered a moment ago, so that is what it is told.
        .unwrap_or(NodeSource::Registered);

    match outcome {
        Registered::New => info!(
            node = %node_id,
            address = %request.address,
            version = ?request.version,
            fleet_size = state.nodes.len(),
            "A node registered itself; it now receives deployment pushes and is eligible to run cron occurrences"
        ),
        Registered::AddressChanged => warn!(
            node = %node_id,
            address = %request.address,
            "A known node registered at a different address and the address it had was asked first: nothing answers there for this node any more, so the move is real. Every management call now goes to the new address"
        ),
        Registered::Refreshed => tracing::debug!(
            node = %node_id,
            address = %request.address,
            "Node registration refreshed"
        ),
    }

    let refresh = state.nodes.registration_refresh_interval().as_secs();
    (
        StatusCode::OK,
        Json(RegisterResponse {
            node_id,
            address: request.address,
            outcome: match outcome {
                Registered::New => "new",
                Registered::AddressChanged => "address_changed",
                Registered::Refreshed => "refreshed",
            },
            source,
            refresh_after_seconds: refresh,
            registration_ttl_seconds: state.nodes.registration_ttl().as_secs(),
            assignment_refresh_seconds: refresh,
        }),
    )
        .into_response()
}

/// DELETE /api/nodes/:id/registration
///
/// A node that is stopping on purpose. Without it the control plane keeps the
/// entry until it expires and spends that window pushing deployments and cron
/// fires at a process that has gone.
///
/// A node named in the configuration file is not removed: the file says it
/// exists, and a running fleet that disagrees with the file until somebody
/// restarts the process is worse than one stale entry.
pub async fn deregister(
    State(state): State<RegistrationState>,
    Path(node_id): Path<String>,
) -> impl IntoResponse {
    match state.nodes.deregister(&node_id) {
        Some(Deregistered::FromFleet) => {
            info!(
                node = %node_id,
                fleet_size = state.nodes.len(),
                "A node deregistered itself; it receives no further deployment push or cron fire until it registers again"
            );
            (
                StatusCode::OK,
                Json(json!({ "node_id": node_id, "removed": true })),
            )
        }
        Some(Deregistered::Forgotten) => {
            info!(
                node = %node_id,
                lost_nodes = state.nodes.departed().len(),
                "Cleared the record of a node this control plane had lost; deployments and application deletions stop reporting it as unreached. If that machine is in fact still running, it is still serving what it last received"
            );
            (
                StatusCode::OK,
                Json(json!({ "node_id": node_id, "removed": true })),
            )
        }
        Some(Deregistered::Configured) => {
            warn!(
                node = %node_id,
                "A node that is named in deis_runtime_nodes asked to be deregistered; it stays on the fleet list because the configuration file says it exists, so pushes and cron fires keep being aimed at it and will fail while it is down"
            );
            (
                StatusCode::CONFLICT,
                Json(json!({
                    "node_id": node_id,
                    "removed": false,
                    "error": "this node is listed in deis_runtime_nodes; remove it from the control plane's configuration instead"
                })),
            )
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "node_id": node_id, "removed": false })),
        ),
    }
}

/// The node-facing registration routes, with their own authentication layer
/// applied here so there is no way to mount them without it.
pub fn node_router(state: RegistrationState) -> axum::Router {
    let auth = state.node_auth.clone();
    axum::Router::new()
        .route(
            deis_wire::conventions::NODE_REGISTRATION_ROUTE,
            axum::routing::put(register).delete(deregister),
        )
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(auth, node_auth))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use serde_json::Value;
    use tower::ServiceExt;

    fn router(
        node_keys: Vec<String>,
        configured: Vec<crate::config::RuntimeNodeConfig>,
    ) -> (axum::Router, Arc<RuntimeNodeManager>) {
        let nodes = Arc::new(RuntimeNodeManager::new_with_ttl(configured, 15, 90));
        let state = RegistrationState {
            nodes: Arc::clone(&nodes),
            node_auth: NodeAuthConfig::new(node_keys),
        };
        (node_router(state), nodes)
    }

    async fn call(
        router: &axum::Router,
        method: &str,
        uri: &str,
        credential: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(credential) = credential {
            builder = builder.header("Authorization", format!("Bearer {credential}"));
        }
        let body = if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        };
        let response = router
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// The point of the whole endpoint: a node the control plane never heard
    /// of becomes a real member of the fleet, so a deployment push and a cron
    /// fire can be aimed at it.
    #[tokio::test]
    async fn a_node_that_registers_joins_the_fleet() {
        let (router, nodes) = router(vec!["node-psk".to_string()], Vec::new());
        assert!(nodes.is_empty());

        let (status, body) = call(
            &router,
            "PUT",
            "/api/nodes/node-a/registration",
            Some("node-psk"),
            json!({"address": "http://10.0.0.7:8086", "version": "0.1.0"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["outcome"], "new");
        assert_eq!(body["source"], "registered");
        assert_eq!(body["refresh_after_seconds"], 30);
        assert_eq!(body["assignment_refresh_seconds"], 30);

        assert_eq!(nodes.ids(), vec!["node-a".to_string()]);
        assert_eq!(nodes.call_order(0), vec!["node-a".to_string()]);
        assert_eq!(nodes.node("node-a").unwrap().addr, "http://10.0.0.7:8086");
        // The dashboard sees it, with the version it reported before any
        // health probe has run.
        let view = &nodes.list()[0];
        assert_eq!(view.source, NodeSource::Registered);
        assert_eq!(view.health.version.as_deref(), Some("0.1.0"));
    }

    #[tokio::test]
    async fn re_registering_moves_the_address_and_refreshes_the_lease() {
        let (router, nodes) = router(vec!["node-psk".to_string()], Vec::new());
        call(
            &router,
            "PUT",
            "/api/nodes/node-a/registration",
            Some("node-psk"),
            json!({"address": "http://10.0.0.7:8086"}),
        )
        .await;

        let (_, body) = call(
            &router,
            "PUT",
            "/api/nodes/node-a/registration",
            Some("node-psk"),
            json!({"address": "http://10.0.0.7:8086"}),
        )
        .await;
        assert_eq!(body["outcome"], "refreshed");

        let (_, body) = call(
            &router,
            "PUT",
            "/api/nodes/node-a/registration",
            Some("node-psk"),
            json!({"address": "http://10.0.0.9:8086"}),
        )
        .await;
        assert_eq!(body["outcome"], "address_changed");
        assert_eq!(nodes.node("node-a").unwrap().addr, "http://10.0.0.9:8086");
        assert_eq!(nodes.len(), 1, "one node moved, it did not become two");
    }

    /// An address nothing can dial is worse than no entry at all: it would be
    /// counted as a delivery target and would take cron occurrences that then
    /// have to be handed to another node.
    #[tokio::test]
    async fn an_unusable_address_is_refused_rather_than_stored() {
        let (router, nodes) = router(vec!["node-psk".to_string()], Vec::new());
        for address in ["", "not a url", "10.0.0.7:8086"] {
            let (status, body) = call(
                &router,
                "PUT",
                "/api/nodes/node-a/registration",
                Some("node-psk"),
                json!({ "address": address }),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{address:?} was accepted: {body}"
            );
        }
        assert!(nodes.is_empty());
    }

    /// The credential that guards the assignment list guards this too: a
    /// caller that can add a node can have that node handed every
    /// environment's decrypted secrets on its next assignment fetch.
    #[tokio::test]
    async fn registration_needs_the_node_credential() {
        let (router, nodes) = router(vec!["node-psk".to_string()], Vec::new());

        for credential in [None, Some("wrong-key")] {
            let (status, _) = call(
                &router,
                "PUT",
                "/api/nodes/node-a/registration",
                credential,
                json!({"address": "http://10.0.0.7:8086"}),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        assert!(nodes.is_empty());
    }

    /// The blocker this route had: with no node key configured it used to
    /// let anyone through, so anyone who could reach the port could put a
    /// machine on the fleet list - and the fleet list is what receives
    /// deployment pushes, cron fires and decrypted secrets.
    #[tokio::test]
    async fn a_control_plane_without_a_node_key_registers_nobody() {
        let (router, nodes) = router(Vec::new(), Vec::new());

        for credential in [None, Some("anything")] {
            let (status, body) = call(
                &router,
                "PUT",
                "/api/nodes/intruder/registration",
                credential,
                json!({"address": "http://10.0.0.7:8086"}),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::SERVICE_UNAVAILABLE,
                "an unconfigured node channel must be shut, not open: {body}"
            );
        }
        assert!(
            nodes.is_empty(),
            "nothing may join the fleet through an unauthenticated call"
        );
    }

    #[tokio::test]
    async fn a_node_can_take_itself_off_but_a_configured_one_stays() {
        let (router, nodes) = router(
            vec!["node-psk".to_string()],
            vec![crate::config::RuntimeNodeConfig {
                id: "from-file".to_string(),
                addr: "http://127.0.0.1:8086".to_string(),
            }],
        );
        call(
            &router,
            "PUT",
            "/api/nodes/node-a/registration",
            Some("node-psk"),
            json!({"address": "http://10.0.0.7:8086"}),
        )
        .await;
        assert_eq!(nodes.len(), 2);

        let (status, body) = call(
            &router,
            "DELETE",
            "/api/nodes/node-a/registration",
            Some("node-psk"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["removed"], true);
        assert_eq!(nodes.ids(), vec!["from-file".to_string()]);

        let (status, body) = call(
            &router,
            "DELETE",
            "/api/nodes/from-file/registration",
            Some("node-psk"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["removed"], false);
        assert_eq!(nodes.len(), 1, "the file still says this node exists");

        let (status, _) = call(
            &router,
            "DELETE",
            "/api/nodes/never-seen/registration",
            Some("node-psk"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
