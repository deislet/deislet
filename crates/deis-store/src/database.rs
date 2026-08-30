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
//!
//! # Interactive transactions
//!
//! `Batch` runs several statements atomically, but the caller has to know all
//! of them before the first one runs. That is not what a transaction is for.
//! Reading a row, deciding from what it says, and writing accordingly is the
//! whole point, and it needs the transaction to stay open across a round trip
//! to whoever is deciding. A distributed SQL service cannot offer that. One
//! SQLite file per tenant on one box can, so this one does:
//! `BeginTransaction` pins a connection, `ExecuteInTransaction` and
//! `QueryInTransaction` run on it, `CommitTransaction` or
//! `RollbackTransaction` end it and give the connection back.
//!
//! Four things make it safe rather than a foot-gun, and none of them is
//! optional:
//!
//! * **`BEGIN IMMEDIATE`, never deferred.** A deferred transaction that reads
//!   and then writes takes a read snapshot first and fails the write with
//!   `SQLITE_BUSY_SNAPSHOT` - at *commit* time, after the caller has been
//!   told every statement succeeded. Read-then-write is exactly the workload
//!   this exists for, so the write lock is taken up front, where contention
//!   is still something the caller can be told about honestly.
//! * **The tenant is checked on every use**, as it is for prepared
//!   statements: a handle that leaks out of one tenant opens nothing in
//!   another.
//! * **A hard server-side deadline.** The connection is pinned for the whole
//!   life of the transaction, so a handler that opens one and then awaits
//!   something slow is sitting on its tenant's writer. At the deadline the
//!   server rolls the transaction back itself, releases the connection, and
//!   remembers why: the next call on that handle is told its work was rolled
//!   back for exceeding the deadline, rather than that the handle is unknown.
//! * **A bound on how many may be open at once**, per tenant and in total.
//!   The per-tenant bound is additionally clamped to one below the tenant's
//!   pool size, so a client looping on `BeginTransaction` can never take the
//!   last connection its own ordinary queries need.
//!
//! SQLite still allows one writer per file. A second interactive transaction
//! on the same tenant therefore waits at `BEGIN IMMEDIATE` for the first to
//! finish, and gives up with "database is locked" if it waits out the pool's
//! busy timeout. That is inherent, and it is why the per-tenant bound is
//! small: queueing writers behind a writer does not make them any faster.

use crate::error::{StoreError, StoreResult};
use crate::proto::storage::{
    BatchRequest, BatchResponse, BeginTransactionRequest, BeginTransactionResponse,
    CommitTransactionRequest, CommitTransactionResponse, ExecuteInTransactionRequest,
    ExecutePreparedRequest, ExecuteRequest, ExecuteResponse, PrepareRequest, PrepareResponse,
    QueryInTransactionRequest, QueryPreparedRequest, QueryRequest, QueryResponse,
    RollbackTransactionRequest, RollbackTransactionResponse, database_server::Database,
};
use crate::sqlite::{PoolSize, PooledConnection, SqlitePool};
use crate::tenant::{TenantId, tenant_of};
use rusqlite::types::{Value as SqlValue, ValueRef};
use serde_json::{Map as JsonMap, Value as JsonValue};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
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

/// How long an interactive transaction may stay open before the server rolls
/// it back on its own.
///
/// Small on purpose. The transaction is holding one of its tenant's few
/// connections and, because it began `IMMEDIATE`, that file's write lock; a
/// handler that means to think for longer than this between statements wants
/// two transactions, not a longer one. It is also the same order as the
/// pool's busy timeout, so a second writer waiting at `BEGIN` outlives the
/// first one's deadline rather than the other way round.
pub const DEFAULT_TRANSACTION_DEADLINE: Duration = Duration::from_secs(5);

/// How many transactions one tenant may hold open at once, before the clamp
/// to its pool size in [`clamp_to_pool`].
///
/// Two, because SQLite gives a file one writer: the second is already only
/// waiting for the first, and a third would be waiting for a wait.
pub const DEFAULT_MAX_OPEN_TRANSACTIONS_PER_TENANT: usize = 2;

/// How many transactions the whole process may hold open at once, across
/// every tenant. Each one is a pinned connection and an open file, so this is
/// the ceiling on what a crowd of tenants can pin between them.
pub const DEFAULT_MAX_OPEN_TRANSACTIONS: usize = 64;

/// How many finished handles are remembered, so a caller that uses one gets
/// "committed" or "rolled back at the deadline" instead of "unknown".
const REMEMBERED_ENDED_TRANSACTIONS: usize = 1024;

/// What an interactive transaction is allowed to cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionLimits {
    /// Ceiling on how long one may stay open. A caller may ask for less; it
    /// cannot ask for more.
    pub deadline: Duration,
    /// How many one tenant may hold open at once.
    pub max_open_per_tenant: usize,
    /// How many the process may hold open at once, over all tenants.
    pub max_open_total: usize,
}

impl Default for TransactionLimits {
    fn default() -> Self {
        Self {
            deadline: DEFAULT_TRANSACTION_DEADLINE,
            max_open_per_tenant: DEFAULT_MAX_OPEN_TRANSACTIONS_PER_TENANT,
            max_open_total: DEFAULT_MAX_OPEN_TRANSACTIONS,
        }
    }
}

impl TransactionLimits {
    /// How long this transaction actually gets: what the caller asked for,
    /// never more than the ceiling.
    fn grant(&self, requested: Option<Duration>) -> StoreResult<Duration> {
        let Some(requested) = requested else {
            return Ok(self.deadline);
        };
        if requested.is_zero() {
            return Err(StoreError::InvalidArgument(
                "a transaction timeout must be positive; one that has already expired cannot be \
                 opened"
                    .to_string(),
            ));
        }
        Ok(requested.min(self.deadline))
    }
}

/// Hold the per-tenant ceiling one below the tenant's pool size.
///
/// Transactions pin their connection for their whole life, so a ceiling equal
/// to the pool size would let a client that loops on `BeginTransaction` pin
/// every connection and then block its own ordinary queries behind its own
/// transactions. Leaving one back means the plain `Execute`/`Query` path
/// always has somewhere to run.
fn clamp_to_pool(limits: TransactionLimits, connections_per_tenant: usize) -> TransactionLimits {
    TransactionLimits {
        max_open_per_tenant: limits
            .max_open_per_tenant
            .min(connections_per_tenant.saturating_sub(1).max(1))
            .max(1),
        max_open_total: limits.max_open_total.max(1),
        ..limits
    }
}

