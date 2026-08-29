//! SQL database service, one SQLite file per tenant.
//!
//! `<data_dir>/db/<tenant>.sqlite`. File-level isolation is the cheapest
//! correct answer here: a tenant cannot name another tenant's table, cannot
//! lock another tenant's rows, and a `DROP TABLE` blast radius stops at its
//! own file. Two environments of one application are two tenants, so a
//! preview deployment cannot drop production's tables either.
//!
//! The price is that the tenant id becomes a filename, which is why
//! [`TenantId`] refuses anything the wire conventions do not accept as
//! exactly two plain identifiers.
//!
//! "Prepared" statements are prepared per call, not held open. A pooled
//! connection cannot own a live `rusqlite::Statement` across requests, so
//! `Prepare` validates the SQL, stores the text under a handle, and every
//! later use re-prepares it on whichever connection it lands on. Callers get
//! the property they actually want from `Prepare` - the SQL is parsed once,
//! up front, and parameters are always bound rather than interpolated.

use crate::error::{StoreError, StoreResult};
use crate::proto::storage::{
    BatchRequest, BatchResponse, ExecutePreparedRequest, ExecuteRequest, ExecuteResponse,
    PrepareRequest, PrepareResponse, QueryPreparedRequest, QueryRequest, QueryResponse,
    database_server::Database,
};
use crate::sqlite::{PoolSize, SqlitePool};
use crate::tenant::{TenantId, tenant_of};
use rusqlite::types::{Value as SqlValue, ValueRef};
use serde_json::{Map as JsonMap, Value as JsonValue};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tonic::{Request, Response, Status};

/// How many statements the whole process will remember at once. Handles are
/// small (an id and the SQL text) but a client that prepares in a loop and
/// never stops would otherwise grow the map without bound.
const MAX_PREPARED_STATEMENTS: usize = 4096;

/// One remembered statement.
#[derive(Clone, Debug)]
struct PreparedStatement {
    /// Whoever prepared it. Checked on every use, so a leaked handle is
    /// still useless to another tenant.
    tenant: TenantId,
    sql: String,
    /// Monotonic sequence number, used to evict the oldest handle first.
    seq: u64,
}

/// Handles for statements prepared by `Prepare`.
#[derive(Default)]
struct StatementRegistry {
    entries: Mutex<HashMap<String, PreparedStatement>>,
    next_seq: Mutex<u64>,
}

impl StatementRegistry {
    fn insert(&self, tenant: TenantId, sql: String) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let seq = {
            let mut next = self.next_seq.lock().unwrap_or_else(|e| e.into_inner());
            *next += 1;
            *next
        };
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() >= MAX_PREPARED_STATEMENTS
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, s)| s.seq)
                .map(|(id, _)| id.clone())
        {
            entries.remove(&oldest);
            tracing::warn!(
                capacity = MAX_PREPARED_STATEMENTS,
                evicted = %oldest,
                "prepared statement table is full, dropped the oldest handle; a client still \
                 holding it will get an unknown-statement error and has to prepare again"
            );
        }
        entries.insert(id.clone(), PreparedStatement { tenant, sql, seq });
        id
    }

    fn resolve(&self, tenant: &TenantId, id: &str) -> StoreResult<String> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let stmt = entries.get(id).ok_or_else(|| {
            tracing::debug!(
                statement = %id,
                tenant = %tenant,
                "unknown prepared statement handle; it was never prepared, or it was evicted"
            );
            StoreError::NotFound(format!("prepared statement {id} is unknown"))
        })?;
        if &stmt.tenant != tenant {
            tracing::warn!(
                statement = %id,
                owner = %stmt.tenant,
                caller = %tenant,
                "refused a prepared statement belonging to another tenant; the call touched no data"
            );
            return Err(StoreError::Tenant(format!(
                "prepared statement {id} belongs to another tenant"
            )));
        }
        Ok(stmt.sql.clone())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

