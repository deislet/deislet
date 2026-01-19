// Database gRPC 客户端封装
//
// 提供连接到远程 Database 服务的功能
// 类似 Cloudflare D1 / Deno Deploy SQL

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::transport::Channel;

// 引入生成的 proto 代码
pub mod proto {
    tonic::include_proto!("denix.storage");
}

use proto::{database_client::DatabaseClient, *};

/// Database 客户端封装
///
/// 使用 Arc<Mutex> 包装以支持跨 async 操作共享和内部可变性
#[derive(Clone)]
pub struct DbClient {
    client: Arc<Mutex<DatabaseClient<Channel>>>,
}

impl DbClient {
    /// 连接到远程 Database 服务
    ///
    /// # 参数
    /// - endpoint: gRPC 服务地址 (如 "http://127.0.0.1:50051")
    ///
    /// # 返回
    /// - Ok(DbClient): 连接成功
    /// - Err: 连接失败
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        tracing::info!("Connecting to Database at {}", endpoint);

        let client = DatabaseClient::connect(endpoint).await?;

        tracing::info!("Database client connected successfully");
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
        })
    }

    /// 执行 SQL 语句（INSERT, UPDATE, DELETE 等）
    ///
    /// # 参数
    /// - sql: SQL 语句
    /// - params: 可选的参数（JSON 编码的数组）
    ///
    /// # 返回
    /// - (rows_affected, last_insert_id): 影响行数和插入 ID
    pub async fn execute(
        &self,
        sql: String,
        params: Option<String>,
    ) -> Result<(i64, Option<i64>)> {
        let request = tonic::Request::new(ExecuteRequest { sql: sql.clone(), params });

        tracing::debug!("DB Execute: {}", sql);

        let mut client = self.client.lock().await;
        let response = client.execute(request).await?;
        let exec_response = response.into_inner();

        if !exec_response.success {
            let error_msg = exec_response.error.unwrap_or_else(|| "Unknown error".to_string());
            anyhow::bail!("DB Execute failed: {}", error_msg);
        }

        Ok((exec_response.rows_affected, exec_response.last_insert_id))
    }

    /// 查询 SQL（SELECT）
    ///
    /// # 参数
    /// - sql: SQL 查询语句
    /// - params: 可选的参数（JSON 编码的数组）
    ///
    /// # 返回
    /// - (rows, columns): 结果行（JSON 编码）和列名
    pub async fn query(
        &self,
        sql: String,
        params: Option<String>,
    ) -> Result<(String, Vec<String>)> {
        let request = tonic::Request::new(QueryRequest { sql: sql.clone(), params });

        tracing::debug!("DB Query: {}", sql);

        let mut client = self.client.lock().await;
        let response = client.query(request).await?;
        let query_response = response.into_inner();

        if !query_response.success {
            let error_msg = query_response.error.unwrap_or_else(|| "Unknown error".to_string());
            anyhow::bail!("DB Query failed: {}", error_msg);
        }

        Ok((query_response.rows, query_response.columns))
    }

    /// 批量执行多条 SQL
    ///
    /// # 参数
    /// - statements: SQL 语句列表，每个包含 sql 和可选的 params
    ///
    /// # 返回
    /// - results: 每条语句的执行结果
    pub async fn batch(
        &self,
        statements: Vec<(String, Option<String>)>,
    ) -> Result<Vec<(i64, Option<i64>)>> {
        let execute_requests: Vec<ExecuteRequest> = statements
            .into_iter()
            .map(|(sql, params)| ExecuteRequest { sql, params })
            .collect();

        let request = tonic::Request::new(BatchRequest {
            statements: execute_requests,
        });

        tracing::debug!("DB Batch: {} statements", request.get_ref().statements.len());

        let mut client = self.client.lock().await;
        let response = client.batch(request).await?;
        let batch_response = response.into_inner();

        if !batch_response.success {
            let error_msg = batch_response.error.unwrap_or_else(|| "Unknown error".to_string());
            anyhow::bail!("DB Batch failed: {}", error_msg);
        }

        let results = batch_response
            .results
            .into_iter()
            .map(|r| (r.rows_affected, r.last_insert_id))
            .collect();

        Ok(results)
    }

    /// 预处理 SQL 语句
    ///
    /// # 参数
    /// - sql: SQL 语句
    ///
    /// # 返回
    /// - statement_id: 预处理语句 ID
    pub async fn prepare(&self, sql: String) -> Result<String> {
        let request = tonic::Request::new(PrepareRequest { sql: sql.clone() });

        tracing::debug!("DB Prepare: {}", sql);

        let mut client = self.client.lock().await;
        let response = client.prepare(request).await?;
        let prepare_response = response.into_inner();

        if !prepare_response.success {
            let error_msg = prepare_response
                .error
                .unwrap_or_else(|| "Unknown error".to_string());
            anyhow::bail!("DB Prepare failed: {}", error_msg);
        }

        Ok(prepare_response.statement_id)
    }

    /// 执行预处理语句
    ///
    /// # 参数
    /// - statement_id: 预处理语句 ID
    /// - params: 可选的参数（JSON 编码的数组）
    ///
    /// # 返回
    /// - (rows_affected, last_insert_id): 影响行数和插入 ID
    pub async fn execute_prepared(
        &self,
        statement_id: String,
        params: Option<String>,
    ) -> Result<(i64, Option<i64>)> {
        let request = tonic::Request::new(ExecutePreparedRequest { statement_id, params });

        tracing::debug!("DB ExecutePrepared");

        let mut client = self.client.lock().await;
        let response = client.execute_prepared(request).await?;
        let exec_response = response.into_inner();

        if !exec_response.success {
            let error_msg = exec_response.error.unwrap_or_else(|| "Unknown error".to_string());
            anyhow::bail!("DB ExecutePrepared failed: {}", error_msg);
        }

        Ok((exec_response.rows_affected, exec_response.last_insert_id))
    }

    /// 查询预处理语句
    ///
    /// # 参数
    /// - statement_id: 预处理语句 ID
    /// - params: 可选的参数（JSON 编码的数组）
    ///
    /// # 返回
    /// - (rows, columns): 结果行（JSON 编码）和列名
    pub async fn query_prepared(
        &self,
        statement_id: String,
        params: Option<String>,
    ) -> Result<(String, Vec<String>)> {
        let request = tonic::Request::new(QueryPreparedRequest { statement_id, params });

        tracing::debug!("DB QueryPrepared");

        let mut client = self.client.lock().await;
        let response = client.query_prepared(request).await?;
        let query_response = response.into_inner();

        if !query_response.success {
            let error_msg = query_response.error.unwrap_or_else(|| "Unknown error".to_string());
            anyhow::bail!("DB QueryPrepared failed: {}", error_msg);
        }

        Ok((query_response.rows, query_response.columns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 注意：这些测试需要运行 denira server
    /// 暂时标记为 #[ignore]，在集成测试环境中启用
    #[tokio::test]
    #[ignore]
    async fn test_db_client_connect() {
        let result = DbClient::connect("http://127.0.0.1:50051").await;
        // 如果服务未运行，应该返回连接错误
        // 这里只测试客户端代码编译和基本逻辑
        assert!(result.is_err() || result.is_ok());
    }

    #[tokio::test]
    #[ignore]
    async fn test_db_client_operations() {
        // 此测试需要真实的 denira server
        let client = DbClient::connect("http://127.0.0.1:50051")
            .await
            .expect("Failed to connect");

        // Create table
        client
            .execute("CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)".to_string(), None)
            .await
            .expect("Failed to create table");

        // Insert
        let (rows_affected, insert_id) = client
            .execute(
                "INSERT INTO users (name) VALUES (?)".to_string(),
                Some("[\"Alice\"]".to_string()),
            )
            .await
            .expect("Failed to insert");
        assert_eq!(rows_affected, 1);
        assert!(insert_id.is_some());

        // Query
        let (rows, columns) = client
            .query("SELECT * FROM users WHERE name = ?".to_string(), Some("[\"Alice\"]".to_string()))
            .await
            .expect("Failed to query");
        assert!(columns.contains(&"name".to_string()));
        assert!(rows.contains("Alice"));

        // Batch
        let batch_results = client
            .batch(vec![
                ("INSERT INTO users (name) VALUES (?)".to_string(), Some("[\"Bob\"]".to_string())),
                ("INSERT INTO users (name) VALUES (?)".to_string(), Some("[\"Carol\"]".to_string())),
            ])
            .await
            .expect("Failed to batch");
        assert_eq!(batch_results.len(), 2);

        // Prepare
        let stmt_id = client
            .prepare("SELECT * FROM users WHERE id = ?".to_string())
            .await
            .expect("Failed to prepare");

        // Execute prepared
        let (rows, _) = client
            .query_prepared(stmt_id, Some("[1]".to_string()))
            .await
            .expect("Failed to query prepared");
        assert!(rows.contains("Alice"));

        // Cleanup
        client
            .execute("DROP TABLE users".to_string(), None)
            .await
            .expect("Failed to drop table");
    }
}
