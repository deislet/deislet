//! The generic operation routes, and what the two ways of cancelling a
//! provider workflow share.
//!
//! A provider `deploy` or `rollback` operation can be stopped through its
//! scoped route, `POST /apps/{app}/environments/{env}/provider-deployments/{id}/cancel`
//! (`api::catalog::providers::cancel_provider_deployment`), or through the
//! generic `POST /operations/{id}/cancel` ([`cancel_operation`]),
//! which dispatches on the operation's kind. Both have to mean exactly the
//! same thing - the same `If-Match` fence, the same optional `Idempotency-Key`
//! replay, the same refusal codes, the same answer - so the one place that
//! says what a cancel is lives here and both handlers call it.

use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    routing::get,
};
use deis_wire::{CanonicalId, OperationKind, ProductOperation};
use tracing::{info, warn};

use crate::api::catalog::actor_subject;
use crate::api::catalog::providers::{keyed_operation_context, operation_headers};
use crate::api::error::ApiError;
use crate::api::error::ApiResult;
use crate::api::headers::{IDEMPOTENCY_KEY, optional_header, single_header};
use crate::api::openapi;
use crate::api::paths::ApiPaths;
use crate::api::versioning::{
    CORRELATION_ID_HEADER as CORRELATION_ID, REQUEST_ID_HEADER as REQUEST_ID,
};
use crate::catalog::{CatalogStore, OperationScopeNames, ProviderDeploymentCancellation};
use crate::identity::{Demand, Principal, Selector, Verb};

const IF_MATCH: &str = "if-match";

/// One cancel request for a provider workflow, as either route received it.
pub(crate) struct ProviderWorkflowCancel<'a> {
    /// The application and environment the workflow belongs to. The scoped
    /// route takes them from its path; the generic route resolves them from
    /// the operation record, after checking the caller against them.
    pub scope: &'a OperationScopeNames,
    pub operation_id: CanonicalId,
    /// The operation revision the caller saw (`If-Match`), already parsed by
    /// the route, so each route keeps its own wording for a missing or
    /// malformed header.
    pub expected_revision: u64,
    /// The route the request came in on, in the legacy spelling the
    /// idempotency fingerprint is built from. Two routes are two requests:
    /// a key replayed across them is refused as a different request.
    pub fingerprint_route: &'a str,
}

/// Ask a provider workflow to stop and answer the way the scoped route does.
///
/// `Location` names the scoped provider deployment rather than the generic
/// operation, whichever route the request came in on: it is the workflow
/// view - the operation together with `cancelRequestedAt` and the provider
/// side of it - that the credential which may cancel a deployment can read,
/// and it is what the scoped route has always answered. The generic
/// operation read answers to the same record's scope and would also serve
/// the caller; keeping one `Location` for both routes keeps their answers
/// identical.
pub(crate) async fn cancel_provider_workflow(
    store: &CatalogStore,
    principal: &Principal,
    headers: &HeaderMap,
    paths: ApiPaths,
    cancel: ProviderWorkflowCancel<'_>,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProductOperation>)> {
    let replay = optional_header(headers, IDEMPOTENCY_KEY)?
        .map(|key| keyed_operation_context(headers, principal, cancel.fingerprint_route, key))
        .transpose()?;
    let operation = store
        .request_provider_deployment_cancellation(ProviderDeploymentCancellation {
            application_id: cancel.scope.application_id,
            environment_id: cancel.scope.environment_id,
            operation_id: cancel.operation_id,
            expected_revision: cancel.expected_revision,
            actor_subject: actor_subject(principal),
            replay,
        })
        .await?;
    info!(
        operation_id = %cancel.operation_id,
        kind = operation.kind.as_str(),
        state = operation.state.as_str(),
        app = %cancel.scope.application_name,
        environment = %cancel.scope.environment_name,
        route = cancel.fingerprint_route,
        caller = %principal.label(),
        "Provider workflow cancel answered"
    );
    let headers = operation_headers(
        paths,
        &operation,
        &cancel.scope.application_name,
        &cancel.scope.environment_name,
    )?;
    Ok((StatusCode::ACCEPTED, headers, Json(operation)))
}

// ---------------------------------------------------------------------------
// The generic operation routes: `GET /operations/{id}` and
// `POST /operations/{id}/cancel`. They dispatch on the operation's kind,
// which is read from its record, never from the path.
// ---------------------------------------------------------------------------

/// State of the generic operation routes: the catalog the records live in.
#[derive(Clone)]
pub struct OperationApiState {
    store: Arc<CatalogStore>,
}

impl OperationApiState {
    pub fn new(store: Arc<CatalogStore>) -> Self {
        Self { store }
    }
}

/// `prefix` is where the routes are mounted: `"/api"` for the flat tree,
/// `""` inside a `/api/v1` nest.
pub fn router(prefix: &str, state: OperationApiState) -> Router {
    Router::new()
        .route(
            &format!("{prefix}/operations/:operation_id"),
            get(get_operation),
        )
        .route(
            &format!("{prefix}/operations/:operation_id/cancel"),
            axum::routing::post(cancel_operation),
        )
        .with_state(state)
}