/// One open interactive transaction.
struct TransactionEntry {
    /// Whoever opened it. Checked on every use, so a leaked handle is still
    /// useless to another tenant.
    tenant: TenantId,
    /// The pinned connection, `None` once whoever ended the transaction has
    /// taken it back out. Guarded by a std mutex that is only ever locked
    /// inside a blocking task: two statements on one handle are one session
    /// and must run one after the other.
    conn: Mutex<Option<PooledConnection>>,
    /// The task that will roll this back at the deadline, cancelled when the
    /// transaction ends first.
    deadline_task: Mutex<Option<JoinHandle<()>>>,
    /// What the caller was granted, quoted back when the deadline fires.
    granted: Duration,
}

impl TransactionEntry {
    /// Stop the deadline task; the transaction has ended some other way.
    fn cancel_deadline(&self) {
        if let Some(task) = self
            .deadline_task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            task.abort();
        }
    }
}

/// How a transaction ended, remembered so its handle can answer for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    Committed,
    RolledBack,
    DeadlineExceeded,
}

/// The tombstone left behind by a finished transaction.
#[derive(Debug, Clone)]
struct EndedTransaction {
    tenant: TenantId,
    how: Ended,
    granted: Duration,
}

impl EndedTransaction {
    /// What a caller that still holds this handle is told.
    fn as_error(&self, id: &str) -> StoreError {
        match self.how {
            // The one message that has to be specific: the caller did not ask
            // for this and its work is gone, so "unknown handle" would send
            // it looking for a bug it does not have.
            Ended::DeadlineExceeded => StoreError::Expired(format!(
                "transaction {id} was rolled back because it exceeded its {} ms deadline; nothing \
                 it wrote was applied. Keep the work between BeginTransaction and \
                 CommitTransaction short, or ask for a longer timeout when opening it",
                self.granted.as_millis()
            )),
            Ended::Committed => {
                StoreError::NotFound(format!("transaction {id} was already committed"))
            }
            Ended::RolledBack => {
                StoreError::NotFound(format!("transaction {id} was already rolled back"))
            }
        }
    }
}

/// Everything the process knows about interactive transactions: which are
/// open, and why the recently closed ones closed.
#[derive(Default)]
struct TransactionRegistry {
    state: Mutex<RegistryState>,
}

#[derive(Default)]
struct RegistryState {
    open: HashMap<String, Arc<TransactionEntry>>,
    ended: HashMap<String, EndedTransaction>,
    /// Insertion order of `ended`, so the oldest tombstone is the one that
    /// goes when the table is full.
    ended_order: VecDeque<String>,
}

impl RegistryState {
    fn remember_ended(&mut self, id: String, ended: EndedTransaction) {
        if self.ended.insert(id.clone(), ended).is_none() {
            self.ended_order.push_back(id);
        }
        while self.ended_order.len() > REMEMBERED_ENDED_TRANSACTIONS
            && let Some(oldest) = self.ended_order.pop_front()
        {
            self.ended.remove(&oldest);
        }
    }

    fn open_for(&self, tenant: &TenantId) -> usize {
        self.open.values().filter(|e| &e.tenant == tenant).count()
    }
}

impl TransactionRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take a slot for `tenant`, or say which ceiling is in the way.
    ///
    /// The entry is inserted before its connection exists, so the slot is
    /// held for the whole of `BEGIN IMMEDIATE` - which is exactly when a
    /// client looping on begin would otherwise slip past the count.
    fn admit(
        &self,
        tenant: &TenantId,
        granted: Duration,
        limits: &TransactionLimits,
    ) -> StoreResult<(String, Arc<TransactionEntry>)> {
        let mut state = self.lock();
        if state.open.len() >= limits.max_open_total {
            tracing::warn!(
                tenant = %tenant,
                open = state.open.len(),
                ceiling = limits.max_open_total,
                "refused to open a transaction: the server is already holding as many as it \
                 allows, and each one pins a connection"
            );
            return Err(StoreError::Exhausted(format!(
                "this server already holds {} open transactions, which is its ceiling; commit or \
                 roll one back, or retry in a moment",
                state.open.len()
            )));
        }
        let mine = state.open_for(tenant);
        if mine >= limits.max_open_per_tenant {
            tracing::warn!(
                tenant = %tenant,
                open = mine,
                ceiling = limits.max_open_per_tenant,
                "refused to open a transaction: this tenant is already holding as many as it may, \
                 and more would only queue behind them for the one write lock its file has"
            );
            return Err(StoreError::Exhausted(format!(
                "this tenant already holds {mine} open transactions, which is its ceiling; commit \
                 or roll one back before opening another"
            )));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let entry = Arc::new(TransactionEntry {
            tenant: tenant.clone(),
            conn: Mutex::new(None),
            deadline_task: Mutex::new(None),
            granted,
        });
        state.open.insert(id.clone(), Arc::clone(&entry));
        Ok((id, entry))
    }

    /// Give a slot back for a transaction that never opened. No tombstone:
    /// the id was never handed out, so nobody can ask about it.
    fn abandon(&self, id: &str) {
        self.lock().open.remove(id);
    }

    /// The entry behind `id`, if this tenant owns it and it is still open.
    fn resolve(&self, tenant: &TenantId, id: &str) -> StoreResult<Arc<TransactionEntry>> {
        let state = self.lock();
        if let Some(entry) = state.open.get(id) {
            if &entry.tenant != tenant {
                tracing::warn!(
                    transaction = %id,
                    owner = %entry.tenant,
                    caller = %tenant,
                    "refused a transaction handle belonging to another tenant; the call touched \
                     no data"
                );
                return Err(StoreError::Tenant(format!(
                    "transaction {id} belongs to another tenant"
                )));
            }
            return Ok(Arc::clone(entry));
        }
        if let Some(ended) = state.ended.get(id) {
            if &ended.tenant != tenant {
                tracing::warn!(
                    transaction = %id,
                    owner = %ended.tenant,
                    caller = %tenant,
                    "refused a finished transaction handle belonging to another tenant"
                );
                return Err(StoreError::Tenant(format!(
                    "transaction {id} belongs to another tenant"
                )));
            }
            return Err(ended.as_error(id));
        }
        Err(StoreError::NotFound(format!(
            "transaction {id} is unknown; it was never opened, or it ended so long ago that the \
             server no longer remembers it"
        )))
    }

    /// Claim the right to end `id` on behalf of `tenant`.
    ///
    /// Removal from `open` is the arbiter: commit, rollback and the deadline
    /// all go through here, and exactly one of them can win.
    fn claim(&self, tenant: &TenantId, id: &str, how: Ended) -> StoreResult<Arc<TransactionEntry>> {
        let mut state = self.lock();
        let Some(entry) = state.open.get(id) else {
            drop(state);
            return Err(self
                .resolve(tenant, id)
                .err()
                .unwrap_or_else(|| StoreError::NotFound(format!("transaction {id} is unknown"))));
        };
        if &entry.tenant != tenant {
            tracing::warn!(
                transaction = %id,
                owner = %entry.tenant,
                caller = %tenant,
                "refused to end a transaction belonging to another tenant"
            );
            return Err(StoreError::Tenant(format!(
                "transaction {id} belongs to another tenant"
            )));
        }
        let entry = state.open.remove(id).expect("just looked it up");
        state.remember_ended(
            id.to_string(),
            EndedTransaction {
                tenant: entry.tenant.clone(),
                how,
                granted: entry.granted,
            },
        );
        Ok(entry)
    }

    /// The deadline's claim: no tenant to check, and losing the race to a
    /// commit that got there first is ordinary rather than an error.
    fn claim_expired(&self, id: &str) -> Option<Arc<TransactionEntry>> {
        let mut state = self.lock();
        let entry = state.open.remove(id)?;
        state.remember_ended(
            id.to_string(),
            EndedTransaction {
                tenant: entry.tenant.clone(),
                how: Ended::DeadlineExceeded,
                granted: entry.granted,
            },
        );
        Some(entry)
    }

    #[cfg(test)]
    fn open_count(&self) -> usize {
        self.lock().open.len()
    }
}