/// SQL service. Pools are created on first use and kept for the process
/// lifetime, one per tenant.
#[derive(Clone)]
pub struct DatabaseService {
    root: PathBuf,
    pools: Arc<Mutex<HashMap<TenantId, SqlitePool>>>,
    connections_per_tenant: usize,
    statements: Arc<StatementRegistry>,
}

impl DatabaseService {
    /// Prepare `<data_dir>/db/`. Nothing is opened until a tenant asks.
    pub fn open(data_dir: &std::path::Path, connections_per_tenant: usize) -> StoreResult<Self> {
        let root = data_dir.join("db");
        std::fs::create_dir_all(&root)?;
        // One file per tenant. A file named by a bare application id belongs
        // to the layout this change replaced, and the tables in it would
        // never be opened again.
        crate::legacy::reject_pre_tenant_database_files(&root)?;
        tracing::info!(path = %root.display(), "database service ready, one file per tenant");
        Ok(Self {
            root,
            pools: Arc::new(Mutex::new(HashMap::new())),
            connections_per_tenant,
            statements: Arc::new(StatementRegistry::default()),
        })
    }

    /// The pool for one tenant, opened on first use.
    fn pool(&self, tenant: &TenantId) -> SqlitePool {
        let mut pools = self.pools.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pool) = pools.get(tenant) {
            return pool.clone();
        }
        // A tenant id carries one dot, between its two halves, and neither
        // half may contain a dot or a path separator - so the whole value is
        // a single path component and cannot resolve outside `root`.
        let path = self.root.join(format!("{tenant}.sqlite"));
        tracing::info!(
            tenant = %tenant,
            path = %path.display(),
            "opening this tenant's database file"
        );
        let pool = SqlitePool::new(
            path,
            format!("db:{tenant}"),
            PoolSize::new(self.connections_per_tenant),
        );
        pools.insert(tenant.clone(), pool.clone());
        pool
    }

    /// Run one statement that does not return rows.
    pub async fn exec_statement(
        &self,
        tenant: &TenantId,
        sql: &str,
        params: &[SqlValue],
    ) -> StoreResult<ExecOutcome> {
        let sql = check_sql(sql)?;
        let params = params.to_vec();
        self.pool(tenant)
            .with_conn(move |conn| {
                let rows_affected = conn.execute(&sql, rusqlite::params_from_iter(params))?;
                Ok(ExecOutcome {
                    rows_affected: rows_affected as i64,
                    last_insert_id: conn.last_insert_rowid(),
                })
            })
            .await
    }

    /// Run one statement and collect its rows.
    pub async fn fetch_rows(
        &self,
        tenant: &TenantId,
        sql: &str,
        params: &[SqlValue],
    ) -> StoreResult<ResultSet> {
        let sql = check_sql(sql)?;
        let params = params.to_vec();
        self.pool(tenant)
            .with_conn(move |conn| {
                let mut stmt = conn.prepare(&sql)?;
                let columns: Vec<String> = stmt
                    .column_names()
                    .into_iter()
                    .map(str::to_string)
                    .collect();
                let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
                let mut out = Vec::new();
                while let Some(row) = rows.next()? {
                    let mut object = JsonMap::with_capacity(columns.len());
                    for (index, name) in columns.iter().enumerate() {
                        object.insert(name.clone(), sql_to_json(row.get_ref(index)?));
                    }
                    out.push(JsonValue::Object(object));
                }
                Ok(ResultSet {
                    columns,
                    rows: JsonValue::Array(out),
                })
            })
            .await
    }

    /// Run several statements inside one transaction: either all of them
    /// land or none of them do.
    pub async fn exec_batch(
        &self,
        tenant: &TenantId,
        statements: Vec<(String, Vec<SqlValue>)>,
    ) -> StoreResult<Result<Vec<ExecOutcome>, BatchFailure>> {
        for (sql, _) in &statements {
            check_sql(sql)?;
        }
        self.pool(tenant)
            .with_conn(move |conn| {
                // Deferred is enough here: `tx.execute` refuses a statement
                // that returns rows, so a batch never reads before it writes
                // and never has to upgrade a read snapshot to a write lock -
                // the upgrade being the case SQLite refuses outright instead
                // of waiting out with `busy_timeout`.
                let tx = conn.transaction()?;
                let mut done = Vec::with_capacity(statements.len());
                for (index, (sql, params)) in statements.into_iter().enumerate() {
                    match tx.execute(&sql, rusqlite::params_from_iter(params)) {
                        Ok(rows_affected) => done.push(ExecOutcome {
                            rows_affected: rows_affected as i64,
                            last_insert_id: tx.last_insert_rowid(),
                        }),
                        Err(e) => {
                            // Dropping the transaction rolls it back.
                            return Ok(Err(BatchFailure {
                                index,
                                message: e.to_string(),
                                partial: done,
                            }));
                        }
                    }
                }
                tx.commit()?;
                Ok(Ok(done))
            })
            .await
    }

    /// Validate `sql` and remember it under a handle.
    pub async fn register_statement(&self, tenant: &TenantId, sql: &str) -> StoreResult<String> {
        let sql = check_sql(sql)?;
        let owned = sql.clone();
        // Prepare it once for real so a syntax error is reported now, at the
        // call that can explain it, rather than at some later execution.
        self.pool(tenant)
            .with_conn(move |conn| {
                conn.prepare(&owned)?;
                Ok(())
            })
            .await?;
        let id = self.statements.insert(tenant.clone(), sql);
        tracing::debug!(tenant = %tenant, statement = %id, "prepared statement registered");
        Ok(id)
    }

    /// The SQL behind a handle, if this tenant owns it.
    fn prepared_sql(&self, tenant: &TenantId, id: &str) -> StoreResult<String> {
        self.statements.resolve(tenant, id)
    }
}

