//! The control channels open to this process.
//!
//! A row here is one node that dialled this control plane over gRPC and is
//! holding an `EstablishChannel` stream open: it authenticated, registered,
//! and is sending heartbeats. That is what lets the control plane push a
//! configuration or a command down to it, and it is the only thing these
//! endpoints are about. In practice deis-proxy is what appears here.
//!
//! This is **not** the runtime fleet. The fleet - the nodes that receive
//! deployments and run cron occurrences - is
//! [`crate::api::apps::list_fleet`] at `GET /api/runtime`, and it is built
//! from the configuration file plus self-registrations, not from open
//! channels. The two lists overlap in nothing but the word "node", which is
//! why these routes live under `/api/sessions` and not under `/api/nodes`:
//! that prefix belongs to the endpoints nodes themselves call
//! (`/api/nodes/:id/registration`, `/api/nodes/:id/assignments`).

use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::Serialize;
use utoipa::ToSchema;

use super::{
    AppState,
    error::{ApiError, ApiResult},
    openapi,
    pagination::{
        ControlListPage, ListQuery, list_page, map_page, node_scope, text_keyset_after,
        text_keyset_position,
    },
};
use crate::database::HeartbeatRecord;

/// One open control channel.
#[derive(Serialize, ToSchema)]
pub struct SessionView {
    pub id: String,
    pub instance_id: String,
    pub node_role: String,
    pub host: String,
    pub ports: Vec<u16>,
    pub state: String,
    pub connected_at: String,
    pub last_heartbeat: String,
}

/// Every open control channel.
#[derive(Serialize, ToSchema)]
pub struct SessionsResponse {
    pub sessions: Vec<SessionView>,
    pub total: usize,
}

/// One recorded heartbeat.
///
/// A heartbeat proves the node was there at that moment. It carries no load
/// figure: the wire's `Heartbeat` message is a status string, so any number
/// on this row would have been invented by the control plane.
#[derive(Serialize, ToSchema)]
pub struct HeartbeatView {
    pub id: i64,
    /// RFC 3339, as recorded.
    pub timestamp: String,
    pub latency_ms: Option<i64>,
}

impl From<HeartbeatRecord> for HeartbeatView {
    fn from(heartbeat: HeartbeatRecord) -> Self {
        Self {
            id: heartbeat.id,
            timestamp: heartbeat.timestamp.to_rfc3339(),
            latency_ms: heartbeat.latency_ms,
        }
    }
}

fn role_of(role: &crate::message::NodeRole) -> String {
    match role {
        crate::message::NodeRole::Runtime(v) => format!("deis_runtime:{}", v),
        crate::message::NodeRole::Proxy(v) => format!("deis_proxy:{}", v),
    }
}

async fn view_of(session: &crate::session::NodeSession) -> SessionView {
    SessionView {
        id: session.node_id.clone(),
        instance_id: session.instance_id.clone(),
        node_role: role_of(&session.node_role),
        host: session.host.clone(),
        ports: session.ports.clone(),
        state: session.get_state().await.as_str().to_string(),
        connected_at: session.connected_at.to_rfc3339(),
        last_heartbeat: session.last_heartbeat.lock().await.to_rfc3339(),
    }
}

