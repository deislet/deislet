//! Administrative API for durable canonical-identity migration inventory.
//!
//! Starting is asynchronous: once the plan is durable, closing the HTTP
//! connection cannot cancel it. Plans left `planned` or `running` by a stopped
//! Control process are resumed when the next process builds its router.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    routing::get,
};
use deis_wire::{CanonicalId, OperationKind, ProductOperation};
use serde::Serialize;
use tracing::{error, info, warn};
use utoipa::ToSchema;

use super::openapi;
use crate::api::error::{ApiError, ApiResult};
use crate::api::headers::{IDEMPOTENCY_KEY, required_header, single_header};
use crate::api::operations::{ProviderWorkflowCancel, cancel_provider_workflow};
use crate::api::paths::ApiPaths;
use crate::api::versioning::{
    CORRELATION_ID_HEADER as CORRELATION_ID, REQUEST_ID_HEADER as REQUEST_ID,
};
use crate::catalog::{
    BackendMigrationAttempt, CatalogStore, IdentityMigrationCoordinator, IdentityMigrationRun,
    IdentityMigrationState, OperationRequestContext, OperationScopeNames,
    summarize_identity_migration,
};
use crate::config::ControlConfig;
use crate::identity::{Demand, Principal, PrincipalKind, Selector, Verb};

const IF_MATCH: &str = "if-match";

/// What the generic cancel's `403` names as the scope that would have been
/// enough, for every operation it refuses: a template, not the record's
/// values. The record's application and environment, or its absence, must
/// not be readable from the refusal (see `cancel_operation`).
const CANCEL_REFUSAL_SCOPE: &str = "deploy:<app>/<env>";
/// The fixed detail beside it. It says what the route demands of each kind
/// without saying which kind, or whether any operation, is behind this id.
const CANCEL_REFUSAL_DETAIL: &str = "this credential holds no scope over the operation named, if \
                                     there is one: a provider deploy or rollback answers to \
                                     deploy:<app>/<env> of the application and environment in its \
                                     record, an identity migration or backend restore to admin:*/*";

#[derive(Clone)]
enum CoordinatorAvailability {
    Ready(Arc<IdentityMigrationCoordinator>),
    Unavailable(Arc<str>),
}

/// State is separate from the ordinary catalog router because migration
/// management credentials must never enter application catalog handlers.
#[derive(Clone)]
pub struct MigrationApiState {
    store: Arc<CatalogStore>,
    coordinator: CoordinatorAvailability,
    running: Arc<Mutex<BTreeSet<CanonicalId>>>,
    cancelling: Arc<Mutex<BTreeSet<CanonicalId>>>,
}

