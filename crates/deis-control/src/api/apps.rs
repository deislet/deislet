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
    /// Which environment of that application this line is about. A node
    /// runs production and every preview side by side, each with its own
    /// version and counters, so the pair is what names one of them.
    environment: String,
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
                    environment: a.environment.clone(),
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

#[derive(Deserialize)]
pub struct UnloadParams {
    /// Which environment of the application to take off this node. Omitted
    /// means every environment of it.
    ///
    /// An application on a node is identified by `(app_id, environment)`:
    /// production and a preview of the same app are two separate loads with
    /// their own version and secrets, so unloading without naming one would
    /// otherwise be ambiguous.
    #[serde(default)]
    pub environment: Option<String>,
}

/// DELETE /api/runtime/:node/apps/:app_id[?environment=NAME] — unload an app.
///
/// This is a node-local operation and changes nothing in the catalog: the
/// next assignment refresh puts back whatever the catalog still says this
/// node should run.
pub async fn unload_app(
    State(state): State<AppsState>,
    Path((node, app_id)): Path<(String, String)>,
    Query(params): Query<UnloadParams>,
) -> impl IntoResponse {
    let environment = params.environment.unwrap_or_default();
    match state
        .deis_runtime
        .unload_app(&node, &app_id, &environment)
        .await
    {
        Ok(resp) if resp.success => (
            StatusCode::OK,
            Json(json!({
                "success": true,
                "message": resp.message,
                "environments": resp.environments,
            })),
        ),
        Ok(resp) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "success": false, "message": resp.message })),
        ),
        Err(e) => {
            warn!(
                node = %node,
                app_id = %app_id,
                environment = %environment,
                error = %e,
                "Unload failed; this node is still serving the application"
            );
            node_error_response(&e)
        }
    }
}
