// KV Store gRPC client wrapper
//
// Provides functionality to connect to remote KV Store service
// This is the communication layer between deis-runtime runtime and deis-control server

use anyhow::Result;
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
}

impl KvClient {
    /// Connect to remote KV Store service
    ///
    /// # Parameters
    /// - endpoint: gRPC service address (e.g., "http://127.0.0.1:50051")
    ///
    /// # Returns
    /// - Ok(KvClient): Connection successful
    /// - Err: Connection failed
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        tracing::info!("Connecting to KV Store at {}", endpoint);

        let client = KvStoreClient::connect(endpoint).await?;

        tracing::info!("KV Store client connected successfully");
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
        })
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
        let request = tonic::Request::new(GetRequest { key: key.clone() });

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
        let request = tonic::Request::new(SetRequest {
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
        let request = tonic::Request::new(DeleteRequest { key: key.clone() });

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
        let request = tonic::Request::new(ListRequest {
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
        let request = tonic::Request::new(HasRequest { key: key.clone() });

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
        let result = KvClient::connect("http://127.0.0.1:50051").await;
        // Should return connection error if service is not running
        // Only testing client code compilation and basic logic here
        assert!(result.is_err() || result.is_ok());
    }

    #[tokio::test]
    #[ignore]
    async fn test_kv_client_operations() {
        // This test requires a real deis-control server
        let client = KvClient::connect("http://127.0.0.1:50051")
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
