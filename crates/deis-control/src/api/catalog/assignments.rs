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
//! **The list is authoritative, not additive.** It names everything the node
//! should be running, so a node that reconciles against it stops serving
//! whatever is not in it. That is the half of deletion that push cannot do:
//! the control plane pushes an unload when an application is deleted, and a
//! node that was unreachable at that moment never hears it and serves the
//! application for ever. Reconciliation closes that on its own, and the same
//! mechanism repairs any other drift - a version applied on one node and not
//! another, an app loaded by hand.
//!
//! The one thing that would make removal wrong is a list that is short for a
//! reason of the control plane's own, so completeness is stated explicitly in
//! [`ASSIGNMENTS_COMPLETE_HEADER`] rather than inferred from the array.
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
    http::{HeaderMap, HeaderValue},
};
use serde::Serialize;
use tracing::{debug, error, info, warn};

use super::CatalogState;
use crate::api::error::ApiResult;

/// Whether this answer is the whole truth about what the node should run.
///
/// `true` means the array below is the complete expected set, so a node
/// reconciling against it must stop serving anything that is not in it - that
/// is how a deleted application finally leaves a node that was unreachable
/// when the unload was pushed.
///
/// `false` means at least one assignment exists that this answer could not
/// describe (see [`ASSIGNMENTS_WITHHELD_HEADER`]). The node must still load
/// what is listed, and must remove nothing on the strength of this answer: an
/// application whose secrets happen not to decrypt today is a control-plane
/// problem, and unloading it would turn that into an outage for code that is
/// running perfectly well.
///
/// A header rather than a field because the body is the wire contract of
/// docs/implementation-plan.zh.md section 4 - a bare array - and an older node
/// that ignores this keeps working exactly as it did, additively.
pub use deis_wire::conventions::ASSIGNMENTS_COMPLETE_HEADER;

/// How many assignments were left out of this answer, and are therefore the
/// reason [`ASSIGNMENTS_COMPLETE_HEADER`] says `false`. Present so a node can
/// say in its own log how much it is missing.
pub use deis_wire::conventions::ASSIGNMENTS_WITHHELD_HEADER;

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
) -> ApiResult<(HeaderMap, Json<Vec<AssignmentView>>)> {
    let assignments = state.store.list_assignments().await?;
    let base = state.deliverer.base_url(&headers);

    let mut views: Vec<AssignmentView> = Vec::with_capacity(assignments.len());
    let mut withheld: usize = 0;
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
                    "This control plane has no node_preshared_keys, so the assignment list is unauthenticated and must not carry decrypted secrets; this application is left out, the list is marked incomplete so no node removes anything because of it, and this application will not run on any node until a node key is configured"
                );
                withheld += 1;
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
                    "Could not build the environment for this assignment, so it is left out of the list; the list is marked incomplete so a node reconciling against it does not unload an application that is running fine, and this node will not start this application until the problem is fixed and it asks again"
                );
                withheld += 1;
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

    // The answer is authoritative only when every assignment in the catalog
    // could be described. That is what lets a node use it to remove things:
    // an application the catalog no longer names is simply absent here, and
    // absence is an instruction. Absence caused by this control plane failing
    // to build a row is not, which is the whole reason the two cases are told
    // apart on the wire.
    let complete = withheld == 0;
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        ASSIGNMENTS_COMPLETE_HEADER,
        HeaderValue::from_static(if complete { "true" } else { "false" }),
    );
    response_headers.insert(
        ASSIGNMENTS_WITHHELD_HEADER,
        // Only ASCII digits, so this cannot fail; a failure would still not
        // be worth failing the request over, and an absent count reads as
        // zero on a node that predates the header.
        HeaderValue::from_str(&withheld.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );

    if !state.node_auth.is_configured() {
        warn!(
            node = %node_id,
            "Served an assignment list over an unauthenticated channel: node_preshared_keys is empty, so anyone who can reach this port can read which applications and versions this platform runs"
        );
    }

    if !complete {
        warn!(
            node = %node_id,
            served = views.len(),
            withheld,
            "Served an incomplete assignment list; it is marked as such, so the node loads what is in it and removes nothing - an application it is still running that this control plane can no longer describe keeps serving instead of being unloaded on a mistake"
        );
    } else if views.is_empty() {
        info!(
            node = %node_id,
            "Node asked for its assignments and there are none; it will report healthy with no app loaded, and a node reconciling against this answer unloads whatever it was still serving"
        );
    } else {
        debug!(
            node = %node_id,
            assignments = views.len(),
            "Served the authoritative assignment list"
        );
    }
    Ok((response_headers, Json(views)))
}
