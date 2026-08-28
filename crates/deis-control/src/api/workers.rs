use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use super::{error::{ApiError, ApiResult}, AppState};

#[derive(Serialize)]
pub struct WorkerInfo {
    pub id: String,
    pub instance_id: String,
    pub worker_type: String,
    pub host: String,
    pub ports: Vec<u16>,
    pub state: String,
    pub connected_at: String,
    pub last_heartbeat: String,
    pub power_reserve: u8,
}

#[derive(Serialize)]
pub struct WorkersResponse {
    pub workers: Vec<WorkerInfo>,
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

/// List all workers
pub async fn list_workers(State(app_state): State<AppState>) -> ApiResult<Json<WorkersResponse>> {
    let mut workers = Vec::new();

    for entry in app_state.sessions.iter() {
        let session = entry.value();
        let state_str = match session.get_state().await {
            crate::session::WorkerState::Connected => "Connected",
            crate::session::WorkerState::Registered => "Registered",
            crate::session::WorkerState::Active => "Active",
            crate::session::WorkerState::Disconnected => "Disconnected",
        };

        let worker_type = match &session.worker_type {
            crate::message::WorkerType::Runtime(v) => format!("deis_runtime:{}", v),
            crate::message::WorkerType::Proxy(v) => format!("deis_proxy:{}", v),
        };

        workers.push(WorkerInfo {
            id: session.worker_id.clone(),
            instance_id: session.instance_id.clone(),
            worker_type,
            host: session.host.clone(),
            ports: session.ports.clone(),
            state: state_str.to_string(),
            connected_at: session.connected_at.to_rfc3339(),
            last_heartbeat: session.last_heartbeat.lock().await.to_rfc3339(),
            power_reserve: 0, // Will be updated from database if available
        });
    }

    let total = workers.len();

    Ok(Json(WorkersResponse { workers, total }))
}

/// Get worker details
pub async fn get_worker(
    State(app_state): State<AppState>,
    Path(worker_id): Path<String>,
) -> ApiResult<Json<WorkerInfo>> {
    let session = app_state
        .sessions
        .get_worker(&worker_id)
        .ok_or_else(|| ApiError::NotFound(format!("Worker {} not found", worker_id)))?;

    let state_str = match session.get_state().await {
        crate::session::WorkerState::Connected => "Connected",
        crate::session::WorkerState::Registered => "Registered",
        crate::session::WorkerState::Active => "Active",
        crate::session::WorkerState::Disconnected => "Disconnected",
    };

    let worker_type = match &session.worker_type {
        crate::message::WorkerType::Runtime(v) => format!("deis_runtime:{}", v),
        crate::message::WorkerType::Proxy(v) => format!("deis_proxy:{}", v),
    };

    Ok(Json(WorkerInfo {
        id: session.worker_id.clone(),
        instance_id: session.instance_id.clone(),
        worker_type,
        host: session.host.clone(),
        ports: session.ports.clone(),
        state: state_str.to_string(),
        connected_at: session.connected_at.to_rfc3339(),
        last_heartbeat: session.last_heartbeat.lock().await.to_rfc3339(),
        power_reserve: 0,
    }))
}

/// Get worker heartbeats
pub async fn get_heartbeats(
    State(app_state): State<AppState>,
    Path(worker_id): Path<String>,
    Query(query): Query<HeartbeatsQuery>,
) -> ApiResult<Json<Vec<HeartbeatInfo>>> {
    let db = app_state
        .database
        .as_ref()
        .ok_or_else(|| ApiError::InternalError("Database not available".to_string()))?;

    let limit = query.limit.unwrap_or(100);
    let heartbeats = db.get_recent_heartbeats(&worker_id, limit).await?;

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
