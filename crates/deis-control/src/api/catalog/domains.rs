//! The hostname registry: who may claim which name.
//!
//! A hostname is the one namespace on this platform that is global. An
//! application id belongs to whoever created the application; an environment
//! name belongs to its application; a secret name belongs to its environment.
//! `shop.example.com` belongs to whoever registered the domain, which is a
//! fact about the world that `deploy:<app>/<env>` cannot spell - and a route
//! is created by posting that name into the body of a call authorised
//! against the caller's *own* application and environment. Until this
//! registry existed, a credential scoped to one preview environment could
//! post any hostname on the internet and be answered 201.
//!
//! So the names are written down, by an administrator, and
//! `catalog::store::CatalogStore::create_route` refuses every claim the
//! registry does not cover. These four endpoints are how the registry is
//! written. The rule they feed is in `catalog::domains`; nothing here decides
//! anything, which is deliberate - a second copy of the rule in a handler is
//! how two rules that disagree get shipped.
//!
//! # Why this is an administrator's
//!
//! Granting a name decides which tenant receives somebody's cookies. It is a
//! platform decision, not an application one, so `crate::api::authz` demands
//! `admin:*/*` for all four - the same demand accounts and tokens answer to,
//! and for the same reason.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa::ToSchema;

use super::CatalogState;
use crate::api::error::{ApiError, ApiResult};
use crate::api::openapi;
use crate::api::pagination::{ControlListPage, ListQuery, list_page};
use crate::catalog::domains::EVERY_ENVIRONMENT;
use crate::catalog::{DomainGrant, PagePosition, domain_grant_cursor_secondary};

#[derive(Debug, Deserialize, ToSchema)]
pub struct GrantRequest {
    /// `example.com`, `*.example.com` (every name strictly under the zone,
    /// exactly as a wildcard route matches), `(no-host)` (the route that
    /// names no host, and nothing else) or `*` (every name, including that
    /// route).
    pub pattern: String,
    pub app_id: String,
    /// Which environment of that application may claim it. Omitted means
    /// every environment.
    #[serde(default)]
    pub environment: Option<String>,
    /// Free text for whoever reads this list later. Never interpreted.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GrantResponse {
    pub grant: DomainGrant,
}

/// Which grant a `DELETE` is about. The pattern is in the path; a grant is
/// identified by all three columns, because two applications can hold the
/// same pattern.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GrantSelector {
    /// The application that holds the grant.
    pub app_id: String,
    /// The environment the grant was made for. Absent means the grant that
    /// covers every environment (`*`).
    #[serde(default)]
    pub environment: Option<String>,
}

/// List the hostname grants.
///
/// The whole registry in `(pattern, app_id, environment)` order. The scope
/// is global: a hostname belongs to nobody's application, which is the
/// whole point of the table.
#[utoipa::path(
    get,
    path = "/api/v1/domains",
    operation_id = "listDomainGrants",
    tag = "product",
    params(ListQuery),
    responses(
        (status = 200, description = "One page of hostname grants.", body = ControlListPage<DomainGrant>),
        (status = 400, response = openapi::BadRequest),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn list_grants(
    State(state): State<CatalogState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ControlListPage<DomainGrant>>> {
    let collection = "domain-grants";
    let binding = "scope=global;filter=all;sort=pattern-asc,application-asc,environment-asc-v1";
    let limit = query.bounded_limit()?;
    let cursor = query
        .position(&state.store.page_cursors(), collection, binding)
        .await?;
    let page = state.store.list_domain_grants_page(limit, cursor).await?;
    let response = list_page(
        &state.store.page_cursors(),
        collection,
        binding,
        limit,
        page,
        |grant| PagePosition {
            sort_value: 0,
            identity: grant.pattern.clone(),
            secondary: Some(domain_grant_cursor_secondary(
                &grant.app_id,
                &grant.environment,
            )),
        },
    )
    .await?;
    Ok(Json(response))
}

/// Grant a hostname pattern to an application.
#[utoipa::path(
    post,
    path = "/api/v1/domains",
    operation_id = "createDomainGrant",
    tag = "product",
    request_body = GrantRequest,
    responses(
        (status = 201, description = "The grant as created.", body = GrantResponse),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn create_grant(
    State(state): State<CatalogState>,
    Json(request): Json<GrantRequest>,
) -> ApiResult<(StatusCode, Json<GrantResponse>)> {
    let environment = request
        .environment
        .as_deref()
        .unwrap_or(EVERY_ENVIRONMENT)
        .to_string();
    // No field is named on the 400: the pattern, the application id and
    // the environment are each validated in the store and the refusal does
    // not say which of the three it was, so the envelope says the same as
    // the message rather than guessing. The 409 is unambiguous - this
    // application already holds this pattern for this environment.
    let grant = state
        .store
        .create_domain_grant(
            &request.pattern,
            &request.app_id,
            &environment,
            request.note,
        )
        .await
        .map_err(|error| ApiError::from(error).creating("domain-grant"))?;
    Ok((StatusCode::CREATED, Json(GrantResponse { grant })))
}

/// Withdraw a hostname grant.
///
/// The pattern travels in the path because that is what it is - the name of
/// the thing - and the grantee in the query because a pattern can be held by
/// more than one application and a delete has to say which one it means.
#[utoipa::path(
    delete,
    path = "/api/v1/domains/{pattern}",
    operation_id = "deleteDomainGrant",
    tag = "product",
    params(("pattern" = String, Path, description = "The hostname pattern the grant holds."), GrantSelector),
    responses(
        (status = 200, description = "The grant was withdrawn.", body = GrantWithdrawn),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn delete_grant(
    State(state): State<CatalogState>,
    Path(pattern): Path<String>,
    Query(selector): Query<GrantSelector>,
) -> ApiResult<Json<GrantWithdrawn>> {
    if selector.app_id.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "name which grant to withdraw: a pattern can be held by more than one application, \
             so `?app_id=<app>` is required and `&environment=<env>` narrows it to one \
             environment's grant"
                .to_string(),
        ));
    }
    let environment = selector
        .environment
        .as_deref()
        .unwrap_or(EVERY_ENVIRONMENT)
        .to_string();
    state
        .store
        .delete_domain_grant(&pattern, &selector.app_id, &environment)
        .await?;
    Ok(Json(GrantWithdrawn {
        withdrawn: pattern,
        app_id: selector.app_id,
        environment,
    }))
}

/// A grant was withdrawn.
#[derive(Debug, Serialize, ToSchema)]
pub struct GrantWithdrawn {
    /// The pattern the grant held.
    pub withdrawn: String,
    pub app_id: String,
    /// The environment the grant was scoped to, or `*` for every one.
    pub environment: String,
}
