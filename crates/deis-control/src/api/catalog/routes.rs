//! Route endpoints. A route binds `host + path prefix` to one environment
//! of one app; the edge reads these to decide which app serves a request.
//!
//! The host in the body is **not** covered by the authorisation
//! `crate::api::authz` did on the way in: that judges the caller against the
//! application and environment named in the URL, which are the caller's own,
//! and a hostname belongs to whoever registered the domain. What decides
//! whether this application may claim this name is the grant registry, read
//! by `catalog::store::CatalogStore::create_route` inside the same database
//! call that writes the row. See `catalog::domains`.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use super::CatalogState;
use crate::api::error::ApiResult;
use crate::api::pagination::{ControlListPage, ListQuery, environment_scope, list_page};
use crate::catalog::{PagePosition, Route};

#[derive(Debug, Deserialize)]
pub struct RouteRequest {
    /// Omit or null to match any domain - which is the *widest* claim on
    /// this platform, not the mildest, and needs the widest grant.
    #[serde(default)]
    pub host: Option<String>,
    pub path_prefix: String,
    /// Higher wins when several routes match. Defaults to 0, and is bounded
    /// (`catalog::model::MAX_ROUTE_PRIORITY`) because it is a caller-supplied
    /// integer that becomes an ordering key at the edge.
    #[serde(default)]
    pub priority: i64,
}

#[derive(Debug, Serialize)]
pub struct RouteResponse {
    pub route: Route,
}

/// The whole routing table as one unpaginated diagnostic (`GET /api/routes`).
///
/// This is deliberately not the unified list envelope: the endpoint is a
/// platform-wide edge diagnostic whose paging is still a pending product
/// decision, and the per-environment list no longer shares this shape.
#[derive(Debug, Serialize)]
pub struct GlobalRoutesResponse {
    pub routes: Vec<Route>,
    pub total: usize,
}

/// POST /api/apps/:app_id/environments/:env/routes
pub async fn create_route(
    State(state): State<CatalogState>,
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
///
/// One environment's routes in the order the matcher applies them: highest
/// priority first, then by path prefix, with the route id as the
/// tie-breaker so paging across two routes on one prefix is stable.
pub async fn list_routes(
    State(state): State<CatalogState>,
    Path((app_id, env_name)): Path<(String, String)>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ControlListPage<Route>>> {
    let (app, env) = super::apps::resolve_app_environment(&state, &app_id, &env_name).await?;
    let collection = "environment-routes";
    let binding = format!(
        "{};filter=all;sort=priority-desc,path-prefix-asc,id-asc-v1",
        environment_scope(app.canonical_id, env.canonical_id)
    );
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, &binding)
        .await?;
    let page = state.store.list_routes_page(&env.id, limit, cursor).await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        &binding,
        limit,
        page,
        |route| PagePosition {
            sort_value: route.priority,
            identity: route.id.clone(),
            secondary: Some(route.path_prefix.clone()),
        },
    )
    .await?;
    Ok(Json(response))
}

/// GET /api/routes
///
/// Every route the platform knows, highest priority first. This is the
/// whole routing table an edge needs in one call, and it stays unpaginated
/// until the product decides how an edge should read a table in pieces.
pub async fn list_all_routes(
    State(state): State<CatalogState>,
) -> ApiResult<Json<GlobalRoutesResponse>> {
    let routes = state.store.list_routes(None, None).await?;
    Ok(Json(GlobalRoutesResponse {
        total: routes.len(),
        routes,
    }))
}

/// PUT /api/apps/:app_id/environments/:env/routes/:route_id
///
/// Replaces the route's fields wholesale, so an omitted `host` clears it
/// back to "any domain" rather than keeping the old value.
pub async fn update_route(
    State(state): State<CatalogState>,
    Path((app_id, env_name, route_id)): Path<(String, String, String)>,
    Json(request): Json<RouteRequest>,
) -> ApiResult<Json<RouteResponse>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    let route = state
        .store
        .update_route(
            &env,
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
    State(state): State<CatalogState>,
    Path((app_id, env_name, route_id)): Path<(String, String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let env = super::apps::resolve_environment(&state, &app_id, &env_name).await?;
    state.store.delete_route(&env.id, &route_id).await?;
    Ok(Json(serde_json::json!({ "deleted": route_id })))
}
