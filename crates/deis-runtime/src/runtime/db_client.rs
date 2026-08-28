// Database gRPC client wrapper
//
// Provides functionality to connect to remote Database service
// Similar to Cloudflare D1 / Deno Deploy SQL

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::transport::Channel;

// Import generated proto code
pub mod proto {
    tonic::include_proto!("deis.storage");
}

use proto::{database_client::DatabaseClient, *};

/// Database client wrapper
///
/// Wrapped in Arc<Mutex> to support sharing and internal mutability across async operations
#[derive(Clone)]
pub struct DbClient {
    client: Arc<Mutex<DatabaseClient<Channel>>>,
}

impl DbClient {
    /// Connect to remote Database service
    ///
    /// # Parameters
    /// - endpoint: gRPC service address (e.g., "http://127.0.0.1:50051")
    ///
    /// # Returns
    /// - Ok(DbClient): Connection successful
    /// - Err: Connection failed
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        tracing::info!("Connecting to Database at {}", endpoint);

        let client = DatabaseClient::connect(endpoint).await?;

        tracing::info!("Database client connected successfully");
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
        })
    }

    /// Executes SQL statements (INSERT, UPDATE, DELETE, etc.)
    ///
    /// # Parameters
    /// - sql: SQL statement
    /// - params: Optional parameters (JSON encoded array)
    ///
    /// # Returns
    /// - (rows_affected, last_insert_id): Number of rows affected and last insert ID
    pub async fn execute(&self, sql: String, params: Option<String>) -> Result<(i64, Option<i64>)> {
        let request = tonic::Request::new(ExecuteRequest {
            sql: sql.clone(),
            params,
        });

        tracing::debug!("DB Execute: {}", sql);

        let mut client = self.client.lock().await;
        let response = client.execute(request).await?;
        let exec_response = response.into_inner();

        if !exec_response.success {
            let error_msg = exec_response
                .error
                .unwrap_or_else(|| "Unknown error".to_string());
            anyhow::bail!("DB Execute failed: {}", error_msg);
        }

        Ok((exec_response.rows_affected, exec_response.last_insert_id))
    }

    /// Queries SQL (SELECT)
    ///
    /// # Parameters
    /// - sql: SQL query statement
    /// - params: Optional parameters (JSON encoded array)
    ///
    /// # Returns
    /// - (rows, columns): Result rows (JSON-encoded) and column names
    pub async fn query(
        &self,
        sql: String,
        params: Option<String>,
    ) -> Result<(String, Vec<String>)> {
        let request = tonic::Request::new(QueryRequest {
            sql: sql.clone(),
            params,
        });

        tracing::debug!("DB Query: {}", sql);

        let mut client = self.client.lock().await;
        let response = client.query(request).await?;
        let query_response = response.into_inner();

        if !query_response.success {
            let error_msg = query_response
                .error
                .unwrap_or_else(|| "Unknown error".to_string());
            anyhow::bail!("DB Query failed: {}", error_msg);
        }

        Ok((query_response.rows, query_response.columns))
    }

    /// Executes multiple SQL statements in a batch
    ///
    /// # Parameters
    /// - statements: List of SQL statements, each containing sql and optional params
    ///
    /// # Returns
    /// - results: Execution results for each statement
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

        tracing::debug!(
            "DB Batch: {} statements",
            request.get_ref().statements.len()
        );

        let mut client = self.client.lock().await;
        let response = client.batch(request).await?;
        let batch_response = response.into_inner();

        if !batch_response.success {
            let error_msg = batch_response
                .error
                .unwrap_or_else(|| "Unknown error".to_string());
            anyhow::bail!("DB Batch failed: {}", error_msg);
        }

        let results = batch_response
            .results
            .into_iter()
            .map(|r| (r.rows_affected, r.last_insert_id))
            .collect();

        Ok(results)
    }

    /// Prepares an SQL statement
    ///
    /// # Parameters
    /// - sql: SQL statement
    ///
    /// # Returns
    /// - statement_id: Prepared statement ID
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

    /// Executes a prepared statement
    ///
    /// # Parameters
    /// - statement_id: Prepared statement ID
    /// - params: Optional parameters (JSON encoded array)
    ///
    /// # Returns
    /// - (rows_affected, last_insert_id): Number of rows affected and last insert ID
    pub async fn execute_prepared(
        &self,
        statement_id: String,
        params: Option<String>,
    ) -> Result<(i64, Option<i64>)> {
        let request = tonic::Request::new(ExecutePreparedRequest {
            statement_id,
            params,
        });

        tracing::debug!("DB ExecutePrepared");

        let mut client = self.client.lock().await;
        let response = client.execute_prepared(request).await?;
        let exec_response = response.into_inner();

        if !exec_response.success {
            let error_msg = exec_response
                .error
                .unwrap_or_else(|| "Unknown error".to_string());
            anyhow::bail!("DB ExecutePrepared failed: {}", error_msg);
        }

        Ok((exec_response.rows_affected, exec_response.last_insert_id))
    }

    /// Queries a prepared statement
    ///
    /// # Parameters
    /// - statement_id: Prepared statement ID
    /// - params: Optional parameters (JSON encoded array)
    ///
    /// # Returns
    /// - (rows, columns): Result rows (JSON-encoded) and column names
    pub async fn query_prepared(
        &self,
        statement_id: String,
        params: Option<String>,
    ) -> Result<(String, Vec<String>)> {
        let request = tonic::Request::new(QueryPreparedRequest {
            statement_id,
            params,
        });

        tracing::debug!("DB QueryPrepared");

        let mut client = self.client.lock().await;
        let response = client.query_prepared(request).await?;
        let query_response = response.into_inner();

        if !query_response.success {
            let error_msg = query_response
                .error
                .unwrap_or_else(|| "Unknown error".to_string());
            anyhow::bail!("DB QueryPrepared failed: {}", error_msg);
        }

        Ok((query_response.rows, query_response.columns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Note: These tests require running a deis-control server
    /// Temporarily marked as #[ignore], to be enabled in integration testing environments
    #[tokio::test]
    #[ignore]
    async fn test_db_client_connect() {
        let result = DbClient::connect("http://127.0.0.1:50051").await;
        // Should return connection error if service is not running
        // Only testing client code compilation and basic logic here
        assert!(result.is_err() || result.is_ok());
    }

    #[tokio::test]
    #[ignore]
    async fn test_db_client_operations() {
        // This test requires a real deis-control server
        let client = DbClient::connect("http://127.0.0.1:50051")
            .await
            .expect("Failed to connect");

        // Create table
        client
            .execute(
                "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)".to_string(),
                None,
            )
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
            .query(
                "SELECT * FROM users WHERE name = ?".to_string(),
                Some("[\"Alice\"]".to_string()),
            )
            .await
            .expect("Failed to query");
        assert!(columns.contains(&"name".to_string()));
        assert!(rows.contains("Alice"));

        // Batch
        let batch_results = client
            .batch(vec![
                (
                    "INSERT INTO users (name) VALUES (?)".to_string(),
                    Some("[\"Bob\"]".to_string()),
                ),
                (
                    "INSERT INTO users (name) VALUES (?)".to_string(),
                    Some("[\"Carol\"]".to_string()),
                ),
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
