//! Authenticated migration inventory and writer-fence management for the
//! embedded telemetry backend.

use deis_backend_migration::FenceRequest;
use deis_backend_migration::TenantWriteFences;
use deis_backend_migration::proto::backend_migration_server::BackendMigration;
use deis_backend_migration::proto::{
    AcquireWriteFenceRequest, BackendInfo, FenceReceipt as FenceReceiptMessage,
    GetBackendInfoRequest, InspectTenantRequest, ReleaseWriteFenceRequest,
    ReleaseWriteFenceResponse, TenantInventory,
};
use deis_wire::conventions::split_tenant;
use tonic::{Request, Response, Status};

use crate::store::TelemetryStore;

#[derive(Clone)]
pub struct TelemetryMigrationService {
    store: TelemetryStore,
    fences: TenantWriteFences,
}

impl TelemetryMigrationService {
    #[must_use]
    pub fn new(store: TelemetryStore, fences: TenantWriteFences) -> Self {
        Self { store, fences }
    }

    #[allow(clippy::result_large_err)] // tonic's service contract returns Status by value.
    fn validate_tenant(tenant: &str) -> Result<(), Status> {
        split_tenant(tenant).map(|_| ()).ok_or_else(|| {
            Status::invalid_argument(
                "tenant must be exactly <application>.<environment> using valid tenant parts",
            )
        })
    }
}

#[tonic::async_trait]
impl BackendMigration for TelemetryMigrationService {
    async fn get_backend_info(
        &self,
        _request: Request<GetBackendInfoRequest>,
    ) -> Result<Response<BackendInfo>, Status> {
        Ok(Response::new(BackendInfo {
            backend: "deis-telemetry".to_string(),
            backend_incarnation_id: self.fences.backend_incarnation_id().to_string(),
        }))
    }

    async fn acquire_write_fence(
        &self,
        request: Request<AcquireWriteFenceRequest>,
    ) -> Result<Response<FenceReceiptMessage>, Status> {
        let request = request.into_inner();
        Self::validate_tenant(&request.tenant)?;
        let request = FenceRequest::parse(
            request.tenant,
            &request.migration_id,
            request.expected_application_revision,
            &request.fence_token,
            &request.expected_backend_incarnation_id,
        )
        .map_err(Status::from)?;
        let receipt = self.fences.acquire(request).await.map_err(Status::from)?;
        Ok(Response::new(receipt.into()))
    }

    async fn inspect_tenant(
        &self,
        request: Request<InspectTenantRequest>,
    ) -> Result<Response<TenantInventory>, Status> {
        let tenant = request.into_inner().tenant;
        Self::validate_tenant(&tenant)?;
        let (receipt, _fence) = self
            .fences
            .fenced_read(&tenant)
            .await
            .map_err(Status::from)?;
        let store = self.store.clone();
        let inventory_tenant = tenant.clone();
        let component =
            tokio::task::spawn_blocking(move || store.tenant_inventory(&inventory_tenant))
                .await
                .map_err(|error| {
                    Status::internal(format!("telemetry inventory task failed: {error}"))
                })?
                .map_err(|error| {
                    Status::internal(format!("cannot inventory telemetry: {error:#}"))
                })?;
        let captured_at_unix_seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs() as i64);
        Ok(Response::new(TenantInventory {
            backend: "deis-telemetry".to_string(),
            tenant,
            fence: Some(receipt.into()),
            components: vec![component.into()],
            captured_at_unix_seconds,
        }))
    }

    async fn release_write_fence(
        &self,
        request: Request<ReleaseWriteFenceRequest>,
    ) -> Result<Response<ReleaseWriteFenceResponse>, Status> {
        let request = request.into_inner();
        Self::validate_tenant(&request.tenant)?;
        let request = FenceRequest::parse(
            request.tenant,
            &request.migration_id,
            request.expected_application_revision,
            &request.fence_token,
            &request.expected_backend_incarnation_id,
        )
        .map_err(Status::from)?;
        let epoch = self.fences.release(request).await.map_err(Status::from)?;
        Ok(Response::new(ReleaseWriteFenceResponse {
            released: true,
            fence_epoch: epoch,
            backend_incarnation_id: self.fences.backend_incarnation_id().to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::store::{LogRecord, MetricRecord, TraceRecord};

    const MIGRATION: &str = "01990f44-1000-7000-8000-000000000050";
    const TOKEN: &str = "test-fence-token-00000000000000000001";

    fn acquire(incarnation: &str) -> AcquireWriteFenceRequest {
        AcquireWriteFenceRequest {
            tenant: "shop.production".to_string(),
            migration_id: MIGRATION.to_string(),
            expected_application_revision: 7,
            fence_token: TOKEN.to_string(),
            expected_backend_incarnation_id: incarnation.to_string(),
        }
    }

    #[tokio::test]
    async fn inventory_is_environment_scoped_and_fence_survives_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let store = TelemetryStore::open(directory.path()).unwrap();
        let fences = TenantWriteFences::open(directory.path().join("migration-fences.sqlite"))
            .await
            .unwrap();
        let mut production = BTreeMap::new();
        production.insert("environment".to_string(), "production".to_string());
        store
            .insert_logs(&[LogRecord {
                timestamp_us: 1,
                level: 3,
                component: "runtime".to_string(),
                app_id: "shop".to_string(),
                message: "sold".to_string(),
                fields: production.clone(),
                trace_id: "trace-1".to_string(),
                span_id: String::new(),
            }])
            .unwrap();
        store
            .insert_metrics(&[MetricRecord {
                name: "requests".to_string(),
                app_id: "shop".to_string(),
                labels: BTreeMap::new(),
                value: 1.0,
                timestamp_us: 2,
            }])
            .unwrap();
        store
            .insert_traces(&[TraceRecord {
                trace_id: "trace-preview".to_string(),
                started_us: 3,
                duration_us: 1,
                app_id: "shop".to_string(),
                environment: "preview".to_string(),
                node: "node-1".to_string(),
                kind: 1,
                outcome: 1,
                target: "/".to_string(),
                method: "GET".to_string(),
                status: 200,
                error: String::new(),
                attributes: BTreeMap::new(),
            }])
            .unwrap();

        let service = TelemetryMigrationService::new(store.clone(), fences.clone());
        let incarnation = fences.backend_incarnation_id().to_string();
        BackendMigration::acquire_write_fence(&service, Request::new(acquire(&incarnation)))
            .await
            .unwrap();
        let inventory = BackendMigration::inspect_tenant(
            &service,
            Request::new(InspectTenantRequest {
                tenant: "shop.production".to_string(),
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(inventory.components.len(), 1);
        assert_eq!(inventory.components[0].component, "telemetry");
        assert_eq!(inventory.components[0].item_count, 2);
        assert_eq!(inventory.components[0].unchecked_orphan_count, 0);
        assert!(!inventory.components[0].sha256.is_empty());
        assert!(fences.permit("shop.production").await.is_err());
        assert!(fences.permit("shop.preview").await.is_ok());

        drop(service);
        drop(fences);
        let reopened = TenantWriteFences::open(directory.path().join("migration-fences.sqlite"))
            .await
            .unwrap();
        assert_eq!(reopened.backend_incarnation_id().to_string(), incarnation);
        assert!(reopened.permit("shop.production").await.is_err());
    }
}