/// Holds a tenant's transaction slot while `BeginTransaction` is still
/// getting the transaction open.
///
/// Drop is what covers the case no error branch can reach: the caller went
/// away - tonic drops the handler future when the client resets the stream -
/// while the call was still waiting for a connection or for the write lock.
/// The slot would then stay taken for the life of the process, with no
/// transaction behind it and no deadline task to reap it, and the tenant
/// would be permanently one transaction poorer.
struct TransactionSlot {
    registry: Arc<TransactionRegistry>,
    id: String,
    /// Set once the transaction is open and its deadline task is about to be
    /// armed, after which the slot belongs to the transaction.
    handed_over: bool,
}

impl TransactionSlot {
    /// The transaction is open; stop guarding the slot and give up the id.
    fn keep(mut self) -> String {
        self.handed_over = true;
        std::mem::take(&mut self.id)
    }
}

impl Drop for TransactionSlot {
    fn drop(&mut self) {
        if self.handed_over {
            return;
        }
        self.registry.abandon(&self.id);
        tracing::debug!(
            transaction = %self.id,
            "gave a transaction slot back: opening it did not finish, so nothing was left holding \
             it"
        );
    }
}

/// A transaction that is open and the caller's to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionHandle {
    pub id: String,
    /// How long the server will keep it open, counted from the moment `BEGIN`
    /// returned.
    pub expires_in: Duration,
}

/// Take the pinned connection back and end the transaction on it.
///
/// The connection is taken out of the entry *before* the statement runs, so
/// even a `COMMIT` that fails releases it - the pool guard rolls back whatever
/// the failure left open before the connection is reused. Everything happens
/// inside the blocking closure, which means an aborted caller (the deadline
/// task being cancelled, say) cannot strand the connection either.
async fn end_transaction(entry: Arc<TransactionEntry>, sql: &'static str) -> StoreResult<()> {
    tokio::task::spawn_blocking(move || {
        let taken = entry.conn.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(pinned) = taken else {
            return Ok(());
        };
        let result = pinned.execute_batch(sql);
        drop(pinned);
        result.map_err(StoreError::from)
    })
    .await?
}

/// Run one statement on a transaction's pinned connection.
async fn on_transaction<F, T>(entry: Arc<TransactionEntry>, id: String, f: F) -> StoreResult<T>
where
    F: FnOnce(&mut rusqlite::Connection) -> StoreResult<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut guard = entry.conn.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(pinned) => f(pinned),
            // Only reachable by racing a commit, a rollback or the deadline:
            // the handle was still open when it was looked up and had ended by
            // the time this statement got the connection.
            None => Err(StoreError::NotFound(format!(
                "transaction {id} ended while this statement was waiting for it"
            ))),
        }
    })
    .await?
}

/// Open the transaction on a freshly pinned connection.
async fn begin_immediate(pinned: PooledConnection) -> StoreResult<PooledConnection> {
    tokio::task::spawn_blocking(move || {
        // IMMEDIATE rather than the default DEFERRED. A deferred transaction
        // that reads and then writes has to upgrade its read snapshot to a
        // write lock, which SQLite refuses outright with SQLITE_BUSY_SNAPSHOT
        // instead of waiting out - and it refuses at COMMIT, by which point
        // the caller has been told every statement succeeded. Read-then-write
        // is the whole reason this API exists, so the lock is taken here.
        pinned.execute_batch("BEGIN IMMEDIATE")?;
        Ok(pinned)
    })
    .await?
}

