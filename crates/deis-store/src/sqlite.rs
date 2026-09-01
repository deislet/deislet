//! A small SQLite connection pool that never holds a connection across an
//! await point.
//!
//! Two problems are solved together here. First, `rusqlite::Connection` is a
//! blocking handle; calling it straight from an async task parks a Tokio
//! worker thread for the duration of the query. Second, a connection kept
//! inside an async mutex serialises every caller behind the slowest one and,
//! in WAL mode, throws away the concurrent readers SQLite is happy to give.
//!
//! So: every statement runs inside `spawn_blocking`, and the connection it
//! runs on is checked out of a fixed-size pool for exactly that closure. A
//! semaphore bounds how many blocking threads the pool can occupy, which also
//! bounds how many SQLite handles the process opens.
//!
//! # One exception: a connection can be pinned
//!
//! An interactive transaction cannot live inside one closure - the caller
//! decides what to run next from what it read, and that decision happens in
//! another process. [`SqlitePool::checkout`] therefore hands out a
//! [`PooledConnection`] that stays out of the pool until it is dropped, and
//! whoever holds it runs each statement on it inside its own `spawn_blocking`.
//! The permit is held for the whole time, so a pinned connection is one fewer
//! connection the rest of the tenant's traffic can use; [`crate::database`]
//! bounds how many may be pinned at once and puts a deadline on each.
//!
//! Every release goes through the same guard, which refuses to hand a
//! connection back while a transaction is still open on it: an unfinished
//! `BEGIN` returned to the pool would carry its write lock into whichever
//! unrelated request picked the connection up next.
//!
//! # Pools that run SQL nobody here wrote
//!
//! The KV and blob pools only ever run this crate's own statements. A
//! tenant's SQL database pool runs whatever the application sent, so it is
//! built with [`SqlitePool::new_guarded`]: every connection it opens carries
//! an authorizer callback and has `SQLITE_LIMIT_ATTACHED` set to zero. What
//! that authorizer decides is [`crate::database`]'s business; this module
//! only makes sure it is installed on every connection, that it is installed
//! *after* the pool's own settings, and that a refusal comes back as
//! something the caller can read.
//!
//! # Deciding what a statement may touch is only half of it
//!
//! An authorizer answers *what*. Nothing in it answers *how much*, and a
//! statement that is allowed to run is otherwise allowed to run for as long
//! as it likes on as much memory as it likes: one `WITH RECURSIVE` counting
//! to a hundred million pegs a core for a minute and a half, and one
//! `SELECT randomblob(20000000)` takes the process from 18 MB resident to
//! 1476 MB - on a process that holds every tenant's data. So a guarded
//! connection carries two more settings, and every connection carries a
//! third:
//!
//! * **`SQLITE_LIMIT_LENGTH`** at [`TENANT_MAX_VALUE_BYTES`] and
//!   **`SQLITE_LIMIT_SQL_LENGTH`** at [`TENANT_MAX_SQL_BYTES`], on guarded
//!   connections only - the KV and blob pools store values a caller sent in
//!   one gRPC message and must keep taking them. These are the ceilings
//!   SQLite enforces for itself, inside the allocation, which is the only
//!   place `randomblob(1000000000)` can be stopped: it is one virtual
//!   machine instruction, so nothing that watches a statement *between*
//!   instructions ever gets a turn.
//! * **A cost budget**, consulted between instructions by a progress
//!   callback installed on every connection this module opens. It stops a
//!   statement that has run past its deadline, and one whose caller has gone
//!   away - a `spawn_blocking` closure is not cancelled when the future
//!   awaiting it is dropped, so without this an abandoned query runs to
//!   completion with nobody left to take the answer. See [`within_budget`],
//!   which is also where SQLite's bare "interrupted" is turned back into
//!   which ceiling stopped the statement.
//!
//! The budget belongs to the request rather than to the connection:
//! [`crate::database`] decides the number and passes it in, because only
//! that layer knows whether a statement is a plain query or one inside a
//! transaction with its own, shorter, time left.

use crate::error::{StoreError, StoreResult};
use rusqlite::Connection;
use rusqlite::hooks::{AuthContext, Authorization};
use rusqlite::limits::Limit;
use std::cell::RefCell;
use std::ffi::c_int;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// How long a connection waits for a competing writer before giving up.
/// WAL keeps readers out of the way, so contention here means two writers,
/// which SQLite serialises; five seconds is long enough to ride out a slow
/// checkpoint and short enough that a wedged writer surfaces as an error
/// instead of a hung request.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The largest single string, blob or table row a tenant's SQL may produce
/// or store, in bytes. SQLite's own default is a gigabyte.
///
/// Eight megabytes, derived from the wire rather than from taste. A value a
/// caller wrote arrived in one gRPC message, and a value it reads leaves in
/// one, and that message is capped at
/// [`crate::MAX_GRPC_MESSAGE_BYTES`] - sixteen megabytes. Half of it is the
/// largest single value that still leaves room in the message for the row it
/// sits in and the columns beside it. Anything larger is either unreachable
/// from outside or was built inside SQLite by a function like `randomblob`,
/// where the only thing it can do is cost memory: `SELECT
/// randomblob(20000000)` took the store from 18 MB resident to 1476 MB,
/// because a blob comes back as a JSON array of byte values, and SQLite's
/// default would have allowed fifty times that in one call.
///
/// This also caps the total width of a row, which is what it should do: a
/// row wider than one message can never be read back whole either.
const TENANT_MAX_VALUE_BYTES: i32 = 8 * 1024 * 1024;

