//! The runtime fleet: the nodes that receive deployments and run cron
//! occurrences, inspected through their gRPC management plane, and the one
//! operation that takes an app off one of them.
//!
//! Not to be confused with [`crate::api::nodes`], which lists the control
//! channels open to this process. That list is about who is connected to
//! the control plane; this one is about who can be given work.
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
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{error, warn};
use utoipa::{IntoParams, ToSchema};

use super::error::{ApiError, ApiResult};
use super::openapi;
use crate::catalog::CatalogStore;
use crate::runtime_client::{ContestedNodeId, DepartedNode, NodeView, RuntimeNodeManager};

#[derive(Clone)]
pub struct AppsState {
    pub deis_runtime: Arc<RuntimeNodeManager>,
    /// What the fleet is *supposed* to be running.
    ///
    /// Here so that [`unload_app`] can refuse to do something the catalog
    /// would undo. A node's assignment reconcile is the authority on what it
    /// loads, and this endpoint reaches around it; without the catalog to
    /// check against, the only honest answer it could give was one it had no
    /// way to know was true.
    pub catalog: Arc<CatalogStore>,
}

/// The runtime fleet as `GET /api/platform/runtime` reports it.
#[derive(Debug, Serialize, ToSchema)]
pub struct FleetResponse {
    /// Nodes that can be handed work, with the health last probed.
    pub nodes: Vec<NodeView>,
    /// Nodes that stopped refreshing their registration and are presumed
    /// to still run what they last received.
    pub lost: Vec<DepartedNode>,
    /// Machines refused a node id another machine was answering for.
    pub contested: Vec<ContestedNodeId>,
}

