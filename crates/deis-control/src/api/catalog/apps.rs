//! App lifecycle: create, list, inspect, delete.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::CatalogState;
use crate::api::error::{ApiError, ApiResult};
use crate::catalog::App;

#[derive(Debug, Deserialize)]
pub struct CreateAppRequest {
    /// App name; also the app_id every component keys off.
    pub id: String,
}

#[derive(Debug, Serialize)]
pub struct AppsResponse {
    pub apps: Vec<App>,
    pub total: usize,
}

#[derive(Debug, Serialize)]
pub struct DeleteAppResponse {
    pub app_id: String,
    pub removed: crate::catalog::DeletedApp,
    pub bundle_files_removed: usize,
    /// Which nodes stopped serving the app, and which did not.
    ///
    /// A node in `failed` is still running the deleted application - its
    /// code, its secrets in memory and its queue consumers - so the caller
    /// has to see that here; nothing else will ever mention this app again.
    pub unloaded: crate::delivery::RemovalReport,
}

/// POST /api/apps
pub async fn create_app(
    State(state): State<CatalogState>,
    Json(request): Json<CreateAppRequest>,
) -> ApiResult<(StatusCode, Json<App>)> {
    let app = state.store.create_app(request.id.trim()).await?;
    Ok((StatusCode::CREATED, Json(app)))
}

/// GET /api/apps
pub async fn list_apps(State(state): State<CatalogState>) -> ApiResult<Json<AppsResponse>> {
    let apps = state.store.list_apps().await?;
    Ok(Json(AppsResponse {
        total: apps.len(),
        apps,
    }))
}

/// GET /api/apps/:app_id
pub async fn get_app(
    State(state): State<CatalogState>,
    Path(app_id): Path<String>,
) -> ApiResult<Json<App>> {
    Ok(Json(state.store.get_app(&app_id).await?))
}

/// DELETE /api/apps/:app_id
///
/// Removes the app together with its versions, environments, deployment
/// history, routes and secrets, drops the bundle files those versions
/// referenced, and tells every node to stop serving it.
///
/// That last step is not optional. A node adds applications from its
/// assignment list and never removes one that has left it, so a delete that
/// only touched the database would leave every node serving the application
/// forever: still answering requests, still holding its decrypted secrets
/// in memory, still polling its queues. A delete done to revoke access has
/// to actually revoke it, so a node that cannot be reached is reported in
/// the answer rather than passed over.
pub async fn delete_app(
    State(state): State<CatalogState>,
    Path(app_id): Path<String>,
) -> ApiResult<Json<DeleteAppResponse>> {
    let removed = state.store.delete_app(&app_id).await?;

    // Before the bundle files: the nodes are what is still serving the
    // application, and a failure to delete a file must not stop that.
    let unloaded = state
        .deliverer
        .remove_app(&app_id, removed.environment_names.clone())
        .await;

    let mut bundle_files_removed = 0usize;
    for path in &removed.bundle_paths {
        match state.bundles.remove(std::path::Path::new(path)) {
            Ok(true) => bundle_files_removed += 1,
            Ok(false) => warn!(
                app_id = %app_id,
                path = %path,
                "Bundle file of a deleted app was already gone; the database row is removed either way"
            ),
            Err(e) => warn!(
                app_id = %app_id,
                path = %path,
                error = %e,
                "Could not delete a bundle file of a deleted app; it now occupies disk space nothing references"
            ),
        }
    }

    // Best effort: an empty per-app directory is noise, but a leftover one
    // is harmless, so a failure here is not worth a warning.
    let app_dir = state.bundles.root().join(&app_id);
    if std::fs::remove_dir(&app_dir).is_ok() {
        info!(app_id = %app_id, dir = %app_dir.display(), "Removed the now empty bundle directory");
    }

    if unloaded.fully_removed() {
        info!(
            app_id = %app_id,
            bundle_files_removed,
            nodes = unloaded.unloaded.len(),
            "App deletion finished; nothing serves it any more"
        );
    } else {
        warn!(
            app_id = %app_id,
            bundle_files_removed,
            still_serving = ?unloaded.failed.iter().map(|f| f.node.as_str()).collect::<Vec<_>>(),
            "App deletion finished in the catalog only; the listed nodes were not reached and keep serving the deleted application until they are restarted"
        );
    }
    Ok(Json(DeleteAppResponse {
        app_id,
        removed,
        bundle_files_removed,
        unloaded,
    }))
}

/// Shared by every endpoint nested under an app: resolve the app and the
/// environment in one step so a bad app id never reads as a bad
/// environment name.
pub(super) async fn resolve_environment(
    state: &CatalogState,
    app_id: &str,
    env_name: &str,
) -> Result<crate::catalog::Environment, ApiError> {
    state.store.get_app(app_id).await?;
    Ok(state.store.get_environment(app_id, env_name).await?)
}
