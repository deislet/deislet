//! SQL client backing `ctx.db`, the equivalent of Cloudflare D1.
//!
//! Talks to deis-store, which gives each tenant -- one application in one
//! environment -- its own SQLite file: the tenant is the one named by the
//! capability token this node presents (stamped by [`StorageIdentity`]), not
//! a field of the proto messages, so one client speaks for exactly one environment of one
//! application and a preview's writes never reach production's database.
//! Prepared-statement handles are owned by the tenant that created them and
//! the store refuses them across tenants.
//!
//! Interactive transactions ([`DbClient::begin_transaction`] and the calls
//! that take its handle) are the one part of this surface that holds server
//! state between calls: the store pins one of the tenant's connections to the
//! transaction and holds that file's write lock until it commits or rolls
//! back. That is what lets a caller read a row, decide from it, and write --
//! and it is also why the store gives every transaction a deadline and rolls
//! it back itself when it passes. A handle is owned by the tenant that opened
//! it, checked on every use, and spent once the transaction ends.

use super::bindings::CapabilityChannel;
use super::bindings::StorageIdentity;
use anyhow::Result;

// Import generated proto code
pub mod proto {
    tonic::include_proto!("deis.storage");
}

use proto::{database_client::DatabaseClient, *};

/// The error a transaction call reports when deis-store answered `success =
/// false`.
///
/// The store's own words are passed through unchanged. Every other call in
/// this file puts its own name in front of the message; these do not, because
/// what the store says about a transaction is written for whoever is holding
/// the handle and is already a whole answer -- "was rolled back because it
/// exceeded its 5000 ms deadline; nothing it wrote was applied" tells an
/// application both what happened and what became of its writes. `runtime.js`
/// names the call once on its way to the handler; adding a second frame here
/// would only push that sentence further from the start of the line.
fn transaction_refusal(error: Option<String>) -> anyhow::Error {
    anyhow::anyhow!(error.unwrap_or_else(|| {
        "the store refused a transaction call without saying why".to_string()
    }))
}

/// SQL client for one tenant.
///
/// Cloning is cheap and every call clones: the inner tonic client wraps an
/// HTTP/2 `Channel`, which is a reference-counted handle that multiplexes
/// concurrent calls over one connection. One shared client behind a lock
/// would put every call of this application in one queue, so a transaction
/// waiting on the file's write lock inside deis-store would also hold up the
/// unrelated `SELECT` issued next to it -- and, worse, hold up the COMMIT of
/// the transaction already in flight, which is how a handler that finished on
/// time lost its writes to a deadline it had not exceeded.
#[derive(Clone)]
pub struct DbClient {
    client: DatabaseClient<CapabilityChannel>,
    identity: StorageIdentity,
}

impl DbClient {
    /// The same client, speaking for one named binding of this tenant.
    ///
    /// Shares the channel: the tonic client is a reference-counted handle over
    /// one HTTP/2 connection, so a version declaring four namespaces holds
    /// four identities and one connection rather than four. `None` gives the
    /// tenant's unnamed namespace, which is what an application that declares
    /// no binding of this kind has always used.
    pub(crate) fn for_binding(&self, binding: Option<&str>) -> Result<Self> {
        let identity = self.identity.for_binding(binding)?;
        tracing::debug!(
            app_id = identity.tenant(),
            binding = identity.binding().unwrap_or("<unnamed>"),
            "database calls of this client are charged to {}",
            identity.describe()
        );
        Ok(Self {
            identity,
            ..self.clone()
        })
    }

