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

use crate::error::{StoreError, StoreResult};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

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
    /// What this pool is for, used only in logs (`kv`, `blob`, `db:<app>`).
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
    /// caller pays for it, so a per-app database costs nothing until the app
    /// actually stores something.
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
    /// handle perfectly usable).
    pub async fn with_conn<F, T>(&self, f: F) -> StoreResult<T>
    where
        F: FnOnce(&mut Connection) -> StoreResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = self.inner.clone();
        let permit = inner
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| StoreError::Worker("sqlite pool was closed".to_string()))?;

        tokio::task::spawn_blocking(move || {
            let mut conn = match inner.idle.lock() {
                Ok(mut idle) => idle.pop(),
                Err(poisoned) => {
                    // A previous holder panicked while the lock was held. The
                    // list itself is still readable, so recover it rather
                    // than failing every later request on this pool.
                    tracing::warn!(
                        pool = %inner.label,
                        "sqlite idle list mutex was poisoned by an earlier panic, recovering it; \
                         no data is affected"
                    );
                    poisoned.into_inner().pop()
                }
            };
            if conn.is_none() {
                conn = Some(open(&inner.path, &inner.label)?);
            }
            let mut conn = conn.expect("connection was just opened");

            let result = f(&mut conn);

            match inner.idle.lock() {
                Ok(mut idle) => idle.push(conn),
                Err(poisoned) => poisoned.into_inner().push(conn),
            }
            // The permit lives until here so the connection is back in the
            // pool before another caller is admitted.
            drop(permit);
            result
        })
        .await?
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
}
