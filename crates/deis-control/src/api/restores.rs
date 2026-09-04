//! Administrative API for coordinated embedded-backend restore planning.
//!
//! The POST freezes a validated restore set and returns the shared durable
//! ProductOperation. It does not copy or switch directories in the request:
//! that work belongs to a separately authenticated topology driver and must
//! remain recoverable after this HTTP connection closes.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    routing::{get, post},
};
use deis_wire::{BackendRestoreSet, CanonicalId, ProductOperation};
use serde::Serialize;
use tracing::{error, info, warn};

use crate::api::error::{ApiError, ApiResult};
use crate::catalog::{
    BackendRestoreCoordinator, BackendRestorePlan, CatalogStore, OperationRequestContext,
    ProductInstallation,
};
use crate::config::ControlConfig;
use crate::identity::{Principal, PrincipalKind};

const IDEMPOTENCY_KEY: &str = "idempotency-key";
const REQUEST_ID: &str = "x-request-id";
const CORRELATION_ID: &str = "x-correlation-id";
const RECONCILE_INTERVAL_SECONDS: u64 = 10;

#[derive(Clone)]
pub struct RestoreApiState {
    store: Arc<CatalogStore>,
    coordinator: RestoreCoordinatorAvailability,
    running: Arc<Mutex<BTreeSet<CanonicalId>>>,
}

#[derive(Clone)]
enum RestoreCoordinatorAvailability {
    Ready(Arc<BackendRestoreCoordinator>),
    Unavailable(Arc<str>),
}

impl RestoreApiState {
    pub fn new(store: Arc<CatalogStore>, config: &ControlConfig) -> Self {
        let coordinator = match BackendRestoreCoordinator::from_config(Arc::clone(&store), config) {
            Ok(coordinator) => {
                info!("Coordinated embedded-backend restore driver is configured");
                RestoreCoordinatorAvailability::Ready(Arc::new(coordinator))
            }
            Err(error) => {
                warn!(
                    reason = %error,
                    "Backend restore planning is unavailable; existing restore operations remain readable"
                );
                RestoreCoordinatorAvailability::Unavailable(Arc::from(error.to_string()))
            }
        };
        Self {
            store,
            coordinator,
            running: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    fn ready(&self) -> ApiResult<Arc<BackendRestoreCoordinator>> {
        match &self.coordinator {
            RestoreCoordinatorAvailability::Ready(coordinator) => Ok(Arc::clone(coordinator)),
            RestoreCoordinatorAvailability::Unavailable(reason) => {
                Err(ApiError::NotConfigured(reason.to_string()))
            }
        }
    }

    fn launch(
        &self,
        coordinator: Arc<BackendRestoreCoordinator>,
        operation_id: CanonicalId,
    ) -> bool {
        let inserted = self
            .running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(operation_id);
        if !inserted {
            return false;
        }
        let running = Arc::clone(&self.running);
        tokio::spawn(async move {
            match coordinator.coordinate(operation_id).await {
                Ok(report) => info!(
                    operation_id = %operation_id,
                    restore_set_id = %report.restore_set_id,
                    "Backend restore completed and live services attested every new incarnation"
                ),
                Err(error) => error!(
                    operation_id = %operation_id,
                    error = %error,
                    "Backend restore did not succeed; inspect its durable operation to distinguish a reconciliable unknown outcome from a verified rollback"
                ),
            }
            running
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&operation_id);
        });
        true
    }

    pub fn resume_on_start(&self) {
        let Ok(coordinator) = self.ready() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            error!("Backend restores could not be resumed because no Tokio runtime is active");
            return;
        };
        let state = self.clone();
        runtime.spawn(async move {
            loop {
                match state.store.resumable_backend_restores().await {
                    Ok(plans) => {
                        for plan in plans {
                            state.launch(Arc::clone(&coordinator), plan.operation_id);
                        }
                    }
                    Err(error) => {
                        error!(error = %error, "Could not enumerate resumable backend restores")
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(RECONCILE_INTERVAL_SECONDS))
                    .await;
            }
        });
    }
}

pub fn router(state: RestoreApiState) -> Router {
    Router::new()
        .route("/api/installation", get(get_installation))
        .route("/api/backend-restores", post(start_restore))
        .route("/api/backend-restores/:restore_id", get(get_restore))
        .with_state(state)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendRestoreStatus {
    pub plan: BackendRestorePlan,
    pub operation: ProductOperation,
}

pub async fn get_installation(
    State(state): State<RestoreApiState>,
) -> ApiResult<Json<ProductInstallation>> {
    Ok(Json(state.store.product_installation().await?))
}

pub async fn start_restore(
    State(state): State<RestoreApiState>,
    headers: HeaderMap,
    Extension(principal): Extension<Principal>,
    Json(restore_set): Json<BackendRestoreSet>,
) -> ApiResult<(StatusCode, HeaderMap, Json<ProductOperation>)> {
    let coordinator = state.ready()?;
    let context = operation_context(&headers, &principal)?;
    let plan = state
        .store
        .plan_backend_restore(restore_set, context)
        .await?;
    let operation = state
        .store
        .product_operation(plan.operation_id)
        .await?
        .ok_or_else(|| {
            ApiError::InternalError(format!("operation {} is missing", plan.operation_id))
        })?;
    let response_headers = operation_headers(&operation)?;
    if !operation.state.terminal() {
        state.launch(coordinator, plan.operation_id);
    }
    Ok((StatusCode::ACCEPTED, response_headers, Json(operation)))
}

pub async fn get_restore(
    State(state): State<RestoreApiState>,
    Path(restore_id): Path<String>,
) -> ApiResult<(HeaderMap, Json<BackendRestoreStatus>)> {
    let restore_id = parse_id(&restore_id, "restore")?;
    let plan = state
        .store
        .backend_restore(restore_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("backend restore {restore_id} not found")))?;
    let operation = state
        .store
        .product_operation(plan.operation_id)
        .await?
        .ok_or_else(|| {
            ApiError::InternalError(format!("operation {} is missing", plan.operation_id))
        })?;
    let headers = operation_headers(&operation)?;
    Ok((headers, Json(BackendRestoreStatus { plan, operation })))
}

fn operation_context(
    headers: &HeaderMap,
    principal: &Principal,
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
    OperationRequestContext::new(
        idempotency_key,
        "POST\n/api/backend-restores\nv1",
        actor_subject,
        request_id,
        correlation_id,
    )
    .map_err(Into::into)
}

fn operation_headers(operation: &ProductOperation) -> ApiResult<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(&format!("/api/operations/{}", operation.operation_id))
            .map_err(|error| ApiError::InternalError(error.to_string()))?,
    );
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
    Ok(headers)
}