/// What a row-less statement did: how many rows it touched and, for an
/// insert, the id it handed out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutcome {
    pub rows_affected: i64,
    pub last_insert_id: i64,
}

/// The rows a query returned, plus the column names they are keyed by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSet {
    pub columns: Vec<String>,
    /// JSON array of objects, one per row, keyed by column name.
    pub rows: JsonValue,
}

/// Which statement of a batch failed, and what had run before it. The whole
/// transaction is rolled back regardless; `partial` is for diagnosis only.
#[derive(Debug, Clone)]
pub struct BatchFailure {
    pub index: usize,
    pub message: String,
    pub partial: Vec<ExecOutcome>,
}

/// Refuse SQL that is empty or nothing but whitespace before it reaches the
/// database, where it would come back as a confusing parse error.
fn check_sql(sql: &str) -> StoreResult<String> {
    if sql.trim().is_empty() {
        return Err(StoreError::InvalidArgument("sql is empty".to_string()));
    }
    Ok(sql.to_string())
}

/// Decode the JSON parameter array that storage.proto carries as a string.
///
/// Only scalars are accepted: SQLite has no array or object type, and
/// silently stringifying a nested value would store something the tenant never
/// asked for and cannot get back.
fn decode_params(params: Option<&str>) -> StoreResult<Vec<SqlValue>> {
    let Some(raw) = params else {
        return Ok(Vec::new());
    };
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let parsed: JsonValue = serde_json::from_str(raw)
        .map_err(|e| StoreError::InvalidArgument(format!("params is not valid JSON: {e}")))?;
    let JsonValue::Array(items) = parsed else {
        return Err(StoreError::InvalidArgument(
            "params must be a JSON array".to_string(),
        ));
    };
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| match item {
            JsonValue::Null => Ok(SqlValue::Null),
            JsonValue::Bool(b) => Ok(SqlValue::Integer(i64::from(b))),
            JsonValue::String(s) => Ok(SqlValue::Text(s)),
            JsonValue::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(SqlValue::Integer(i))
                } else if let Some(f) = n.as_f64() {
                    Ok(SqlValue::Real(f))
                } else {
                    Err(StoreError::InvalidArgument(format!(
                        "params[{index}] is a number SQLite cannot hold: {n}"
                    )))
                }
            }
            other => Err(StoreError::InvalidArgument(format!(
                "params[{index}] is {}, only null, boolean, number and string are bindable",
                match other {
                    JsonValue::Array(_) => "an array",
                    JsonValue::Object(_) => "an object",
                    _ => "unsupported",
                }
            ))),
        })
        .collect()
}

