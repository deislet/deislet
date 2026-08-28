//! Deployment endpoints.
//!
//! A deployment is a pointer row, so every operation here is an insert:
//! deploying moves an environment forward, rolling back moves it
//! backwards, and both leave the full history behind them.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use tracing::info;

use super::ProductState;
use super::versions::VersionView;
use crate::api::error::{ApiError, ApiResult};
use crate::product::{Deployment, Environment, Version};

/// Default and maximum number of history rows returned at once.
const DEFAULT_HISTORY_LIMIT: usize = 50;
const MAX_HISTORY_LIMIT: usize = 500;

#[derive(Debug, Deserialize)]
pub struct DeployRequest {
    /// Target version by id. Takes precedence over `version_number`.
    #[serde(default)]
    pub version_id: Option<String>,
    /// Target version by its per-app number, i.e. the "v3" a human says.
    #[serde(default)]
    pub version_number: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct RollbackRequest {
    /// Explicit target. When omitted, the previous distinct version this
    /// environment ran is used.
    #[serde(default)]
    pub version_id: Option<String>,
    #[serde(default)]
    pub version_number: Option<i64>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct DeploymentResponse {
    pub deployment: Deployment,
    pub environment: String,
    pub version: VersionView,
}

#[derive(Debug, Serialize)]
pub struct DeploymentHistoryResponse {
    pub deployments: Vec<Deployment>,
    pub total: usize,
}

/// POST /api/apps/:app_id/environments/:env/deployments
pub async fn deploy(
    State(state): State<ProductState>,
    Path((app_id, env_name)): Path<(String, String)>,
    Json(request): Json<DeployRequest>,
) -> ApiResult<(StatusCode, Json<DeploymentResponse>)> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    let version = resolve_version(
        &state,
        &app_id,
        request.version_id.as_deref(),
        request.version_number,
    )
    .await?;

    let deployment = state.store.deploy(&env, &version.id, request.note).await?;
    Ok((
        StatusCode::CREATED,
        Json(response(&env, deployment, &version)),
    ))
}

/// POST /api/apps/:app_id/environments/:env/rollback
///
/// Inserts a pointer row aimed at an older version. Nothing is mutated and
/// nothing is deleted, so the rollback itself is just as reversible.
pub async fn rollback(
    State(state): State<ProductState>,
    Path((app_id, env_name)): Path<(String, String)>,
    body: Option<Json<RollbackRequest>>,
) -> ApiResult<(StatusCode, Json<DeploymentResponse>)> {
    let request = body.map(|Json(request)| request).unwrap_or_default();
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;

    let version =
        if request.version_id.is_some() || request.version_number.is_some() {
            resolve_version(
                &state,
                &app_id,
                request.version_id.as_deref(),
                request.version_number,
            )
            .await?
        } else {
            let previous = state.store.previous_version(&env.id).await?.ok_or_else(|| {
            ApiError::Conflict(format!(
                "environment {env_name} of app {app_id} has no earlier version to roll back to"
            ))
        })?;
            state.store.get_version(&app_id, &previous).await?
        };

    let note = request
        .note
        .unwrap_or_else(|| format!("rollback to version {}", version.number));
    let deployment = state.store.deploy(&env, &version.id, Some(note)).await?;

    info!(
        app_id = %app_id,
        environment = %env_name,
        version_id = %version.id,
        number = version.number,
        "Rollback recorded as a new deployment; the previous pointer stays in the history"
    );
    Ok((
        StatusCode::CREATED,
        Json(response(&env, deployment, &version)),
    ))
}

/// GET /api/apps/:app_id/environments/:env/deployment
///
/// The current version of an environment: the newest pointer row.
pub async fn current_deployment(
    State(state): State<ProductState>,
    Path((app_id, env_name)): Path<(String, String)>,
) -> ApiResult<Json<DeploymentResponse>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    let deployment = state
        .store
        .current_deployment(&env.id)
        .await?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "environment {env_name} of app {app_id} has never been deployed"
            ))
        })?;
    let version = state
        .store
        .get_version(&app_id, &deployment.version_id)
        .await?;
    Ok(Json(response(&env, deployment, &version)))
}

/// GET /api/apps/:app_id/environments/:env/deployments
pub async fn list_deployments(
    State(state): State<ProductState>,
    Path((app_id, env_name)): Path<(String, String)>,
    Query(query): Query<HistoryQuery>,
) -> ApiResult<Json<DeploymentHistoryResponse>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_HISTORY_LIMIT)
        .clamp(1, MAX_HISTORY_LIMIT);
    let deployments = state.store.list_deployments(&env.id, limit).await?;
    Ok(Json(DeploymentHistoryResponse {
        total: deployments.len(),
        deployments,
    }))
}

fn response(env: &Environment, deployment: Deployment, version: &Version) -> DeploymentResponse {
    DeploymentResponse {
        deployment,
        environment: env.name.clone(),
        version: VersionView::from(version),
    }
}

/// Accept either form of "which version", and say so plainly when neither
/// was given.
async fn resolve_version(
    state: &ProductState,
    app_id: &str,
    version_id: Option<&str>,
    version_number: Option<i64>,
) -> Result<Version, ApiError> {
    if let Some(version_id) = version_id {
        return Ok(state.store.get_version(app_id, version_id).await?);
    }
    if let Some(number) = version_number {
        return Ok(state.store.get_version_by_number(app_id, number).await?);
    }
    Err(ApiError::BadRequest(
        "name the version to deploy with either \"version_id\" or \"version_number\"".to_string(),
    ))
}
