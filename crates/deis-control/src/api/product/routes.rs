//! Route endpoints. A route binds `host + path prefix` to one environment
//! of one app; the edge reads these to decide which app serves a request.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use super::ProductState;
use crate::api::error::ApiResult;
use crate::product::Route;

#[derive(Debug, Deserialize)]
pub struct RouteRequest {
    /// Omit or null to match any domain.
    #[serde(default)]
    pub host: Option<String>,
    pub path_prefix: String,
    /// Higher wins when several routes match. Defaults to 0.
    #[serde(default)]
    pub priority: i64,
}

#[derive(Debug, Serialize)]
pub struct RouteResponse {
    pub route: Route,
}

#[derive(Debug, Serialize)]
pub struct RoutesResponse {
    pub routes: Vec<Route>,
    pub total: usize,
}

/// POST /api/apps/:app_id/environments/:env/routes
pub async fn create_route(
    State(state): State<ProductState>,
    Path((app_id, env_name)): Path<(String, String)>,
    Json(request): Json<RouteRequest>,
) -> ApiResult<(StatusCode, Json<RouteResponse>)> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    let route = state
        .store
        .create_route(&env, request.host, &request.path_prefix, request.priority)
        .await?;
    Ok((StatusCode::CREATED, Json(RouteResponse { route })))
}

/// GET /api/apps/:app_id/environments/:env/routes
pub async fn list_routes(
    State(state): State<ProductState>,
    Path((app_id, env_name)): Path<(String, String)>,
) -> ApiResult<Json<RoutesResponse>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    let routes = state
        .store
        .list_routes(Some(&app_id), Some(&env.id))
        .await?;
    Ok(Json(RoutesResponse {
        total: routes.len(),
        routes,
    }))
}

/// GET /api/routes
///
/// Every route the platform knows, highest priority first. This is the
/// whole routing table an edge needs in one call.
pub async fn list_all_routes(State(state): State<ProductState>) -> ApiResult<Json<RoutesResponse>> {
    let routes = state.store.list_routes(None, None).await?;
    Ok(Json(RoutesResponse {
        total: routes.len(),
        routes,
    }))
}

/// PUT /api/apps/:app_id/environments/:env/routes/:route_id
///
/// Replaces the route's fields wholesale, so an omitted `host` clears it
/// back to "any domain" rather than keeping the old value.
pub async fn update_route(
    State(state): State<ProductState>,
    Path((app_id, env_name, route_id)): Path<(String, String, String)>,
    Json(request): Json<RouteRequest>,
) -> ApiResult<Json<RouteResponse>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    let route = state
        .store
        .update_route(
            &env.id,
            &route_id,
            request.host,
            &request.path_prefix,
            request.priority,
        )
        .await?;
    Ok(Json(RouteResponse { route }))
}

/// DELETE /api/apps/:app_id/environments/:env/routes/:route_id
pub async fn delete_route(
    State(state): State<ProductState>,
    Path((app_id, env_name, route_id)): Path<(String, String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    state.store.delete_route(&env.id, &route_id).await?;
    Ok(Json(serde_json::json!({ "deleted": route_id })))
}