/// GET /api/platform/runtime - the runtime fleet: every node that can be
/// handed a deployment or a cron occurrence, with the health last probed
/// for it.
///
/// A node is on this list because the control plane's configuration file
/// names it or because it registered itself; whether it holds a control
/// channel open has nothing to do with it, and the nodes that do that are a
/// different list at `GET /api/platform/sessions` ([`crate::api::nodes`]).
///
/// `lost` is the other half of the same question, and the reason it is served
/// here rather than only appearing inside a deployment's answer: these are
/// nodes that were on the list, stopped refreshing their registration and are
/// presumed to be still running what they last received. Every deploy and
/// every application deletion names them as unreached, so an operator has to
/// be able to see which ones they are and clear the ones whose machine is
/// really gone with `DELETE /api/nodes/{id}/registration`.
///
/// `contested` is the third: machines that registered a node id another
/// machine was already answering for and were refused it. They are running
/// this platform's code, nothing can be sent to them, and they are on no other
/// list - so this is where an operator finds out that two machines were given
/// one name. See `crate::runtime_client::ContestedNodeId`.
#[utoipa::path(
    get,
    path = "/api/platform/runtime",
    operation_id = "listRuntimeFleet",
    tag = "platform",
    responses(
        (status = 200, description = "The fleet, the lost nodes and the contested ids.", body = FleetResponse),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_fleet(State(state): State<AppsState>) -> Json<FleetResponse> {
    Json(FleetResponse {
        nodes: state.deis_runtime.list(),
        lost: state.deis_runtime.departed(),
        contested: state.deis_runtime.contested(),
    })
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StatusParams {
    /// Include per-app details (default true).
    #[serde(default = "default_true")]
    pub apps: bool,
}

fn default_true() -> bool {
    true
}

/// One application on a node, as the node reports it.
#[derive(Serialize, ToSchema)]
pub struct AppStatusView {
    pub app_id: String,
    /// Which environment of that application this line is about. A node
    /// runs production and every preview side by side, each with its own
    /// version and counters, so the pair is what names one of them.
    pub environment: String,
    /// `loading`, `running`, `stopped`, `error` or `unknown`.
    pub state: String,
    pub request_count: i64,
    pub error_count: i64,
    pub error_message: String,
}

/// What a node says about itself. Every member is absent when the node
/// did not report it.
#[derive(Serialize, ToSchema)]
pub struct NodeInfoView {
    #[schema(required)]
    pub node_id: Option<String>,
    #[schema(required)]
    pub version: Option<String>,
    #[schema(required)]
    pub uptime_seconds: Option<i64>,
    #[schema(required)]
    pub hostname: Option<String>,
}

/// The node's own resource reading.
#[derive(Serialize, ToSchema)]
pub struct ResourceUsageView {
    pub cpu_percent: f64,
    pub memory_bytes: i64,
}

/// Live status straight from one node.
#[derive(Serialize, ToSchema)]
pub struct NodeStatusResponse {
    /// The node id as asked for.
    pub node: String,
    pub node_info: NodeInfoView,
    /// Empty when `apps=false` was asked for.
    pub apps: Vec<AppStatusView>,
    #[schema(required)]
    pub resource_usage: Option<ResourceUsageView>,
}

/// GET /api/platform/runtime/{node}/status - live status straight from the
/// node.
#[utoipa::path(
    get,
    path = "/api/platform/runtime/{node}/status",
    operation_id = "getRuntimeNodeStatus",
    tag = "platform",
    params(("node" = String, Path, description = "The node's id in the fleet."), StatusParams),
    responses(
        (status = 200, description = "What the node reported.", body = NodeStatusResponse),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 502, response = openapi::BadGateway),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn node_status(
    State(state): State<AppsState>,
    Path(node): Path<String>,
    Query(params): Query<StatusParams>,
) -> ApiResult<Json<NodeStatusResponse>> {
    let status = match state.deis_runtime.get_status(&node, params.apps).await {
        Ok(status) => status,
        Err(e) => {
            warn!(node = %node, error = %e, "Status query failed");
            return Err(ApiError::from(e));
        }
    };
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
    Ok(Json(NodeStatusResponse {
        node,
        node_info: NodeInfoView {
            node_id: info.map(|i| i.node_id.clone()),
            version: info.map(|i| i.version.clone()),
            uptime_seconds: info.map(|i| i.uptime_seconds),
            hostname: info.map(|i| i.hostname.clone()),
        },
        apps,
        resource_usage: status.resource_usage.as_ref().map(|r| ResourceUsageView {
            cpu_percent: r.cpu_percent,
            memory_bytes: r.memory_bytes,
        }),
    }))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
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

/// The node took the application off.
#[derive(Serialize, ToSchema)]
pub struct AppUnloaded {
    pub success: bool,
    /// What the node said.
    pub message: String,
    /// The environments that were unloaded.
    pub environments: Vec<String>,
}

/// The node answered but did not unload.
#[derive(Serialize, ToSchema)]
pub struct AppNotUnloaded {
    pub success: bool,
    /// What the node said.
    pub message: String,
}

/// The unload was not sent, and why. Not the `ErrorBody` envelope: this
/// answer predates it and is kept byte for byte on every prefix.
#[derive(Serialize, ToSchema)]
pub struct UnloadRefused {
    pub success: bool,
    /// Why nothing was sent, and what to do instead.
    pub error: String,
    /// On 409: the environments the catalog still deploys to this node.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub assigned_environments: Option<Vec<String>>,
    /// On 409: how often the node reconciles its assignment list, in
    /// seconds - the time within which the application would be back.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub assignment_refresh_seconds: Option<u64>,
}

/// DELETE /api/platform/runtime/{node}/apps/{app_id}[?environment=NAME] -
/// unload an app.
///
/// This is a node-local operation and changes nothing in the catalog, which
/// is why it is **refused** while the catalog still deploys the application:
/// a node reconciles against `GET /api/nodes/{id}/assignments` on its own
/// timer, that list is built from the catalog and names the same application,
/// and the node loads it straight back. The endpoint used to send the unload
/// anyway and answer `200 {"success": true}`, so an operator was told the
/// application was off a node while the node was already putting it back -
/// and there was nothing in the answer, or anywhere else, to say so. An
/// operation that reports success and then reverses itself is worse than one
/// that refuses, because the refusal is the only version of it an operator
/// can act on.
///
/// What it is still for: an application the catalog no longer names but a node
/// is still running - a node that was unreachable when the application was
/// deleted, or one that has drifted. That case is the one where the unload
/// sticks, because the next assignment list will not contain it either.
#[utoipa::path(
    delete,
    path = "/api/platform/runtime/{node}/apps/{app_id}",
    operation_id = "unloadRuntimeApp",
    tag = "platform",
    params(
        ("node" = String, Path, description = "The node's id in the fleet."),
        ("app_id" = String, Path, description = "The application's id."),
        UnloadParams,
    ),
    responses(
        (status = 200, description = "The node took the application off.", body = AppUnloaded),
        (status = 404, response = openapi::NotFound),
        (status = 409, description = "The catalog still deploys the application to this node, or the node's key is retired; nothing was sent.", body = UnloadRefused),
        (status = 422, description = "The node answered but did not unload.", body = AppNotUnloaded),
        (status = 502, response = openapi::BadGateway),
        (status = 503, description = "The catalog could not be read, so nothing was sent.", body = UnloadRefused),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn unload_app(
    State(state): State<AppsState>,
    Path((node, app_id)): Path<(String, String)>,
    Query(params): Query<UnloadParams>,
) -> ApiResult<Response> {
    let environment = params.environment.unwrap_or_default();

    let assigned = match assigned_environments(&state, &app_id, &environment).await {
        Ok(assigned) => assigned,
        Err(e) => {
            // Not "unload anyway". The check is what decides whether this
            // operation can keep its word, so a check that did not run leaves
            // nothing to stand behind the answer.
            error!(
                node = %node,
                app_id = %app_id,
                error = %e,
                "Could not read the catalog, so it is not known whether this node would load the application straight back; the unload was not sent and this node is still serving it"
            );
            // This body is not the error envelope, so nothing downstream
            // makes it opaque: what the catalog said - an SQLite sentence
            // naming a column, a file or a lock - would travel to a caller
            // holding nothing but a deploy scope. The caller gets the fixed
            // sentence and the ids this answer already echoes, which is
            // what the log line above is found by.
            let reason = format!(
                "cannot read the catalog to check whether {app_id} would be reassigned to this node; quote the x-request-id of this answer to an operator, whose log has why. Nothing was sent to the node"
            );
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(UnloadRefused {
                    success: false,
                    error: reason,
                    assigned_environments: None,
                    assignment_refresh_seconds: None,
                }),
            )
                .into_response());
        }
    };

    if !assigned.is_empty() {
        // The node's own reconcile timer, which is what would undo this: the
        // control plane tells every node to come back on this interval when it
        // registers, so it is the number of seconds an operator would have
        // been told the application was off a node it was already back on.
        let refresh_secs = state.deis_runtime.registration_refresh_interval().as_secs();
        warn!(
            node = %node,
            app_id = %app_id,
            environments = ?assigned,
            assignment_refresh_seconds = refresh_secs,
            "Refused to unload an application the catalog still deploys; the node reconciles its assignment list every {refresh_secs}s and would load it straight back, so the unload was not sent"
        );
        return Ok((
            StatusCode::CONFLICT,
            Json(UnloadRefused {
                success: false,
                error: format!(
                    "the catalog still deploys {app_id} ({}), and this node reconciles its assignment list every {refresh_secs}s, so it would be serving it again within that time. Nothing was sent to the node. Take the application off the fleet with DELETE /api/v1/apps/{app_id}, which removes it from the catalog and unloads it everywhere at once",
                    assigned.join(", ")
                ),
                assigned_environments: Some(assigned),
                assignment_refresh_seconds: Some(refresh_secs),
            }),
        )
            .into_response());
    }

    match state
        .deis_runtime
        .unload_app(&node, &app_id, &environment)
        .await
    {
        Ok(resp) if resp.success => Ok((
            StatusCode::OK,
            Json(AppUnloaded {
                success: true,
                message: resp.message,
                environments: resp.environments,
            }),
        )
            .into_response()),
        Ok(resp) => Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(AppNotUnloaded {
                success: false,
                message: resp.message,
            }),
        )
            .into_response()),
        Err(e) => {
            warn!(
                node = %node,
                app_id = %app_id,
                environment = %environment,
                error = %e,
                "Unload failed; this node is still serving the application"
            );
            Err(ApiError::from(e))
        }
    }
}

