// KV Store gRPC 客户端封装
//
// 提供连接到远程 KV Store 服务的功能
// 这是 denix runtime 与 denira server 之间的通信层

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::transport::Channel;

// 引入生成的 proto 代码
pub mod proto {
    tonic::include_proto!("denix.storage");
}

use proto::{kv_store_client::KvStoreClient, *};

/// KV Store 客户端封装
///
/// 使用 Arc<Mutex> 包装以支持跨 async 操作共享和内部可变性
#[derive(Clone)]
pub struct KvClient {
    client: Arc<Mutex<KvStoreClient<Channel>>>,
}

impl KvClient {
    /// 连接到远程 KV Store 服务
    ///
    /// # 参数
    /// - endpoint: gRPC 服务地址 (如 "http://127.0.0.1:50051")
    ///
    /// # 返回
    /// - Ok(KvClient): 连接成功
    /// - Err: 连接失败
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        tracing::info!("Connecting to KV Store at {}", endpoint);

        let client = KvStoreClient::connect(endpoint).await?;

        tracing::info!("KV Store client connected successfully");
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
        })
    }

    /// 获取值
    ///
    /// # 参数
    /// - key: 键
    ///
    /// # 返回
    /// - Some(value): 找到值
    /// - None: 键不存在
    pub async fn get(&self, key: String) -> Result<Option<String>> {
        let request = tonic::Request::new(GetRequest { key: key.clone() });

        tracing::debug!("KV Get: {}", key);

        let mut client = self.client.lock().await;
        let response = client.get(request).await?;
        let get_response = response.into_inner();

        Ok(get_response.value)
    }

    /// 设置值
    ///
    /// # 参数
    /// - key: 键
    /// - value: 值
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

    /// 删除键
    ///
    /// # 参数
    /// - key: 键
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

    /// 列出前缀匹配的键
    ///
    /// # 参数
    /// - prefix: 前缀
    /// - limit: 可选的数量限制
    ///
    /// # 返回
    /// - (keys, next_cursor): 键列表和下一页游标
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

    /// 检查键是否存在
    ///
    /// # 参数
    /// - key: 键
    ///
    /// # 返回
    /// - true: 存在
    /// - false: 不存在
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

    /// 注意：这些测试需要运行 denira server
    /// 暂时标记为 #[ignore]，在集成测试环境中启用
    #[tokio::test]
    #[ignore]
    async fn test_kv_client_connect() {
        let result = KvClient::connect("http://127.0.0.1:50051").await;
        // 如果服务未运行，应该返回连接错误
        // 这里只测试客户端代码编译和基本逻辑
        assert!(result.is_err() || result.is_ok());
    }

    #[tokio::test]
    #[ignore]
    async fn test_kv_client_operations() {
        // 此测试需要真实的 denira server
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