/// The longest single statement a tenant may send, in bytes. SQLite's own
/// default is again a gigabyte.
///
/// Sixty-four kilobytes is around fifteen hundred lines of SQL, and it is
/// also roughly where SQLite's own `SQLITE_LIMIT_VARIABLE_NUMBER` of 32766
/// already stops an `INSERT ... VALUES (?),(?),...`: two characters a
/// placeholder puts that statement at 64 KB give or take. So the two
/// ceilings meet, and nothing that fits under the existing one is refused by
/// this one.
///
/// It is here rather than in [`crate::database`] because the memory it
/// bounds is not the statement's own. `Prepare` remembers statement text
/// under a handle, up to `MAX_PREPARED_STATEMENTS` of them at once, and
/// until this limit existed the size of each was bounded only by the gRPC
/// message that carried it - 4096 handles of 16 MB is 64 GB of remembered
/// SQL. With it the same table cannot exceed 256 MB.
const TENANT_MAX_SQL_BYTES: i32 = 64 * 1024;

/// How many SQLite virtual-machine instructions run between two consultations
/// of the cost budget.
///
/// A thousand is what SQLite's own documentation suggests for a progress
/// callback used to keep a UI responsive, and it is the right order here for
/// the same reason: the check is one thread-local read and at most one clock
/// read, which is nothing against a thousand instructions, while a statement
/// still notices its deadline within well under a millisecond of it passing.
const BUDGET_CHECK_EVERY_OPS: c_int = 1000;

/// The budget is still running.
const RUNNING: u8 = 0;
/// The statement ran past the time it was given.
const STOPPED_AT_DEADLINE: u8 = 1;
/// Whoever asked for the statement is no longer waiting for the answer.
const STOPPED_CALLER_GONE: u8 = 2;

