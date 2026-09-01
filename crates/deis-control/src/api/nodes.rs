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
use serde::{Deserialize, Serialize};

use super::{
    AppState,
    error::{ApiError, ApiResult},
};

/// One open control channel.
#[derive(Serialize)]
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

#[derive(Serialize)]
pub struct SessionsResponse {
    pub sessions: Vec<SessionView>,
    pub total: usize,
}

/// One recorded heartbeat.
///
/// A heartbeat proves the node was there at that moment. It carries no load
/// figure: the wire's `Heartbeat` message is a status string, so any number
/// on this row would have been invented by the control plane.
#[derive(Serialize)]
pub struct HeartbeatView {
    pub timestamp: String,
    pub latency_ms: Option<i64>,
}

#[derive(Deserialize)]
pub struct HeartbeatsQuery {
    pub limit: Option<usize>,
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

/// GET /api/sessions — every node currently holding a control channel open.
pub async fn list_sessions(State(app_state): State<AppState>) -> ApiResult<Json<SessionsResponse>> {
    let mut sessions = Vec::new();
    for entry in app_state.sessions.iter() {
        sessions.push(view_of(entry.value()).await);
    }

    let total = sessions.len();
    Ok(Json(SessionsResponse { sessions, total }))
}

/// GET /api/sessions/:id — one open control channel.
pub async fn get_session(
    State(app_state): State<AppState>,
    Path(node_id): Path<String>,
) -> ApiResult<Json<SessionView>> {
    let session = app_state.sessions.get_node(&node_id).ok_or_else(|| {
        ApiError::NotFound(format!("No open control channel for node {}", node_id))
    })?;

    Ok(Json(view_of(&session).await))
}

/// GET /api/sessions/:id/heartbeats — the recorded heartbeat history.
pub async fn get_heartbeats(
    State(app_state): State<AppState>,
    Path(node_id): Path<String>,
    Query(query): Query<HeartbeatsQuery>,
) -> ApiResult<Json<Vec<HeartbeatView>>> {
    let db = app_state
        .database
        .as_ref()
        .ok_or_else(|| ApiError::InternalError("Database not available".to_string()))?;

    let limit = query.limit.unwrap_or(100);
    let heartbeats = db.get_recent_heartbeats(&node_id, limit).await?;

    Ok(Json(
        heartbeats
            .into_iter()
            .map(|h| HeartbeatView {
                timestamp: h.timestamp.to_rfc3339(),
                latency_ms: h.latency_ms,
            })
            .collect(),
    ))
}
