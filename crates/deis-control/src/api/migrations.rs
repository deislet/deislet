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
use deis_wire::{CanonicalId, ProductOperation};
use serde::Serialize;
use tracing::{error, info, warn};

use crate::api::error::{ApiError, ApiResult};
use crate::catalog::{
    BackendMigrationAttempt, CatalogStore, IdentityMigrationCoordinator, IdentityMigrationRun,
    IdentityMigrationState, OperationRequestContext, summarize_identity_migration,
};
use crate::config::ControlConfig;
use crate::identity::{Principal, PrincipalKind};

const IDEMPOTENCY_KEY: &str = "idempotency-key";
const REQUEST_ID: &str = "x-request-id";
const CORRELATION_ID: &str = "x-correlation-id";
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

pub fn router(state: MigrationApiState) -> Router {
    Router::new()
        .route(
            "/api/apps/:app_id/identity-migration",
            get(get_migration).post(start_migration),
        )
        .route("/api/operations/:operation_id", get(get_operation))
        .route(
            "/api/operations/:operation_id/cancel",
            axum::routing::post(cancel_operation),
        )
        .with_state(state)
}

/// A public status deliberately omits the fence token and every management
/// credential. The operation and migration IDs are stable polling handles.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationStatusResponse {
    pub schema_version: u32,
    pub migration_id: CanonicalId,
    pub operation_id: CanonicalId,
    pub state: IdentityMigrationState,
    pub legacy_application_name: String,
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

pub async fn get_migration(
    State(state): State<MigrationApiState>,
    Path(application_name): Path<String>,
) -> ApiResult<Json<MigrationStatusResponse>> {
    Ok(Json(status(&state, &application_name).await?))
}

pub async fn start_migration(
    State(state): State<MigrationApiState>,
    Path(application_name): Path<String>,
    headers: HeaderMap,
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
        HeaderValue::from_str(&format!("/api/operations/{operation_id}"))
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
    response_headers.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", operation.revision))
            .map_err(|error| ApiError::InternalError(error.to_string()))?,
    );
    response_headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok((StatusCode::ACCEPTED, response_headers, Json(operation)))
}

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

pub async fn cancel_operation(
    State(state): State<MigrationApiState>,
    Path(operation_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProductOperation>)> {
    let coordinator = state.ready()?;
    let operation_id = operation_id.parse().map_err(|error| {
        ApiError::BadRequest(format!("operation ID must be a canonical UUIDv7: {error}"))
    })?;
    let expected_revision = required_if_match(&headers)?;
    let plan = state
        .store
        .request_identity_migration_cancellation(operation_id, expected_revision)
        .await?;
    let operation = state
        .store
        .product_operation(operation_id)
        .await?
        .ok_or_else(|| ApiError::InternalError(format!("operation {operation_id} is missing")))?;
    state.launch_abort(coordinator, plan.migration_id);
    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::LOCATION,
        HeaderValue::from_str(&format!("/api/operations/{operation_id}"))
            .map_err(|error| ApiError::InternalError(error.to_string()))?,
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

fn required_header<'a>(headers: &'a HeaderMap, name: &str) -> ApiResult<&'a str> {
    single_header(headers, name)?
        .ok_or_else(|| ApiError::BadRequest(format!("{name} header is required")))?
        .to_str()
        .map_err(|_| ApiError::BadRequest(format!("{name} header is not valid visible ASCII")))
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

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> ApiResult<Option<&'a HeaderValue>> {
    let all = headers.get_all(name);
    let mut values = all.iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(ApiError::BadRequest(format!(
            "{name} header must appear exactly once"
        )));
    }
    Ok(first)
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
        let api = router(state);

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
            store_migration_admin_token: Some("s".repeat(32)),
            queue_migration_admin_token: Some("q".repeat(32)),
            ..Default::default()
        };
        let api = router(MigrationApiState::new(store, &config));
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
            store_migration_admin_token: Some("s".repeat(32)),
            queue_migration_admin_token: Some("q".repeat(32)),
            ..Default::default()
        };
        let api = router(MigrationApiState::new(Arc::clone(&store), &config));
        let path = format!("/api/operations/{}/cancel", plan.operation_id);

        let missing_precondition = api
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .body(Body::empty())
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
}