    /// Build a client that dials deis-store on its first call; see
    /// [`super::kv_client::KvClient::connect_lazy`] for why bindings use this
    /// rather than connecting eagerly.
    pub(crate) fn connect_lazy(endpoint: &str, identity: &StorageIdentity) -> Result<Self> {
        let channel =
            super::bindings::unary_channel(endpoint, deis_identity::ServiceKind::Store, identity)?;
        let client = DatabaseClient::new(channel)
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES);
        tracing::debug!(
            endpoint,
            app_id = identity.tenant(),
            "database client prepared; it will dial on the first call"
        );
        Ok(Self {
            client,
            identity: identity.clone(),
        })
    }

    /// The tenant -- application and environment -- every call of this client
    /// is charged to.
    pub fn tenant(&self) -> &str {
        self.identity.tenant()
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
        let request = self.identity.stamp(ExecuteRequest {
            sql: sql.clone(),
            params,
        });

        tracing::debug!("DB Execute: {}", sql);

        let mut client = self.client.clone();
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
        let request = self.identity.stamp(QueryRequest {
            sql: sql.clone(),
            params,
        });

        tracing::debug!("DB Query: {}", sql);

        let mut client = self.client.clone();
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

        let request = self.identity.stamp(BatchRequest {
            statements: execute_requests,
        });

        tracing::debug!(
            "DB Batch: {} statements",
            request.get_ref().statements.len()
        );

        let mut client = self.client.clone();
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
        let request = self.identity.stamp(PrepareRequest { sql: sql.clone() });

        tracing::debug!("DB Prepare: {}", sql);

        let mut client = self.client.clone();
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
        let request = self.identity.stamp(ExecutePreparedRequest {
            statement_id,
            params,
        });

        tracing::debug!("DB ExecutePrepared");

        let mut client = self.client.clone();
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
        let request = self.identity.stamp(QueryPreparedRequest {
            statement_id,
            params,
        });

        tracing::debug!("DB QueryPrepared");

        let mut client = self.client.clone();
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

    /// Opens an interactive transaction and returns the handle every later
    /// call in it is made against.
    ///
    /// Unlike [`Self::batch`], which is decided before the first statement
    /// runs, this one stays open across round trips, so a caller can read a
    /// row, branch on what it read, and write. deis-store pays for that by
    /// pinning one of this tenant's few connections to the transaction and
    /// holding the file's write lock, which is why it puts a deadline on it.
    ///
    /// # Parameters
    /// - timeout_ms: how long the caller wants it open. `None` asks for the
    ///   store's ceiling; more than the ceiling is clamped down to it.
    ///
    /// # Returns
    /// - (transaction_id, expires_in_ms): the handle, and the deadline that
    ///   was actually granted counted from the moment the store answered.
    pub async fn begin_transaction(&self, timeout_ms: Option<i64>) -> Result<(String, i64)> {
        let request = self.identity.stamp(BeginTransactionRequest { timeout_ms });

        tracing::debug!(timeout_ms, "DB BeginTransaction");

        let mut client = self.client.clone();
        let response = client.begin_transaction(request).await?;
        let begun = response.into_inner();

        if !begun.success {
            return Err(transaction_refusal(begun.error));
        }

        Ok((begun.transaction_id, begun.expires_in_ms))
    }

    /// Runs a row-less statement inside an open transaction.
    ///
    /// # Parameters
    /// - transaction_id: handle from [`Self::begin_transaction`]
    /// - sql: SQL statement; the transaction-control words (BEGIN, COMMIT,
    ///   ROLLBACK, SAVEPOINT and friends) are refused by the store, because
    ///   the transaction is ended through [`Self::commit_transaction`] and
    ///   [`Self::rollback_transaction`] and nowhere else
    /// - params: Optional parameters (JSON encoded array)
    ///
    /// # Returns
    /// - (rows_affected, last_insert_id): Number of rows affected and last insert ID
    pub async fn execute_in_transaction(
        &self,
        transaction_id: String,
        sql: String,
        params: Option<String>,
    ) -> Result<(i64, Option<i64>)> {
        let request = self.identity.stamp(ExecuteInTransactionRequest {
            transaction_id,
            sql: sql.clone(),
            params,
        });

        tracing::debug!("DB ExecuteInTransaction: {}", sql);

        let mut client = self.client.clone();
        let response = client.execute_in_transaction(request).await?;
        let exec_response = response.into_inner();

        if !exec_response.success {
            return Err(transaction_refusal(exec_response.error));
        }

        Ok((exec_response.rows_affected, exec_response.last_insert_id))
    }

    /// Queries inside an open transaction, which is the only way to read the
    /// transaction's own uncommitted writes.
    ///
    /// # Parameters
    /// - transaction_id: handle from [`Self::begin_transaction`]
    /// - sql: SQL query statement
    /// - params: Optional parameters (JSON encoded array)
    ///
    /// # Returns
    /// - (rows, columns): Result rows (JSON-encoded) and column names
    pub async fn query_in_transaction(
        &self,
        transaction_id: String,
        sql: String,
        params: Option<String>,
    ) -> Result<(String, Vec<String>)> {
        let request = self.identity.stamp(QueryInTransactionRequest {
            transaction_id,
            sql: sql.clone(),
            params,
        });

        tracing::debug!("DB QueryInTransaction: {}", sql);

        let mut client = self.client.clone();
        let response = client.query_in_transaction(request).await?;
        let query_response = response.into_inner();

        if !query_response.success {
            return Err(transaction_refusal(query_response.error));
        }

        Ok((query_response.rows, query_response.columns))
    }

    /// Applies everything the transaction did and releases its connection.
    ///
    /// The handle is spent either way: a commit that fails means the work was
    /// rolled back, not that the transaction is still there to retry.
    pub async fn commit_transaction(&self, transaction_id: String) -> Result<()> {
        let request = self
            .identity
            .stamp(CommitTransactionRequest { transaction_id });

        tracing::debug!("DB CommitTransaction");

        let mut client = self.client.clone();
        let response = client.commit_transaction(request).await?;
        let committed = response.into_inner();

        if !committed.success {
            return Err(transaction_refusal(committed.error));
        }

        Ok(())
    }

    /// Discards everything the transaction did and releases its connection.
    pub async fn rollback_transaction(&self, transaction_id: String) -> Result<()> {
        let request = self
            .identity
            .stamp(RollbackTransactionRequest { transaction_id });

        tracing::debug!("DB RollbackTransaction");

        let mut client = self.client.clone();
        let response = client.rollback_transaction(request).await?;
        let rolled_back = response.into_inner();

        if !rolled_back.success {
            return Err(transaction_refusal(rolled_back.error));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::test_store::TestStore;
    use std::time::{Duration, Instant};

    /// A client wired the way an application binding is: dialled on first
    /// use, carrying whatever proof the node was handed for this tenant.
    fn client(store: &TestStore, tenant: &str) -> DbClient {
        let token = store.capability_token_for(tenant, "sql");
        let identity = StorageIdentity::new(crate::runtime::tenant::test_tenant(tenant), Some(&token))
            .expect("a legal tenant");
        DbClient::connect_lazy(&store.endpoint, &identity).expect("client is built")
    }

    #[tokio::test]
    async fn the_sql_surface_round_trips_against_a_real_store() {
        // Every statement below crosses the gRPC boundary into deis-store and
        // back. Asserting the client's own code alone would prove nothing:
        // the contract - parameter encoding, the row/column shape, the batch
        // atomicity, prepared-statement handles - is owned by the other side.
        let store = TestStore::start().await;
        let client = client(&store, "db_client_test.production");

        client
            .execute(
                "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)".to_string(),
                None,
            )
            .await
            .expect("create table");

        let (rows_affected, insert_id) = client
            .execute(
                "INSERT INTO users (name) VALUES (?)".to_string(),
                Some("[\"Alice\"]".to_string()),
            )
            .await
            .expect("insert");
        assert_eq!(rows_affected, 1);
        assert!(
            insert_id.is_some(),
            "an autoincrement insert reports its id"
        );

        let (rows, columns) = client
            .query(
                "SELECT * FROM users WHERE name = ?".to_string(),
                Some("[\"Alice\"]".to_string()),
            )
            .await
            .expect("query");
        assert!(columns.contains(&"name".to_string()), "{columns:?}");
        assert!(rows.contains("Alice"), "{rows}");

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
            .expect("batch");
        assert_eq!(batch_results.len(), 2);

        let stmt_id = client
            .prepare("SELECT * FROM users WHERE id = ?".to_string())
            .await
            .expect("prepare");
        let (rows, _) = client
            .query_prepared(stmt_id, Some("[1]".to_string()))
            .await
            .expect("query prepared");
        assert!(rows.contains("Alice"), "{rows}");

        client
            .execute("DROP TABLE users".to_string(), None)
            .await
            .expect("drop table");
    }

    #[tokio::test]
    async fn a_batch_that_fails_partway_leaves_nothing_behind() {
        // This is the one property `database.transaction` would have offered
        // and does not: deis-store runs a batch atomically, so a statement
        // that fails takes the earlier ones in the same batch with it.
        let store = TestStore::start().await;
        let client = client(&store, "db_batch_test.production");

        client
            .execute(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)".to_string(),
                None,
            )
            .await
            .expect("create table");

        let failed = client
            .batch(vec![
                (
                    "INSERT INTO t (v) VALUES (?)".to_string(),
                    Some("[\"first\"]".to_string()),
                ),
                (
                    "INSERT INTO nonexistent_table (v) VALUES (1)".to_string(),
                    None,
                ),
            ])
            .await;
        assert!(failed.is_err(), "a batch with a bad statement must fail");

        let (rows, _) = client
            .query("SELECT v FROM t".to_string(), None)
            .await
            .expect("query");
        assert!(
            !rows.contains("first"),
            "the successful half of a failed batch must have been rolled back: {rows}"
        );
    }

    #[tokio::test]
    async fn an_interactive_transaction_commits_what_it_wrote_and_only_it_can_read_it_first() {
        // The whole reason this exists rather than `batch`: the caller reads
        // inside the transaction, decides from what it read, and writes. So
        // the test reads its own uncommitted row through the handle, and
        // checks that a read on any other connection cannot see it until the
        // commit.
        let store = TestStore::start().await;
        let client = client(&store, "db_tx_commit_test.production");

        client
            .execute(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)".to_string(),
                None,
            )
            .await
            .expect("create table");

        let (id, expires_in_ms) = client
            .begin_transaction(None)
            .await
            .expect("a transaction is available");
        assert!(
            expires_in_ms > 0,
            "the store has to say how long the handle lives, got {expires_in_ms}"
        );

        client
            .execute_in_transaction(
                id.clone(),
                "INSERT INTO t (v) VALUES (?)".to_string(),
                Some("[\"inside\"]".to_string()),
            )
            .await
            .expect("insert inside the transaction");

        let (rows, _) = client
            .query_in_transaction(id.clone(), "SELECT v FROM t".to_string(), None)
            .await
            .expect("read inside the transaction");
        assert!(
            rows.contains("inside"),
            "a transaction reads its own uncommitted writes: {rows}"
        );

        // WAL keeps this read off the transaction's write lock, so it answers
        // now rather than waiting for the commit -- with the old contents.
        let (outside, _) = client
            .query("SELECT v FROM t".to_string(), None)
            .await
            .expect("read outside the transaction");
        assert!(
            !outside.contains("inside"),
            "an uncommitted write must not be visible outside its transaction: {outside}"
        );

        client.commit_transaction(id).await.expect("commit");

        let (after, _) = client
            .query("SELECT v FROM t".to_string(), None)
            .await
            .expect("read after the commit");
        assert!(
            after.contains("inside"),
            "the commit must have applied: {after}"
        );
    }

    #[tokio::test]
    async fn a_rolled_back_transaction_leaves_nothing_and_its_handle_is_spent() {
        let store = TestStore::start().await;
        let client = client(&store, "db_tx_rollback_test.production");

        client
            .execute(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)".to_string(),
                None,
            )
            .await
            .expect("create table");

        let (id, _) = client.begin_transaction(None).await.expect("begin");
        client
            .execute_in_transaction(
                id.clone(),
                "INSERT INTO t (v) VALUES (?)".to_string(),
                Some("[\"doomed\"]".to_string()),
            )
            .await
            .expect("insert");
        client
            .rollback_transaction(id.clone())
            .await
            .expect("rollback");

        let (rows, _) = client
            .query("SELECT v FROM t".to_string(), None)
            .await
            .expect("query");
        assert!(
            !rows.contains("doomed"),
            "a rolled-back write must be gone: {rows}"
        );

        // A spent handle is refused rather than quietly running the statement
        // on some other connection, which would commit on its own.
        let after = client
            .execute_in_transaction(
                id,
                "INSERT INTO t (v) VALUES (?)".to_string(),
                Some("[\"after\"]".to_string()),
            )
            .await;
        let message = after.expect_err("a spent handle is not usable").to_string();
        assert!(
            message.contains("rolled back"),
            "the refusal has to say what became of the handle: {message}"
        );
    }

    #[tokio::test]
    async fn a_transaction_reports_its_failures_in_the_stores_own_words() {
        // These arrive as `success = false` with a message rather than as a
        // gRPC status, so the client has to look at the flag. Passing the
        // message through unchanged is what lets `runtime.js` hand an
        // application a sentence it can act on.
        let store = TestStore::start().await;
        let client = client(&store, "db_tx_errors_test.production");

        let refused = client
            .begin_transaction(Some(0))
            .await
            .expect_err("a transaction that is already over cannot be opened");
        assert!(refused.to_string().contains("positive"), "{refused}");

        let (id, _) = client.begin_transaction(Some(5_000)).await.expect("begin");

        // The transaction ends through Commit/Rollback and nowhere else; SQL
        // that tries to end it from the inside would leave the store's idea
        // of the connection and the connection itself disagreeing.
        let control = client
            .execute_in_transaction(id.clone(), "COMMIT".to_string(), None)
            .await
            .expect_err("transaction-control SQL is refused");
        assert!(
            control.to_string().to_lowercase().contains("commit"),
            "{control}"
        );

        let bad_sql = client
            .execute_in_transaction(
                id.clone(),
                "INSERT INTO nonexistent (v) VALUES (1)".to_string(),
                None,
            )
            .await
            .expect_err("a statement against a table that is not there fails");
        assert!(bad_sql.to_string().contains("nonexistent"), "{bad_sql}");

        client.rollback_transaction(id).await.expect("rollback");
    }

    /// Two calls on one client must overlap, not queue behind each other.
    ///
    /// This client used to hold its tonic client in an `Arc<Mutex<_>>` and
    /// lock it across the whole RPC, which made a transaction waiting for the
    /// file's write lock inside deis-store stall every other database call the
    /// application made -- including the COMMIT of a transaction that had
    /// finished on time, whose work was then thrown away for exceeding a
    /// deadline it had not exceeded. A `Channel` multiplexes concurrent calls
    /// over one connection, so cloning it per call is what the transport is
    /// for.
    #[tokio::test]
    async fn a_call_blocked_in_the_store_does_not_hold_up_the_rest_of_the_client() {
        let store = TestStore::start().await;
        let client = client(&store, "db_concurrency_test.production");

        client
            .execute("CREATE TABLE t (v INTEGER)".to_string(), None)
            .await
            .expect("create table");

        // The first transaction holds the file's write lock until it ends.
        let (first, _) = client.begin_transaction(Some(10_000)).await.expect("begin");
        client
            .execute_in_transaction(first.clone(), "INSERT INTO t VALUES (1)".to_string(), None)
            .await
            .expect("insert");

        // The second blocks server-side in BEGIN IMMEDIATE, for as long as
        // SQLite's busy timeout allows.
        let (in_flight, blocked_started) = tokio::sync::oneshot::channel();
        let blocked = {
            let client = client.clone();
            tokio::spawn(async move {
                // Sent immediately before the call, so the query below is
                // ordered behind it rather than racing it.
                let _ = in_flight.send(());
                client.begin_transaction(Some(10_000)).await
            })
        };
        blocked_started.await.expect("the blocked task started");

        // Under one shared lock this could not be answered until the blocked
        // BeginTransaction gave the lock back, five seconds later.
        let started = Instant::now();
        let (rows, _) = client
            .query("SELECT 1 AS one".to_string(), None)
            .await
            .expect("a plain query is served while a transaction waits");
        let waited = started.elapsed();
        assert!(rows.contains('1'), "{rows}");
        assert!(
            waited < Duration::from_secs(2),
            "an unrelated query waited {waited:?} behind a blocked transaction; the client is \
             serialising its calls again"
        );

        // And the transaction that was already open can still be committed:
        // its COMMIT was not queued behind the blocked call either.
        let started = Instant::now();
        client
            .commit_transaction(first)
            .await
            .expect("the first transaction commits");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a commit that waits behind another call loses the work it was committing"
        );

        // The blocked call gets the write lock once the first transaction
        // releases it; roll it back so the store is not left holding one.
        let (second, _) = blocked
            .await
            .expect("the blocked task")
            .expect("the second transaction begins once the first commits");
        client
            .rollback_transaction(second)
            .await
            .expect("rollback the second");

        let (after, _) = client
            .query("SELECT v FROM t".to_string(), None)
            .await
            .expect("read after the commit");
        assert!(after.contains('1'), "the commit must have applied: {after}");
    }
}