#[utoipa::path(
    get,
    path = "/api/v1/operations/{operation_id}",
    operation_id = "getOperation",
    tag = "product",
    params(("operation_id" = String, Path, description = "The operation's canonical UUIDv7.")),
    responses(
        (status = 200, description = "The operation. What the route demands is read from the operation's record, not from the path: a provider `deploy` or `rollback` answers to `read:<app>/<env>` of the application and environment in its record, exactly as `getProviderDeployment` does; a `restore` operation, and any other kind, answers to `admin:*/*`. A caller whose scope does not cover the operation is refused with `403` `insufficient-scope` whether or not the operation exists, and with the same body either way.", body = openapi::ProductOperationDocument, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_operation(
    State(state): State<OperationApiState>,
    Path(operation_id): Path<String>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<(HeaderMap, Json<ProductOperation>)> {
    let operation_id: CanonicalId = operation_id.parse().map_err(|error| {
        ApiError::BadRequest(format!("operation ID must be a canonical UUIDv7: {error}"))
    })?;
    // The gate admitted any scope at all (see `authz::RecordClaim`); what
    // this operation actually answers to is in its record, and the caller
    // is checked against it before the answer says whether the id exists.
    let (current, _) =
        settle_operation_access(&state, &principal, operation_id, OperationAccess::Read).await?;
    let operation =
        current.ok_or_else(|| ApiError::NotFound(format!("operation {operation_id} not found")))?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", operation.revision))
            .map_err(|error| ApiError::InternalError(error.to_string()))?,
    );
    headers.insert(
        REQUEST_ID,
        HeaderValue::from_str(&operation.request_id.to_string())
            .map_err(|error| ApiError::InternalError(error.to_string()))?,
    );
    headers.insert(
        CORRELATION_ID,
        HeaderValue::from_str(&operation.correlation_id.to_string())
            .map_err(|error| ApiError::InternalError(error.to_string()))?,
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok((headers, Json(operation)))
}

/// Which controller an operation belongs to, and what it demands of the
/// caller. Decided from the operation record, never from the path: the id in
/// the path says nothing about the operation's application or environment.
/// The generic read and the generic cancel settle it the same way; they
/// differ only in the verb a provider workflow answers to.
enum OperationController {
    /// A provider `deploy` or `rollback` workflow in a live application and
    /// environment: read by whoever may read them, stopped exactly as the
    /// scoped `cancelProviderDeployment` route stops it, by whoever may
    /// deploy into them.
    ProviderWorkflow(OperationScopeNames),
    /// A planned backend restore: cancelled in the catalog.
    BackendRestore,
    /// A kind these routes have no controller for, a provider workflow whose
    /// application or environment no longer resolves, or no operation at
    /// all. An administrator's: the read answers `404` or the record, the
    /// cancel `409`, and a narrower credential is not told which.
    None,
}

/// What a caller wants of an operation, which decides the verb a provider
/// workflow answers to. Everything else answers to `admin:*/*` either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperationAccess {
    /// `GET /operations/{id}`: `read:<app>/<env>`, as the scoped
    /// `getProviderDeployment` route demands.
    Read,
    /// `POST /operations/{id}/cancel`: `deploy:<app>/<env>`, as the scoped
    /// `cancelProviderDeployment` route demands.
    Cancel,
}

impl OperationAccess {
    fn workflow_verb(self) -> Verb {
        match self {
            OperationAccess::Read => Verb::Read,
            OperationAccess::Cancel => Verb::Deploy,
        }
    }

    fn route_name(self) -> &'static str {
        match self {
            OperationAccess::Read => "generic operation read",
            OperationAccess::Cancel => "generic operation cancel",
        }
    }

    /// What this route's `403` names as the scope that would have been
    /// enough, for every operation it refuses: a template in the route's
    /// own verb, not the record's values. The record's application and
    /// environment, or its absence, must not be readable from the refusal
    /// (see [`settle_operation_access`]), so the body is one fixed sentence
    /// per route - the minimal scope the route demands of a provider
    /// workflow, which is what a caller refused here should go and ask for.
    fn refusal_scope(self) -> &'static str {
        match self {
            OperationAccess::Read => "read:<app>/<env>",
            OperationAccess::Cancel => "deploy:<app>/<env>",
        }
    }

    /// The fixed detail beside it. It says what the route demands of each
    /// kind without saying which kind, or whether any operation, is behind
    /// this id.
    fn refusal_detail(self) -> &'static str {
        match self {
            OperationAccess::Read => {
                "this credential holds no scope over the operation named, if there is one: a \
                 provider deploy or rollback answers to read:<app>/<env> of the application and \
                 environment in its record, a backend restore to admin:*/*"
            }
            OperationAccess::Cancel => {
                "this credential holds no scope over the operation named, if there is one: a \
                 provider deploy or rollback answers to deploy:<app>/<env> of the application \
                 and environment in its record, a backend restore to admin:*/*"
            }
        }
    }
}

impl OperationController {
    /// Settle the controller from the record and the application and
    /// environment names its scope resolved to, both read by the one query
    /// in [`CatalogStore::product_operation_with_scope_names`]. A workflow
    /// whose scope no longer resolves - its environment was deleted under
    /// it - has nobody but an administrator left to answer to.
    fn of(current: Option<&(ProductOperation, Option<OperationScopeNames>)>) -> Self {
        match current {
            Some((operation, names)) => match operation.kind {
                OperationKind::Deploy | OperationKind::Rollback => names
                    .clone()
                    .map(OperationController::ProviderWorkflow)
                    .unwrap_or(OperationController::None),
                OperationKind::Restore => OperationController::BackendRestore,
                _ => OperationController::None,
            },
            None => OperationController::None,
        }
    }

    /// The exact scope this access needs. Everything that is not a provider
    /// workflow in a live application/environment is an administrator's, as
    /// both routes were before provider workflows were dispatched here.
    fn demand(&self, access: OperationAccess) -> Demand<'_> {
        match self {
            OperationController::ProviderWorkflow(scope) => Demand {
                verb: access.workflow_verb(),
                app: Selector::One(&scope.application_name),
                environment: Selector::One(&scope.environment_name),
            },
            OperationController::BackendRestore | OperationController::None => Demand {
                verb: Verb::Admin,
                app: Selector::Every,
                environment: Selector::Every,
            },
        }
    }
}