fn parse_id(value: &str, label: &str) -> ApiResult<CanonicalId> {
    value.parse().map_err(|error| {
        ApiError::BadRequest(format!("{label} ID must be a canonical UUIDv7: {error}"))
    })
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
            parse_id(value, name)
        })
        .transpose()
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> ApiResult<Option<&'a HeaderValue>> {
    let mut values = headers.get_all(name).iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(ApiError::BadRequest(format!(
            "{name} header must appear exactly once"
        )));
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use serde_json::Value;
    use tempfile::TempDir;
    use tower::ServiceExt;

    use crate::database::Database;
    use crate::identity::Principal;

    use super::*;

    #[tokio::test]
    async fn restore_plan_is_discoverable_durable_and_body_bound() {
        let directory = TempDir::new().unwrap();
        let database = Arc::new(
            Database::init(directory.path().join("control.sqlite"))
                .await
                .unwrap(),
        );
        let store = Arc::new(CatalogStore::new(database));
        let driver_config = directory.path().join("topology.toml");
        std::fs::write(&driver_config, "# test topology driver configuration\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&driver_config, std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let config = ControlConfig {
            backend_restore_driver_executable: Some("/bin/true".into()),
            backend_restore_driver_config: Some(driver_config),
            store_endpoint: Some("http://127.0.0.1:41001/".to_string()),
            queue_endpoint: Some("http://127.0.0.1:41002/".to_string()),
            telemetry_endpoint: Some("http://127.0.0.1:41003/".to_string()),
            store_migration_admin_token: Some("s".repeat(32)),
            queue_migration_admin_token: Some("q".repeat(32)),
            telemetry_migration_admin_token: Some("t".repeat(32)),
            ..ControlConfig::default()
        };
        let restore_state = RestoreApiState::new(Arc::clone(&store), &config);
        if let RestoreCoordinatorAvailability::Unavailable(reason) = &restore_state.coordinator {
            panic!("test restore coordinator is unavailable: {reason}");
        }
        let api = router(restore_state);

        let installation_response = api
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/installation")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(installation_response.status(), StatusCode::OK);
        let installation: ProductInstallation = serde_json::from_slice(
            &to_bytes(installation_response.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();

        let mut value: Value = serde_json::from_str(include_str!(
            "../../../../contracts/v1/fixtures/valid/backend-restore-set.json"
        ))
        .unwrap();
        value["$schema"] = deis_wire::BACKEND_RESTORE_SET_SCHEMA_V1.into();
        value["scope"]["organizationId"] = installation.organization_id.to_string().into();
        value["scope"]["installationId"] = installation.installation_id.to_string().into();
        let body = serde_json::to_vec(&value).unwrap();
        let make_request = |body: Vec<u8>| {
            Request::builder()
                .method("POST")
                .uri("/api/backend-restores")
                .header("content-type", "application/json")
                .header(IDEMPOTENCY_KEY, "restore-api-key-2026-09-04")
                .body(Body::from(body))
                .map(|mut request| {
                    request.extensions_mut().insert(Principal::anonymous());
                    request
                })
                .unwrap()
        };

        let response = api
            .clone()
            .oneshot(make_request(body.clone()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let location = response.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .to_string();
        let operation: ProductOperation =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(operation.kind, deis_wire::OperationKind::Restore);
        assert_eq!(operation.target.kind, "installation");
        assert_eq!(operation.target.id, installation.installation_id);
        assert_eq!(operation.state, deis_wire::OperationState::Queued);
        assert_eq!(
            location,
            format!("/api/operations/{}", operation.operation_id)
        );

        let duplicate = api.clone().oneshot(make_request(body)).await.unwrap();
        let duplicate: ProductOperation =
            serde_json::from_slice(&to_bytes(duplicate.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(duplicate.operation_id, operation.operation_id);

        let plan = store
            .backend_restore_by_operation(operation.operation_id)
            .await
            .unwrap()
            .unwrap();
        let status_response = api
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/backend-restores/{}", plan.restore_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(status_response.status(), StatusCode::OK);
        let status: Value = serde_json::from_slice(
            &to_bytes(status_response.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            status["operation"]["operationId"],
            operation.operation_id.to_string()
        );
        assert_eq!(
            status["plan"]["restoreSet"]["components"]
                .as_array()
                .unwrap()
                .len(),
            3
        );

        value["components"][0]["snapshotUri"] =
            "file:///var/lib/deislet/backups/another/store".into();
        let conflict = api
            .oneshot(make_request(serde_json::to_vec(&value).unwrap()))
            .await
            .unwrap();
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
    }
}