/// The policy a guarded pool applies to every statement prepared on it.
///
/// A bare function pointer rather than a closure: the decision is a pure
/// function of one action, and every connection in the pool - including the
/// ones opened long after the pool was created - has to carry the same one.
pub type SqlAuthorizer = fn(AuthContext<'_>) -> Authorization;

thread_local! {
    /// Why the authorizer last refused something on this thread.
    ///
    /// SQLite reports a denied statement as a bare "not authorized", which
    /// tells an application nothing it can act on. The callback runs
    /// synchronously on the thread preparing the statement, and every
    /// statement in this crate runs inside `spawn_blocking`, so the reason
    /// can be left here and collected a few frames up by
    /// [`explaining_refusals`] - which is the only reason this is a
    /// thread-local rather than something threaded through the callback,
    /// which SQLite gives no room for.
    static LAST_REFUSAL: RefCell<Option<String>> = const { RefCell::new(None) };

    /// The ceiling the statement running on this thread is being held to.
    ///
    /// The same shape as [`LAST_REFUSAL`] above and for the same reason:
    /// SQLite's progress callback has no room for an argument Rust can own,
    /// it runs synchronously on the thread executing the statement, and every
    /// statement in this crate runs inside `spawn_blocking` - one statement
    /// to a thread at a time. So [`within_budget`] leaves the budget here and
    /// [`budget_is_spent`] reads it back from inside the callback.
    static BUDGET: RefCell<Option<ActiveBudget>> = const { RefCell::new(None) };
}

/// What the statement running on this thread may spend, while it spends it.
struct ActiveBudget {
    /// The instant past which the statement must be stopped, whatever it is
    /// in the middle of.
    until: Instant,
    /// Set by the async half when its future is dropped. That is the only
    /// signal a `spawn_blocking` closure gets that nobody is waiting for its
    /// answer: dropping the future awaiting a blocking task does not cancel
    /// the task.
    abandoned: Arc<AtomicBool>,
    /// Which ceiling stopped the statement, written by the callback and read
    /// back after it returns - SQLite reports both as a bare "interrupted".
    stopped: Arc<AtomicU8>,
}

/// SQLite's progress callback: stop the statement, or let it carry on.
///
/// Installed on every connection this module opens, so no call site has to
/// remember it exists; on a connection running a statement with no budget it
/// reads one empty thread-local and returns.
///
/// `try_with` rather than `with`: a connection can be closed while its
/// thread's locals are already being destroyed, and a panic raised from
/// inside a C callback is worth avoiding for a check whose answer would be
/// "no budget" anyway.
fn budget_is_spent() -> bool {
    BUDGET
        .try_with(|slot| {
            let borrowed = slot.borrow();
            let Some(budget) = borrowed.as_ref() else {
                return false;
            };
            if budget.abandoned.load(Ordering::Relaxed) {
                budget.stopped.store(STOPPED_CALLER_GONE, Ordering::Relaxed);
                return true;
            }
            if Instant::now() >= budget.until {
                budget.stopped.store(STOPPED_AT_DEADLINE, Ordering::Relaxed);
                return true;
            }
            false
        })
        .unwrap_or(false)
}

/// Clears the thread's budget however the statement ended, panic included.
///
/// Without this a statement that panicked would leave an expired budget on a
/// blocking thread that the pool hands to unrelated requests, and every one
/// of them would be stopped the moment it started.
struct BudgetScope;

impl Drop for BudgetScope {
    fn drop(&mut self) {
        let _ = BUDGET.try_with(|slot| *slot.borrow_mut() = None);
    }
}

/// The async half of a budget: flips the flag when the caller goes away.
///
/// Held in the frame of the `async fn` that is awaiting the blocking task, so
/// it is dropped both when that future completes and when it is cancelled -
/// a tonic handler future is dropped when the client resets the stream. On
/// the completing path the closure has already finished and the flag is read
/// by nobody, which is why this does not need to know which happened.
pub struct CallerPresence(Arc<AtomicBool>);

impl Drop for CallerPresence {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl CallerPresence {
    /// A fresh flag and the guard that sets it.
    pub fn new() -> (Self, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        (Self(Arc::clone(&flag)), flag)
    }
}

/// Whether SQLite stopped this statement because the progress callback told
/// it to. Any caller that has to decide something before
/// [`within_budget`] gets its turn needs this - see the batch path in
/// [`crate::database`].
pub fn was_interrupted(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::OperationInterrupted
    )
}

/// The sentence for a statement or a value that crossed one of the two size
/// ceilings a guarded connection carries, or `None` for anything else.
///
/// SQLite reports both as `SQLITE_TOOBIG` with a phrase that says which
/// ("statement too long", "string or blob too big") but no number and no
/// suggestion, and the numbers are this module's, not SQLite's - a caller
/// reading "string or blob too big" has no way to find out how big is too
/// big.
pub fn size_limit_message(error: &rusqlite::Error) -> Option<String> {
    let rusqlite::Error::SqliteFailure(e, said) = error else {
        return None;
    };
    if e.code != rusqlite::ErrorCode::TooBig {
        return None;
    }
    Some(format!(
        "{}: this store holds a tenant's SQL to {TENANT_MAX_VALUE_BYTES} bytes for any single \
         string, blob or row, and {TENANT_MAX_SQL_BYTES} bytes for any single statement. Keep \
         payloads that size in the blob service, which stores whole objects, rather than in a \
         column",
        said.as_deref().unwrap_or("too big")
    ))
}

/// Run `f` and turn a crossed size ceiling into a sentence naming it.
///
/// The same shape as [`explaining_refusals`], and applied in the same places
/// and for the same reason: SQLite's own report names neither the number nor
/// what to do instead.
pub fn explaining_size_limits<T>(f: impl FnOnce() -> StoreResult<T>) -> StoreResult<T> {
    match f() {
        Err(StoreError::Sqlite(e)) => match size_limit_message(&e) {
            Some(message) => Err(StoreError::TooLarge(message)),
            None => Err(StoreError::Sqlite(e)),
        },
        other => other,
    }
}

/// One request's worth of tenant SQL, held to `granted`, with every way
/// SQLite has of saying no turned into a sentence an application can act on.
///
/// The one place the three translations are composed, so a call site cannot
/// pick up two of them and miss the third. The budget goes outermost because
/// it is the only one that has to see the error whatever the other two made
/// of it: a statement that was stopped must say which ceiling stopped it even
/// if an inner layer had already given the failure a name of its own.
pub fn tenant_statement<T>(
    granted: Duration,
    abandoned: Arc<AtomicBool>,
    f: impl FnOnce() -> StoreResult<T>,
) -> StoreResult<T> {
    within_budget(granted, abandoned, || {
        explaining_size_limits(|| explaining_refusals(f))
    })
}

/// Run `f` under a ceiling on how long it may take, and turn SQLite's bare
/// "interrupted" back into which ceiling stopped it.
///
/// The ceiling is enforced from inside SQLite, by the progress callback every
/// connection here carries; this function only says what the callback should
/// hold this statement to. Two things make that the mechanism rather than
/// `sqlite3_interrupt` from a timer task. An interrupt raced against the end
/// of one statement lands on whichever statement starts next on that
/// connection, and there is no way to join a timer task from inside the
/// blocking closure to rule that out. And a callback needs no second task per
/// statement at all.
///
/// `f` is everything the request does on the connection, not one statement:
/// what a batch is holding while it runs its twentieth `INSERT` is the same
/// connection and the same blocking thread it held for its first.
pub fn within_budget<T>(
    granted: Duration,
    abandoned: Arc<AtomicBool>,
    f: impl FnOnce() -> StoreResult<T>,
) -> StoreResult<T> {
    let stopped = Arc::new(AtomicU8::new(RUNNING));
    BUDGET.with(|slot| {
        *slot.borrow_mut() = Some(ActiveBudget {
            until: Instant::now() + granted,
            abandoned,
            stopped: Arc::clone(&stopped),
        });
    });
    let scope = BudgetScope;
    let outcome = f();
    drop(scope);

    // Only an error is rewritten. A statement stopped one instruction before
    // it would have finished anyway is a statement that finished, and the
    // rows it produced are as good as any others.
    match (outcome, stopped.load(Ordering::Relaxed)) {
        (Err(_), STOPPED_AT_DEADLINE) => Err(StoreError::Expired(format!(
            "this statement ran longer than the {} ms the store gives one request and was \
             stopped; nothing it had written was kept. Narrow it - add a WHERE clause, an index \
             or a LIMIT - or split it into several requests",
            granted.as_millis()
        ))),
        // Nobody is left to read this, which is exactly what it says. It is
        // still an error rather than a success so that no caller further up
        // mistakes a half-built answer for a whole one, and so the layer that
        // knows the tenant can log it.
        (Err(_), STOPPED_CALLER_GONE) => Err(StoreError::Expired(
            "this statement was stopped because whoever asked for it stopped waiting; nothing it \
             had written was kept"
                .to_string(),
        )),
        (outcome, _) => outcome,
    }
}

/// Refuse the action, and leave behind the sentence the caller should see.
pub fn refuse(reason: String) -> Authorization {
    LAST_REFUSAL.with(|slot| *slot.borrow_mut() = Some(reason));
    Authorization::Deny
}

/// Take what the authorizer refused on this thread, if anything.
///
/// For the one caller that has to build its own message - a batch reports
/// which statement failed rather than failing outright.
pub fn take_refusal() -> Option<String> {
    LAST_REFUSAL.with(|slot| slot.borrow_mut().take())
}

/// Run `f`, and turn SQLite's "not authorized" into what was actually
/// refused.
///
/// Applied to every pooled statement, guarded pool or not: a connection with
/// no authorizer never fills the slot, so wrapping the KV and blob pools too
/// costs one thread-local clear and means no future call site has to
/// remember this exists.
pub fn explaining_refusals<T>(f: impl FnOnce() -> StoreResult<T>) -> StoreResult<T> {
    // Cleared first: this thread is reused for unrelated statements, and a
    // reason left over from an earlier one would be attached to this one.
    take_refusal();
    let outcome = f();
    match (outcome, take_refusal()) {
        (Err(StoreError::Sqlite(_)), Some(reason)) => Err(StoreError::Refused(reason)),
        (outcome, _) => outcome,
    }
}

/// Connections handed out concurrently, per pool.
#[derive(Debug, Clone, Copy)]
pub struct PoolSize(usize);

impl PoolSize {
    /// Clamp to at least one connection; a zero-sized pool would deadlock
    /// every caller forever.
    pub fn new(n: usize) -> Self {
        Self(n.max(1))
    }
}

struct Inner {
    path: PathBuf,
    /// What this pool is for, used only in logs (`kv`, `blob`, `db:<tenant>`).
    label: String,
    /// Installed on every connection this pool opens, when the SQL that will
    /// run here is not this crate's own. `None` for the KV and blob pools.
    authorizer: Option<SqlAuthorizer>,
    /// Connections not currently checked out. Guarded by a std mutex that is
    /// only ever locked inside a blocking task, never across an await.
    idle: Mutex<Vec<Connection>>,
    permits: Arc<Semaphore>,
}

/// A handle to one SQLite file. Cloning shares the same pool.
#[derive(Clone)]
pub struct SqlitePool {
    inner: Arc<Inner>,
}

impl SqlitePool {
    /// Create a pool over `path`. No connection is opened yet: the first
    /// caller pays for it, so a per-tenant database costs nothing until that
    /// tenant actually stores something.
    pub fn new(path: impl Into<PathBuf>, label: impl Into<String>, size: PoolSize) -> Self {
        Self::build(path, label, size, None)
    }

    /// Create a pool whose connections run SQL this crate did not write.
    ///
    /// `authorizer` is consulted for every statement prepared on every
    /// connection, and `SQLITE_LIMIT_ATTACHED` is pinned to zero on each one,
    /// so an `ATTACH` cannot succeed even if that policy is ever loosened by
    /// mistake. Only [`crate::database`] uses this.
    pub fn new_guarded(
        path: impl Into<PathBuf>,
        label: impl Into<String>,
        size: PoolSize,
        authorizer: SqlAuthorizer,
    ) -> Self {
        Self::build(path, label, size, Some(authorizer))
    }

    fn build(
        path: impl Into<PathBuf>,
        label: impl Into<String>,
        size: PoolSize,
        authorizer: Option<SqlAuthorizer>,
    ) -> Self {
        let path = path.into();
        let label = label.into();
        tracing::debug!(
            pool = %label,
            path = %path.display(),
            connections = size.0,
            guarded = authorizer.is_some(),
            "sqlite pool created, connections open lazily"
        );
        Self {
            inner: Arc::new(Inner {
                path,
                label,
                authorizer,
                idle: Mutex::new(Vec::new()),
                permits: Arc::new(Semaphore::new(size.0)),
            }),
        }
    }

    /// The file this pool talks to.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Run `f` on a pooled connection, on a blocking thread.
    ///
    /// The closure owns the connection for its whole body, so it can open a
    /// transaction; the connection goes back to the pool afterwards whether
    /// the closure succeeded or not (a rolled-back transaction leaves the
    /// handle perfectly usable, and one left open is rolled back by the
    /// guard before anybody else sees it).
    pub async fn with_conn<F, T>(&self, f: F) -> StoreResult<T>
    where
        F: FnOnce(&mut Connection) -> StoreResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = self.inner.clone();
        let permit = self.permit().await?;
        tokio::task::spawn_blocking(move || {
            let mut conn = PooledConnection::take(inner, permit)?;
            // The guard is dropped here, on the blocking thread, because
            // releasing a connection may have to talk to SQLite.
            explaining_refusals(|| f(&mut conn))
        })
        .await?
    }

    /// The same as [`Self::with_conn`], under a ceiling on what the request
    /// may spend.
    ///
    /// `granted` is the wall clock the whole closure gets, not one statement
    /// inside it: what a request holds is one connection and one blocking
    /// thread, and it holds them from its first statement to its last.
    ///
    /// The closure is also stopped early if the caller goes away. That is not
    /// free anywhere else: `spawn_blocking` cannot be cancelled, so dropping
    /// the future returned here leaves the closure running to completion with
    /// nobody to give the answer to, which is how one abandoned query used to
    /// keep a core busy for a minute and a half.
    pub async fn with_conn_within<F, T>(&self, granted: Duration, f: F) -> StoreResult<T>
    where
        F: FnOnce(&mut Connection) -> StoreResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = self.inner.clone();
        let permit = self.permit().await?;
        let (presence, abandoned) = CallerPresence::new();
        let task = tokio::task::spawn_blocking(move || {
            let mut conn = PooledConnection::take(inner, permit)?;
            tenant_statement(granted, abandoned, || f(&mut conn))
        });
        // Dropped after the await either way, which is the point: if this
        // future is cancelled instead of completing, the drop is what tells
        // the closure that nobody is waiting for it.
        let outcome = task.await;
        drop(presence);
        outcome?
    }

    /// Take a connection out of the pool and keep it until the guard drops.
    ///
    /// For work that spans several requests - today, exactly one thing: an
    /// interactive transaction, which has to run `BEGIN`, then whatever the
    /// caller decides, then `COMMIT`, all on the same connection. The pool
    /// permit is held for the whole time, so the caller is responsible for
    /// bounding how long that is and how many it does at once.
    pub async fn checkout(&self) -> StoreResult<PooledConnection> {
        let inner = self.inner.clone();
        let permit = self.permit().await?;
        tokio::task::spawn_blocking(move || PooledConnection::take(inner, permit)).await?
    }

    /// Wait for the pool to admit one more connection.
    async fn permit(&self) -> StoreResult<OwnedSemaphorePermit> {
        self.inner
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| StoreError::BlockingTask("sqlite pool was closed".to_string()))
    }

    /// Run `f` once at startup, typically to create tables. Separate from
    /// [`Self::with_conn`] only so the log line says what failed.
    pub async fn migrate<F>(&self, f: F) -> StoreResult<()>
    where
        F: FnOnce(&mut Connection) -> StoreResult<()> + Send + 'static,
    {
        let label = self.inner.label.clone();
        let path = self.inner.path.clone();
        self.with_conn(f).await.inspect_err(|e| {
            tracing::error!(
                pool = %label,
                path = %path.display(),
                error = %e,
                "schema setup failed; this store cannot serve any request"
            );
        })
    }
}

