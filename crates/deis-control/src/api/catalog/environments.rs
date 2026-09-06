//! Environment endpoints. An environment is the second key of the whole
//! catalog: deployment pointers, routes and secrets all hang off
//! `app + environment`, never off the app alone.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use utoipa::ToSchema;

use super::CatalogState;
use crate::api::error::ApiResult;
use crate::api::openapi;
use crate::api::pagination::{ControlListPage, ListQuery, id_scope, list_page};
use crate::catalog::Environment;

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateEnvironmentRequest {
    /// `production` or any preview name.
    pub name: String,
}

/// POST /api/apps/:app_id/environments
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/environments",
    operation_id = "createEnvironment",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id.")),
    request_body = CreateEnvironmentRequest,
    responses(
        (status = 201, description = "The environment as created.", body = Environment),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn create_environment(
    State(state): State<CatalogState>,
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
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/environments",
    operation_id = "listEnvironments",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), ListQuery),
    responses(
        (status = 200, description = "One page of environments.", body = ControlListPage<Environment>),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_environments(
    State(state): State<CatalogState>,
    Path(app_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ControlListPage<Environment>>> {
    let app = state.store.get_app(&app_id).await?;
    let collection = "application-environments";
    let binding = format!(
        "{};filter=all;sort=name-asc-v1",
        id_scope("application", app.canonical_id)
    );
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, &binding)
        .await?;
    let page = state
        .store
        .list_environments_page(&app_id, limit, cursor)
        .await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        &binding,
        limit,
        page,
        |environment| crate::catalog::PagePosition {
            sort_value: 0,
            identity: environment.name.clone(),
            secondary: None,
        },
    )
    .await?;
    Ok(Json(response))
}

/// GET /api/apps/:app_id/environments/:env
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/environments/{env_name}",
    operation_id = "getEnvironment",
    tag = "product",
    params(("app_id" = String, Path, description = "The application's id."), ("env_name" = String, Path, description = "The environment's name.")),
    responses(
        (status = 200, description = "The environment.", body = Environment),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_environment(
    State(state): State<CatalogState>,
    Path((app_id, env_name)): Path<(String, String)>,
) -> ApiResult<Json<Environment>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    Ok(Json(env))
}
