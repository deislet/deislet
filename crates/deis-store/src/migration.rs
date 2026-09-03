//! Authenticated migration inventory and writer-fence management for KV,
//! SQL and blob storage.

use crate::server::Stores;
use crate::tenant::TenantId;
use deis_backend_migration::FenceRequest;
use deis_backend_migration::proto::backend_migration_server::BackendMigration;
use deis_backend_migration::proto::{
    AcquireWriteFenceRequest, FenceReceipt as FenceReceiptMessage, InspectTenantRequest,
    ReleaseWriteFenceRequest, ReleaseWriteFenceResponse, TenantInventory,
};
use tonic::{Request, Response, Status};

#[derive(Clone)]
pub struct StoreMigrationService {
    stores: Stores,
}

impl StoreMigrationService {
    #[must_use]
    pub fn new(stores: Stores) -> Self {
        Self { stores }
    }
}

#[tonic::async_trait]
impl BackendMigration for StoreMigrationService {
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
        )
        .map_err(Status::from)?;
        let receipt = self
            .stores
            .fences
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
        // Keep the fence pinned across all three components, so a concurrent
        // release cannot resume writers between their individual snapshots.
        let (receipt, _fence) = self
            .stores
            .fences
            .fenced_read(tenant.as_str())
            .await
            .map_err(Status::from)?;
        let captured_at_unix_seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs() as i64);
        let components = vec![
            self.stores
                .kv
                .inventory_with_fence_held(&tenant, receipt.acquired_at_unix_seconds)
                .await
                .map_err(Status::from)?
                .into(),
            self.stores
                .database
                .inventory_with_fence_held(&tenant)
                .await
                .map_err(Status::from)?
                .into(),
            self.stores
                .blob
                .inventory_with_fence_held(&tenant)
                .await
                .map_err(Status::from)?
                .into(),
        ];
        Ok(Response::new(TenantInventory {
            backend: "deis-store".to_string(),
            tenant: tenant.as_str().to_string(),
            fence: Some(receipt.into()),
            components,
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
        )
        .map_err(Status::from)?;
        let epoch = self
            .stores
            .fences
            .release(request)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(ReleaseWriteFenceResponse {
            released: true,
            fence_epoch: epoch,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::error::StoreError;
    use crate::server::Stores;
    use deis_backend_migration::proto::backend_migration_server::BackendMigration;
    use tempfile::TempDir;

    const MIGRATION: &str = "01990f44-1000-7000-8000-000000000050";
    const TOKEN: &str = "test-fence-token-00000000000000000001";

    fn acquire() -> AcquireWriteFenceRequest {
        AcquireWriteFenceRequest {
            tenant: "shop.production".to_string(),
            migration_id: MIGRATION.to_string(),
            expected_application_revision: 7,
            fence_token: TOKEN.to_string(),
        }
    }

    fn release() -> ReleaseWriteFenceRequest {
        ReleaseWriteFenceRequest {
            tenant: "shop.production".to_string(),
            migration_id: MIGRATION.to_string(),
            expected_application_revision: 7,
            fence_token: TOKEN.to_string(),
        }
    }

    #[tokio::test]
    async fn one_fence_drains_sql_transactions_and_guards_every_store_component() {
        let directory = TempDir::new().unwrap();
        let stores = Stores::open(&Config {
            data_dir: directory.path().to_path_buf(),
            ..Config::default()
        })
        .await
        .unwrap();
        let tenant = TenantId::parse("shop.production").unwrap();
        stores
            .kv
            .set(&tenant, "session", b"one".to_vec(), None, None)
            .await
            .unwrap();
        stores
            .blob
            .store_object(
                &tenant,
                "asset",
                None,
                None,
                tokio_stream::iter(vec![Ok(b"body".to_vec())]),
            )
            .await
            .unwrap();
        let transaction = stores
            .database
            .open_transaction(&tenant, None)
            .await
            .unwrap();
        stores
            .database
            .exec_in_transaction(
                &tenant,
                &transaction.id,
                "CREATE TABLE records(value TEXT)",
                &[],
            )
            .await
            .unwrap();

        let management = StoreMigrationService::new(stores.clone());
        let acquiring = {
            let management = management.clone();
            tokio::spawn(async move {
                BackendMigration::acquire_write_fence(&management, Request::new(acquire())).await
            })
        };
        tokio::task::yield_now().await;
        assert!(
            !acquiring.is_finished(),
            "the fence must wait for the open SQL writer"
        );
        stores
            .database
            .commit(&tenant, &transaction.id)
            .await
            .unwrap();
        acquiring.await.unwrap().unwrap();

        assert!(matches!(
            stores
                .kv
                .set(&tenant, "session", b"two".to_vec(), None, None)
                .await,
            Err(StoreError::WriteFenced(_))
        ));
        assert!(matches!(
            stores
                .database
                .exec_statement(&tenant, "INSERT INTO records VALUES('two')", &[])
                .await,
            Err(StoreError::WriteFenced(_))
        ));
        let rows = stores
            .database
            .fetch_rows(&tenant, "SELECT count(*) AS n FROM records", &[])
            .await
            .unwrap();
        assert_eq!(rows.rows[0]["n"], serde_json::json!(0));
        assert!(matches!(
            stores
                .database
                .fetch_rows(
                    &tenant,
                    "INSERT INTO records VALUES('query-write') RETURNING value",
                    &[],
                )
                .await,
            Err(StoreError::Refused(_))
        ));
        assert!(matches!(
            stores.blob.remove_object(&tenant, "asset", None).await,
            Err(StoreError::WriteFenced(_))
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
        assert_eq!(inventory.backend, "deis-store");
        assert_eq!(
            inventory
                .components
                .iter()
                .map(|component| component.component.as_str())
                .collect::<Vec<_>>(),
            ["kv", "sql", "blob"]
        );
        assert!(
            inventory
                .components
                .iter()
                .all(|component| component.sha256.len() == 64)
        );
        assert!(
            inventory
                .components
                .iter()
                .all(|component| component.unchecked_orphan_count == 0),
            "an empty blob upload directory is normal, not unchecked evidence"
        );

        // A release queued behind the management service's outer fence pin
        // must not block component inventory on a recursive read-lock attempt.
        let (receipt, fence_pin) = stores.fences.fenced_read(tenant.as_str()).await.unwrap();
        let releasing = {
            let management = management.clone();
            tokio::spawn(async move {
                BackendMigration::release_write_fence(&management, Request::new(release())).await
            })
        };
        tokio::task::yield_now().await;
        assert!(!releasing.is_finished());
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stores
                .kv
                .inventory_with_fence_held(&tenant, receipt.acquired_at_unix_seconds),
        )
        .await
        .expect("queued release must not deadlock KV inventory")
        .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stores.database.inventory_with_fence_held(&tenant),
        )
        .await
        .expect("queued release must not deadlock SQL inventory")
        .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stores.blob.inventory_with_fence_held(&tenant),
        )
        .await
        .expect("queued release must not deadlock blob inventory")
        .unwrap();
        drop(fence_pin);
        releasing.await.unwrap().unwrap();
        stores
            .kv
            .set(&tenant, "session", b"two".to_vec(), None, None)
            .await
            .unwrap();
    }
}
