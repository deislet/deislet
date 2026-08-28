//! What a node should be running.
//!
//! Nodes pull this on startup and refresh it later, then fetch each bundle
//! themselves over HTTP (implementation plan section 4). The control plane
//! sends pointers, never payloads: a deployment notification stays small
//! and the bundle travels as a stream.
//!
//! Every runtime node runs every app, so the list is identical for all
//! nodes today. The node id is still part of the path because that is the
//! contract nodes are written against, and because per-node placement
//! would change only this query.

use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use serde::Serialize;
use tracing::{debug, info};

use super::ProductState;
use crate::api::error::ApiResult;

#[derive(Debug, Serialize)]
pub struct AssignmentView {
    pub app_id: String,
    pub version_id: String,
    pub environment: String,
    /// Absolute URL the node downloads the bundle from.
    pub bundle_url: String,
    /// Digest the node must verify after downloading.
    pub bundle_sha: String,
}

/// GET /api/nodes/:id/assignments
pub async fn assignments(
    State(state): State<ProductState>,
    Path(node_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<AssignmentView>>> {
    let assignments = state.store.list_assignments().await?;
    let base = state.address.resolve(&headers);

    let views: Vec<AssignmentView> = assignments
        .iter()
        .map(|assignment| AssignmentView {
            bundle_url: super::bundle_url(&base, &assignment.app_id, &assignment.version_id),
            app_id: assignment.app_id.clone(),
            version_id: assignment.version_id.clone(),
            environment: assignment.environment.clone(),
            bundle_sha: assignment.bundle_sha.clone(),
        })
        .collect();

    if views.is_empty() {
        info!(
            node = %node_id,
            "Node asked for its assignments and there are none; it will report healthy with no app loaded"
        );
    } else {
        debug!(
            node = %node_id,
            assignments = views.len(),
            "Served the assignment list"
        );
    }
    Ok(Json(views))
}