/// Map one SQLite cell to JSON. Blobs become arrays of byte values, which is
/// the only lossless shape that survives `JSON.parse` on the runtime side.
fn sql_to_json(value: ValueRef<'_>) -> JsonValue {
    match value {
        ValueRef::Null => JsonValue::Null,
        ValueRef::Integer(i) => JsonValue::from(i),
        ValueRef::Real(f) => serde_json::Number::from_f64(f)
            .map(JsonValue::Number)
            // NaN and the infinities have no JSON spelling. They cannot be
            // produced by a bound parameter, only by arithmetic inside the
            // statement, so null is the honest answer.
            .unwrap_or(JsonValue::Null),
        ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => JsonValue::String(text.to_string()),
            Err(_) => {
                tracing::warn!(
                    "a TEXT column holds bytes that are not UTF-8; returning it as a byte array so \
                     the row is still readable"
                );
                JsonValue::Array(bytes.iter().map(|b| JsonValue::from(*b)).collect())
            }
        },
        ValueRef::Blob(bytes) => {
            JsonValue::Array(bytes.iter().map(|b| JsonValue::from(*b)).collect())
        }
    }
}

/// storage.proto reports statement failures inside the response, not as a
/// gRPC status: a tenant's SQL typo is not a transport fault.
fn failed_execute(message: String) -> ExecuteResponse {
    ExecuteResponse {
        success: false,
        rows_affected: 0,
        last_insert_id: None,
        error: Some(message),
    }
}

fn failed_query(message: String) -> QueryResponse {
    QueryResponse {
        success: false,
        rows: "[]".to_string(),
        columns: Vec::new(),
        error: Some(message),
    }
}

impl DatabaseService {
    async fn run_execute(
        &self,
        tenant: &TenantId,
        sql: &str,
        params: Option<&str>,
    ) -> Result<ExecuteResponse, Status> {
        let params = match decode_params(params) {
            Ok(p) => p,
            Err(e) => return Ok(failed_execute(e.to_string())),
        };
        match self.exec_statement(tenant, sql, &params).await {
            Ok(done) => {
                tracing::debug!(
                    tenant = %tenant,
                    rows_affected = done.rows_affected,
                    "sql execute finished"
                );
                Ok(ExecuteResponse {
                    success: true,
                    rows_affected: done.rows_affected,
                    last_insert_id: Some(done.last_insert_id),
                    error: None,
                })
            }
            Err(StoreError::Sqlite(e)) => {
                tracing::debug!(tenant = %tenant, error = %e, "sql execute rejected by sqlite");
                Ok(failed_execute(e.to_string()))
            }
            Err(e @ StoreError::InvalidArgument(_)) => Ok(failed_execute(e.to_string())),
            Err(other) => {
                tracing::error!(
                    tenant = %tenant,
                    error = %other,
                    "sql execute failed below the statement level; the write did not happen"
                );
                Err(other.into())
            }
        }
    }

    async fn run_query(
        &self,
        tenant: &TenantId,
        sql: &str,
        params: Option<&str>,
    ) -> Result<QueryResponse, Status> {
        let params = match decode_params(params) {
            Ok(p) => p,
            Err(e) => return Ok(failed_query(e.to_string())),
        };
        match self.fetch_rows(tenant, sql, &params).await {
            Ok(result) => {
                let row_count = result.rows.as_array().map(Vec::len).unwrap_or(0);
                tracing::debug!(tenant = %tenant, rows = row_count, "sql query finished");
                Ok(QueryResponse {
                    success: true,
                    rows: result.rows.to_string(),
                    columns: result.columns,
                    error: None,
                })
            }
            Err(StoreError::Sqlite(e)) => {
                tracing::debug!(tenant = %tenant, error = %e, "sql query rejected by sqlite");
                Ok(failed_query(e.to_string()))
            }
            Err(e @ StoreError::InvalidArgument(_)) => Ok(failed_query(e.to_string())),
            Err(other) => {
                tracing::error!(
                    tenant = %tenant,
                    error = %other,
                    "sql query failed below the statement level; no rows were returned"
                );
                Err(other.into())
            }
        }
    }
}