/// SQL service. Pools are created on first use and kept for the process
/// lifetime, one per tenant.
#[derive(Clone)]
pub struct DatabaseService {
    root: PathBuf,
    pools: Arc<Mutex<HashMap<TenantId, SqlitePool>>>,
    connections_per_tenant: usize,
    statements: Arc<StatementRegistry>,
    transactions: Arc<TransactionRegistry>,
    limits: TransactionLimits,
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
            transactions: Arc::new(TransactionRegistry::default()),
            limits: clamp_to_pool(TransactionLimits::default(), connections_per_tenant),
        })
    }

    /// Replace what interactive transactions are allowed to cost.
    ///
    /// The per-tenant ceiling is clamped to the tenant's pool size here, not
    /// where it is configured, because the pool size is what makes a given
    /// ceiling safe or not; see [`clamp_to_pool`].
    pub fn with_transaction_limits(mut self, limits: TransactionLimits) -> Self {
        self.limits = clamp_to_pool(limits, self.connections_per_tenant);
        self
    }

    /// The ceilings actually in force, after clamping. Logged at startup so
    /// an operator reading about a refused `BeginTransaction` can see what it
    /// was refused against.
    pub fn transaction_limits(&self) -> TransactionLimits {
        self.limits
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

    /// Open an interactive transaction on a connection pinned to it.
    ///
    /// `requested` is what the caller would like the deadline to be; it is
    /// clamped to the configured ceiling, and the handle says what was
    /// actually granted.
    pub async fn open_transaction(
        &self,
        tenant: &TenantId,
        requested: Option<Duration>,
    ) -> StoreResult<TransactionHandle> {
        let granted = self.limits.grant(requested)?;
        let (id, entry) = self.transactions.admit(tenant, granted, &self.limits)?;
        // Every way out from here - an error, or the caller disappearing
        // mid-call - gives the slot back, because the guard's drop does.
        let slot = TransactionSlot {
            registry: Arc::clone(&self.transactions),
            id,
            handed_over: false,
        };

        let pinned = self.pool(tenant).checkout().await?;
        let pinned = begin_immediate(pinned).await.inspect_err(|e| {
            tracing::debug!(
                tenant = %tenant,
                error = %e,
                "could not open a transaction; SQLite gives one file one writer, so this is \
                 usually another transaction on the same tenant still holding the write lock"
            );
        })?;

        *entry.conn.lock().unwrap_or_else(|e| e.into_inner()) = Some(pinned);
        // Nothing below awaits, so the slot cannot be dropped between being
        // handed over and the deadline task that takes over guarding it.
        let id = slot.keep();
        // The clock starts now, not before BEGIN: a transaction that spent
        // time waiting for the write lock has not spent any of the caller's.
        let task = self.spawn_deadline(id.clone(), granted);
        *entry
            .deadline_task
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(task);

        tracing::debug!(
            tenant = %tenant,
            transaction = %id,
            deadline_ms = granted.as_millis(),
            "opened an interactive transaction; it holds one of this tenant's connections until \
             it ends"
        );
        Ok(TransactionHandle {
            id,
            expires_in: granted,
        })
    }

    /// Roll the transaction back once its deadline passes.
    ///
    /// A transaction that ends on its own cancels this; one that does not -
    /// a handler awaiting something slow, a runtime that crashed, a client
    /// that dropped its connection - is ended here instead, which is the only
    /// thing standing between a forgotten handle and a tenant with no writer.
    fn spawn_deadline(&self, id: String, granted: Duration) -> JoinHandle<()> {
        let transactions = Arc::clone(&self.transactions);
        tokio::spawn(async move {
            tokio::time::sleep(granted).await;
            let Some(entry) = transactions.claim_expired(&id) else {
                return;
            };
            tracing::info!(
                tenant = %entry.tenant,
                transaction = %id,
                deadline_ms = granted.as_millis(),
                "rolling back a transaction that outlived its deadline; whatever it had written \
                 is discarded and the connection it pinned goes back to this tenant's pool"
            );
            if let Err(e) = end_transaction(entry, "ROLLBACK").await {
                tracing::error!(
                    transaction = %id,
                    error = %e,
                    "could not roll back a transaction that outlived its deadline; the connection \
                     is being closed rather than pooled, so the tenant is one connection down \
                     until the process restarts"
                );
            }
        })
    }

    /// Run a row-less statement inside an open transaction.
    pub async fn exec_in_transaction(
        &self,
        tenant: &TenantId,
        id: &str,
        sql: &str,
        params: &[SqlValue],
    ) -> StoreResult<ExecOutcome> {
        let sql = check_transaction_sql(sql)?;
        let entry = self.transactions.resolve(tenant, id)?;
        let params = params.to_vec();
        on_transaction(entry, id.to_string(), move |conn| {
            let rows_affected = conn.execute(&sql, rusqlite::params_from_iter(params))?;
            Ok(ExecOutcome {
                rows_affected: rows_affected as i64,
                last_insert_id: conn.last_insert_rowid(),
            })
        })
        .await
    }

    /// Run a query inside an open transaction. It sees that transaction's own
    /// uncommitted writes, which is the entire point of doing it here rather
    /// than with `Query`.
    pub async fn fetch_rows_in_transaction(
        &self,
        tenant: &TenantId,
        id: &str,
        sql: &str,
        params: &[SqlValue],
    ) -> StoreResult<ResultSet> {
        let sql = check_transaction_sql(sql)?;
        let entry = self.transactions.resolve(tenant, id)?;
        let params = params.to_vec();
        on_transaction(entry, id.to_string(), move |conn| {
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

    /// Apply everything the transaction did and release its connection.
    pub async fn commit(&self, tenant: &TenantId, id: &str) -> StoreResult<()> {
        self.finish(tenant, id, Ended::Committed, "COMMIT").await
    }

    /// Discard everything the transaction did and release its connection.
    pub async fn roll_back(&self, tenant: &TenantId, id: &str) -> StoreResult<()> {
        self.finish(tenant, id, Ended::RolledBack, "ROLLBACK").await
    }

    async fn finish(
        &self,
        tenant: &TenantId,
        id: &str,
        how: Ended,
        sql: &'static str,
    ) -> StoreResult<()> {
        let entry = self.transactions.claim(tenant, id, how)?;
        // Claimed, so the deadline cannot also end it; stopping the task now
        // keeps a begin/commit loop from leaving a timer per transaction
        // pending for the rest of its deadline.
        entry.cancel_deadline();
        let result = end_transaction(entry, sql).await;
        match &result {
            Ok(()) => tracing::debug!(
                tenant = %tenant,
                transaction = %id,
                outcome = ?how,
                "transaction ended, its connection is back in the pool"
            ),
            Err(e) => tracing::warn!(
                tenant = %tenant,
                transaction = %id,
                outcome = ?how,
                error = %e,
                "transaction could not be ended cleanly; its connection was released either way, \
                 and anything the statement left open was rolled back rather than pooled"
            ),
        }
        result
    }

    /// Hand the deadline's task over, so a test can wait for exactly the work
    /// it does instead of watching the clock.
    #[cfg(test)]
    fn take_deadline_task(&self, id: &str) -> Option<JoinHandle<()>> {
        let state = self.transactions.lock();
        let entry = state.open.get(id)?;
        entry
            .deadline_task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
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

/// The same check as [`check_sql`], plus a refusal of the statements that
/// would end the transaction behind the server's back.
///
/// A caller that sends `COMMIT` through `ExecuteInTransaction` would leave the
/// handle naming a transaction that is no longer open and the pinned
/// connection sitting in autocommit, so the next statement on that handle
/// would land outside any transaction and be kept. Transaction control is the
/// four RPCs, and only those.
fn check_transaction_sql(sql: &str) -> StoreResult<String> {
    let sql = check_sql(sql)?;
    let leading = sql
        .trim_start()
        .split(|c: char| c.is_whitespace() || c == ';' || c == '(')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if matches!(
        leading.as_str(),
        "BEGIN" | "COMMIT" | "END" | "ROLLBACK" | "SAVEPOINT" | "RELEASE"
    ) {
        return Err(StoreError::InvalidArgument(format!(
            "{leading} cannot be run inside an interactive transaction; use CommitTransaction or \
             RollbackTransaction, which also release the connection this transaction is holding"
        )));
    }
    Ok(sql)
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

fn failed_begin(message: String) -> BeginTransactionResponse {
    BeginTransactionResponse {
        success: false,
        transaction_id: String::new(),
        expires_in_ms: 0,
        error: Some(message),
    }
}

/// Errors a caller has to be able to read as text, because the answer is in
/// the message: which ceiling refused it, or what happened to the handle it
/// is still holding. Like a SQL typo these ride inside the response rather
/// than becoming a gRPC status - a tenant that used up its transaction slots
/// is not the store being down.
fn is_caller_visible(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::Sqlite(_)
            | StoreError::InvalidArgument(_)
            | StoreError::NotFound(_)
            | StoreError::Expired(_)
            | StoreError::Exhausted(_)
    )
}

/// The text a caller is given for such an error. A SQLite failure loses this
/// crate's `sqlite:` prefix, matching what the non-transactional handlers
/// have always reported.
fn caller_visible_text(error: &StoreError) -> String {
    match error {
        StoreError::Sqlite(e) => e.to_string(),
        other => other.to_string(),
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

    async fn begin_transaction(
        &self,
        request: Request<BeginTransactionRequest>,
    ) -> Result<Response<BeginTransactionResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let requested = match req.timeout_ms {
            None => None,
            Some(ms) if ms > 0 => Some(Duration::from_millis(ms as u64)),
            Some(ms) => {
                return Ok(Response::new(failed_begin(format!(
                    "timeout_ms must be positive, got {ms}"
                ))));
            }
        };
        match self.open_transaction(&tenant, requested).await {
            Ok(handle) => Ok(Response::new(BeginTransactionResponse {
                success: true,
                transaction_id: handle.id,
                expires_in_ms: handle.expires_in.as_millis() as i64,
                error: None,
            })),
            Err(e) if is_caller_visible(&e) => {
                Ok(Response::new(failed_begin(caller_visible_text(&e))))
            }
            Err(other) => {
                tracing::error!(
                    tenant = %tenant,
                    error = %other,
                    "could not open a transaction; no connection was pinned"
                );
                Err(other.into())
            }
        }
    }

    async fn execute_in_transaction(
        &self,
        request: Request<ExecuteInTransactionRequest>,
    ) -> Result<Response<ExecuteResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        tracing::trace!(
            tenant = %tenant,
            transaction = %req.transaction_id,
            sql = %req.sql,
            "sql execute in transaction"
        );
        let params = match decode_params(req.params.as_deref()) {
            Ok(params) => params,
            Err(e) => return Ok(Response::new(failed_execute(e.to_string()))),
        };
        match self
            .exec_in_transaction(&tenant, &req.transaction_id, &req.sql, &params)
            .await
        {
            Ok(done) => Ok(Response::new(ExecuteResponse {
                success: true,
                rows_affected: done.rows_affected,
                last_insert_id: Some(done.last_insert_id),
                error: None,
            })),
            Err(e) if is_caller_visible(&e) => {
                Ok(Response::new(failed_execute(caller_visible_text(&e))))
            }
            Err(other) => Err(other.into()),
        }
    }

    async fn query_in_transaction(
        &self,
        request: Request<QueryInTransactionRequest>,
    ) -> Result<Response<QueryResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        tracing::trace!(
            tenant = %tenant,
            transaction = %req.transaction_id,
            sql = %req.sql,
            "sql query in transaction"
        );
        let params = match decode_params(req.params.as_deref()) {
            Ok(params) => params,
            Err(e) => return Ok(Response::new(failed_query(e.to_string()))),
        };
        match self
            .fetch_rows_in_transaction(&tenant, &req.transaction_id, &req.sql, &params)
            .await
        {
            Ok(result) => Ok(Response::new(QueryResponse {
                success: true,
                rows: result.rows.to_string(),
                columns: result.columns,
                error: None,
            })),
            Err(e) if is_caller_visible(&e) => {
                Ok(Response::new(failed_query(caller_visible_text(&e))))
            }
            Err(other) => Err(other.into()),
        }
    }

    async fn commit_transaction(
        &self,
        request: Request<CommitTransactionRequest>,
    ) -> Result<Response<CommitTransactionResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        match self.commit(&tenant, &req.transaction_id).await {
            Ok(()) => Ok(Response::new(CommitTransactionResponse {
                success: true,
                error: None,
            })),
            Err(e) if is_caller_visible(&e) => Ok(Response::new(CommitTransactionResponse {
                success: false,
                error: Some(caller_visible_text(&e)),
            })),
            Err(other) => Err(other.into()),
        }
    }

    async fn rollback_transaction(
        &self,
        request: Request<RollbackTransactionRequest>,
    ) -> Result<Response<RollbackTransactionResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        match self.roll_back(&tenant, &req.transaction_id).await {
            Ok(()) => Ok(Response::new(RollbackTransactionResponse {
                success: true,
                error: None,
            })),
            Err(e) if is_caller_visible(&e) => Ok(Response::new(RollbackTransactionResponse {
                success: false,
                error: Some(caller_visible_text(&e)),
            })),
            Err(other) => Err(other.into()),
        }
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

    fn service_with(limits: TransactionLimits) -> (TempDir, DatabaseService) {
        let dir = TempDir::new().unwrap();
        let svc = DatabaseService::open(dir.path(), 4)
            .unwrap()
            .with_transaction_limits(limits);
        (dir, svc)
    }

    /// Fire a transaction's deadline without waiting for it, and wait for
    /// exactly the work the deadline does.
    ///
    /// The clock is frozen only for the jump: the task's own `sleep` is what
    /// is being driven, and leaving time paused afterwards would let the next
    /// transaction's deadline fire the moment the test awaited anything.
    async fn run_deadline(svc: &DatabaseService, handle: &TransactionHandle) {
        let task = svc
            .take_deadline_task(&handle.id)
            .expect("an open transaction has a deadline task");
        tokio::time::pause();
        tokio::time::advance(handle.expires_in + Duration::from_secs(1)).await;
        task.await.expect("the deadline task must not panic");
        tokio::time::resume();
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

    /// The whole reason this exists: read a row, decide from what it says,
    /// write accordingly - all inside one transaction, which `Batch` cannot
    /// do because it needs every statement up front.
    #[tokio::test]
    async fn a_transaction_reads_then_decides_then_writes() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        svc.exec_statement(
            &a,
            "CREATE TABLE seats(id INTEGER PRIMARY KEY, taken INTEGER)",
            &[],
        )
        .await
        .unwrap();
        svc.exec_statement(&a, "INSERT INTO seats(id, taken) VALUES(1, 0)", &[])
            .await
            .unwrap();

        let tx = svc.open_transaction(&a, None).await.unwrap();
        let seen = svc
            .fetch_rows_in_transaction(&a, &tx.id, "SELECT taken FROM seats WHERE id = 1", &[])
            .await
            .unwrap();
        // The branch a batch cannot express: what is written depends on what
        // was read, and the row cannot change underneath in between.
        assert_eq!(seen.rows[0]["taken"], serde_json::json!(0));
        svc.exec_in_transaction(&a, &tx.id, "UPDATE seats SET taken = 1 WHERE id = 1", &[])
            .await
            .unwrap();
        svc.commit(&a, &tx.id).await.unwrap();

        let after = svc
            .fetch_rows(&a, "SELECT taken FROM seats WHERE id = 1", &[])
            .await
            .unwrap();
        assert_eq!(after.rows[0]["taken"], serde_json::json!(1));
    }

    #[tokio::test]
    async fn a_transaction_sees_its_own_writes_and_nobody_else_does_until_it_commits() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let tx = svc.open_transaction(&a, None).await.unwrap();
        svc.exec_in_transaction(&a, &tx.id, "INSERT INTO users(name) VALUES('Alice')", &[])
            .await
            .unwrap();

        let inside = svc
            .fetch_rows_in_transaction(&a, &tx.id, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(inside.rows, serde_json::json!([{ "n": 1 }]));
        let outside = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(
            outside.rows,
            serde_json::json!([{ "n": 0 }]),
            "an uncommitted write must not be visible outside its transaction"
        );

        svc.commit(&a, &tx.id).await.unwrap();
        let after = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(after.rows, serde_json::json!([{ "n": 1 }]));
    }

    #[tokio::test]
    async fn rolling_back_discards_everything_the_transaction_did() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let tx = svc.open_transaction(&a, None).await.unwrap();
        svc.exec_in_transaction(&a, &tx.id, "INSERT INTO users(name) VALUES('Alice')", &[])
            .await
            .unwrap();
        svc.roll_back(&a, &tx.id).await.unwrap();

        let after = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(after.rows, serde_json::json!([{ "n": 0 }]));

        // The spent handle says what happened to it rather than going blank.
        let err = svc
            .exec_in_transaction(&a, &tx.id, "INSERT INTO users(name) VALUES('Bob')", &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already rolled back"), "{err}");
    }

    #[tokio::test]
    async fn a_committed_handle_says_it_was_committed() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let tx = svc.open_transaction(&a, None).await.unwrap();
        svc.commit(&a, &tx.id).await.unwrap();

        let err = svc.commit(&a, &tx.id).await.unwrap_err();
        assert!(err.to_string().contains("already committed"), "{err}");
        let unknown = svc.commit(&a, "no-such-handle").await.unwrap_err();
        assert!(matches!(unknown, StoreError::NotFound(_)), "{unknown}");
    }

    #[tokio::test]
    async fn a_transaction_handle_belongs_to_the_tenant_that_opened_it() {
        let (_dir, svc) = service();
        let one = tenant("app-one.production");
        let two = tenant("app-two.production");
        make_table(&svc, &one).await;

        let tx = svc.open_transaction(&one, None).await.unwrap();
        for attempt in [
            svc.exec_in_transaction(&two, &tx.id, "INSERT INTO users(name) VALUES('x')", &[])
                .await
                .err(),
            svc.fetch_rows_in_transaction(&two, &tx.id, "SELECT 1", &[])
                .await
                .err(),
            svc.commit(&two, &tx.id).await.err(),
            svc.roll_back(&two, &tx.id).await.err(),
        ] {
            let error = attempt.expect("another tenant must not touch this handle");
            assert!(matches!(error, StoreError::Tenant(_)), "{error}");
        }

        // Still the owner's to use, and still holding its own work.
        svc.exec_in_transaction(&one, &tx.id, "INSERT INTO users(name) VALUES('Alice')", &[])
            .await
            .unwrap();
        svc.commit(&one, &tx.id).await.unwrap();

        // A handle that has ended is still not the other tenant's business.
        let error = svc.commit(&two, &tx.id).await.unwrap_err();
        assert!(matches!(error, StoreError::Tenant(_)), "{error}");
    }

    /// A handler that opens a transaction and then awaits something slow is
    /// sitting on its tenant's writer. The deadline is what takes it back.
    #[tokio::test]
    async fn the_deadline_rolls_a_forgotten_transaction_back_and_says_so() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let tx = svc.open_transaction(&a, None).await.unwrap();
        svc.exec_in_transaction(&a, &tx.id, "INSERT INTO users(name) VALUES('Alice')", &[])
            .await
            .unwrap();
        run_deadline(&svc, &tx).await;

        let err = svc
            .exec_in_transaction(&a, &tx.id, "INSERT INTO users(name) VALUES('Bob')", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Expired(_)), "{err}");
        let message = err.to_string();
        assert!(
            message.contains("rolled back") && message.contains("deadline"),
            "the caller has to be told what happened to its work, not that the handle is \
             unknown: {message}"
        );

        let after = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(after.rows, serde_json::json!([{ "n": 0 }]));
    }

    /// The dropped-client case: nothing ever commits or rolls back. The
    /// connection has to come back to the pool clean, without the write lock
    /// the transaction was holding.
    #[tokio::test]
    async fn a_handle_abandoned_without_commit_leaves_no_lock_behind() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let tx = svc.open_transaction(&a, None).await.unwrap();
        // A write, so the transaction really is holding the file's write lock
        // rather than merely occupying a connection.
        svc.exec_in_transaction(&a, &tx.id, "INSERT INTO users(name) VALUES('ghost')", &[])
            .await
            .unwrap();
        run_deadline(&svc, &tx).await;

        assert_eq!(
            svc.transactions.open_count(),
            0,
            "the abandoned transaction must not still be counted as open"
        );

        // More writes than the pool has connections, so every connection in
        // it is exercised. Any one still carrying the abandoned transaction
        // would wait out the busy timeout and then fail here.
        for n in 0..8 {
            svc.exec_statement(
                &a,
                "INSERT INTO users(name) VALUES(?1)",
                &[SqlValue::Text(format!("after-{n}"))],
            )
            .await
            .unwrap();
        }
        let after = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(
            after.rows,
            serde_json::json!([{ "n": 8 }]),
            "the abandoned insert must be gone and every later write must have landed"
        );

        // And the tenant can take the write lock again, which it could not if
        // the abandoned transaction still held it.
        let next = svc.open_transaction(&a, None).await.unwrap();
        svc.exec_in_transaction(&a, &next.id, "DELETE FROM users", &[])
            .await
            .unwrap();
        svc.commit(&a, &next.id).await.unwrap();
    }

    #[tokio::test]
    async fn a_tenant_cannot_open_more_transactions_than_its_ceiling() {
        let (_dir, svc) = service_with(TransactionLimits {
            max_open_per_tenant: 1,
            ..TransactionLimits::default()
        });
        let one = tenant("app-one.production");
        let two = tenant("app-two.production");

        let held = svc.open_transaction(&one, None).await.unwrap();
        let refused = svc.open_transaction(&one, None).await.unwrap_err();
        assert!(matches!(refused, StoreError::Exhausted(_)), "{refused}");
        let message = refused.to_string();
        assert!(
            message.contains("ceiling") && message.contains("tenant"),
            "a refusal has to say why: {message}"
        );

        // The ceiling is per tenant, so another tenant is unaffected.
        let other = svc.open_transaction(&two, None).await.unwrap();
        svc.commit(&two, &other.id).await.unwrap();

        // And the slot comes back when the transaction ends.
        svc.commit(&one, &held.id).await.unwrap();
        let again = svc.open_transaction(&one, None).await.unwrap();
        svc.roll_back(&one, &again.id).await.unwrap();
    }

    #[tokio::test]
    async fn the_server_wide_ceiling_refuses_across_tenants() {
        let (_dir, svc) = service_with(TransactionLimits {
            max_open_total: 1,
            ..TransactionLimits::default()
        });
        let one = tenant("app-one.production");
        let two = tenant("app-two.production");

        let held = svc.open_transaction(&one, None).await.unwrap();
        let refused = svc.open_transaction(&two, None).await.unwrap_err();
        assert!(matches!(refused, StoreError::Exhausted(_)), "{refused}");
        assert!(refused.to_string().contains("server"), "{refused}");

        svc.commit(&one, &held.id).await.unwrap();
        let now_free = svc.open_transaction(&two, None).await.unwrap();
        svc.commit(&two, &now_free.id).await.unwrap();
    }

    #[tokio::test]
    async fn transaction_control_sql_is_refused_inside_a_transaction() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let tx = svc.open_transaction(&a, None).await.unwrap();
        for sql in [
            "COMMIT",
            "commit;",
            "ROLLBACK",
            "BEGIN",
            "END",
            "SAVEPOINT s",
        ] {
            let err = svc
                .exec_in_transaction(&a, &tx.id, sql, &[])
                .await
                .unwrap_err();
            assert!(
                matches!(err, StoreError::InvalidArgument(_)),
                "{sql} must be refused: {err}"
            );
        }
        // The transaction is untouched by the refusals.
        svc.exec_in_transaction(&a, &tx.id, "INSERT INTO users(name) VALUES('Alice')", &[])
            .await
            .unwrap();
        svc.commit(&a, &tx.id).await.unwrap();
        let after = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(after.rows, serde_json::json!([{ "n": 1 }]));
    }

    #[tokio::test]
    async fn a_caller_may_ask_for_less_time_but_never_for_more() {
        let (_dir, svc) = service_with(TransactionLimits {
            deadline: Duration::from_secs(2),
            ..TransactionLimits::default()
        });
        let a = tenant("app-one.production");

        let shorter = svc
            .open_transaction(&a, Some(Duration::from_millis(500)))
            .await
            .unwrap();
        assert_eq!(shorter.expires_in, Duration::from_millis(500));
        svc.commit(&a, &shorter.id).await.unwrap();

        let clamped = svc
            .open_transaction(&a, Some(Duration::from_secs(3600)))
            .await
            .unwrap();
        assert_eq!(
            clamped.expires_in,
            Duration::from_secs(2),
            "the configured ceiling is a ceiling, not a suggestion"
        );
        svc.commit(&a, &clamped.id).await.unwrap();

        let refused = svc
            .open_transaction(&a, Some(Duration::ZERO))
            .await
            .unwrap_err();
        assert!(
            matches!(refused, StoreError::InvalidArgument(_)),
            "{refused}"
        );
    }

    /// The client that disappears while `BeginTransaction` is still waiting.
    /// Nothing was opened, so no deadline task exists to clean up after it,
    /// and the slot it had taken has to come back on its own or the tenant is
    /// permanently one transaction poorer.
    #[tokio::test]
    async fn a_begin_that_never_finishes_gives_its_slot_back() {
        let dir = TempDir::new().unwrap();
        // One connection, so taking it is enough to park any later caller on
        // the pool for as long as the test likes.
        let svc = DatabaseService::open(dir.path(), 1).unwrap();
        let a = tenant("app-one.production");
        let hog = svc.pool(&a).checkout().await.unwrap();

        // A zero timeout polls the call exactly once - long enough to take a
        // slot and park on the pool - and then drops it, which is what tonic
        // does to a handler whose client went away.
        let abandoned = tokio::time::timeout(Duration::ZERO, svc.open_transaction(&a, None)).await;
        assert!(
            abandoned.is_err(),
            "the pool has no connection to give, so the call cannot have finished"
        );
        assert_eq!(
            svc.transactions.open_count(),
            0,
            "the slot taken by a call that never finished has to be given back"
        );

        drop(hog);
        let tx = svc.open_transaction(&a, None).await.unwrap();
        svc.commit(&a, &tx.id).await.unwrap();
    }

    /// `BEGIN IMMEDIATE` is the difference between contention that surfaces
    /// now, at begin, and contention that surfaces at commit as
    /// `SQLITE_BUSY_SNAPSHOT` - after the caller has been told every one of
    /// its statements succeeded. So the write lock has to be held from the
    /// moment the transaction opens, before it has written anything.
    ///
    /// The outsider here opens the file itself with no busy timeout, so it
    /// reports the lock instead of waiting for it. A deferred transaction
    /// would let it straight through.
    #[tokio::test]
    async fn a_transaction_holds_the_write_lock_from_the_moment_it_opens() {
        let (dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;
        let path = dir.path().join("db/app-one.production.sqlite");

        let tx = svc.open_transaction(&a, None).await.unwrap();
        // Not one statement has run inside the transaction yet.
        let outsider = rusqlite::Connection::open(&path).unwrap();
        outsider.busy_timeout(Duration::ZERO).unwrap();
        let blocked = outsider.execute_batch("BEGIN IMMEDIATE");
        assert!(
            blocked.is_err(),
            "the write lock must already be held, or a read-then-write transaction would only \
             find out at commit"
        );

        svc.commit(&a, &tx.id).await.unwrap();
        outsider
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
            .expect("the lock must be released by the commit");
    }

    /// The handlers themselves, not the methods under them: this is what
    /// proves the five RPCs are wired to the service the server registers,
    /// that a dead handle is reported inside the response rather than as a
    /// transport failure, and that the field names the runtime will read are
    /// the ones the server fills in.
    #[tokio::test]
    async fn the_transaction_rpcs_are_wired_to_the_service() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let opened =
            Database::begin_transaction(&svc, tagged(&a, BeginTransactionRequest::default()))
                .await
                .unwrap()
                .into_inner();
        assert!(opened.success, "{:?}", opened.error);
        assert!(
            opened.expires_in_ms > 0,
            "the caller has to be told how long it has"
        );
        let id = opened.transaction_id.clone();

        let wrote = Database::execute_in_transaction(
            &svc,
            tagged(
                &a,
                ExecuteInTransactionRequest {
                    transaction_id: id.clone(),
                    sql: "INSERT INTO users(name) VALUES(?1)".to_string(),
                    params: Some(r#"["Alice"]"#.to_string()),
                },
            ),
        )
        .await
        .unwrap()
        .into_inner();
        assert!(wrote.success, "{:?}", wrote.error);
        assert_eq!(wrote.rows_affected, 1);

        let read = Database::query_in_transaction(
            &svc,
            tagged(
                &a,
                QueryInTransactionRequest {
                    transaction_id: id.clone(),
                    sql: "SELECT name FROM users".to_string(),
                    params: None,
                },
            ),
        )
        .await
        .unwrap()
        .into_inner();
        assert!(read.success, "{:?}", read.error);
        assert!(read.rows.contains("Alice"), "{}", read.rows);

        let committed = Database::commit_transaction(
            &svc,
            tagged(
                &a,
                CommitTransactionRequest {
                    transaction_id: id.clone(),
                },
            ),
        )
        .await
        .unwrap()
        .into_inner();
        assert!(committed.success, "{:?}", committed.error);

        // A spent handle is a response with an error in it, not a broken
        // transport: the store is fine, the caller's handle is not.
        let again = Database::rollback_transaction(
            &svc,
            tagged(&a, RollbackTransactionRequest { transaction_id: id }),
        )
        .await
        .unwrap()
        .into_inner();
        assert!(!again.success);
        assert!(
            again
                .error
                .unwrap_or_default()
                .contains("already committed"),
            "the response has to say what happened to the handle"
        );
    }

    /// A request carrying the tenant header the handlers read.
    fn tagged<T>(tenant: &TenantId, message: T) -> Request<T> {
        let mut request = Request::new(message);
        request.metadata_mut().insert(
            crate::tenant::TENANT_HEADER,
            tenant.as_str().parse().expect("a tenant id is ASCII"),
        );
        request
    }

    #[test]
    fn the_per_tenant_ceiling_leaves_a_connection_for_ordinary_statements() {
        let asked = TransactionLimits {
            max_open_per_tenant: 100,
            ..TransactionLimits::default()
        };
        assert_eq!(clamp_to_pool(asked, 4).max_open_per_tenant, 3);
        assert_eq!(clamp_to_pool(asked, 2).max_open_per_tenant, 1);
        // A one-connection pool has nothing to hold back, so the clamp stops
        // at one rather than at zero, which would refuse every transaction.
        assert_eq!(clamp_to_pool(asked, 1).max_open_per_tenant, 1);
        assert_eq!(
            clamp_to_pool(TransactionLimits::default(), 4).max_open_per_tenant,
            DEFAULT_MAX_OPEN_TRANSACTIONS_PER_TENANT
        );
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
