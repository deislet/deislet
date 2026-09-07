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
    params(("application_name" = String, Path, description = "The application's id."), ("Idempotency-Key" = String, Header, description = "Required. Replays of the same key return the same operation."), ("If-Match" = String, Header, description = "Required: the application `revision` the caller expects, quoted, as `GET /api/v1/apps/{application_name}` reports it. A migration freezes an inventory of everything the application owns, so the caller says which revision it read that inventory at; the comparison happens inside the transaction that plans the migration, and a revision that has moved since is refused with 412 rather than snapshotted.")),
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
    // The caller's fence, which this route has always documented as
    // required and, until it was read here, did not apply: a migration
    // freezes an inventory of everything the application owns, so the
    // caller says which revision it read that inventory at. The comparison
    // happens inside the planning transaction, where the revision is read.
    let expected_application_revision = optional_if_match(&headers)?.ok_or_else(|| {
        ApiError::PreconditionRequired(
            "If-Match with the application's current revision is required".to_string(),
        )
    })?;
    // Planning is the durable acceptance point. A duplicate request for the
    // same application/revision returns the same operation rather than
    // creating another one.
    let plan = state
        .store
        .plan_identity_migration_with_operation(
            &application_name,
            context,
            Some(expected_application_revision),
        )
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
        (status = 200, description = "The operation. What the route demands is read from the operation's record, not from the path: a provider `deploy` or `rollback` answers to `read:<app>/<env>` of the application and environment in its record, exactly as `getProviderDeployment` does; a `migrate` or `restore` operation, and any other kind, answers to `admin:*/*`. A caller whose scope does not cover the operation is refused with `403` `insufficient-scope` whether or not the operation exists, and with the same body either way.", body = openapi::ProductOperationDocument, headers(("ETag" = String, description = "The resource revision, for `If-Match`."))),
        (status = 400, response = openapi::BadRequest),
        (status = 404, response = openapi::NotFound),
        (status = 401, response = openapi::Unauthenticated),
        (status = 403, response = openapi::Forbidden),
    )
)]
pub async fn get_operation(
    State(state): State<MigrationApiState>,
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
    /// An identity migration: the coordinator releases its backend fences.
    IdentityMigration,
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
                 environment in its record, an identity migration or backend restore to admin:*/*"
            }
            OperationAccess::Cancel => {
                "this credential holds no scope over the operation named, if there is one: a \
                 provider deploy or rollback answers to deploy:<app>/<env> of the application \
                 and environment in its record, an identity migration or backend restore to \
                 admin:*/*"
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
                OperationKind::Migrate => OperationController::IdentityMigration,
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
            OperationController::IdentityMigration
            | OperationController::BackendRestore
            | OperationController::None => Demand {
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
    state: &MigrationApiState,
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
    params(("operation_id" = String, Path, description = "The operation's canonical UUIDv7."), ("Idempotency-Key" = Option<String>, Header, nullable = false, description = "Optional. For a provider `deploy` or `rollback` operation a replay of the same key by the same principal returns the same operation, exactly as on `cancelProviderDeployment`; without one a redelivered cancel finds nothing left to cancel and answers `409`. An identity migration or backend restore cancel is fenced by `If-Match` alone."), ("If-Match" = String, Header, description = "The operation revision the caller expects.")),
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
        OperationController::IdentityMigration => {
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

    /// The route documents an `If-Match` and now honours it: a migration
    /// freezes an inventory of everything the application owns, so a caller
    /// that read that inventory can insist nothing has changed since. The
    /// header used to be read by nothing, which meant a caller fencing
    /// against a stale revision was told its migration had started against
    /// exactly what it had seen.
    #[tokio::test]
    async fn an_if_match_that_names_another_revision_refuses_the_migration() {
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
        let revision = store.get_app("shop").await.unwrap().revision;
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
        let start = |key: &'static str, if_match: Option<String>| {
            let mut builder = Request::builder()
                .method("POST")
                .uri("/api/apps/shop/identity-migration")
                .header(IDEMPOTENCY_KEY, key);
            if let Some(value) = if_match {
                builder = builder.header(header::IF_MATCH, value);
            }
            builder
                .body(Body::empty())
                .map(|mut request| {
                    request.extensions_mut().insert(Principal::anonymous());
                    request
                })
                .unwrap()
        };

        // A revision the application is not at: nothing is planned.
        let response = api
            .clone()
            .oneshot(start(
                "migration-stale-fence",
                Some(format!("\"{}\"", revision + 41)),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);

        // A malformed fence is a mistake, not an absent fence.
        let response = api
            .clone()
            .oneshot(start("migration-bad-fence", Some("not-a-revision".into())))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // No fence at all is refused too: the route has documented the
        // header as required since it was published.
        let response = api
            .clone()
            .oneshot(start("migration-no-fence", None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PRECONDITION_REQUIRED);

        // The revision the application is at: accepted.
        let response = api
            .clone()
            .oneshot(start(
                "migration-good-fence",
                Some(format!("\"{revision}\"")),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
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
        let store_revision = store.get_app("shop").await.unwrap();
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
        let revision = store_revision.revision;
        let make_request = move || {
            Request::builder()
                .method("POST")
                .uri("/api/apps/shop/identity-migration")
                .header(IDEMPOTENCY_KEY, idempotency_key)
                .header(REQUEST_ID, request_id.to_string())
                .header(CORRELATION_ID, correlation_id.to_string())
                // The fence this route demands: the application revision the
                // caller planned against.
                .header(header::IF_MATCH, format!("\"{revision}\""))
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
                    .header(header::IF_MATCH, format!("\"{revision}\""))
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
                    .map(|mut request| {
                        request.extensions_mut().insert(Principal::anonymous());
                        request
                    })
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
            format!("/api/apps/demo/environments/production/provider-deployments/{operation_id}")
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
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use axum::response::IntoResponse;
    use axum::{Router, middleware::from_fn_with_state};
    use serde_json::Value;
    use tower::ServiceExt;

    use super::{
        CORRELATION_ID, CoordinatorAvailability, MigrationApiState, OperationAccess, REQUEST_ID,
        router,
    };
    use crate::api::authz::{AuthzState, authorize};
    use crate::api::catalog::providers::tests::{
        attach_artifact, begin_deployment, provider_fixture,
    };
    use crate::api::error::{ApiError, codes};
    use crate::api::versioning::{ApiSurface, attach_request_context, render_errors};
    use crate::audit::{AuditQuery, AuditStore, Outcome};
    use crate::catalog::CatalogStore;
    use crate::identity::{Authority, Denial, IdentityStore, Role, Scope, Verb};
    use deis_wire::CanonicalId;

    const ROOT_KEY: &str = "the-operators-root-key";

    struct Gated {
        router: Router,
        audit: Arc<AuditStore>,
        /// The secret of a token scoped to `deploy:demo/production`.
        deploy_secret: String,
    }

    async fn gated(store: Arc<CatalogStore>) -> Gated {
        let database = store.database();
        let identity = Arc::new(IdentityStore::new(Arc::clone(&database)));
        let audit = Arc::new(AuditStore::new(database));
        let config_keys = vec![ROOT_KEY.to_string()];
        identity.sync_config_keys(&config_keys).await.unwrap();
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
            enforced: true,
            config_keys: Arc::new(config_keys),
            identity,
            audit: Arc::clone(&audit),
        };
        let state = MigrationApiState {
            store,
            coordinator: CoordinatorAvailability::Unavailable(Arc::from("not configured")),
            running: Arc::new(Mutex::new(BTreeSet::new())),
            cancelling: Arc::new(Mutex::new(BTreeSet::new())),
        };
        let surfaced = |surface: ApiSurface, mount: Router| {
            mount
                .layer(from_fn_with_state(surface, render_errors))
                .layer(from_fn_with_state(surface, attach_request_context))
        };
        let router = surfaced(ApiSurface::Legacy, router("/api", state.clone()))
            .merge(surfaced(
                ApiSurface::V1,
                Router::new().nest("/api/v1", router("", state)),
            ))
            .layer(from_fn_with_state(authz, authorize));
        Gated {
            router,
            audit,
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

    /// The gate is layered outside all three mounts, so a call it refuses
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
            ("/api/v1/apps/demo/identity-migration", None),
            // A credential that reaches one environment of one
            // application, asked for an administrator's route: 403.
            (
                "/api/v1/apps/demo/identity-migration",
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
    /// body, everything else - a migration, a workflow in another
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
        // An identity migration, on an application of its own: the provider
        // fixture's versions carry placeholder manifests the migration
        // inventory would refuse to read.
        store.create_app("shop").await.unwrap();
        store
            .create_environment("shop", "production")
            .await
            .unwrap();
        let migration = store
            .plan_identity_migration("shop")
            .await
            .unwrap()
            .operation_id
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

        // (b) The migration, the workflow in the other environment and an
        // id that does not exist are all refused with `insufficient-scope`,
        // and - under one request id - with exactly the same bytes: the
        // body names neither the record nor whether there is one.
        let mut refusals = Vec::new();
        for id in [&migration, &elsewhere.to_string(), &unknown] {
            let (status, answer) = get(&gated, &format!("/api/v1/operations/{id}"), &deploy).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{id}: {}", answer.text());
            let body = answer.json();
            assert_eq!(body["error"]["code"], codes::INSUFFICIENT_SCOPE, "{id}");
            assert_eq!(body["error"]["requestId"], REQUEST, "{id}");
            let text = answer.text();
            for secret in [
                "demo",
                "shop",
                "production",
                "preview",
                "migrate",
                id.as_str(),
            ] {
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
            "a migration and a workflow in another environment must be refused identically"
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
            (&migration, "migrate"),
        ] {
            let (status, answer) = get(&gated, &format!("/api/v1/operations/{id}"), ROOT_KEY).await;
            assert_eq!(status, StatusCode::OK, "{id}: {}", answer.text());
            assert_eq!(answer.json()["kind"], kind, "{id}");
            assert!(answer.headers.contains_key(header::ETAG), "{id}");
        }
        let (status, answer) =
            get(&gated, &format!("/api/v1/operations/{unknown}"), ROOT_KEY).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{}", answer.text());
        assert_eq!(answer.json()["error"]["code"], codes::NOT_FOUND);

        // (e) The legacy spelling refuses with the legacy body, byte for
        // byte what `insufficient_scope` has always rendered there: two
        // strings and nothing structured.
        let (status, answer) = get(&gated, &format!("/api/operations/{migration}"), &deploy).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{}", answer.text());
        let expected = ApiError::insufficient_scope(
            OperationAccess::Read.refusal_scope(),
            OperationAccess::Read.refusal_detail(),
        )
        .into_response();
        let expected = to_bytes(expected.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(answer.bytes, expected.to_vec());
        let text = answer.text();
        assert!(
            text.starts_with(r#"{"error":"This credential may not do that: "#),
            "{text}"
        );
        assert!(
            !text.contains("requestId") && !text.contains("\"code\""),
            "{text}"
        );
        let (status, answer) = get(&gated, &format!("/api/operations/{own}"), &deploy).await;
        assert_eq!(status, StatusCode::OK, "{}", answer.text());

        // The gate records the handler's refusals from the status the answer
        // had - four of them, under the route's action, naming the id the
        // caller asked about and, in `detail`, the denial the handler
        // settled, exactly as its own refusals do - and none of the
        // successful reads. The denial is the record's, which the body kept
        // quiet about: the token's scope not covering `read:demo/preview`
        // for the workflow in the other environment, and the deployer role
        // not reaching `admin` for the migration and the unknown id.
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
            migration.clone(),
            elsewhere.to_string(),
            unknown.clone(),
            migration.clone(),
        ];
        expected_ids.sort();
        assert_eq!(refused_ids, expected_ids);
        assert!(
            lines.iter().all(|line| line.status == 403),
            "a successful read is not an event: {lines:#?}"
        );
    }
}