#[tonic::async_trait]
impl Database for DatabaseService {
    async fn execute(
        &self,
        request: Request<ExecuteRequest>,
    ) -> Result<Response<ExecuteResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        tracing::trace!(tenant = %tenant, sql = %req.sql, "sql execute");
        Ok(Response::new(
            self.run_execute(&tenant, &req.sql, req.params.as_deref())
                .await?,
        ))
    }

    async fn query(
        &self,
        request: Request<QueryRequest>,
    ) -> Result<Response<QueryResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        tracing::trace!(tenant = %tenant, sql = %req.sql, "sql query");
        Ok(Response::new(
            self.run_query(&tenant, &req.sql, req.params.as_deref())
                .await?,
        ))
    }

    async fn batch(
        &self,
        request: Request<BatchRequest>,
    ) -> Result<Response<BatchResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        tracing::debug!(tenant = %tenant, statements = req.statements.len(), "sql batch");

        let mut decoded = Vec::with_capacity(req.statements.len());
        for (index, statement) in req.statements.iter().enumerate() {
            match decode_params(statement.params.as_deref()) {
                Ok(params) => decoded.push((statement.sql.clone(), params)),
                Err(e) => {
                    return Ok(Response::new(BatchResponse {
                        success: false,
                        results: Vec::new(),
                        error: Some(format!("statement {index}: {e}")),
                    }));
                }
            }
        }

        match self.exec_batch(&tenant, decoded).await {
            Ok(Ok(done)) => Ok(Response::new(BatchResponse {
                success: true,
                results: done
                    .into_iter()
                    .map(|d| ExecuteResponse {
                        success: true,
                        rows_affected: d.rows_affected,
                        last_insert_id: Some(d.last_insert_id),
                        error: None,
                    })
                    .collect(),
                error: None,
            })),
            Ok(Err(failure)) => {
                tracing::warn!(
                    tenant = %tenant,
                    statement = failure.index,
                    error = %failure.message,
                    "sql batch rolled back; none of its statements were applied"
                );
                Ok(Response::new(BatchResponse {
                    success: false,
                    results: failure
                        .partial
                        .into_iter()
                        .map(|d| ExecuteResponse {
                            success: true,
                            rows_affected: d.rows_affected,
                            last_insert_id: Some(d.last_insert_id),
                            error: None,
                        })
                        .collect(),
                    error: Some(format!(
                        "statement {} failed and the batch was rolled back: {}",
                        failure.index, failure.message
                    )),
                }))
            }
            Err(e @ StoreError::InvalidArgument(_)) => Ok(Response::new(BatchResponse {
                success: false,
                results: Vec::new(),
                error: Some(e.to_string()),
            })),
            Err(other) => {
                tracing::error!(
                    tenant = %tenant,
                    error = %other,
                    "sql batch failed below the statement level; nothing was applied"
                );
                Err(other.into())
            }
        }
    }

    async fn prepare(
        &self,
        request: Request<PrepareRequest>,
    ) -> Result<Response<PrepareResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        match self.register_statement(&tenant, &req.sql).await {
            Ok(statement_id) => Ok(Response::new(PrepareResponse {
                success: true,
                statement_id,
                error: None,
            })),
            Err(StoreError::Sqlite(e)) => {
                tracing::debug!(tenant = %tenant, error = %e, "prepare rejected by sqlite");
                Ok(Response::new(PrepareResponse {
                    success: false,
                    statement_id: String::new(),
                    error: Some(e.to_string()),
                }))
            }
            Err(e @ StoreError::InvalidArgument(_)) => Ok(Response::new(PrepareResponse {
                success: false,
                statement_id: String::new(),
                error: Some(e.to_string()),
            })),
            Err(other) => {
                tracing::error!(
                    tenant = %tenant,
                    error = %other,
                    "prepare failed; no handle was issued"
                );
                Err(other.into())
            }
        }
    }

    async fn execute_prepared(
        &self,
        request: Request<ExecutePreparedRequest>,
    ) -> Result<Response<ExecuteResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let sql = match self.prepared_sql(&tenant, &req.statement_id) {
            Ok(sql) => sql,
            Err(e @ StoreError::NotFound(_)) => {
                return Ok(Response::new(failed_execute(e.to_string())));
            }
            Err(other) => return Err(other.into()),
        };
        Ok(Response::new(
            self.run_execute(&tenant, &sql, req.params.as_deref())
                .await?,
        ))
    }

    async fn query_prepared(
        &self,
        request: Request<QueryPreparedRequest>,
    ) -> Result<Response<QueryResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let sql = match self.prepared_sql(&tenant, &req.statement_id) {
            Ok(sql) => sql,
            Err(e @ StoreError::NotFound(_)) => {
                return Ok(Response::new(failed_query(e.to_string())));
            }
            Err(other) => return Err(other.into()),
        };
        Ok(Response::new(
            self.run_query(&tenant, &sql, req.params.as_deref()).await?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn service() -> (TempDir, DatabaseService) {
        let dir = TempDir::new().unwrap();
        let svc = DatabaseService::open(dir.path(), 4).unwrap();
        (dir, svc)
    }

    fn tenant(name: &str) -> TenantId {
        TenantId::parse(name).unwrap()
    }

    async fn make_table(svc: &DatabaseService, tenant: &TenantId) {
        svc.exec_statement(
            tenant,
            "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT)",
            &[],
        )
        .await
        .unwrap();
    }

    /// Two of a tenant's batches overlapping is ordinary and must not make
    /// either of them fail. They contend for one file's write lock, and the
    /// loser is expected to wait its turn rather than come back "database is
    /// locked".
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_batches_wait_for_each_other() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let batches: Vec<_> = (0..16)
            .map(|n| {
                let (svc, a) = (svc.clone(), a.clone());
                tokio::spawn(async move {
                    svc.exec_batch(
                        &a,
                        vec![(
                            "INSERT INTO users(name) VALUES(?1)".to_string(),
                            vec![SqlValue::Text(format!("user-{n}"))],
                        )],
                    )
                    .await
                })
            })
            .collect();
        for (n, batch) in batches.into_iter().enumerate() {
            let outcome = batch
                .await
                .unwrap()
                .unwrap_or_else(|e| panic!("batch {n} failed: {e}"));
            assert!(outcome.is_ok(), "batch {n} rolled back: {outcome:?}");
        }

        let found = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(found.rows[0]["n"], serde_json::json!(16));
    }

    #[tokio::test]
    async fn executes_and_queries() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let done = svc
            .exec_statement(
                &a,
                "INSERT INTO users(name) VALUES(?1)",
                &[SqlValue::Text("Alice".into())],
            )
            .await
            .unwrap();
        assert_eq!(done.rows_affected, 1);
        assert_eq!(done.last_insert_id, 1);

        let result = svc
            .fetch_rows(&a, "SELECT id, name FROM users", &[])
            .await
            .unwrap();
        assert_eq!(result.columns, vec!["id".to_string(), "name".to_string()]);
        assert_eq!(
            result.rows,
            serde_json::json!([{ "id": 1, "name": "Alice" }])
        );
    }

    #[tokio::test]
    async fn each_app_gets_its_own_file() {
        let (dir, svc) = service();
        let one = tenant("app-one.production");
        let two = tenant("app-two.production");
        make_table(&svc, &one).await;
        svc.exec_statement(&one, "INSERT INTO users(name) VALUES('Alice')", &[])
            .await
            .unwrap();

        // The second tenant has no such table at all.
        let err = svc.fetch_rows(&two, "SELECT * FROM users", &[]).await;
        assert!(matches!(err, Err(StoreError::Sqlite(_))));

        assert!(dir.path().join("db/app-one.production.sqlite").exists());
        assert!(dir.path().join("db/app-two.production.sqlite").exists());
    }

    #[tokio::test]
    async fn a_batch_is_all_or_nothing() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let failure = svc
            .exec_batch(
                &a,
                vec![
                    (
                        "INSERT INTO users(name) VALUES('Alice')".to_string(),
                        vec![],
                    ),
                    ("INSERT INTO nope(name) VALUES('Bob')".to_string(), vec![]),
                ],
            )
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(failure.index, 1);
        assert_eq!(failure.partial.len(), 1);

        let result = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(result.rows, serde_json::json!([{ "n": 0 }]));

        let done = svc
            .exec_batch(
                &a,
                vec![
                    (
                        "INSERT INTO users(name) VALUES('Alice')".to_string(),
                        vec![],
                    ),
                    ("INSERT INTO users(name) VALUES('Bob')".to_string(), vec![]),
                ],
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.len(), 2);
        let result = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(result.rows, serde_json::json!([{ "n": 2 }]));
    }

    #[tokio::test]
    async fn prepared_statements_are_scoped_to_their_app() {
        let (_dir, svc) = service();
        let one = tenant("app-one.production");
        let two = tenant("app-two.production");
        make_table(&svc, &one).await;

        let id = svc
            .register_statement(&one, "INSERT INTO users(name) VALUES(?1)")
            .await
            .unwrap();
        assert!(matches!(
            svc.prepared_sql(&two, &id),
            Err(StoreError::Tenant(_))
        ));
        assert_eq!(
            svc.prepared_sql(&one, &id).unwrap(),
            "INSERT INTO users(name) VALUES(?1)"
        );
        assert!(matches!(
            svc.prepared_sql(&one, "no-such-handle"),
            Err(StoreError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn prepare_rejects_bad_sql_up_front() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        assert!(matches!(
            svc.register_statement(&a, "SELCT 1").await,
            Err(StoreError::Sqlite(_))
        ));
        assert!(matches!(
            svc.register_statement(&a, "   ").await,
            Err(StoreError::InvalidArgument(_))
        ));
    }

    #[test]
    fn statement_registry_evicts_the_oldest_when_full() {
        let registry = StatementRegistry::default();
        let a = tenant("app-one.production");
        let first = registry.insert(a.clone(), "SELECT 1".to_string());
        for i in 0..MAX_PREPARED_STATEMENTS {
            registry.insert(a.clone(), format!("SELECT {i}"));
        }
        assert_eq!(registry.len(), MAX_PREPARED_STATEMENTS);
        assert!(matches!(
            registry.resolve(&a, &first),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn decodes_scalar_params_only() {
        assert!(decode_params(None).unwrap().is_empty());
        assert!(decode_params(Some("")).unwrap().is_empty());
        assert_eq!(
            decode_params(Some(r#"[1, 2.5, "x", null, true]"#)).unwrap(),
            vec![
                SqlValue::Integer(1),
                SqlValue::Real(2.5),
                SqlValue::Text("x".into()),
                SqlValue::Null,
                SqlValue::Integer(1),
            ]
        );
        for bad in [r#"{"a":1}"#, "[[1]]", r#"[{"a":1}]"#, "not json"] {
            assert!(
                matches!(
                    decode_params(Some(bad)),
                    Err(StoreError::InvalidArgument(_))
                ),
                "{bad} should be rejected"
            );
        }
    }

    #[tokio::test]
    async fn blob_columns_come_back_as_byte_arrays() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        svc.exec_statement(&a, "CREATE TABLE raw(b BLOB)", &[])
            .await
            .unwrap();
        svc.exec_statement(&a, "INSERT INTO raw(b) VALUES(x'00ff10')", &[])
            .await
            .unwrap();
        let result = svc.fetch_rows(&a, "SELECT b FROM raw", &[]).await.unwrap();
        assert_eq!(result.rows, serde_json::json!([{ "b": [0, 255, 16] }]));
    }
}
