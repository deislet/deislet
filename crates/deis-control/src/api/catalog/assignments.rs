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
//! `ApplyDeployment` push), which is why it is served on the machine
//! channel (`deis.machine.MachineChannel`) with mutual TLS instead of
//! behind the dashboard's API key. There is no anonymous case to handle
//! here: a control plane with no machine identity answers 503 on a call
//! into this handler (`machine::service` checks the peer certificate),
//! so everything below is talking to a caller that proved its SPIFFE name.

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
#[derive(Serialize)]
pub struct AssignmentView {
    pub app_id: String,
    /// Permanent application identity used by the canonical tenant contract.
    pub canonical_application_id: deis_wire::CanonicalId,
    pub version_id: String,
    /// Durable plan and immutable route generation of the current pointer.
    pub deployment_id: String,
    pub routing_generation: String,
    /// `prepare` publishes this exact generation for generation-addressed
    /// HTTP without moving the runtime's current selector or starting
    /// triggers. `activate` makes it current. This keeps assignment-based
    /// recovery inside the same barrier as push delivery.
    pub deployment_action: &'static str,
    /// The environment *name*, `production` or a preview name. Application
    /// code and rolling-upgrade routing still branch on it; permanent storage
    /// authority comes from the separate canonical id below.
    ///
    /// It is half of the identity of a running application: the node keys
    /// what it loads by `(app_id, environment)`, and builds its storage
    /// tenant from the same pair with `deis_wire::conventions::tenant_id`,
    /// so a preview cannot read or overwrite what production stored.
    pub environment: String,
    /// Permanent environment identity used by the canonical tenant contract.
    pub canonical_environment_id: deis_wire::CanonicalId,
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
    /// What this version's manifest asked to be held to, exactly as the
    /// `ApplyDeployment` push carries it. Omitted when the application
    /// declared nothing, so an older node sees the same body it always did.
    ///
    /// Here and not only on the push because this is how a restarting node
    /// gets its applications back: without it, an application that narrowed
    /// its own CPU budget would come back on the node's default and stay
    /// there until the next deployment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limits: Option<crate::catalog::DeclaredLimits>,
    /// Which day's platform this version was written against, exactly as the
    /// `ApplyDeployment` push carries it: the manifest's own words, not this
    /// control plane's resolution of them. Each build answers the declaration
    /// with its own table - see `DeploymentPayload::compatibility` for why a
    /// resolved list would be lossy.
    ///
    /// Always present, including for a version that declares nothing: an
    /// application with no date is a complete answer ("the oldest behaviour"),
    /// and leaving the field out would make it indistinguishable from a
    /// control plane too old to have an opinion - at which point a restarting
    /// node would have to guess, which is the one thing this field exists to
    /// stop.
    pub compatibility: crate::catalog::DeclaredCompatibility,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment_declarations: Option<deis_wire::EnvironmentDeclarations>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment_snapshot: Option<deis_wire::EnvironmentBindingSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment_snapshot_id: Option<String>,
}

