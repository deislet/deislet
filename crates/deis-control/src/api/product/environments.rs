//! Environment endpoints. An environment is the second key of the whole
//! product model: deployment pointers, routes and secrets all hang off
//! `app + environment`, never off the app alone.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use super::ProductState;
use crate::api::error::ApiResult;
use crate::product::Environment;

#[derive(Debug, Deserialize)]
pub struct CreateEnvironmentRequest {
    /// `production` or any preview name.
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct EnvironmentsResponse {
    pub environments: Vec<Environment>,
    pub total: usize,
}

/// POST /api/apps/:app_id/environments
pub async fn create_environment(
    State(state): State<ProductState>,
    Path(app_id): Path<String>,
    Json(request): Json<CreateEnvironmentRequest>,
) -> ApiResult<(StatusCode, Json<Environment>)> {
    let env = state
        .store
        .create_environment(&app_id, request.name.trim())
        .await?;
    Ok((StatusCode::CREATED, Json(env)))
}

/// GET /api/apps/:app_id/environments
pub async fn list_environments(
    State(state): State<ProductState>,
    Path(app_id): Path<String>,
) -> ApiResult<Json<EnvironmentsResponse>> {
    let environments = state.store.list_environments(&app_id).await?;
    Ok(Json(EnvironmentsResponse {
        total: environments.len(),
        environments,
    }))
}

/// GET /api/apps/:app_id/environments/:env
pub async fn get_environment(
    State(state): State<ProductState>,
    Path((app_id, env_name)): Path<(String, String)>,
) -> ApiResult<Json<Environment>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    Ok(Json(env))
}