/// Environments of `app_id` the catalog currently holds a deployment for, and
/// which a node would therefore be told to run on its next reconcile.
///
/// `environment` empty means every environment of the application, which is
/// what an unload that names none asks for.
///
/// An environment whose deployment the catalog cannot fully describe counts
/// too. It is left out of the assignment list, so an unload of it would in
/// fact stick - but the catalog still says that application belongs on the
/// fleet, and answering "removed" for it would report a divergence as a
/// completed operation.
async fn assigned_environments(
    state: &AppsState,
    app_id: &str,
    environment: &str,
) -> Result<Vec<String>, crate::catalog::CatalogError> {
    let list = state.catalog.list_assignments().await?;
    let mut names: Vec<String> = list
        .assignments
        .iter()
        .filter(|assignment| assignment.app_id == app_id)
        .map(|assignment| assignment.environment.clone())
        .chain(
            list.undescribable
                .iter()
                .filter(|broken| broken.app_id == app_id)
                .map(|broken| broken.environment.clone()),
        )
        .filter(|name| environment.is_empty() || name == environment)
        .collect();
    names.sort_unstable();
    names.dedup();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use axum::body::to_bytes;

    const MANIFEST: &str = r#"{"schema":1,"app":"demo","entry":"worker/index.js"}"#;

    /// A control plane whose catalog deploys `demo` to production, and a fleet
    /// with one node. Nothing is ever dialled: every test here has to be
    /// refused before an RPC is attempted, which is itself part of what is
    /// being checked.
    async fn state_with_demo_deployed() -> (AppsState, Arc<CatalogStore>) {
        let database = Arc::new(Database::init(":memory:").await.unwrap());
        let catalog = Arc::new(CatalogStore::new(database));
        catalog.create_app("demo").await.unwrap();
        let env = catalog
            .create_environment("demo", "production")
            .await
            .unwrap();
        let (version, _) = catalog
            .create_version("demo", &format!("{:064x}", 7), "/tmp/demo.zip", MANIFEST)
            .await
            .unwrap();
        catalog.deploy(&env, &version.id, None).await.unwrap();

        const TRUST_DOMAIN: &str = "deis.internal";

        // A node this control plane could dial, pointed at a port nothing
        // holds. Both halves matter: without an identity and a registered
        // name `channel` refuses before it opens a socket, and a test that
        // wanted to prove the request left this process would instead be
        // proving it never started.
        let authority = deis_identity_test_pki::TestCa::new();
        let pki = tempfile::tempdir().expect("a directory for the identity");
        let control_name = deis_identity::names::control(TRUST_DOMAIN).expect("a control name");
        let files = deis_identity_test_pki::write_files(
            pki.path(),
            "control",
            &authority.issue(&control_name),
            &authority.root_pem,
        );
        let identity = Arc::new(
            deis_identity::MachineIdentity::load(files.as_identity_files(), &[])
                .expect("the control plane's own identity"),
        );
        let nodes =
            Arc::new(RuntimeNodeManager::new(Vec::new(), 15).with_machine_identity(Some(identity)));
        nodes
            .register(
                "node-a",
                crate::runtime_client::NodeClaim {
                    addr: "http://127.0.0.1:1".to_string(),
                    // Port 1, so the dial is made and refused rather than
                    // hanging on something that might answer.
                    machine_addr: Some("https://127.0.0.1:1".to_string()),
                    version: None,
                    identity: deis_identity::names::node(TRUST_DOMAIN, "node-a")
                        .expect("a node name"),
                },
                chrono::Utc::now(),
            )
            .expect("the node registers");

        let state = AppsState {
            deis_runtime: nodes,
            catalog: Arc::clone(&catalog),
        };
        (state, catalog)
    }

    async fn unload(state: &AppsState, environment: Option<&str>) -> (StatusCode, String) {
        let response = unload_app(
            State(state.clone()),
            Path(("node-a".to_string(), "demo".to_string())),
            Query(UnloadParams {
                environment: environment.map(str::to_string),
            }),
        )
        .await
        .into_response();
        let status = response.status();
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    /// The endpoint used to send the unload and answer `200 success` while
    /// the catalog still deployed the application, so the node loaded it back
    /// on its next reconcile: an operator was told an application was off a
    /// node that was already putting it on again.
    #[tokio::test]
    async fn unloading_an_application_the_catalog_still_deploys_is_refused() {
        let (state, catalog) = state_with_demo_deployed().await;

        let (status, body) = unload(&state, None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body.contains("production"),
            "the answer has to name what is still deployed: {body}"
        );
        assert!(
            body.contains("DELETE /api/v1/apps/demo"),
            "the answer has to name the operation that does stick: {body}"
        );

        // And the reason it was refused is still true: the assignment list a
        // node reconciles against names the application, so an unload that
        // had gone through would have been undone.
        let assignments = catalog.list_assignments().await.unwrap();
        assert!(
            assignments
                .assignments
                .iter()
                .any(|assignment| assignment.app_id == "demo"),
            "the catalog would hand demo straight back to this node"
        );
    }

    /// The catalog read that decides this answer can fail, and what it
    /// says is an SQLite sentence. A caller holding nothing but a deploy
    /// scope must not read it; the answer says what to quote instead.
    #[tokio::test]
    async fn a_catalog_failure_says_nothing_about_sqlite_to_the_caller() {
        let (state, _) = state_with_demo_deployed().await;
        // The read the refusal stands on, broken the way a half-applied
        // migration or a dropped table breaks it.
        state
            .catalog
            .database()
            .with_connection(|connection| {
                connection.execute_batch("DROP TABLE deployments;")?;
                Ok::<_, crate::catalog::CatalogError>(())
            })
            .await
            .unwrap();

        let (status, body) = unload(&state, None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(
            !body.contains("no such table") && !body.contains("SQL"),
            "the body carries the catalog's own text: {body}"
        );
        assert!(
            body.contains("x-request-id"),
            "the answer has to say what to quote instead: {body}"
        );
        assert!(
            body.contains("Nothing was sent to the node"),
            "the answer still has to say nothing was sent: {body}"
        );
    }

    /// Naming an environment does not change the answer while that
    /// environment is the deployed one.
    #[tokio::test]
    async fn naming_the_deployed_environment_is_refused_too() {
        let (state, _) = state_with_demo_deployed().await;
        let (status, body) = unload(&state, Some("production")).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }

    /// An environment the catalog does not deploy is the case this endpoint
    /// is for: nothing would put it back, so the call goes to the node. It
    /// fails here only because nothing is listening at the node's address,
    /// which is what proves the request left this process.
    #[tokio::test]
    async fn an_environment_the_catalog_does_not_deploy_reaches_the_node() {
        let (state, _) = state_with_demo_deployed().await;
        let (status, body) = unload(&state, Some("preview-7")).await;
        assert_eq!(
            status,
            StatusCode::BAD_GATEWAY,
            "the unload has to be attempted, not refused: {body}"
        );
    }
}