impl MigrationApiState {
    pub fn new(store: Arc<CatalogStore>, config: &ControlConfig) -> Self {
        let coordinator = match IdentityMigrationCoordinator::from_config(
            Arc::clone(&store),
            config,
        ) {
            Ok(coordinator) => {
                info!(
                    "Identity migration coordination is configured for deis-store and deis-queue; management credentials stay inside Control"
                );
                CoordinatorAvailability::Ready(Arc::new(coordinator))
            }
            Err(error) => {
                warn!(
                    reason = %error,
                    "Identity migration coordination is unavailable; existing operation evidence remains readable, but starting or resuming one is refused before the catalog is fenced"
                );
                CoordinatorAvailability::Unavailable(Arc::from(error.to_string()))
            }
        };
        Self {
            store,
            coordinator,
            running: Arc::new(Mutex::new(BTreeSet::new())),
            cancelling: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    fn ready(&self) -> ApiResult<Arc<IdentityMigrationCoordinator>> {
        match &self.coordinator {
            CoordinatorAvailability::Ready(coordinator) => Ok(Arc::clone(coordinator)),
            CoordinatorAvailability::Unavailable(reason) => {
                Err(ApiError::NotConfigured(reason.to_string()))
            }
        }
    }

    fn launch(
        &self,
        coordinator: Arc<IdentityMigrationCoordinator>,
        application_name: String,
        migration_id: CanonicalId,
    ) -> bool {
        let inserted = self
            .running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(migration_id);
        if !inserted {
            return false;
        }
        let running = Arc::clone(&self.running);
        tokio::spawn(async move {
            match coordinator.coordinate(&application_name).await {
                Ok(run) => info!(
                    migration_id = %run.plan.migration_id,
                    operation_id = %run.plan.operation_id,
                    state = ?run.plan.state,
                    ready_for_copy = run.ready_for_copy,
                    blocking_findings = run.blocking_findings.len(),
                    "Identity migration backend-inventory pass finished"
                ),
                Err(error) => error!(
                    migration_id = %migration_id,
                    app = %application_name,
                    error = %error,
                    "Identity migration backend-inventory pass stopped; its journal is durable and an administrator may retry the same operation"
                ),
            }
            running
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&migration_id);
        });
        true
    }

    fn launch_abort(
        &self,
        coordinator: Arc<IdentityMigrationCoordinator>,
        migration_id: CanonicalId,
    ) -> bool {
        let inserted = self
            .cancelling
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(migration_id);
        if !inserted {
            return false;
        }
        let running = Arc::clone(&self.running);
        let cancelling = Arc::clone(&self.cancelling);
        tokio::spawn(async move {
            loop {
                let still_running = running
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .contains(&migration_id);
                if !still_running {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            match coordinator.abort(migration_id).await {
                Ok(run) => info!(
                    migration_id = %migration_id,
                    operation_id = %run.plan.operation_id,
                    "Identity migration cancellation released every recorded backend fence"
                ),
                Err(error) => error!(
                    migration_id = %migration_id,
                    error = %error,
                    "Identity migration cancellation remains non-terminal because a backend fence could not be proven released"
                ),
            }
            cancelling
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&migration_id);
        });
        true
    }

    /// Resume only records that stopped before the implemented backend pass
    /// reached `waiting`. The spawned enumeration itself keeps router
    /// construction synchronous and never delays readiness on a backend.
    pub fn resume_on_start(&self) {
        let Ok(coordinator) = self.ready() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            error!(
                "The API router was built outside a Tokio runtime, so durable identity migrations cannot be resumed until the process explicitly starts them"
            );
            return;
        };
        let state = self.clone();
        runtime.spawn(async move {
            match state.store.resumable_identity_migrations().await {
                Ok(plans) => {
                    for plan in plans {
                        state.launch(
                            Arc::clone(&coordinator),
                            plan.application_name,
                            plan.migration_id,
                        );
                    }
                }
                Err(error) => error!(
                    error = %error,
                    "Could not enumerate ordinary identity migrations during startup"
                ),
            }
            match state.store.cancelling_identity_migrations().await {
                Ok(plans) => {
                    for plan in plans {
                        state.launch_abort(Arc::clone(&coordinator), plan.migration_id);
                    }
                }
                Err(error) => error!(
                    error = %error,
                    "Could not enumerate cancelling identity migrations during startup"
                ),
            }
        });
    }
}

/// `prefix` is where the routes are mounted: `"/api"` for the legacy flat
/// tree, `""` inside a `/api/v1` nest.
pub fn router(prefix: &str, state: MigrationApiState) -> Router {
    Router::new()
        .route(
            &format!("{prefix}/apps/:app_id/identity-migration"),
            get(get_migration).post(start_migration),
        )
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

/// A public status deliberately omits the fence token and every management
/// credential. The operation and migration IDs are stable polling handles.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MigrationStatusResponse {
    pub schema_version: u32,
    #[schema(value_type = String, format = Uuid)]
    pub migration_id: CanonicalId,
    #[schema(value_type = String, format = Uuid)]
    pub operation_id: CanonicalId,
    pub state: IdentityMigrationState,
    pub legacy_application_name: String,
    #[schema(value_type = String, format = Uuid)]
    pub canonical_application_id: CanonicalId,
    pub expected_application_revision: i64,
    pub record_revision: i64,
    pub attempts: Vec<BackendMigrationAttempt>,
    pub captured_components: Vec<String>,
    pub blocking_findings: Vec<String>,
    pub ready_for_copy: bool,
    pub created_at_unix_seconds: i64,
    pub updated_at_unix_seconds: i64,
}

impl From<IdentityMigrationRun> for MigrationStatusResponse {
    fn from(run: IdentityMigrationRun) -> Self {
        Self {
            schema_version: 1,
            migration_id: run.plan.migration_id,
            operation_id: run.plan.operation_id,
            state: run.plan.state,
            legacy_application_name: run.plan.application_name,
            canonical_application_id: run.plan.canonical_application_id,
            expected_application_revision: run.plan.expected_application_revision,
            record_revision: run.plan.record_revision,
            attempts: run.attempts,
            captured_components: run.captured_components,
            blocking_findings: run.blocking_findings,
            ready_for_copy: run.ready_for_copy,
            created_at_unix_seconds: run.plan.created_at,
            updated_at_unix_seconds: run.plan.updated_at,
        }
    }
}

async fn status(
    state: &MigrationApiState,
    application_name: &str,
) -> ApiResult<MigrationStatusResponse> {
    let plan = state
        .store
        .identity_migration(application_name)
        .await?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "identity migration for application {application_name} not found"
            ))
        })?;
    let attempts = state
        .store
        .identity_migration_backend_attempts(plan.migration_id)
        .await?;
    Ok(summarize_identity_migration(plan, attempts).into())
}