/// Load the operation and check the caller against what its record says
/// it answers to, before either generic route looks at the request any
/// further. The one check both routes share, so that a scope which may read
/// an operation and a scope which may cancel it are decided in one place.
///
/// A missing operation is an administrator's question too: a narrower
/// credential learns nothing about which ids exist. The refusal is the same
/// fixed `insufficient-scope` body for every refused id, present or not,
/// and every id costs the same single database read whatever is behind it,
/// so the time a refusal takes says nothing either.
async fn settle_operation_access(
    state: &OperationApiState,
    principal: &Principal,
    operation_id: CanonicalId,
    access: OperationAccess,
) -> ApiResult<(Option<ProductOperation>, OperationController)> {
    let current = state
        .store
        .product_operation_with_scope_names(operation_id)
        .await?;
    let controller = OperationController::of(current.as_ref());
    let current = current.map(|(operation, _)| operation);
    let demand = controller.demand(access);
    if let Err(denial) = principal.permits(&demand) {
        // The exact demand, and the denial that names it, stay in the log
        // and the audit record: both were read from the record, so a body
        // carrying either would tell a caller which application and
        // environment an id they may not see belongs to, and that it
        // exists at all.
        warn!(
            %operation_id,
            route = access.route_name(),
            kind = current.as_ref().map(|operation| operation.kind.as_str()),
            caller = %principal.label(),
            needed = %demand.required_scope(),
            %denial,
            "Refused a generic operation route: the caller authenticated and holds no scope over the operation's application and environment; nothing was changed and the answer does not say whether the operation exists"
        );
        return Err(
            ApiError::insufficient_scope(access.refusal_scope(), access.refusal_detail())
                .audited_as(denial.to_string()),
        );
    }
    Ok((current, controller))
}

