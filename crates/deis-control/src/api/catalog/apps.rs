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
/// history, routes and secrets, then drops the bundle files those versions
/// referenced. Nodes keep serving what they already loaded until their
/// next assignment refresh, which is why the log line says so.
pub async fn delete_app(
    State(state): State<CatalogState>,
    Path(app_id): Path<String>,
) -> ApiResult<Json<DeleteAppResponse>> {
    let removed = state.store.delete_app(&app_id).await?;

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

    info!(
        app_id = %app_id,
        bundle_files_removed,
        "App deletion finished"
    );
    Ok(Json(DeleteAppResponse {
        app_id,
        removed,
        bundle_files_removed,
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
