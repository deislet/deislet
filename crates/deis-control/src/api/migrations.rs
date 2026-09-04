//! Administrative API for durable canonical-identity migration inventory.
//!
//! Starting is asynchronous: once the plan is durable, closing the HTTP
//! connection cannot cancel it. Plans left `planned` or `running` by a stopped
//! Control process are resumed when the next process builds its router.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use deis_wire::CanonicalId;
use serde::Serialize;
use tracing::{error, info, warn};

use crate::api::error::{ApiError, ApiResult};
use crate::catalog::{
    BackendMigrationAttempt, CatalogStore, IdentityMigrationCoordinator, IdentityMigrationRun,
    IdentityMigrationState, summarize_identity_migration,
};
use crate::config::ControlConfig;

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
                    "Could not enumerate identity migrations during startup; no operation was resumed"
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
) -> ApiResult<(StatusCode, Json<MigrationStatusResponse>)> {
    let coordinator = state.ready()?;
    // Planning is the durable acceptance point. A duplicate request for the
    // same application/revision returns the same operation rather than
    // creating another one.
    let plan = state
        .store
        .plan_identity_migration(&application_name)
        .await?;
    if !matches!(
        plan.state,
        IdentityMigrationState::Planned
            | IdentityMigrationState::Running
            | IdentityMigrationState::Waiting
    ) {
        return Err(ApiError::Conflict(format!(
            "identity migration {} is {:?}; retrying a terminal or cutover operation requires a new operation",
            plan.migration_id, plan.state
        )));
    }
    let migration_id = plan.migration_id;
    let attempts = state
        .store
        .identity_migration_backend_attempts(migration_id)
        .await?;
    let response: MigrationStatusResponse = summarize_identity_migration(plan, attempts).into();
    state.launch(coordinator, application_name, migration_id);
    Ok((StatusCode::ACCEPTED, Json(response)))
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use serde_json::Value;
    use tempfile::TempDir;
    use tower::ServiceExt;

    use crate::database::Database;

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
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }
}