/// A connection checked out of a pool, returned to it when this is dropped.
///
/// Deref gives the `rusqlite::Connection` itself, so a holder runs statements
/// on it exactly as `with_conn`'s closure does.
///
/// Drop is where the pool's one invariant is enforced: a connection only goes
/// back if no transaction is open on it. Dropping this guard can therefore
/// talk to SQLite, which is why every release in this crate happens on a
/// blocking thread.
pub struct PooledConnection {
    inner: Arc<Inner>,
    /// `None` only during drop, so the connection can be moved out of a
    /// `&mut self`.
    conn: Option<Connection>,
    /// Held until the connection is back in the pool, so the pool never
    /// admits more callers than it has connections for.
    permit: Option<OwnedSemaphorePermit>,
}

impl PooledConnection {
    /// Pop an idle connection or open a new one. Blocking: it may open a
    /// file, so it is only ever called from inside `spawn_blocking`.
    fn take(inner: Arc<Inner>, permit: OwnedSemaphorePermit) -> StoreResult<Self> {
        let idle = match inner.idle.lock() {
            Ok(mut idle) => idle.pop(),
            Err(poisoned) => {
                // A previous holder panicked while the lock was held. The
                // list itself is still readable, so recover it rather than
                // failing every later request on this pool.
                tracing::warn!(
                    pool = %inner.label,
                    "sqlite idle list mutex was poisoned by an earlier panic, recovering it; \
                     no data is affected"
                );
                poisoned.into_inner().pop()
            }
        };
        let conn = match idle {
            Some(conn) => conn,
            None => open(&inner.path, &inner.label, inner.authorizer)?,
        };
        Ok(Self {
            inner,
            conn: Some(conn),
            permit: Some(permit),
        })
    }
}

