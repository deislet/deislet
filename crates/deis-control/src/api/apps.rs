//! REST endpoints for inspecting deis-runtime nodes through their gRPC
//! management plane, and for taking an app off one.
//!
//! Deploying is deliberately *not* here. An application reaches a node
//! because an environment points at a version (`POST
//! /api/apps/:app_id/environments/:env/deployments`) and the control plane
//! then pushes that pointer to every node; there is no endpoint that
//! deploys to one node out of band, because the result would not be
//! recorded anywhere and the next assignment refresh would undo it.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tracing::warn;

use crate::runtime_client::{NodeError, RuntimeNodeManager};

#[derive(Clone)]
pub struct AppsState {
    pub deis_runtime: Arc<RuntimeNodeManager>,
}

fn node_error_response(e: &NodeError) -> (StatusCode, Json<serde_json::Value>) {
    let status = match e {
        NodeError::UnknownNode => StatusCode::NOT_FOUND,
        NodeError::Transport(_) => StatusCode::BAD_GATEWAY,
        NodeError::Rpc(_) => StatusCode::BAD_GATEWAY,
    };
    (status, Json(json!({ "error": e.to_string() })))
}

/// GET /api/runtime — all configured nodes with cached health.
pub async fn list_nodes(State(state): State<AppsState>) -> impl IntoResponse {
    Json(json!({ "nodes": state.deis_runtime.list() }))
}

#[derive(Deserialize)]
pub struct StatusParams {
    /// Include per-app details (default true).
    #[serde(default = "default_true")]
    pub apps: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize)]
struct AppStatusView {
    app_id: String,
    state: String,
    request_count: i64,
    error_count: i64,
    error_message: String,
}

/// GET /api/runtime/:node/status — live status straight from the node.
pub async fn node_status(
    State(state): State<AppsState>,
    Path(node): Path<String>,
    Query(params): Query<StatusParams>,
) -> impl IntoResponse {
    match state.deis_runtime.get_status(&node, params.apps).await {
        Ok(status) => {
            let apps: Vec<AppStatusView> = status
                .apps
                .iter()
                .map(|a| AppStatusView {
                    app_id: a.app_id.clone(),
                    state: match a.state {
                        1 => "loading",
                        2 => "running",
                        3 => "stopped",
                        4 => "error",
                        _ => "unknown",
                    }
                    .to_string(),
                    request_count: a.request_count,
                    error_count: a.error_count,
                    error_message: a.error_message.clone(),
                })
                .collect();
            let info = status.node_info.as_ref();
            (
                StatusCode::OK,
                Json(json!({
                    "node": node,
                    "node_info": {
                        "node_id": info.map(|i| i.node_id.clone()),
                        "version": info.map(|i| i.version.clone()),
                        "uptime_seconds": info.map(|i| i.uptime_seconds),
                        "hostname": info.map(|i| i.hostname.clone()),
                    },
                    "apps": apps,
                    "resource_usage": status.resource_usage.as_ref().map(|r| json!({
                        "cpu_percent": r.cpu_percent,
                        "memory_bytes": r.memory_bytes,
                    })),
                })),
            )
        }
        Err(e) => {
            warn!(node = %node, error = %e, "Status query failed");
            node_error_response(&e)
        }
    }
}

/// DELETE /api/runtime/:node/apps/:app_id — unload an app.
pub async fn unload_app(
    State(state): State<AppsState>,
    Path((node, app_id)): Path<(String, String)>,
) -> impl IntoResponse {
    match state.deis_runtime.unload_app(&node, &app_id).await {
        Ok(resp) if resp.success => (
            StatusCode::OK,
            Json(json!({ "success": true, "message": resp.message })),
        ),
        Ok(resp) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "success": false, "message": resp.message })),
        ),
        Err(e) => {
            warn!(node = %node, app_id = %app_id, error = %e, "Unload failed");
            node_error_response(&e)
        }
    }
}
