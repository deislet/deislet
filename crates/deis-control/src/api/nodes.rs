use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use super::{error::{ApiError, ApiResult}, AppState};

#[derive(Serialize)]
pub struct NodeInfo {
    pub id: String,
    pub instance_id: String,
    pub node_role: String,
    pub host: String,
    pub ports: Vec<u16>,
    pub state: String,
    pub connected_at: String,
    pub last_heartbeat: String,
    pub power_reserve: u8,
}

#[derive(Serialize)]
pub struct NodesResponse {
    pub nodes: Vec<NodeInfo>,
    pub total: usize,
}

#[derive(Serialize)]
pub struct HeartbeatInfo {
    pub timestamp: String,
    pub power_reserve: u8,
    pub latency_ms: Option<i64>,
}

#[derive(Deserialize)]
pub struct HeartbeatsQuery {
    pub limit: Option<usize>,
}

/// List all nodes
pub async fn list_nodes(State(app_state): State<AppState>) -> ApiResult<Json<NodesResponse>> {
    let mut nodes = Vec::new();

    for entry in app_state.sessions.iter() {
        let session = entry.value();
        let state_str = match session.get_state().await {
            crate::session::NodeState::Connected => "Connected",
            crate::session::NodeState::Registered => "Registered",
            crate::session::NodeState::Active => "Active",
            crate::session::NodeState::Disconnected => "Disconnected",
        };

        let node_role = match &session.node_role {
            crate::message::NodeRole::Runtime(v) => format!("deis_runtime:{}", v),
            crate::message::NodeRole::Proxy(v) => format!("deis_proxy:{}", v),
        };

        nodes.push(NodeInfo {
            id: session.node_id.clone(),
            instance_id: session.instance_id.clone(),
            node_role,
            host: session.host.clone(),
            ports: session.ports.clone(),
            state: state_str.to_string(),
            connected_at: session.connected_at.to_rfc3339(),
            last_heartbeat: session.last_heartbeat.lock().await.to_rfc3339(),
            power_reserve: 0, // Will be updated from database if available
        });
    }

    let total = nodes.len();

    Ok(Json(NodesResponse { nodes, total }))
}

/// Get node details
pub async fn get_node(
    State(app_state): State<AppState>,
    Path(node_id): Path<String>,
) -> ApiResult<Json<NodeInfo>> {
    let session = app_state
        .sessions
        .get_node(&node_id)
        .ok_or_else(|| ApiError::NotFound(format!("Node {} not found", node_id)))?;

    let state_str = match session.get_state().await {
        crate::session::NodeState::Connected => "Connected",
        crate::session::NodeState::Registered => "Registered",
        crate::session::NodeState::Active => "Active",
        crate::session::NodeState::Disconnected => "Disconnected",
    };

    let node_role = match &session.node_role {
        crate::message::NodeRole::Runtime(v) => format!("deis_runtime:{}", v),
        crate::message::NodeRole::Proxy(v) => format!("deis_proxy:{}", v),
    };

    Ok(Json(NodeInfo {
        id: session.node_id.clone(),
        instance_id: session.instance_id.clone(),
        node_role,
        host: session.host.clone(),
        ports: session.ports.clone(),
        state: state_str.to_string(),
        connected_at: session.connected_at.to_rfc3339(),
        last_heartbeat: session.last_heartbeat.lock().await.to_rfc3339(),
        power_reserve: 0,
    }))
}

/// Get node heartbeats
pub async fn get_heartbeats(
    State(app_state): State<AppState>,
    Path(node_id): Path<String>,
    Query(query): Query<HeartbeatsQuery>,
) -> ApiResult<Json<Vec<HeartbeatInfo>>> {
    let db = app_state
        .database
        .as_ref()
        .ok_or_else(|| ApiError::InternalError("Database not available".to_string()))?;

    let limit = query.limit.unwrap_or(100);
    let heartbeats = db.get_recent_heartbeats(&node_id, limit).await?;

    let heartbeat_infos: Vec<HeartbeatInfo> = heartbeats
        .into_iter()
        .map(|h| HeartbeatInfo {
            timestamp: h.timestamp.to_rfc3339(),
            power_reserve: h.power_reserve,
            latency_ms: h.latency_ms,
        })
        .collect();

    Ok(Json(heartbeat_infos))
}