impl std::ops::Deref for PooledConnection {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("connection is only taken on drop")
    }
}

impl std::ops::DerefMut for PooledConnection {
    fn deref_mut(&mut self) -> &mut Connection {
        self.conn
            .as_mut()
            .expect("connection is only taken on drop")
    }
}

impl Drop for PooledConnection {
    fn drop(&mut self) {
        let Some(conn) = self.conn.take() else { return };

        // An open transaction rides on the connection, not on the request
        // that opened it: handing this back mid-`BEGIN` would give the next
        // unrelated statement someone else's write lock and someone else's
        // uncommitted rows. Roll it back, or throw the connection away.
        if !conn.is_autocommit() {
            match conn.execute_batch("ROLLBACK") {
                Ok(()) => tracing::warn!(
                    pool = %self.inner.label,
                    "a connection was released with a transaction still open on it; it has been \
                     rolled back before the connection goes back into the pool, so whatever that \
                     transaction had written is gone"
                ),
                Err(e) => {
                    tracing::error!(
                        pool = %self.inner.label,
                        error = %e,
                        "a connection was released inside a transaction that will not roll back; \
                         it is being closed instead of pooled so its write lock cannot outlive \
                         the request, and the pool will open a replacement on demand"
                    );
                    // Closing the connection is what actually releases the
                    // lock, and dropping `conn` here is that close.
                    drop(self.permit.take());
                    return;
                }
            }
        }

        match self.inner.idle.lock() {
            Ok(mut idle) => idle.push(conn),
            Err(poisoned) => poisoned.into_inner().push(conn),
        }
        // Released last, so the connection is back in the pool before
        // another caller is admitted.
        drop(self.permit.take());
    }
}