/// GET /api/platform/sessions - every node currently holding a control
/// channel open.
#[utoipa::path(
    get,
    path = "/api/platform/sessions",
    operation_id = "listSessions",
    tag = "platform",
    responses(
        (status = 200, description = "Every open control channel.", body = SessionsResponse),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_sessions(State(app_state): State<AppState>) -> ApiResult<Json<SessionsResponse>> {
    let mut sessions = Vec::new();
    for entry in app_state.sessions.iter() {
        sessions.push(view_of(entry.value()).await);
    }

    let total = sessions.len();
    Ok(Json(SessionsResponse { sessions, total }))
}

/// GET /api/platform/sessions/{node_id} - one open control channel.
#[utoipa::path(
    get,
    path = "/api/platform/sessions/{node_id}",
    operation_id = "getSession",
    tag = "platform",
    params(("node_id" = String, Path, description = "The node id the node announced.")),
    responses(
        (status = 200, description = "The open control channel.", body = SessionView),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_session(
    State(app_state): State<AppState>,
    Path(node_id): Path<String>,
) -> ApiResult<Json<SessionView>> {
    let session = app_state.sessions.get_node(&node_id).ok_or_else(|| {
        ApiError::NotFound(format!("No open control channel for node {}", node_id))
    })?;

    Ok(Json(view_of(&session).await))
}

/// GET /api/platform/sessions/{node_id}/heartbeats - the recorded heartbeat
/// history, newest first, in the unified list envelope. The node need not
/// hold a channel open now: its history is what it left behind.
#[utoipa::path(
    get,
    path = "/api/platform/sessions/{node_id}/heartbeats",
    operation_id = "listSessionHeartbeats",
    tag = "platform",
    params(("node_id" = String, Path, description = "The node id the node announced."), ListQuery),
    responses(
        (status = 200, description = "One page of heartbeats, newest first.", body = ControlListPage<HeartbeatView>),
        (status = 400, response = openapi::BadRequest),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
        (status = 500, response = openapi::Internal),
    )
)]
pub async fn get_heartbeats(
    State(app_state): State<AppState>,
    Path(node_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ControlListPage<HeartbeatView>>> {
    let db = app_state
        .database
        .as_ref()
        .ok_or_else(|| ApiError::InternalError("Database not available".to_string()))?;

    let collection = "heartbeats";
    let binding = format!("{};sort=timestamp-desc,id-desc-v1", node_scope(&node_id));
    let limit = query.bounded_limit()?;
    let cursors = db.page_cursors();
    let position = query.position(&cursors, collection, &binding).await?;
    let after = text_keyset_after::<i64>(position, "heartbeats")?;
    let page = db.list_heartbeats_page(&node_id, limit, after).await?;
    let response = list_page(
        &cursors,
        collection,
        &binding,
        limit,
        map_page(page, HeartbeatView::from),
        |heartbeat| text_keyset_position(heartbeat.timestamp.clone(), heartbeat.id),
    )
    .await?;
    Ok(Json(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::catalog::tests::json;
    use crate::database::{Database, HeartbeatEntry};
    use crate::message::{NodeInfo, NodeRole};
    use crate::session::SessionManager;
    use axum::{Router, http::StatusCode, routing::get};
    use chrono::{DateTime, Utc};
    use serde_json::Value;
    use std::sync::Arc;

    async fn harness() -> (Router, Arc<Database>) {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        for node in ["node-a", "node-b"] {
            let session = crate::session::NodeSession::new(
                node.to_string(),
                NodeInfo {
                    node_role: NodeRole::Proxy("1.0.0".to_string()),
                    listen_host: "localhost".to_string(),
                    listen_port: vec![8000],
                },
            );
            database.save_node(&session).await.unwrap();
        }
        let router = Router::new()
            .route("/api/sessions/:id/heartbeats", get(get_heartbeats))
            .with_state(AppState {
                sessions: Arc::new(SessionManager::new()),
                database: Some(Arc::clone(&database)),
            });
        (router, database)
    }

    async fn beat(database: &Database, node: &str, at: DateTime<Utc>, latency_ms: i64) {
        database
            .save_heartbeat(&HeartbeatEntry {
                node_id: node.to_string(),
                instance_id: "instance-1".to_string(),
                timestamp: at,
                latency_ms: Some(latency_ms),
            })
            .await
            .unwrap();
    }

    fn latencies(page: &Value) -> Vec<i64> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|heartbeat| heartbeat["latency_ms"].as_i64().unwrap())
            .collect()
    }

    /// The heartbeat history answers in the unified envelope: exact total
    /// for the node, newest first with the id breaking a same-instant tie,
    /// a cursor bound to the node that survives a change of page size.
    #[tokio::test]
    async fn the_heartbeat_history_pages_newest_first_through_the_unified_envelope() {
        let (router, database) = harness().await;
        let base = DateTime::parse_from_rfc3339("2026-09-06T10:00:00.5+00:00")
            .unwrap()
            .with_timezone(&Utc);
        beat(&database, "node-a", base, 1).await;
        beat(&database, "node-a", base, 2).await;
        beat(&database, "node-b", base + chrono::Duration::seconds(1), 99).await;
        beat(&database, "node-a", base + chrono::Duration::seconds(1), 3).await;
        beat(&database, "node-a", base + chrono::Duration::seconds(2), 4).await;

        let (status, first) = json(
            &router,
            "GET",
            "/api/sessions/node-a/heartbeats?limit=2",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["schemaVersion"], 1);
        assert_eq!(first["page"]["total"], 4);
        assert_eq!(first["page"]["count"], 2);
        assert_eq!(latencies(&first), [4, 3]);
        assert_eq!(
            first["items"][0]["timestamp"],
            "2026-09-06T10:00:02.500+00:00"
        );
        assert!(first["items"][0]["id"].is_i64());
        let cursor = first["page"]["nextCursor"].as_str().unwrap().to_string();
        assert_eq!(cursor.len(), 43);

        let (status, second) = json(
            &router,
            "GET",
            &format!("/api/sessions/node-a/heartbeats?limit=1&cursor={cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert_eq!(latencies(&second), [2], "same instant: the later row first");
        let cursor = second["page"]["nextCursor"].as_str().unwrap().to_string();
        let (status, third) = json(
            &router,
            "GET",
            &format!("/api/sessions/node-a/heartbeats?limit=10&cursor={cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{third}");
        assert_eq!(latencies(&third), [1]);
        assert_eq!(third["page"]["total"], 4);
        assert!(third["page"]["nextCursor"].is_null());

        // The cursor is bound to the node it was issued for.
        let (status, _) = json(
            &router,
            "GET",
            &format!("/api/sessions/node-b/heartbeats?cursor={cursor}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, node_b) = json(
            &router,
            "GET",
            "/api/sessions/node-b/heartbeats",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{node_b}");
        assert_eq!(node_b["page"]["total"], 1);
        assert_eq!(latencies(&node_b), [99]);

        // A node that never beat is an empty page; the shared bounds apply.
        let (status, none) = json(
            &router,
            "GET",
            "/api/sessions/node-c/heartbeats",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{none}");
        assert_eq!(none["page"]["total"], 0);
        let (status, _) = json(
            &router,
            "GET",
            "/api/sessions/node-a/heartbeats?limit=1000",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = json(
            &router,
            "GET",
            "/api/sessions/node-a/heartbeats?limit=1001",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = json(
            &router,
            "GET",
            "/api/sessions/node-a/heartbeats?offset=1",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