// `Idempotency-Key` is `Option<String>` so the document renders it
// `required: false`, and `nullable = false` so its schema stays the plain
// `string` the baseline promised: a header is present or absent, never null,
// and the compatibility gate reads a type that grew `null` as a change.
#[utoipa::path(
    post,
    path = "/api/v1/operations/{operation_id}/cancel",
    operation_id = "cancelOperation",
    tag = "product",
    params(("operation_id" = String, Path, description = "The operation's canonical UUIDv7."), ("Idempotency-Key" = Option<String>, Header, nullable = false, description = "Optional. For a provider `deploy` or `rollback` operation a replay of the same key by the same principal returns the same operation, exactly as on `cancelProviderDeployment`; without one a redelivered cancel finds nothing left to cancel and answers `409`. A backend restore cancel is fenced by `If-Match` alone."), ("If-Match" = String, Header, description = "The operation revision the caller expects.")),
    responses(
        (status = 202, description = "Cancellation was requested; poll `Location`. The route dispatches on the operation's `kind`. A provider `deploy` or `rollback` is stopped exactly as `cancelProviderDeployment` stops it - `cancelling` until in-flight provider work settles and reversible side effects are cleaned up, then `cancelled` (or `failed` with the cleanup diagnostic) - and `Location` is the scoped provider deployment, which the same credential can read; it answers to `deploy:<app>/<env>` of the application and environment in the operation's record, and `409` `operation-not-cancellable` once terminal or already cancelling. A `restore` operation answers to `admin:*/*`, is stopped by its own controller, and `Location` is this operation. Any other kind is refused with `409`: this route has no cancellation controller for it.", body = openapi::ProductOperationDocument, headers(("Location" = String, description = "Where to poll the operation: the scoped provider deployment for a provider `deploy` or `rollback`, this operation otherwise."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 409, response = openapi::Conflict),
        (status = 412, response = openapi::PreconditionFailed),
        (status = 428, response = openapi::PreconditionRequired),
        (status = 503, response = openapi::ServiceUnavailable),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn cancel_operation(
    State(state): State<OperationApiState>,
    Path(operation_id): Path<String>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    paths: ApiPaths,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProductOperation>)> {
    let operation_id: CanonicalId = operation_id.parse().map_err(|error| {
        ApiError::BadRequest(format!("operation ID must be a canonical UUIDv7: {error}"))
    })?;
    // The gate admitted any scope at all (see `authz::RecordClaim`); what
    // this operation actually answers to is in its record, so that is read
    // and checked before the request is looked at any further.
    let (current, controller) =
        settle_operation_access(&state, &principal, operation_id, OperationAccess::Cancel).await?;
    let current =
        current.ok_or_else(|| ApiError::NotFound(format!("operation {operation_id} not found")))?;
    let expected_revision = required_if_match(&headers)?;
    let operation = match controller {
        OperationController::ProviderWorkflow(scope) => {
            // The `Idempotency-Key` is optional here exactly as it is on the
            // scoped route: `If-Match` is the fence, and the key only makes
            // a redelivered request replay the first answer. With a key
            // present the replay semantics are the shared helper's,
            // identical on both routes; without one, a second cancel finds
            // nothing left to cancel and is told so with `409`.
            let route = format!("/api/operations/{operation_id}/cancel");
            return cancel_provider_workflow(
                &state.store,
                &principal,
                &headers,
                paths,
                ProviderWorkflowCancel {
                    scope: &scope,
                    operation_id,
                    expected_revision,
                    fingerprint_route: &route,
                },
            )
            .await;
        }
        OperationController::BackendRestore => {
            state
                .store
                .cancel_planned_backend_restore(operation_id, expected_revision)
                .await?
        }
        OperationController::None => {
            return Err(ApiError::Conflict(format!(
                "{} operation {operation_id} has no cancellation controller",
                current.kind.as_str()
            )));
        }
    };
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::LOCATION,
        paths.location(&format!("/operations/{operation_id}"))?,
    );
    response_headers.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", operation.revision))
            .map_err(|error| ApiError::InternalError(error.to_string()))?,
    );
    response_headers.insert(
        REQUEST_ID,
        HeaderValue::from_str(&operation.request_id.to_string())
            .map_err(|error| ApiError::InternalError(error.to_string()))?,
    );
    response_headers.insert(
        CORRELATION_ID,
        HeaderValue::from_str(&operation.correlation_id.to_string())
            .map_err(|error| ApiError::InternalError(error.to_string()))?,
    );
    response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok((StatusCode::ACCEPTED, response_headers, Json(operation)))
}

fn required_if_match(headers: &HeaderMap) -> ApiResult<u64> {
    optional_if_match(headers)?.ok_or_else(|| {
        ApiError::PreconditionRequired(
            "If-Match with the current operation ETag is required".to_string(),
        )
    })
}

/// The revision an `If-Match` names, when the caller sent one.
///
/// The same spelling the fenced mutations demand - one strong, quoted
/// decimal revision - read where the header is a fence the caller may
/// choose to set rather than one the route insists on. A value that is
/// there and malformed is still refused: a caller that meant to fence and
/// mis-spelled it must not be treated as one that did not fence at all.
fn optional_if_match(headers: &HeaderMap) -> ApiResult<Option<u64>> {
    let Some(value) = single_header(headers, IF_MATCH)? else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| ApiError::BadRequest("If-Match is not valid visible ASCII".to_string()))?;
    let value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .ok_or_else(|| {
            ApiError::BadRequest(
                "If-Match must contain one strong, quoted operation revision".to_string(),
            )
        })?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ApiError::BadRequest(
            "If-Match must contain one strong, quoted operation revision".to_string(),
        ));
    }
    value.parse().map(Some).map_err(|_| {
        ApiError::BadRequest(
            "If-Match contains an operation revision outside the supported range".to_string(),
        )
    })
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, header};
    use serde_json::Value;
    use tempfile::TempDir;
    use tower::ServiceExt;

    use crate::database::Database;
    use crate::identity::{Principal, PrincipalKind};

    use super::*;

    /// The operation router as the gate would hand a request to it: with
    /// the caller in the extensions, sharing the catalog store the provider
    /// routes wrote to.
    fn generic_router(store: Arc<CatalogStore>, principal: Principal) -> Router {
        router("/api", OperationApiState::new(store)).layer(Extension(principal))
    }

    /// The principal the gate would leave for an admitted caller with every
    /// authority - what an unscoped owner token resolves to. Tests here
    /// insert it directly instead of mounting the real gate.
    fn admitted_principal() -> Principal {
        Principal {
            kind: PrincipalKind::Account,
            account: Some("root".to_string()),
            role: Some(crate::identity::Role::Owner),
            credential_id: Some("0000000000000000".to_string()),
            authority: crate::identity::Authority::Unscoped,
        }
    }

    fn scoped_principal(scopes: &str) -> Principal {
        Principal {
            kind: PrincipalKind::Account,
            account: Some("ci".to_string()),
            role: Some(crate::identity::Role::Deployer),
            credential_id: Some("token-ci".to_string()),
            authority: crate::identity::Authority::Scoped(
                crate::identity::Scope::parse_list(scopes).unwrap(),
            ),
        }
    }

    fn cancel_request(uri: &str, key: Option<&str>, if_match: &str) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header(IF_MATCH, if_match);
        if let Some(key) = key {
            builder = builder.header(IDEMPOTENCY_KEY, key);
        }
        builder.body(Body::empty()).unwrap()
    }

    async fn json(response: axum::response::Response) -> Value {
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&body).unwrap_or(Value::Null)
    }

    /// An error body with its per-request identifier blanked, so two
    /// refusals can be compared for saying exactly the same thing.
    fn without_request_id(body: &Value) -> Value {
        let mut body = body.clone();
        if let Some(request_id) = body
            .get_mut("error")
            .and_then(|error| error.get_mut("requestId"))
        {
            *request_id = Value::Null;
        }
        body
    }

    #[tokio::test]
    async fn generic_cancel_stops_a_queued_provider_deployment_exactly_as_the_scoped_route_does() {
        use crate::api::catalog::providers::tests::{
            HttpTestDriver, attach_artifact, begin_deployment, drive_once, error_code, get,
            provider_fixture, ready_for_dispatch,
        };
        let fixture = crate::catalog::provider_driver_release::tests::Fixture::new();
        let seeded = provider_fixture(&fixture).await;
        let artifact = attach_artifact(&seeded.harness).await;
        let (status, operation) = begin_deployment(
            &seeded.router,
            &seeded.target_id,
            &artifact,
            "deploy-generic",
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{operation}");
        let operation_id = operation["operationId"].as_str().unwrap().to_owned();
        let revision = operation["revision"].as_u64().unwrap();
        let etag = |revision: u64| format!("\"{revision}\"");
        let deployment_uri =
            format!("/api/apps/demo/environments/production/provider-deployments/{operation_id}");
        // The header is spelled for the surface the request context names;
        // this mount carries none, so it is spelled for v1.
        let deployment_location = format!(
            "/api/v1/apps/demo/environments/production/provider-deployments/{operation_id}"
        );
        let cancel_uri = format!("/api/operations/{operation_id}/cancel");

        // The deploy scope of the operation's own application/environment is
        // what cancels it; nothing about the caller is an administrator.
        let api = generic_router(
            Arc::clone(&seeded.harness.state.store),
            scoped_principal("deploy:demo/production"),
        );

        // If-Match is fenced exactly as on the scoped route.
        let response = api
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&cancel_uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_REQUIRED);
        let response = api
            .clone()
            .oneshot(cancel_request(
                &cancel_uri,
                Some("generic-cancel-001"),
                &etag(revision + 5),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);

        let response = api
            .clone()
            .oneshot(cancel_request(
                &cancel_uri,
                Some("generic-cancel-001"),
                &etag(revision),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        // The answer is the scoped route's answer: the deployment the same
        // credential can read, its new revision, and no caching.
        assert_eq!(response.headers()[header::LOCATION], deployment_location);
        assert_eq!(response.headers()[header::ETAG], etag(revision + 1));
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert!(response.headers().contains_key(REQUEST_ID));
        assert!(response.headers().contains_key(CORRELATION_ID));
        let cancelling = json(response).await;
        assert_eq!(cancelling["operationId"], operation_id);
        assert_eq!(cancelling["kind"], "deploy");
        assert_eq!(cancelling["state"], "cancelling");
        assert_eq!(cancelling["cancellable"], false);

        // The same key replays the same answer, stale If-Match and all; a
        // different key finds nothing left to cancel.
        let response = api
            .clone()
            .oneshot(cancel_request(
                &cancel_uri,
                Some("generic-cancel-001"),
                &etag(revision),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(json(response).await, cancelling);
        let response = api
            .clone()
            .oneshot(cancel_request(
                &cancel_uri,
                Some("generic-cancel-002"),
                &etag(revision + 1),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            error_code(&response),
            crate::api::error::codes::OPERATION_NOT_CANCELLABLE
        );

        // The dispatcher settles it: nothing ran, nothing to clean up, and
        // the scoped route shows the same cancelled workflow.
        assert_eq!(
            ready_for_dispatch(&seeded.harness).await,
            std::slice::from_ref(&operation_id)
        );
        let driver = HttpTestDriver {
            manifest: fixture.manifest.clone(),
            calls: std::sync::Mutex::new(Vec::new()),
        };
        drive_once(&seeded, &driver, &operation_id).await;
        assert!(driver.calls.lock().unwrap().is_empty());
        let (status, workflow) = get(&seeded.router, &deployment_uri).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(workflow["operation"]["state"], "cancelled");
        assert!(workflow["cancelRequestedAt"].is_string());

        // A terminal operation refuses a fresh cancel with the stable code,
        // with a key and without one alike: the key is optional here as on
        // the scoped route, so its absence is not a `400` of its own.
        let terminal_etag = etag(workflow["operation"]["revision"].as_u64().unwrap());
        for key in [Some("generic-cancel-003"), None] {
            let response = api
                .clone()
                .oneshot(cancel_request(&cancel_uri, key, &terminal_etag))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CONFLICT, "{key:?}");
            assert_eq!(
                error_code(&response),
                crate::api::error::codes::OPERATION_NOT_CANCELLABLE,
                "{key:?}"
            );
        }
    }

    #[tokio::test]
    async fn generic_cancel_refuses_a_credential_outside_the_operations_scope() {
        use crate::api::catalog::providers::tests::{
            attach_artifact, begin_deployment, error_code, get, provider_fixture,
        };
        let fixture = crate::catalog::provider_driver_release::tests::Fixture::new();
        let seeded = provider_fixture(&fixture).await;
        let artifact = attach_artifact(&seeded.harness).await;
        let (status, operation) = begin_deployment(
            &seeded.router,
            &seeded.target_id,
            &artifact,
            "deploy-refused",
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{operation}");
        let operation_id = operation["operationId"].as_str().unwrap().to_owned();
        let etag = format!("\"{}\"", operation["revision"].as_u64().unwrap());
        let cancel_uri = format!("/api/operations/{operation_id}/cancel");
        let store = &seeded.harness.state.store;

        // Another environment of the same application, a reader of the right
        // environment, and an administrator's scope that is not a deploy
        // scope: each passes the gate's coarse demand and is refused here.
        let mut refusals = Vec::new();
        for held in ["deploy:demo/preview", "read:demo/production", "admin:*/*"] {
            let api = generic_router(Arc::clone(store), scoped_principal(held));
            let response = api
                .oneshot(cancel_request(&cancel_uri, Some("refused-cancel"), &etag))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{held}");
            assert_eq!(
                error_code(&response),
                crate::api::error::codes::INSUFFICIENT_SCOPE,
                "{held}"
            );
            let refusal = json(response).await;
            assert!(
                !refusal.to_string().contains("demo/production"),
                "the refusal must not name the record's application or environment: {refusal}"
            );
            assert!(
                !refusal.to_string().contains("deploy:demo"),
                "the refusal must not name the record's application: {refusal}"
            );
            refusals.push(refusal);
        }
        // Nothing was changed by the refusals.
        let (status, workflow) = get(
            &seeded.router,
            &format!("/api/apps/demo/environments/production/provider-deployments/{operation_id}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(workflow["operation"]["state"], "queued");
        assert_eq!(workflow["operation"]["cancellable"], true);
        assert!(workflow["cancelRequestedAt"].is_null());

        // An operation the caller may not see is refused the same way whether
        // it exists or not: a deploy credential learns nothing about ids.
        let api = generic_router(
            Arc::clone(store),
            scoped_principal("deploy:demo/production"),
        );
        let response = api
            .oneshot(cancel_request(
                &format!("/api/operations/{}/cancel", CanonicalId::new_v7()),
                Some("missing-cancel"),
                &etag,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            error_code(&response),
            crate::api::error::codes::INSUFFICIENT_SCOPE
        );
        // ... and the same way as one that exists in another scope: the two
        // bodies differ only in the request id. The held scope is the
        // caller's own and is not in the body either, so every refusal above
        // reads identically too.
        let missing = json(response).await;
        for refusal in &refusals {
            assert_eq!(
                without_request_id(refusal),
                without_request_id(&missing),
                "an existing operation in another scope and a missing id must be refused with the same body"
            );
        }

        // A backend restore answers to an administrator, not to the
        // application's deploy credential.
        let restore = store
            .insert_test_operation(OperationKind::Restore, None)
            .await;
        let api = generic_router(
            Arc::clone(store),
            scoped_principal("deploy:demo/production"),
        );
        let response = api
            .oneshot(cancel_request(
                &format!("/api/operations/{restore}/cancel"),
                Some("restore-cancel"),
                "\"1\"",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            error_code(&response),
            crate::api::error::codes::INSUFFICIENT_SCOPE
        );
        let unchanged = store.product_operation(restore).await.unwrap().unwrap();
        assert_eq!(unchanged.state, deis_wire::OperationState::Queued);
    }

    #[tokio::test]
    async fn generic_cancel_of_a_kind_without_a_controller_is_still_a_conflict() {
        let directory = TempDir::new().unwrap();
        let database = Arc::new(
            Database::init(directory.path().join("control.sqlite"))
                .await
                .unwrap(),
        );
        let store = Arc::new(CatalogStore::new(database));
        store.create_app("shop").await.unwrap();
        store
            .create_environment("shop", "production")
            .await
            .unwrap();
        // A kind no controller claims, scoped to the application.
        let application = store.get_app("shop").await.unwrap().canonical_id;
        let operation_id = store
            .insert_test_operation(OperationKind::Build, Some(application))
            .await;
        let cancel_uri = format!("/api/operations/{operation_id}/cancel");

        // An administrator is told there is no controller, as before.
        let api = generic_router(Arc::clone(&store), admitted_principal());
        let response = api
            .oneshot(cancel_request(&cancel_uri, Some("build-cancel"), "\"1\""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let refusal = json(response).await.to_string();
        assert!(refusal.contains("no cancellation controller"), "{refusal}");
        // A deploy credential is not told even that much.
        let api = generic_router(store, scoped_principal("deploy:shop/production"));
        let response = api
            .oneshot(cancel_request(&cancel_uri, Some("build-cancel"), "\"1\""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// The `Idempotency-Key` is optional on the generic cancel exactly as it
    /// is on the scoped route: a cancel without one is accepted, and a
    /// second cancel without one finds nothing left to cancel rather than a
    /// replay. The keyed replay is proved beside it in
    /// `generic_cancel_stops_a_queued_provider_deployment_exactly_as_the_scoped_route_does`.
    #[tokio::test]
    async fn generic_cancel_takes_the_idempotency_key_as_optional_like_the_scoped_route() {
        use crate::api::catalog::providers::tests::{
            attach_artifact, begin_deployment, error_code, get, provider_fixture,
        };
        let fixture = crate::catalog::provider_driver_release::tests::Fixture::new();
        let seeded = provider_fixture(&fixture).await;
        let artifact = attach_artifact(&seeded.harness).await;
        let (status, operation) = begin_deployment(
            &seeded.router,
            &seeded.target_id,
            &artifact,
            "deploy-unkeyed-cancel",
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{operation}");
        let operation_id = operation["operationId"].as_str().unwrap().to_owned();
        let revision = operation["revision"].as_u64().unwrap();
        let etag = |revision: u64| format!("\"{revision}\"");
        let cancel_uri = format!("/api/operations/{operation_id}/cancel");
        let api = generic_router(
            Arc::clone(&seeded.harness.state.store),
            scoped_principal("deploy:demo/production"),
        );

        let response = api
            .clone()
            .oneshot(cancel_request(&cancel_uri, None, &etag(revision)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response.headers()[header::LOCATION],
            format!(
                "/api/v1/apps/demo/environments/production/provider-deployments/{operation_id}"
            )
        );
        assert_eq!(response.headers()[header::ETAG], etag(revision + 1));
        let cancelling = json(response).await;
        assert_eq!(cancelling["state"], "cancelling");
        let (status, workflow) = get(
            &seeded.router,
            &format!("/api/apps/demo/environments/production/provider-deployments/{operation_id}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(workflow["operation"]["state"], "cancelling");
        assert!(workflow["cancelRequestedAt"].is_string());

        // Without a key there is nothing to replay: the second request is a
        // fresh cancel of an operation already cancelling, and is refused as
        // one, with the revision it saw.
        let response = api
            .oneshot(cancel_request(&cancel_uri, None, &etag(revision + 1)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            error_code(&response),
            crate::api::error::codes::OPERATION_NOT_CANCELLABLE
        );
    }
}
/// The generic operation routes behind the real gate, the way the human API
/// mounts them: the legacy `/api` spelling and the versioned `/api/v1`
/// spelling, each under its own request context and error rendering, with
/// the authorization gate outside both. Everything below asserts on what a
/// credential is answered and on what the audit log then holds.
#[cfg(test)]
mod gated_tests {
    use std::sync::Arc;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use axum::{Router, middleware::from_fn_with_state};
    use serde_json::Value;
    use tower::ServiceExt;

    use super::{CORRELATION_ID, OperationApiState, REQUEST_ID, router};
    use crate::api::authz::{AuthzState, authorize};
    use crate::api::catalog::providers::tests::{
        attach_artifact, begin_deployment, provider_fixture,
    };
    use crate::api::error::codes;
    use crate::api::versioning::{ApiSurface, attach_request_context, render_errors};
    use crate::audit::{AuditQuery, AuditStore, Outcome};
    use crate::catalog::CatalogStore;
    use crate::identity::{Authority, Denial, IdentityStore, Role, Scope, Verb};
    use deis_wire::CanonicalId;

    struct Gated {
        router: Router,
        audit: Arc<AuditStore>,
        /// The secret of the unscoped owner token standing in for the
        /// administrator.
        root_secret: String,
        /// The secret of a token scoped to `deploy:demo/production`.
        deploy_secret: String,
    }

    async fn gated(store: Arc<CatalogStore>) -> Gated {
        let database = store.database();
        let identity = Arc::new(IdentityStore::new(Arc::clone(&database)));
        let audit = Arc::new(AuditStore::new(database));
        // The administrator of these tests is an unscoped owner token: the
        // old flat configuration key no longer exists.
        identity.create_account("root", Role::Owner).await.unwrap();
        let (_, root_secret) = identity
            .create_token("root", "operator", Authority::Unscoped)
            .await
            .unwrap();
        identity.create_account("ci", Role::Deployer).await.unwrap();
        let (_, deploy_secret) = identity
            .create_token(
                "ci",
                "production-pipeline",
                Authority::Scoped(Scope::parse_list("deploy:demo/production").unwrap()),
            )
            .await
            .unwrap();
        let authz = AuthzState {
            identity,
            audit: Arc::clone(&audit),
            bootstrap_file: None,
        };
        let state = OperationApiState::new(store);
        let surfaced = |surface: ApiSurface, mount: Router| {
            mount
                .layer(from_fn_with_state(surface, render_errors))
                .layer(from_fn_with_state(surface, attach_request_context))
        };
        // The flat mount is the spelling the gate's table is written in;
        // the nested one proves a versioned path reads as the route it is.
        let router = surfaced(ApiSurface::V1, router("/api", state.clone()))
            .merge(surfaced(
                ApiSurface::V1,
                Router::new().nest("/api/v1", router("", state)),
            ))
            .layer(from_fn_with_state(authz, authorize));
        Gated {
            router,
            audit,
            root_secret,
            deploy_secret,
        }
    }

    /// One request id for every call, so two answers that must say the
    /// same thing can be compared byte for byte.
    const REQUEST: &str = "01990f44-1000-7000-8000-00000000abcd";

    async fn get(gated: &Gated, uri: &str, secret: &str) -> (StatusCode, HeaderMapAndBody) {
        let request = Request::builder()
            .method("GET")
            .uri(uri)
            .header("host", "localhost:8080")
            .header(header::AUTHORIZATION, format!("Bearer {secret}"))
            .header(REQUEST_ID, REQUEST)
            .body(Body::empty())
            .unwrap();
        let response = gated.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (
            status,
            HeaderMapAndBody {
                headers,
                bytes: bytes.to_vec(),
            },
        )
    }

    struct HeaderMapAndBody {
        headers: axum::http::HeaderMap,
        bytes: Vec<u8>,
    }

    impl HeaderMapAndBody {
        fn json(&self) -> Value {
            serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
        }
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.bytes).into_owned()
        }
    }

    /// The gate is layered outside both mounts, so a call it refuses
    /// never reaches the request-context middleware and the answer is built
    /// by the gate itself. Both ids still come back: a client that sent a
    /// correlation id has to be able to tie the refusal to the calls it
    /// sent around it, and an operator has to find the audit line by the
    /// same id the caller holds.
    #[tokio::test]
    async fn a_refusal_from_the_gate_echoes_both_ids() {
        const CORRELATION: &str = "01990f44-1000-7000-8000-00000000fee1";
        let database = Arc::new(crate::database::Database::init(":memory:").await.unwrap());
        let store = Arc::new(CatalogStore::new(database));
        store.create_app("demo").await.unwrap();
        let gated = gated(store).await;

        for (uri, credential) in [
            // No credential at all, and the gate is enforcing: 401.
            (
                "/api/v1/operations/01990f44-1000-7000-8000-000000000001",
                None,
            ),
            // A credential that reaches one environment of one
            // application, asked for an administrator's route: 403.
            (
                "/api/v1/operations/01990f44-1000-7000-8000-000000000001",
                Some(gated.deploy_secret.clone()),
            ),
            // No route at all under the versioned prefix: the fallback,
            // which is mounted outside everything.
            ("/api/v1/nothing-of-the-kind", None),
        ] {
            let mut request = Request::builder()
                .method("GET")
                .uri(uri)
                .header("host", "localhost:8080")
                .header(REQUEST_ID, REQUEST)
                .header(CORRELATION_ID, CORRELATION);
            if let Some(secret) = credential {
                request = request.header(header::AUTHORIZATION, format!("Bearer {secret}"));
            }
            let response = gated
                .router
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert!(
                response.status().is_client_error(),
                "{uri} was not refused: {}",
                response.status()
            );
            assert_eq!(response.headers()[REQUEST_ID], REQUEST, "{uri}");
            assert_eq!(response.headers()[CORRELATION_ID], CORRELATION, "{uri}");
        }
    }

    /// A deploy credential over one environment reads its own provider
    /// workflow through the generic route and is refused, with one fixed
    /// body, everything else - a backend restore, a workflow in another
    /// environment, an id that does not exist - while an administrator reads
    /// everything and is told when an id is missing. Every refusal is in the
    /// audit log under the route's action; the successful reads are not.
    #[tokio::test]
    async fn generic_operation_read_answers_to_the_scope_in_the_record() {
        let fixture = crate::catalog::provider_driver_release::tests::Fixture::new();
        let seeded = provider_fixture(&fixture).await;
        let artifact = attach_artifact(&seeded.harness).await;
        let (status, operation) =
            begin_deployment(&seeded.router, &seeded.target_id, &artifact, "deploy-read").await;
        assert_eq!(status, StatusCode::ACCEPTED, "{operation}");
        let own = operation["operationId"].as_str().unwrap().to_owned();
        let revision = operation["revision"].as_u64().unwrap();
        let store = Arc::clone(&seeded.harness.state.store);

        // The same workflow again, in the application's other environment:
        // the record is copied under a new id with `preview`'s canonical id,
        // which is what the scope names resolve from, and under a key digest
        // of its own, since one actor may hold one key per kind.
        let preview = store
            .get_environment("demo", "preview")
            .await
            .unwrap()
            .canonical_id
            .to_string();
        let elsewhere = CanonicalId::new_v7();
        {
            let own = own.clone();
            let elsewhere = elsewhere.to_string();
            store
                .database()
                .with_connection(move |connection| {
                    connection.execute(
                        "INSERT INTO product_operations
                            (operation_id, kind, organization_id, application_id, environment_id,
                             target_kind, target_id, expected_revision, state, phase, revision,
                             idempotency_key_sha256, request_fingerprint_sha256, actor_id,
                             request_id, correlation_id, progress, cancellable, retryable,
                             child_operation_ids, provider_operations, error_json, created_at,
                             started_at, updated_at, finished_at)
                         SELECT ?1, kind, organization_id, application_id, ?2,
                                target_kind, target_id, expected_revision, state, phase, revision,
                                lower(hex(randomblob(32))), request_fingerprint_sha256, actor_id,
                                request_id, correlation_id, progress, cancellable, retryable,
                                child_operation_ids, provider_operations, error_json, created_at,
                                started_at, updated_at, finished_at
                           FROM product_operations WHERE operation_id = ?3",
                        [elsewhere, preview, own],
                    )?;
                    Ok::<_, crate::catalog::CatalogError>(())
                })
                .await
                .unwrap();
        }
        // A backend restore: platform-wide, an administrator's.
        let restore = store
            .insert_test_operation(deis_wire::OperationKind::Restore, None)
            .await
            .to_string();
        let unknown = CanonicalId::new_v7().to_string();

        let gated = gated(store).await;
        let deploy = gated.deploy_secret.clone();

        // (a) The credential reads its own workflow, with the revision as
        // the ETag and the answer uncached, on the versioned spelling.
        let (status, answer) = get(&gated, &format!("/api/v1/operations/{own}"), &deploy).await;
        assert_eq!(status, StatusCode::OK, "{}", answer.text());
        assert_eq!(answer.headers[header::ETAG], format!("\"{revision}\""));
        assert_eq!(answer.headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(answer.headers[REQUEST_ID], REQUEST);
        let body = answer.json();
        assert_eq!(body["operationId"], own);
        assert_eq!(body["kind"], "deploy");

        // (b) The restore, the workflow in the other environment and an
        // id that does not exist are all refused with `insufficient-scope`,
        // and - under one request id - with exactly the same bytes: the
        // body names neither the record nor whether there is one.
        let mut refusals = Vec::new();
        for id in [&restore, &elsewhere.to_string(), &unknown] {
            let (status, answer) = get(&gated, &format!("/api/v1/operations/{id}"), &deploy).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{id}: {}", answer.text());
            let body = answer.json();
            assert_eq!(body["error"]["code"], codes::INSUFFICIENT_SCOPE, "{id}");
            assert_eq!(body["error"]["requestId"], REQUEST, "{id}");
            let text = answer.text();
            for secret in ["demo", "production", "preview", id.as_str()] {
                assert!(
                    !text.contains(secret),
                    "the refusal of {id} must not carry {secret:?}: {text}"
                );
            }
            // The read asks the caller for the minimal scope the read
            // demands, not the cancel's wider one.
            assert!(text.contains("read:<app>/<env>"), "{text}");
            assert!(!text.contains("deploy:<app>/<env>"), "{text}");
            refusals.push(answer.bytes);
        }
        assert_eq!(
            refusals[0], refusals[1],
            "a restore and a workflow in another environment must be refused identically"
        );
        assert_eq!(
            refusals[1], refusals[2],
            "a workflow the caller may not see and an id that does not exist must be refused \
             identically"
        );

        // (c) An administrator reads every one of them and is told when an
        // id is missing: the 404 is an administrator's answer.
        for (id, kind) in [
            (&own, "deploy"),
            (&elsewhere.to_string(), "deploy"),
            (&restore, "restore"),
        ] {
            let (status, answer) = get(
                &gated,
                &format!("/api/v1/operations/{id}"),
                &gated.root_secret,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{id}: {}", answer.text());
            assert_eq!(answer.json()["kind"], kind, "{id}");
            assert!(answer.headers.contains_key(header::ETAG), "{id}");
        }
        let (status, answer) = get(
            &gated,
            &format!("/api/v1/operations/{unknown}"),
            &gated.root_secret,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{}", answer.text());
        assert_eq!(answer.json()["error"]["code"], codes::NOT_FOUND);

        // (e) The unversioned spelling the gate's table is written in
        // refuses the same way: the gate reads the record the path names
        // and answers the envelope with the denial, whatever prefix the
        // route was spelled under.
        let (status, answer) = get(&gated, &format!("/api/operations/{restore}"), &deploy).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{}", answer.text());
        let denied = answer.json();
        assert_eq!(denied["error"]["code"], codes::INSUFFICIENT_SCOPE);
        assert_eq!(denied["error"]["requestId"], REQUEST);
        assert_eq!(denied["error"]["retryable"], false);
        let (status, answer) = get(&gated, &format!("/api/operations/{own}"), &deploy).await;
        assert_eq!(status, StatusCode::OK, "{}", answer.text());

        // The gate records the handler's refusals from the status the answer
        // had - four of them, under the route's action, naming the id the
        // caller asked about and, in `detail`, the denial the handler
        // settled, exactly as its own refusals do - and none of the
        // successful reads. The denial is the record's, which the body kept
        // quiet about: the token's scope not covering `read:demo/preview`
        // for the workflow in the other environment, and the deployer role
        // not reaching `admin` for the restore and the unknown id.
        let lines = gated
            .audit
            .query_page(AuditQuery::default(), 100, None)
            .await
            .unwrap()
            .items;
        let reads: Vec<_> = lines
            .iter()
            .filter(|line| line.action == "operation.get")
            .collect();
        assert_eq!(reads.len(), 4, "{reads:#?}");
        let mut refused_ids: Vec<String> = reads
            .iter()
            .map(|line| {
                assert_eq!(line.outcome, Outcome::Refused, "{line:?}");
                assert_eq!(line.status, 403, "{line:?}");
                assert_eq!(line.actor_account.as_deref(), Some("ci"), "{line:?}");
                assert_eq!(line.request_id.as_deref(), Some(REQUEST), "{line:?}");
                let target = line.target.clone().unwrap();
                let denial = if target == elsewhere.to_string() {
                    Denial::Scope {
                        required: "read:demo/preview".to_string(),
                        held: "deploy:demo/production".to_string(),
                    }
                } else {
                    Denial::Role {
                        role: Role::Deployer,
                        verb: Verb::Admin,
                    }
                };
                assert_eq!(
                    line.detail.as_deref(),
                    Some(denial.to_string().as_str()),
                    "the audit line of {target} must name the denial: {line:?}"
                );
                target
            })
            .collect();
        refused_ids.sort();
        let mut expected_ids = vec![
            restore.clone(),
            elsewhere.to_string(),
            unknown.clone(),
            restore.clone(),
        ];
        expected_ids.sort();
        assert_eq!(refused_ids, expected_ids);
        assert!(
            lines.iter().all(|line| line.status == 403),
            "a successful read is not an event: {lines:#?}"
        );
    }
}