/// GET /api/nodes/:id/assignments (machine channel)
///
/// Served on the machine listener with mutual TLS — the handler receives
/// the caller's SPIFFE identity from the peer certificate. The human API's
/// credential gate does not reach this channel.
pub async fn for_node(
    State(state): State<CatalogState>,
    Path(node_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<(HeaderMap, Json<Vec<AssignmentView>>)> {
    // Every URL in this answer is fetched by the node that asked, so it is
    // built on the machine base even though the request arrived here.
    let base = state.deliverer.machine_base_url(&headers);
    let answer = assignments_for(&state, &node_id, &base).await?;

    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        ASSIGNMENTS_COMPLETE_HEADER,
        HeaderValue::from_static(if answer.complete { "true" } else { "false" }),
    );
    response_headers.insert(
        ASSIGNMENTS_WITHHELD_HEADER,
        // Only ASCII digits, so this cannot fail; a failure would still not
        // be worth failing the request over, and an absent count reads as
        // zero on a node that predates the header.
        HeaderValue::from_str(&answer.withheld.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    Ok((response_headers, Json(answer.views)))
}

/// What a node should be running, and whether that list is the whole truth.
///
/// The two facts travel together because a node may only remove what is
/// missing from a list that says it is complete - see
/// [`ASSIGNMENTS_COMPLETE_HEADER`] for what turns on that.
pub(crate) struct AssignmentAnswer {
    pub(crate) views: Vec<AssignmentView>,
    pub(crate) complete: bool,
    pub(crate) withheld: usize,
}

/// Build one node's assignment list.
///
/// Separate from the HTTP handler above because the machine channel asks for
/// the same list over gRPC, and one of the two callers has no request headers
/// to derive a base URL from. Everything that decides *what* is in the list
/// lives here; the callers only differ in how they say "complete" and where
/// `base` came from.
pub(crate) async fn assignments_for(
    state: &CatalogState,
    node_id: &str,
    base: &str,
) -> ApiResult<AssignmentAnswer> {
    let list = state.store.list_assignments().await?;

    let mut views: Vec<AssignmentView> = Vec::with_capacity(list.assignments.len());

    // Everything this answer should have named and does not. The catalog
    // starts it off: an environment that is deployed and whose version row is
    // missing never reaches the loop below, and counting only what the loop
    // itself skips would report that answer as the whole truth while it was
    // short by exactly those environments.
    let mut withheld: usize = list.undescribable.len();
    for broken in &list.undescribable {
        error!(
            node = %node_id,
            app_id = %broken.app_id,
            environment = %broken.environment,
            version_id = %broken.version_id,
            "The catalog holds a deployment for this environment that it cannot describe, so it is not in this node's list; the list is marked incomplete, which is what stops the node from treating the gap as a deletion"
        );
    }

    for assignment in &list.assignments {
        // One environment whose secrets will not decrypt must not cost the
        // node every other application on the list, so it is dropped from
        // this answer with the reason said out loud. The node then simply
        // does not run it, which is the same outcome a refused deployment
        // has, rather than running it with a hole in its environment.
        let (
            env,
            limits,
            compatibility,
            canonical_application_id,
            canonical_environment_id,
            standard_environment,
            deployment_action,
        ) = match state.deliverer.assignment_payload(assignment, base).await {
            Ok(payload) => {
                let deployment_action = match payload.action() {
                    crate::runtime_client::proto::DeploymentAction::Prepare => "prepare",
                    crate::runtime_client::proto::DeploymentAction::Activate
                    | crate::runtime_client::proto::DeploymentAction::Unspecified => "activate",
                    crate::runtime_client::proto::DeploymentAction::Abort
                    | crate::runtime_client::proto::DeploymentAction::Drain => {
                        error!(
                            node = %node_id,
                            deployment_id = %assignment.deployment_id,
                            "Assignment payload resolved to an action that cannot appear in an authoritative assignment list"
                        );
                        withheld += 1;
                        continue;
                    }
                };
                let standard_environment = payload.standard_environment_for_wire();
                (
                    payload.env_for_wire(),
                    payload.limits.clone(),
                    payload.compatibility,
                    payload.canonical_application_id,
                    payload.canonical_environment_id,
                    standard_environment,
                    deployment_action,
                )
            }
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
                base,
                &assignment.app_id,
                &assignment.version_id,
            ),
            app_id: assignment.app_id.clone(),
            canonical_application_id,
            version_id: assignment.version_id.clone(),
            deployment_id: assignment.deployment_id.clone(),
            routing_generation: assignment.routing_generation.clone(),
            deployment_action,
            environment: assignment.environment.clone(),
            canonical_environment_id,
            bundle_sha: assignment.bundle_sha.clone(),
            env,
            limits,
            compatibility,
            environment_declarations: standard_environment
                .as_ref()
                .map(|(declarations, _, _)| declarations.clone()),
            environment_snapshot: standard_environment
                .as_ref()
                .map(|(_, snapshot, _)| snapshot.clone()),
            environment_snapshot_id: standard_environment
                .as_ref()
                .map(|(_, _, identity)| identity.clone()),
        });
    }

    // The answer is authoritative only when every environment the catalog
    // holds a deployment for could be described, whether it fell over in the
    // catalog's own query or in the loop above. That is what lets a node use
    // this answer to remove things: an application the catalog no longer
    // names is simply absent here, and absence is an instruction. Absence
    // caused by this control plane failing to describe a row is not, which is
    // the whole reason the two cases are told apart on the wire.
    let complete = withheld == 0;

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
    Ok(AssignmentAnswer {
        views,
        complete,
        withheld,
    })
}

/// One deployment a node may act for: where its secrets live, and the
/// storage tenant and generation a capability for it may name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisibleDeployment {
    /// Primary key of the environment row: the half of a secret revision's
    /// storage key and AAD that the snapshot does not carry.
    pub env_id: String,
    /// `None` for a deployment that predates typed snapshots, which binds no
    /// secret revisions and so has nothing to fetch.
    pub snapshot_id: Option<String>,
    /// `<app>.<environment>`, as `deis_wire::conventions::tenant_id` spells
    /// it: the tenant a store, queue or telemetry capability names.
    pub tenant: String,
    /// The routing generation this deployment serves under.
    pub routing_generation: String,
}

/// Which deployments `node_id` may act for - read secret values of, and
/// obtain capabilities for - keyed by id.
///
/// The single place that decides it, so that a placement model narrows it
/// here and nowhere else. Today every node of the fleet runs every
/// application, so this is:
///
/// - every current assignment, **if `node_id` is in the fleet** - registered
///   (with the name it proved) or configured. A machine this control plane
///   has no record of is running nothing of ours and is owed nothing, even
///   if it holds a certificate this fleet signed;
/// - every in-flight plan that names `node_id` as a target: a PREPARE push
///   reaches the node before the deployment pointer is written, and the node
///   fetches its secrets while preparing. Plans freeze their targets, so a
///   node that dropped out of the fleet mid-plan still finishes what it was
///   sent.
pub(crate) async fn deployments_visible_to(
    state: &CatalogState,
    node_id: &str,
) -> crate::catalog::CatalogResult<BTreeMap<String, VisibleDeployment>> {
    let mut visible = BTreeMap::new();
    if state.nodes.node(node_id).is_some() {
        for assignment in state.store.list_assignments().await?.assignments {
            visible.insert(
                assignment.deployment_id,
                VisibleDeployment {
                    env_id: assignment.environment_id,
                    snapshot_id: assignment.snapshot_id,
                    tenant: deis_wire::conventions::tenant_id(
                        &assignment.app_id,
                        &assignment.environment,
                    ),
                    routing_generation: assignment.routing_generation,
                },
            );
        }
    }
    for plan in state.store.nonterminal_deployment_plans().await? {
        if !plan.target_ids.iter().any(|target| target == node_id)
            || visible.contains_key(&plan.id)
        {
            continue;
        }
        let environment = state.store.get_environment_by_id(&plan.env_id).await?;
        visible.insert(
            plan.id,
            VisibleDeployment {
                env_id: plan.env_id,
                snapshot_id: plan.snapshot_id,
                tenant: deis_wire::conventions::tenant_id(&plan.app_id, &environment.name),
                routing_generation: plan.routing_generation.to_string(),
            },
        );
    }
    Ok(visible)
}
