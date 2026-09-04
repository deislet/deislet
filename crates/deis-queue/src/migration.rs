//! Authenticated migration inventory and writer-fence management for queues.

use crate::queue::QueueService;
use crate::tenant::TenantId;
use deis_backend_migration::FenceRequest;
use deis_backend_migration::proto::backend_migration_server::BackendMigration;
use deis_backend_migration::proto::{
    AcquireWriteFenceRequest, BackendInfo, FenceReceipt as FenceReceiptMessage,
    GetBackendInfoRequest, InspectTenantRequest, ReleaseWriteFenceRequest,
    ReleaseWriteFenceResponse, TenantInventory,
};
use tonic::{Request, Response, Status};

#[derive(Clone)]
pub struct QueueMigrationService {
    queue: QueueService,
}

impl QueueMigrationService {
    #[must_use]
    pub fn new(queue: QueueService) -> Self {
        Self { queue }
    }
}

#[tonic::async_trait]
impl BackendMigration for QueueMigrationService {
    async fn get_backend_info(
        &self,
        _request: Request<GetBackendInfoRequest>,
    ) -> Result<Response<BackendInfo>, Status> {
        Ok(Response::new(BackendInfo {
            backend: "deis-queue".to_string(),
            backend_incarnation_id: self.queue.fences().backend_incarnation_id().to_string(),
        }))
    }

    async fn acquire_write_fence(
        &self,
        request: Request<AcquireWriteFenceRequest>,
    ) -> Result<Response<FenceReceiptMessage>, Status> {
        let request = request.into_inner();
        let tenant = TenantId::parse_persisted(&request.tenant).map_err(Status::from)?;
        let request = FenceRequest::parse(
            tenant.as_str(),
            &request.migration_id,
            request.expected_application_revision,
            &request.fence_token,
            &request.expected_backend_incarnation_id,
        )
        .map_err(Status::from)?;
        let receipt = self
            .queue
            .fences()
            .acquire(request)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(receipt.into()))
    }

    async fn inspect_tenant(
        &self,
        request: Request<InspectTenantRequest>,
    ) -> Result<Response<TenantInventory>, Status> {
        let tenant =
            TenantId::parse_persisted(&request.into_inner().tenant).map_err(Status::from)?;
        let (receipt, _fence) = self
            .queue
            .fences()
            .fenced_read(tenant.as_str())
            .await
            .map_err(Status::from)?;
        let component = self
            .queue
            .inventory_with_fence_held(&tenant)
            .await
            .map_err(Status::from)?;
        let captured_at_unix_seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs() as i64);
        Ok(Response::new(TenantInventory {
            backend: "deis-queue".to_string(),
            tenant: tenant.as_str().to_string(),
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
        let tenant = TenantId::parse_persisted(&request.tenant).map_err(Status::from)?;
        let request = FenceRequest::parse(
            tenant.as_str(),
            &request.migration_id,
            request.expected_application_revision,
            &request.fence_token,
            &request.expected_backend_incarnation_id,
        )
        .map_err(Status::from)?;
        let epoch = self
            .queue
            .fences()
            .release(request)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(ReleaseWriteFenceResponse {
            released: true,
            fence_epoch: epoch,
            backend_incarnation_id: self.queue.fences().backend_incarnation_id().to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::QueueError;
    use crate::queue::QueueLimits;
    use deis_backend_migration::proto::backend_migration_server::BackendMigration;
    use tempfile::TempDir;

    const MIGRATION: &str = "01990f44-1000-7000-8000-000000000050";
    const TOKEN: &str = "test-fence-token-00000000000000000001";

    fn acquire(backend_incarnation_id: &str) -> AcquireWriteFenceRequest {
        AcquireWriteFenceRequest {
            tenant: "shop.production".to_string(),
            migration_id: MIGRATION.to_string(),
            expected_application_revision: 7,
            fence_token: TOKEN.to_string(),
            expected_backend_incarnation_id: backend_incarnation_id.to_string(),
        }
    }

    fn release(backend_incarnation_id: &str) -> ReleaseWriteFenceRequest {
        ReleaseWriteFenceRequest {
            tenant: "shop.production".to_string(),
            migration_id: MIGRATION.to_string(),
            expected_application_revision: 7,
            fence_token: TOKEN.to_string(),
            expected_backend_incarnation_id: backend_incarnation_id.to_string(),
        }
    }

    #[tokio::test]
    async fn queue_fence_survives_reopen_and_freezes_every_message_state() {
        let directory = TempDir::new().unwrap();
        let tenant = TenantId::parse("shop.production").unwrap();
        let queue = QueueService::open(directory.path(), 2, QueueLimits::default())
            .await
            .unwrap();
        queue.send(&tenant, "jobs", b"one".to_vec()).await.unwrap();
        let management = QueueMigrationService::new(queue.clone());
        let backend_incarnation_id = queue.fences().backend_incarnation_id().to_string();
        BackendMigration::acquire_write_fence(
            &management,
            Request::new(acquire(&backend_incarnation_id)),
        )
        .await
        .unwrap();
        assert!(matches!(
            queue.send(&tenant, "jobs", b"two".to_vec()).await,
            Err(QueueError::WriteFenced(_))
        ));
        assert!(matches!(
            queue.pull(&tenant, "jobs", None, None).await,
            Err(QueueError::WriteFenced(_))
        ));
        let inventory = BackendMigration::inspect_tenant(
            &management,
            Request::new(InspectTenantRequest {
                tenant: tenant.to_string(),
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(inventory.components.len(), 1);
        assert_eq!(inventory.components[0].component, "queue");
        assert_eq!(inventory.components[0].item_count, 1);
        drop(management);
        drop(queue);

        let reopened = QueueService::open(directory.path(), 2, QueueLimits::default())
            .await
            .unwrap();
        assert!(matches!(
            reopened.send(&tenant, "jobs", b"two".to_vec()).await,
            Err(QueueError::WriteFenced(_))
        ));
        let management = QueueMigrationService::new(reopened.clone());
        let (_receipt, fence_pin) = reopened
            .fences()
            .fenced_read(tenant.as_str())
            .await
            .unwrap();
        let releasing = {
            let management = management.clone();
            let backend_incarnation_id = backend_incarnation_id.clone();
            tokio::spawn(async move {
                BackendMigration::release_write_fence(
                    &management,
                    Request::new(release(&backend_incarnation_id)),
                )
                .await
            })
        };
        tokio::task::yield_now().await;
        assert!(!releasing.is_finished());
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reopened.inventory_with_fence_held(&tenant),
        )
        .await
        .expect("queued release must not deadlock queue inventory")
        .unwrap();
        drop(fence_pin);
        releasing.await.unwrap().unwrap();
        reopened
            .send(&tenant, "jobs", b"two".to_vec())
            .await
            .unwrap();
    }
}