/// Open one connection with the settings every Deislet SQLite file uses,
/// plus the authorizer if this pool has one.
fn open(path: &Path, label: &str, authorizer: Option<SqlAuthorizer>) -> StoreResult<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;

    // WAL is what makes a pool worth having: readers do not block the writer
    // and the writer does not block readers.
    let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(
            pool = %label,
            path = %path.display(),
            mode = %mode,
            "sqlite refused WAL and stayed in this journal mode; readers and the writer will now \
             block each other and request latency will spike under load"
        );
    }
    // NORMAL trades an fsync per commit for an fsync per checkpoint. With WAL
    // that risks losing the last commits on a machine crash, not corruption,
    // which is the right trade for this workload.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(BUSY_TIMEOUT)?;

    // On every connection, guarded or not, for the same reason
    // `explaining_refusals` wraps every pooled statement: a connection whose
    // statement carries no budget reads one empty thread-local and carries
    // on, and no call site added later has to remember this exists. The
    // result is not discardable - a connection that silently kept no budget
    // would run a tenant's `WITH RECURSIVE` until it finished.
    conn.progress_handler(BUDGET_CHECK_EVERY_OPS, Some(budget_is_spent))?;

    // Last, and deliberately so: the settings above are the pool's own and
    // several of them are PRAGMAs the authorizer refuses a tenant. Installing
    // it here means the connection arrives configured and then locked down,
    // rather than having to carve an exemption for this function.
    if let Some(authorizer) = authorizer {
        // Zero attached databases. The authorizer already refuses `ATTACH`;
        // this is the floor under it, so a loosened decision still cannot
        // open a second file - and it catches `VACUUM`, which attaches its
        // destination without the parser ever spelling the word.
        conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
        // The two ceilings the progress callback above cannot reach. It runs
        // between virtual-machine instructions; `randomblob(1000000000)` is
        // one instruction, and the memory is gone before the next one comes
        // round. Only SQLite can refuse those, and only if it is told to.
        conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, TENANT_MAX_VALUE_BYTES)?;
        conn.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, TENANT_MAX_SQL_BYTES)?;
        // The result is not discardable. This call is the tenant SQL sandbox:
        // if it fails and the connection is handed out anyway, every statement
        // the authorizer exists to refuse - ATTACH to another tenant's file,
        // VACUUM INTO an arbitrary path, a PRAGMA off the allowlist - runs
        // unopposed on a connection that looks guarded. rusqlite 0.40 started
        // reporting the failure; before that it could only be silent.
        conn.authorizer(Some(authorizer))?;
        tracing::debug!(
            pool = %label,
            path = %path.display(),
            max_value_bytes = TENANT_MAX_VALUE_BYTES,
            max_sql_bytes = TENANT_MAX_SQL_BYTES,
            "opened a guarded sqlite connection: authorizer installed, no database may be \
             attached, and one value or one statement is bounded"
        );
        return Ok(conn);
    }

    tracing::debug!(pool = %label, path = %path.display(), "opened sqlite connection");
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn pool(dir: &TempDir) -> SqlitePool {
        SqlitePool::new(dir.path().join("t.sqlite"), "test", PoolSize::new(4))
    }

    #[tokio::test]
    async fn runs_statements_and_reuses_connections() {
        let dir = TempDir::new().unwrap();
        let pool = pool(&dir);
        pool.migrate(|c| {
            c.execute_batch("CREATE TABLE t(a INTEGER)")?;
            Ok(())
        })
        .await
        .unwrap();

        for i in 0..10 {
            pool.with_conn(move |c| {
                c.execute("INSERT INTO t(a) VALUES(?1)", [i])?;
                Ok(())
            })
            .await
            .unwrap();
        }

        let n: i64 = pool
            .with_conn(|c| Ok(c.query_row("SELECT count(*) FROM t", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(n, 10);
        // Ten sequential calls must not have opened ten connections.
        assert!(pool.inner.idle.lock().unwrap().len() <= 4);
    }

    #[tokio::test]
    async fn wal_is_enabled() {
        let dir = TempDir::new().unwrap();
        let pool = pool(&dir);
        let mode: String = pool
            .with_conn(|c| Ok(c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[tokio::test]
    async fn a_failing_statement_returns_the_connection() {
        let dir = TempDir::new().unwrap();
        let pool = pool(&dir);
        let err = pool
            .with_conn(|c| {
                c.execute("SELECT * FROM missing", [])?;
                Ok(())
            })
            .await;
        assert!(err.is_err());
        // The pool is still usable afterwards.
        let one: i64 = pool
            .with_conn(|c| Ok(c.query_row("SELECT 1", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(one, 1);
    }

    #[tokio::test]
    async fn concurrent_callers_share_the_pool() {
        let dir = TempDir::new().unwrap();
        let pool = pool(&dir);
        pool.migrate(|c| {
            c.execute_batch("CREATE TABLE t(a INTEGER)")?;
            Ok(())
        })
        .await
        .unwrap();

        let mut tasks = Vec::new();
        for i in 0..32 {
            let p = pool.clone();
            tasks.push(tokio::spawn(async move {
                p.with_conn(move |c| {
                    c.execute("INSERT INTO t(a) VALUES(?1)", [i])?;
                    Ok(())
                })
                .await
            }));
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }

        let n: i64 = pool
            .with_conn(|c| Ok(c.query_row("SELECT count(*) FROM t", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(n, 32);
    }

    #[test]
    fn pool_size_is_never_zero() {
        assert_eq!(PoolSize::new(0).0, 1);
    }

    /// A checked-out connection is gone from the pool until the guard drops,
    /// and comes back when it does.
    #[tokio::test]
    async fn a_checkout_pins_one_connection_and_gives_it_back() {
        let dir = TempDir::new().unwrap();
        let pool = pool(&dir);
        {
            let pinned = pool.checkout().await.unwrap();
            pinned.execute_batch("CREATE TABLE t(a INTEGER)").unwrap();
            assert!(
                pool.inner.idle.lock().unwrap().is_empty(),
                "a pinned connection must not also be sitting in the idle list"
            );
            // The pool still serves everyone else while it is pinned.
            pool.with_conn(|c| {
                c.execute("INSERT INTO t(a) VALUES(1)", [])?;
                Ok(())
            })
            .await
            .unwrap();
        }
        assert_eq!(pool.inner.idle.lock().unwrap().len(), 2);
    }

    /// The SQL a cost test needs: an ordinary, allowed statement with no
    /// answer worth having, that counts far enough that nothing but a
    /// ceiling will end it. This is the shape that ran 95.7 seconds on one
    /// core against a store with no ceiling at all.
    const COUNTS_FOREVER: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c \
                                  WHERE x < 2000000000) SELECT count(*) FROM c";

    /// A statement that runs past its budget has to be stopped by something,
    /// and the caller has to be told which something.
    #[tokio::test]
    async fn a_statement_past_its_deadline_is_stopped_and_says_which_ceiling_it_crossed() {
        let dir = TempDir::new().unwrap();
        let pool = pool(&dir);
        let started = Instant::now();
        let err = pool
            .with_conn_within(Duration::from_millis(300), |c| {
                Ok(c.query_row(COUNTS_FOREVER, [], |r| r.get::<_, i64>(0))?)
            })
            .await
            .expect_err("a statement with no end must not be allowed to run to one");
        assert!(matches!(err, StoreError::Expired(_)), "{err}");
        let message = err.to_string();
        assert!(
            message.contains("300 ms") && message.contains("LIMIT"),
            "a refusal has to name the number and what to change: {message}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it took {:?}, so nothing stopped it near its deadline",
            started.elapsed()
        );
    }

    /// A statement that finishes inside its budget is untouched, including
    /// one that returns nothing.
    #[tokio::test]
    async fn a_statement_inside_its_budget_is_left_alone() {
        let dir = TempDir::new().unwrap();
        let pool = pool(&dir);
        let n: i64 = pool
            .with_conn_within(Duration::from_secs(5), |c| {
                Ok(c.query_row(
                    "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 1000) \
                     SELECT count(*) FROM c",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(n, 1000);
    }

    /// `spawn_blocking` cannot be cancelled, so dropping the future awaiting
    /// one leaves the closure running with nobody to give the answer to. The
    /// pool permit is the observable: a pool of one admits nobody else until
    /// the closure returns, so a second caller getting served is proof the
    /// first statement actually stopped.
    #[tokio::test]
    async fn an_abandoned_statement_gives_its_connection_back_instead_of_finishing() {
        let dir = TempDir::new().unwrap();
        let pool = SqlitePool::new(dir.path().join("t.sqlite"), "test", PoolSize::new(1));
        // A minute of budget, so the deadline is not what ends this one.
        let running = pool.with_conn_within(Duration::from_secs(60), |c| {
            Ok(c.query_row(COUNTS_FOREVER, [], |r| r.get::<_, i64>(0))?)
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(200), running)
                .await
                .is_err(),
            "the statement was supposed to still be running when its caller left"
        );
        let one: i64 = tokio::time::timeout(
            Duration::from_secs(5),
            pool.with_conn(|c| Ok(c.query_row("SELECT 1", [], |r| r.get(0))?)),
        )
        .await
        .expect("the abandoned statement never gave its connection back")
        .unwrap();
        assert_eq!(one, 1);
    }

    /// The two ceilings only SQLite can enforce, and the pools that must not
    /// carry them: KV and blob store whatever a caller sent in one gRPC
    /// message, which is twice the value ceiling a tenant's SQL gets.
    #[tokio::test]
    async fn a_guarded_connection_bounds_one_value_and_one_statement() {
        let dir = TempDir::new().unwrap();
        let guarded = SqlitePool::new_guarded(
            dir.path().join("g.sqlite"),
            "guarded",
            PoolSize::new(2),
            |_| Authorization::Allow,
        );
        let (value, sql) = guarded
            .with_conn(|c| {
                Ok((
                    c.limit(Limit::SQLITE_LIMIT_LENGTH)?,
                    c.limit(Limit::SQLITE_LIMIT_SQL_LENGTH)?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(value, TENANT_MAX_VALUE_BYTES);
        assert_eq!(sql, TENANT_MAX_SQL_BYTES);

        let plain = pool(&dir);
        let (value, sql) = plain
            .with_conn(|c| {
                Ok((
                    c.limit(Limit::SQLITE_LIMIT_LENGTH)?,
                    c.limit(Limit::SQLITE_LIMIT_SQL_LENGTH)?,
                ))
            })
            .await
            .unwrap();
        assert!(
            value > TENANT_MAX_VALUE_BYTES && sql > TENANT_MAX_SQL_BYTES,
            "an unguarded pool must keep SQLite's own ceilings; it got {value} and {sql}"
        );
    }

    /// The case that would poison the pool: a holder that left `BEGIN`
    /// hanging. The connection must not go back carrying the write lock, or
    /// the next unrelated statement inherits it.
    #[tokio::test]
    async fn a_connection_released_mid_transaction_is_rolled_back_first() {
        let dir = TempDir::new().unwrap();
        let pool = pool(&dir);
        pool.migrate(|c| {
            c.execute_batch("CREATE TABLE t(a INTEGER)")?;
            Ok(())
        })
        .await
        .unwrap();

        // Abandon a write transaction: no commit, no rollback, guard dropped.
        pool.with_conn(|c| {
            c.execute_batch("BEGIN IMMEDIATE")?;
            c.execute("INSERT INTO t(a) VALUES(1)", [])?;
            Ok(())
        })
        .await
        .unwrap();

        // The row is gone and the file takes writes again, on the same
        // connection the abandoned transaction had been using.
        let n: i64 = pool
            .with_conn(|c| Ok(c.query_row("SELECT count(*) FROM t", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(n, 0, "the abandoned transaction must have been rolled back");
        pool.with_conn(|c| {
            assert!(c.is_autocommit(), "a pooled connection must arrive clean");
            c.execute("INSERT INTO t(a) VALUES(2)", [])?;
            Ok(())
        })
        .await
        .unwrap();
    }
}
