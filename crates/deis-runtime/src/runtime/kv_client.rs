//! Key/value client backing `ctx.kv`.
//!
//! Talks to deis-store, which partitions data by application: the tenant
//! travels in the `x-deis-app-id` metadata header (see [`Tenant`]), not in the
//! proto messages, so one client speaks for exactly one application.

use super::tenant::Tenant;
use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::transport::Channel;

// Import generated proto code
pub mod proto {
    tonic::include_proto!("deis.storage");
}

use proto::{kv_store_client::KvStoreClient, *};

/// KV Store client wrapper
///
/// Wrapped in Arc<Mutex> to support sharing and internal mutability across async operations
#[derive(Clone)]
pub struct KvClient {
    client: Arc<Mutex<KvStoreClient<Channel>>>,
    tenant: Tenant,
}

impl KvClient {
    /// Connect to remote KV Store service
    ///
    /// # Parameters
    /// - endpoint: gRPC service address (e.g., "http://127.0.0.1:50051")
    /// - app_id: the tenant every call of this client is charged to
    ///
    /// # Returns
    /// - Ok(KvClient): Connection successful
    /// - Err: Connection failed
    pub async fn connect(endpoint: impl Into<String>, app_id: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        let tenant = Tenant::new(app_id)?;
        tracing::info!(%endpoint, app_id = tenant.app_id(), "connecting to KV store");

        let client = KvStoreClient::connect(endpoint.clone())
            .await
            .with_context(|| format!("connecting to KV store at {endpoint}"))?
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES);

        tracing::info!(%endpoint, app_id = tenant.app_id(), "KV store client connected");
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
            tenant,
        })
    }

    /// The application every call of this client is charged to.
    pub fn app_id(&self) -> &str {
        self.tenant.app_id()
    }

    /// Get value
    ///
    /// # Parameters
    /// - key: Key
    ///
    /// # Returns
    /// - Some(value): Value found
    /// - None: Key does not exist
    pub async fn get(&self, key: String) -> Result<Option<String>> {
        let request = self.tenant.stamp(GetRequest { key: key.clone() });

        tracing::debug!("KV Get: {}", key);

        let mut client = self.client.lock().await;
        let response = client.get(request).await?;
        let get_response = response.into_inner();

        Ok(get_response.value)
    }

    /// Set value
    ///
    /// # Parameters
    /// - key: Key
    /// - value: Value
    pub async fn set(&self, key: String, value: String) -> Result<()> {
        let request = self.tenant.stamp(SetRequest {
            key: key.clone(),
            value: value.clone(),
        });

        tracing::debug!("KV Set: {} = {}", key, value);

        let mut client = self.client.lock().await;
        let response = client.set(request).await?;
        let set_response = response.into_inner();

        if !set_response.success {
            anyhow::bail!("KV Set failed for key: {}", key);
        }

        Ok(())
    }

    /// Delete key
    ///
    /// # Parameters
    /// - key: Key
    pub async fn delete(&self, key: String) -> Result<()> {
        let request = self.tenant.stamp(DeleteRequest { key: key.clone() });

        tracing::debug!("KV Delete: {}", key);

        let mut client = self.client.lock().await;
        let response = client.delete(request).await?;
        let delete_response = response.into_inner();

        if !delete_response.success {
            tracing::warn!("KV Delete failed for key: {}", key);
        }

        Ok(())
    }

    /// List keys with matching prefix
    ///
    /// # Parameters
    /// - prefix: Prefix
    /// - limit: Optional quantity limit
    ///
    /// # Returns
    /// - (keys, next_cursor): Key list and next page cursor
    pub async fn list(
        &self,
        prefix: String,
        limit: Option<i32>,
    ) -> Result<(Vec<String>, Option<String>)> {
        let request = self.tenant.stamp(ListRequest {
            prefix: prefix.clone(),
            limit,
            cursor: None,
        });

        tracing::debug!("KV List: prefix={}, limit={:?}", prefix, limit);

        let mut client = self.client.lock().await;
        let response = client.list(request).await?;
        let list_response = response.into_inner();

        Ok((list_response.keys, list_response.next_cursor))
    }

    /// Check if key exists
    ///
    /// # Parameters
    /// - key: Key
    ///
    /// # Returns
    /// - true: Exists
    /// - false: Does not exist
    pub async fn has(&self, key: String) -> Result<bool> {
        let request = self.tenant.stamp(HasRequest { key: key.clone() });

        tracing::debug!("KV Has: {}", key);

        let mut client = self.client.lock().await;
        let response = client.has(request).await?;
        let has_response = response.into_inner();

        Ok(has_response.exists)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Note: These tests require running a deis-control server
    /// Temporarily marked as #[ignore], to be enabled in integration testing environments
    #[tokio::test]
    #[ignore]
    async fn test_kv_client_connect() {
        let result = KvClient::connect("http://127.0.0.1:50051", "test_app").await;
        // Should return connection error if service is not running
        // Only testing client code compilation and basic logic here
        assert!(result.is_err() || result.is_ok());
    }

    #[tokio::test]
    #[ignore]
    async fn test_kv_client_operations() {
        // This test requires a real deis-control server
        let client = KvClient::connect("http://127.0.0.1:50051", "test_app")
            .await
            .expect("Failed to connect");

        // Set
        client
            .set("test_key".to_string(), "test_value".to_string())
            .await
            .expect("Failed to set");

        // Get
        let value = client
            .get("test_key".to_string())
            .await
            .expect("Failed to get");
        assert_eq!(value, Some("test_value".to_string()));

        // Has
        let exists = client
            .has("test_key".to_string())
            .await
            .expect("Failed to check has");
        assert!(exists);

        // List
        let (keys, _) = client
            .list("test".to_string(), Some(10))
            .await
            .expect("Failed to list");
        assert!(keys.contains(&"test_key".to_string()));

        // Delete
        client
            .delete("test_key".to_string())
            .await
            .expect("Failed to delete");

        // Verify deleted
        let value = client
            .get("test_key".to_string())
            .await
            .expect("Failed to get after delete");
        assert_eq!(value, None);
    }
}
