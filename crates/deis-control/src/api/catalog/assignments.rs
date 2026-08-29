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
//!
//! **This response carries decrypted secret values.** It is one of exactly
//! two places where they appear in the clear (the other is the
//! `ApplyDeployment` push), which is why the route is mounted with its own
//! node authentication in [`super::node_router`] instead of behind the
//! dashboard's API key, and why a control plane with no node credential at
//! all serves no environment that has a secret rather than serving its
//! values to whoever asked.

use std::collections::BTreeMap;

use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use serde::Serialize;
use tracing::{debug, error, info, warn};

use super::CatalogState;
use crate::api::error::ApiResult;

/// One line of the list, in exactly the shape the wire contract of
/// docs/implementation-plan.zh.md section 4 specifies.
#[derive(Debug, Serialize)]
pub struct AssignmentView {
    pub app_id: String,
    pub version_id: String,
    /// The environment *name*, `production` or a preview name. The
    /// environment id stays inside the control plane: the node has no use
    /// for it, and the runtime branches on the name.
    ///
    /// It is half of the identity of a running application: the node keys
    /// what it loads by `(app_id, environment)`, and builds its storage
    /// tenant from the same pair with `deis_wire::conventions::tenant_id`,
    /// so a preview cannot read or overwrite what production stored.
    pub environment: String,
    /// Absolute URL the node downloads the bundle from.
    pub bundle_url: String,
    /// Digest the node must verify after downloading.
    pub bundle_sha: String,
    /// Exactly what a push would have carried: the manifest's variables,
    /// this environment's decrypted secrets, and the storage addresses.
    ///
    /// Without it a node that restarts comes back running the right code
    /// with an empty environment - no secrets, and no address for
    /// deis-store or deis-queue - and stays that way until somebody
    /// deploys again. Secrets are decrypted here, for this response only,
    /// and are never stored or logged decrypted.
    ///
    /// This is why the endpoint authenticates the caller as a node: the
    /// values in this map are the platform's secrets in the clear.
    pub env: BTreeMap<String, String>,
}

/// GET /api/nodes/:id/assignments
///
/// Authenticated as a node by [`super::node_router`], whatever
/// `api_auth_enabled` says. When this control plane has no node credential
/// configured at all, every caller is anonymous, so any assignment whose
/// environment holds secrets is left out of the answer entirely rather than
/// handed to whoever asked.
pub async fn for_node(
    State(state): State<CatalogState>,
    Path(node_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<AssignmentView>>> {
    let assignments = state.store.list_assignments().await?;
    let base = state.deliverer.base_url(&headers);

    let mut views: Vec<AssignmentView> = Vec::with_capacity(assignments.len());
    for assignment in &assignments {
        // One environment whose secrets will not decrypt must not cost the
        // node every other application on the list, so it is dropped from
        // this answer with the reason said out loud. The node then simply
        // does not run it, which is the same outcome a refused deployment
        // has, rather than running it with a hole in its environment.
        let env = match state.deliverer.assignment_payload(assignment, &base).await {
            Ok(payload) if payload.secret_count() > 0 && !state.node_auth.is_configured() => {
                // Nobody on this channel can prove they are a node, so the
                // only safe answer about an environment with secrets is
                // none at all. Dropping the row costs this application its
                // place on the fleet; serving it would publish the values
                // to anyone who can reach this port, and that cannot be
                // taken back.
                error!(
                    node = %node_id,
                    app_id = %assignment.app_id,
                    environment = %assignment.environment,
                    secrets = payload.secret_count(),
                    "This control plane has no node_preshared_keys, so the assignment list is unauthenticated and must not carry decrypted secrets; this application is left out and will not run on any node until a node key is configured"
                );
                continue;
            }
            Ok(payload) => payload.env_for_wire(),
            Err(e) => {
                error!(
                    node = %node_id,
                    app_id = %assignment.app_id,
                    environment = %assignment.environment,
                    version_id = %assignment.version_id,
                    error = %e,
                    "Could not build the environment for this assignment, so it is left out of the list; the node will not run this application until the problem is fixed and it asks again"
                );
                continue;
            }
        };

        views.push(AssignmentView {
            bundle_url: crate::delivery::bundle_url(
                &base,
                &assignment.app_id,
                &assignment.version_id,
            ),
            app_id: assignment.app_id.clone(),
            version_id: assignment.version_id.clone(),
            environment: assignment.environment.clone(),
            bundle_sha: assignment.bundle_sha.clone(),
            env,
        });
    }

    if !state.node_auth.is_configured() {
        warn!(
            node = %node_id,
            "Served an assignment list over an unauthenticated channel: node_preshared_keys is empty, so anyone who can reach this port can read which applications and versions this platform runs"
        );
    }

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
