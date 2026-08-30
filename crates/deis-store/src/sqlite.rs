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

use crate::error::{StoreError, StoreResult};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// How long a connection waits for a competing writer before giving up.
/// WAL keeps readers out of the way, so contention here means two writers,
/// which SQLite serialises; five seconds is long enough to ride out a slow
/// checkpoint and short enough that a wedged writer surfaces as an error
/// instead of a hung request.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

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
        let path = path.into();
        let label = label.into();
        tracing::debug!(
            pool = %label,
            path = %path.display(),
            connections = size.0,
            "sqlite pool created, connections open lazily"
        );
        Self {
            inner: Arc::new(Inner {
                path,
                label,
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
            f(&mut conn)
        })
        .await?
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
            None => open(&inner.path, &inner.label)?,
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

/// Open one connection with the settings every Deislet SQLite file uses.
fn open(path: &Path, label: &str) -> StoreResult<Connection> {
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