#[utoipa::path(
    get,
    path = "/api/v1/apps/{application_name}/identity-migration",
    operation_id = "getIdentityMigration",
    tag = "product",
    params(("application_name" = String, Path, description = "The application's id.")),
    responses(
        (status = 200, description = "The migration's status.", body = MigrationStatusResponse),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_migration(
    State(state): State<MigrationApiState>,
    Path(application_name): Path<String>,
) -> ApiResult<Json<MigrationStatusResponse>> {
    Ok(Json(status(&state, &application_name).await?))
}

#[utoipa::path(
    post,
    path = "/api/v1/apps/{application_name}/identity-migration",
    operation_id = "startIdentityMigration",
    tag = "product",
    params(("application_name" = String, Path, description = "The application's id."), ("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same operation."), ("If-Match" = String, Header, description = "The application revision the caller expects.")),
    responses(
        (status = 202, description = "The migration was started or is already running; poll `Location`.", body = openapi::ProductOperationDocument, headers(("Location" = String, description = "Where to poll the operation."))),
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
pub async fn start_migration(
    State(state): State<MigrationApiState>,
    Path(application_name): Path<String>,
    headers: HeaderMap,
    paths: ApiPaths,
    Extension(principal): Extension<Principal>,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProductOperation>)> {
    let coordinator = state.ready()?;
    let context = operation_context(&headers, &principal, &application_name)?;
    // Planning is the durable acceptance point. A duplicate request for the
    // same application/revision returns the same operation rather than
    // creating another one.
    let plan = state
        .store
        .plan_identity_migration_with_operation(&application_name, context)
        .await?;
    let migration_id = plan.migration_id;
    let operation_id = plan.operation_id;
    let operation = state
        .store
        .product_operation(operation_id)
        .await?
        .ok_or_else(|| ApiError::InternalError(format!("operation {operation_id} is missing")))?;
    if matches!(
        plan.state,
        IdentityMigrationState::Planned
            | IdentityMigrationState::Running
            | IdentityMigrationState::Waiting
    ) && !operation.state.terminal()
        && operation.state != deis_wire::OperationState::Cancelling
    {
        state.launch(coordinator, application_name, migration_id);
    }
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::LOCATION,
        paths.location(&format!("/operations/{operation_id}"))?,
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
    response_headers.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", operation.revision))
            .map_err(|error| ApiError::InternalError(error.to_string()))?,
    );
    response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok((StatusCode::ACCEPTED, response_headers, Json(operation)))
}

#[utoipa::path(
    get,
    path = "/api/v1/operations/{operation_id}",
    operation_id = "getOperation",
    tag = "product",
    params(("operation_id" = String, Path, description = "The operation's canonical UUIDv7.")),
    responses(
        (status = 200, description = "The operation.", body = openapi::ProductOperationDocument, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_operation(
    State(state): State<MigrationApiState>,
    Path(operation_id): Path<String>,
) -> ApiResult<(HeaderMap, Json<ProductOperation>)> {
    let operation_id = operation_id.parse().map_err(|error| {
        ApiError::BadRequest(format!("operation ID must be a canonical UUIDv7: {error}"))
    })?;
    let operation = state
        .store
        .product_operation(operation_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("operation {operation_id} not found")))?;
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

/// Which controller a cancel is dispatched to, and what it demands of the
/// caller. Decided from the operation record, never from the path: the id in
/// the path says nothing about the operation's application or environment.
enum CancelController {
    /// A provider `deploy` or `rollback` workflow: stopped exactly as the
    /// scoped `cancelProviderDeployment` route stops it, by whoever may
    /// deploy into the application and environment it is scoped to.
    ProviderWorkflow(OperationScopeNames),
    /// An identity migration: the coordinator releases its backend fences.
    IdentityMigration,
    /// A planned backend restore: cancelled in the catalog.
    BackendRestore,
    /// A kind this route has no controller for. Refused with `409`, but only
    /// to an administrator, so an operation's existence is not told to a
    /// caller who may not see it.
    None,
}

impl CancelController {
    /// The exact scope this cancel needs. Everything that is not a provider
    /// workflow in a live application/environment is an administrator's, as
    /// the whole route was before provider workflows were dispatched here.
    fn demand(&self) -> Demand<'_> {
        match self {
            CancelController::ProviderWorkflow(scope) => Demand {
                verb: Verb::Deploy,
                app: Selector::One(&scope.application_name),
                environment: Selector::One(&scope.environment_name),
            },
            CancelController::IdentityMigration
            | CancelController::BackendRestore
            | CancelController::None => Demand {
                verb: Verb::Admin,
                app: Selector::Every,
                environment: Selector::Every,
            },
        }
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/operations/{operation_id}/cancel",
    operation_id = "cancelOperation",
    tag = "product",
    params(("operation_id" = String, Path, description = "The operation's canonical UUIDv7."), ("Idempotency-Key" = String, Header, description = "Required. For a provider `deploy` or `rollback` operation a replay of the same key by the same principal returns the same operation, exactly as on `cancelProviderDeployment`; an identity migration or backend restore cancel is fenced by `If-Match` alone."), ("If-Match" = String, Header, description = "The operation revision the caller expects.")),
    responses(
        (status = 202, description = "Cancellation was requested; poll `Location`. The route dispatches on the operation's `kind`. A provider `deploy` or `rollback` is stopped exactly as `cancelProviderDeployment` stops it - `cancelling` until in-flight provider work settles and reversible side effects are cleaned up, then `cancelled` (or `failed` with the cleanup diagnostic) - and `Location` is the scoped provider deployment, which the same credential can read; it answers to `deploy:<app>/<env>` of the application and environment in the operation's record, and `409` `operation-not-cancellable` once terminal or already cancelling. A `migrate` or `restore` operation answers to `admin:*/*`, is stopped by its own controller, and `Location` is this operation. Any other kind is refused with `409`: this route has no cancellation controller for it.", body = openapi::ProductOperationDocument, headers(("Location" = String, description = "Where to poll the operation: the scoped provider deployment for a provider `deploy` or `rollback`, this operation otherwise."))),
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
    State(state): State<MigrationApiState>,
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
    // and checked before the request is looked at any further. A missing
    // operation is an administrator's question too: a narrower credential
    // learns nothing about which ids exist.
    let current = state.store.product_operation(operation_id).await?;
    let controller = match &current {
        Some(operation) => match operation.kind {
            OperationKind::Deploy | OperationKind::Rollback => state
                .store
                .operation_scope_names(&operation.scope)
                .await?
                .map(CancelController::ProviderWorkflow)
                .unwrap_or(CancelController::None),
            OperationKind::Migrate => CancelController::IdentityMigration,
            OperationKind::Restore => CancelController::BackendRestore,
            _ => CancelController::None,
        },
        None => CancelController::None,
    };
    let demand = controller.demand();
    if let Err(denial) = principal.permits(&demand) {
        // The exact demand, and the denial that names it, stay in the log:
        // both were read from the record, so a body carrying either would
        // tell a caller which application and environment an id they may
        // not see belongs to, and that it exists at all. The body is the
        // same fixed sentence for every refused id, present or not.
        warn!(
            %operation_id,
            kind = current.as_ref().map(|operation| operation.kind.as_str()),
            caller = %principal.label(),
            needed = %demand.required_scope(),
            %denial,
            "Refused a generic operation cancel: the caller authenticated and holds no scope over the operation's application and environment; nothing was changed"
        );
        return Err(ApiError::insufficient_scope(
            CANCEL_REFUSAL_SCOPE,
            CANCEL_REFUSAL_DETAIL,
        ));
    }
    let current =
        current.ok_or_else(|| ApiError::NotFound(format!("operation {operation_id} not found")))?;
    let expected_revision = required_if_match(&headers)?;
    let operation = match controller {
        CancelController::ProviderWorkflow(scope) => {
            // This route has always declared the key required, and the
            // contract cannot relax it without a major version; the scoped
            // route takes it as optional. With a key present the replay
            // semantics are the shared helper's, identical on both routes.
            required_header(&headers, IDEMPOTENCY_KEY)?;
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
        CancelController::IdentityMigration => {
            let coordinator = state.ready()?;
            let plan = state
                .store
                .request_identity_migration_cancellation(operation_id, expected_revision)
                .await?;
            let operation = state
                .store
                .product_operation(operation_id)
                .await?
                .ok_or_else(|| {
                    ApiError::InternalError(format!("operation {operation_id} is missing"))
                })?;
            state.launch_abort(coordinator, plan.migration_id);
            operation
        }
        CancelController::BackendRestore => {
            state
                .store
                .cancel_planned_backend_restore(operation_id, expected_revision)
                .await?
        }
        CancelController::None => {
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

fn operation_context(
    headers: &HeaderMap,
    principal: &Principal,
    application_name: &str,
) -> ApiResult<OperationRequestContext> {
    let idempotency_key = required_header(headers, IDEMPOTENCY_KEY)?;
    let request_id =
        optional_canonical_header(headers, REQUEST_ID)?.unwrap_or_else(CanonicalId::new_v7);
    let correlation_id = optional_canonical_header(headers, CORRELATION_ID)?.unwrap_or(request_id);
    let actor_subject = match principal.kind {
        PrincipalKind::Account => format!(
            "account:{}",
            principal.account.as_deref().unwrap_or("unknown")
        ),
        PrincipalKind::ConfigKey => format!(
            "config-key:{}",
            principal.credential_id.as_deref().unwrap_or("unknown")
        ),
        PrincipalKind::Anonymous => "anonymous:direct-local".to_string(),
    };
    let fingerprint = format!("POST\n/api/apps/{application_name}/identity-migration\nv1");
    OperationRequestContext::new(
        idempotency_key,
        &fingerprint,
        actor_subject,
        request_id,
        correlation_id,
    )
    .map_err(Into::into)
}

fn optional_canonical_header(headers: &HeaderMap, name: &str) -> ApiResult<Option<CanonicalId>> {
    single_header(headers, name)?
        .map(|value| {
            let value = value.to_str().map_err(|_| {
                ApiError::BadRequest(format!("{name} header is not valid visible ASCII"))
            })?;
            value.parse().map_err(|error| {
                ApiError::BadRequest(format!("{name} header must be a canonical UUIDv7: {error}"))
            })
        })
        .transpose()
}

fn required_if_match(headers: &HeaderMap) -> ApiResult<u64> {
    let value = single_header(headers, IF_MATCH)?
        .ok_or_else(|| {
            ApiError::PreconditionRequired(
                "If-Match with the current operation ETag is required".to_string(),
            )
        })?
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
    value.parse().map_err(|_| {
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
    use crate::identity::Principal;

    use super::*;

    #[tokio::test]
    async fn status_is_readable_without_management_credentials_and_never_returns_fence_token() {
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
        let plan = store.plan_identity_migration("shop").await.unwrap();
        let secret_fence_token = plan.fence_token().to_string();
        let state = MigrationApiState {
            store,
            coordinator: CoordinatorAvailability::Unavailable(Arc::from("not configured")),
            running: Arc::new(Mutex::new(BTreeSet::new())),
            cancelling: Arc::new(Mutex::new(BTreeSet::new())),
        };
        // Legacy prefix on purpose: the flat tree's regression coverage.
        let api = router("/api", state);

        let response = api
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/apps/shop/identity-migration")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["migrationId"], plan.migration_id.to_string());
        assert_eq!(json["operationId"], plan.operation_id.to_string());
        assert_eq!(json["state"], "planned");
        assert_eq!(json["readyForCopy"], false);
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(!text.contains(&secret_fence_token));
        assert!(!text.contains("fenceToken"));

        let response = api
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/apps/shop/identity-migration")
                    .body(Body::empty())
                    .map(|mut request| {
                        request.extensions_mut().insert(Principal::anonymous());
                        request
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn start_returns_a_complete_idempotent_operation_resource_and_poll_location() {
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
        let config = ControlConfig {
            store_endpoint: Some("http://127.0.0.1:9".to_string()),
            queue_endpoint: Some("http://127.0.0.1:10".to_string()),
            telemetry_endpoint: Some("http://127.0.0.1:11".to_string()),
            store_migration_admin_token: Some("s".repeat(32)),
            queue_migration_admin_token: Some("q".repeat(32)),
            telemetry_migration_admin_token: Some("t".repeat(32)),
            ..Default::default()
        };
        let api = router("/api", MigrationApiState::new(store, &config));
        let request_id = CanonicalId::new_v7();
        let correlation_id = CanonicalId::new_v7();
        let idempotency_key = "migration-shop-2026-09-04";
        let make_request = || {
            Request::builder()
                .method("POST")
                .uri("/api/apps/shop/identity-migration")
                .header(IDEMPOTENCY_KEY, idempotency_key)
                .header(REQUEST_ID, request_id.to_string())
                .header(CORRELATION_ID, correlation_id.to_string())
                .body(Body::empty())
                .map(|mut request| {
                    request.extensions_mut().insert(Principal::anonymous());
                    request
                })
                .unwrap()
        };

        let response = api.clone().oneshot(make_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(response.headers()[REQUEST_ID], request_id.to_string());
        assert_eq!(
            response.headers()[CORRELATION_ID],
            correlation_id.to_string()
        );
        let location = response.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .to_string();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let operation: ProductOperation = serde_json::from_slice(&body).unwrap();
        assert_eq!(operation.request_id, request_id);
        assert_eq!(operation.correlation_id, correlation_id);
        assert_eq!(operation.kind, deis_wire::OperationKind::Migrate);
        assert_eq!(operation.state, deis_wire::OperationState::Queued);
        assert_eq!(
            location,
            format!("/api/operations/{}", operation.operation_id)
        );
        assert!(
            !String::from_utf8(body.to_vec())
                .unwrap()
                .contains(idempotency_key)
        );

        let duplicate = api.clone().oneshot(make_request()).await.unwrap();
        assert_eq!(duplicate.status(), StatusCode::ACCEPTED);
        let duplicate: ProductOperation =
            serde_json::from_slice(&to_bytes(duplicate.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(duplicate.operation_id, operation.operation_id);
        assert_eq!(
            duplicate.idempotency_key_digest,
            operation.idempotency_key_digest
        );

        let different_key = api
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/apps/shop/identity-migration")
                    .header(IDEMPOTENCY_KEY, "different-migration-request")
                    .body(Body::empty())
                    .map(|mut request| {
                        request.extensions_mut().insert(Principal::anonymous());
                        request
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(different_key.status(), StatusCode::CONFLICT);

        let polled = api
            .oneshot(
                Request::builder()
                    .uri(location)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(polled.status(), StatusCode::OK);
        let polled: ProductOperation =
            serde_json::from_slice(&to_bytes(polled.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(polled.operation_id, operation.operation_id);
    }

    #[tokio::test]
    async fn cancel_requires_if_match_and_becomes_terminal_only_after_release_work() {
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
        let plan = store.plan_identity_migration("shop").await.unwrap();
        let config = ControlConfig {
            store_endpoint: Some("http://127.0.0.1:9".to_string()),
            queue_endpoint: Some("http://127.0.0.1:10".to_string()),
            telemetry_endpoint: Some("http://127.0.0.1:11".to_string()),
            store_migration_admin_token: Some("s".repeat(32)),
            queue_migration_admin_token: Some("q".repeat(32)),
            telemetry_migration_admin_token: Some("t".repeat(32)),
            ..Default::default()
        };
        let api = router("/api", MigrationApiState::new(Arc::clone(&store), &config));
        let path = format!("/api/operations/{}/cancel", plan.operation_id);

        let missing_precondition = api
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .body(Body::empty())
                    .map(|mut request| {
                        request.extensions_mut().insert(Principal::anonymous());
                        request
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            missing_precondition.status(),
            StatusCode::PRECONDITION_REQUIRED
        );

        let stale_precondition = api
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .header(IF_MATCH, "\"9\"")
                    .body(Body::empty())
                    .map(|mut request| {
                        request.extensions_mut().insert(Principal::anonymous());
                        request
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stale_precondition.status(), StatusCode::PRECONDITION_FAILED);

        let unquoted_precondition = api
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .header(IF_MATCH, "1")
                    .body(Body::empty())
                    .map(|mut request| {
                        request.extensions_mut().insert(Principal::anonymous());
                        request
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unquoted_precondition.status(), StatusCode::BAD_REQUEST);

        let response = api
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .header(IF_MATCH, "\"1\"")
                    .body(Body::empty())
                    .map(|mut request| {
                        request.extensions_mut().insert(Principal::anonymous());
                        request
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(response.headers()[header::ETAG], "\"2\"");
        let cancelling: ProductOperation =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(cancelling.state, deis_wire::OperationState::Cancelling);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let operation = store
                    .product_operation(plan.operation_id)
                    .await
                    .unwrap()
                    .unwrap();
                if operation.state == deis_wire::OperationState::Cancelled {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let migration = store.identity_migration("shop").await.unwrap().unwrap();
        assert_eq!(migration.state, IdentityMigrationState::Failed);
        assert_eq!(migration.error_code.as_deref(), Some("OPERATION_CANCELLED"));
    }
    /// The migration router as the gate would hand a request to it: with
    /// the caller in the extensions, sharing the catalog store the provider
    /// routes wrote to. The coordinator is left unconfigured on purpose - a
    /// provider cancel must never need it.
    fn generic_router(store: Arc<CatalogStore>, principal: Principal) -> Router {
        let state = MigrationApiState {
            store,
            coordinator: CoordinatorAvailability::Unavailable(Arc::from("not configured")),
            running: Arc::new(Mutex::new(BTreeSet::new())),
            cancelling: Arc::new(Mutex::new(BTreeSet::new())),
        };
        router("/api", state).layer(Extension(principal))
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
        assert_eq!(response.headers()[header::LOCATION], deployment_uri);
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
        // and this route holds to the Idempotency-Key its contract declares.
        let terminal_etag = etag(workflow["operation"]["revision"].as_u64().unwrap());
        let response = api
            .clone()
            .oneshot(cancel_request(
                &cancel_uri,
                Some("generic-cancel-003"),
                &terminal_etag,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            error_code(&response),
            crate::api::error::codes::OPERATION_NOT_CANCELLABLE
        );
        let response = api
            .oneshot(cancel_request(&cancel_uri, None, &terminal_etag))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            error_code(&response),
            crate::api::error::codes::IDEMPOTENCY_KEY_REQUIRED
        );
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

        // An identity migration answers to an administrator, not to the
        // application's deploy credential. Planned on an application of its
        // own: the provider fixture's versions carry placeholder manifests
        // the migration inventory would refuse to read.
        store.create_app("shop").await.unwrap();
        store
            .create_environment("shop", "production")
            .await
            .unwrap();
        let plan = store.plan_identity_migration("shop").await.unwrap();
        let api = generic_router(
            Arc::clone(store),
            scoped_principal("deploy:shop/production"),
        );
        let response = api
            .oneshot(cancel_request(
                &format!("/api/operations/{}/cancel", plan.operation_id),
                Some("migration-cancel"),
                "\"1\"",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            error_code(&response),
            crate::api::error::codes::INSUFFICIENT_SCOPE
        );
        let migration = store.identity_migration("shop").await.unwrap().unwrap();
        assert_eq!(migration.state, IdentityMigrationState::Planned);
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
        // The only producer of operations this store has is the identity
        // migration, and a record's kind is immutable once written; the
        // migration's record is copied under a new id as a kind no
        // controller claims.
        let plan = store.plan_identity_migration("shop").await.unwrap();
        let operation_id = CanonicalId::new_v7();
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
                     SELECT ?1, 'build', organization_id, application_id, environment_id,
                            target_kind, target_id, expected_revision, state, phase, revision,
                            idempotency_key_sha256, request_fingerprint_sha256, actor_id,
                            request_id, correlation_id, progress, cancellable, retryable,
                            child_operation_ids, provider_operations, error_json, created_at,
                            started_at, updated_at, finished_at
                       FROM product_operations WHERE operation_id = ?2",
                    [operation_id.to_string(), plan.operation_id.to_string()],
                )?;
                Ok::<_, crate::catalog::CatalogError>(())
            })
            .await
            .unwrap();
        let cancel_uri = format!("/api/operations/{operation_id}/cancel");

        // An administrator is told there is no controller, as before.
        let api = generic_router(Arc::clone(&store), Principal::anonymous());
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
}
