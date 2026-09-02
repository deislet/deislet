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
    response::{IntoResponse, Json},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tracing::{error, warn};

use crate::catalog::CatalogStore;
use crate::runtime_client::{NodeError, RuntimeNodeManager};

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

fn node_error_response(e: &NodeError) -> (StatusCode, Json<serde_json::Value>) {
    let status = match e {
        NodeError::UnknownNode => StatusCode::NOT_FOUND,
        NodeError::Transport(_) => StatusCode::BAD_GATEWAY,
        NodeError::Rpc { .. } => StatusCode::BAD_GATEWAY,
        // Nothing was dialled: the node is on the list but the key it
        // proved is no longer one this control plane holds. Not the caller's
        // fault and not a gateway failure - the node has to register again.
        NodeError::KeyRetired { .. } => StatusCode::CONFLICT,
    };
    (status, Json(json!({ "error": e.to_string() })))
}

/// GET /api/runtime — the runtime fleet: every node that can be handed a
/// deployment or a cron occurrence, with the health last probed for it.
///
/// A node is on this list because the control plane's configuration file
/// names it or because it registered itself; holding a gRPC control channel
/// open has nothing to do with it, and the nodes that do that are a
/// different list at `GET /api/sessions` ([`crate::api::nodes`]).
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
pub async fn list_fleet(State(state): State<AppsState>) -> impl IntoResponse {
    Json(json!({
        "nodes": state.deis_runtime.list(),
        "lost": state.deis_runtime.departed(),
        "contested": state.deis_runtime.contested(),
    }))
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
pub async fn unload_app(
    State(state): State<AppsState>,
    Path((node, app_id)): Path<(String, String)>,
    Query(params): Query<UnloadParams>,
) -> impl IntoResponse {
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
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "success": false,
                    "error": format!(
                        "cannot read the catalog to check whether {app_id} would be reassigned to this node: {e}. Nothing was sent to the node"
                    ),
                })),
            );
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
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "success": false,
                "error": format!(
                    "the catalog still deploys {app_id} ({}), and this node reconciles its assignment list every {refresh_secs}s, so it would be serving it again within that time. Nothing was sent to the node. Take the application off the fleet with DELETE /api/apps/{app_id}, which removes it from the catalog and unloads it everywhere at once",
                    assigned.join(", ")
                ),
                "assigned_environments": assigned,
                "assignment_refresh_seconds": refresh_secs,
            })),
        );
    }

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

        let state = AppsState {
            deis_runtime: Arc::new(RuntimeNodeManager::new(
                vec![crate::config::RuntimeNodeConfig {
                    id: "node-a".to_string(),
                    // Nothing listens here. A test that reaches the node at
                    // all fails on the connection rather than passing quietly.
                    addr: "http://127.0.0.1:1".to_string(),
                }],
                15,
            )),
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
            body.contains("DELETE /api/apps/demo"),
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
