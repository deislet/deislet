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
//! This is the same pool deis-store runs, kept here rather than shared
//! because neither service may depend on the other. The two copies must not
//! drift; a shared crate is the right home once a third service needs one.

use crate::error::{QueueError, QueueResult};
use rusqlite::{Connection, Transaction, TransactionBehavior};
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
    /// What this pool is for, used only in logs.
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
    /// caller pays for it.
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
    pub async fn with_conn<F, T>(&self, f: F) -> QueueResult<T>
    where
        F: FnOnce(&mut Connection) -> QueueResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = self.inner.clone();
        let permit = inner
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| QueueError::BlockingTask("sqlite pool was closed".to_string()))?;

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
                         no queued message is affected"
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

    /// Run `f` inside one `IMMEDIATE` transaction, committing when it returns
    /// `Ok` and rolling back otherwise.
    ///
    /// `IMMEDIATE` and not the rusqlite default (`DEFERRED`) because every
    /// transaction in this crate reads rows and then writes them. A
    /// transaction that starts as a reader and asks to become a writer later
    /// is refused outright the moment another connection has committed in
    /// between - SQLite reports `SQLITE_BUSY_SNAPSHOT` and `busy_timeout`
    /// cannot wait that out, because the upgrade can never succeed on that
    /// snapshot. deis-store hit exactly this. Taking the write lock up front
    /// makes two consumers pulling at the same time wait for each other
    /// instead of one of them failing.
    pub async fn with_tx<F, T>(&self, f: F) -> QueueResult<T>
    where
        F: FnOnce(&Transaction<'_>) -> QueueResult<T> + Send + 'static,
        T: Send + 'static,
    {
        self.with_conn(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let value = f(&tx)?;
            tx.commit()?;
            Ok(value)
        })
        .await
    }

    /// Run `f` once at startup, typically to create tables. Separate from
    /// [`Self::with_conn`] only so the log line says what failed.
    pub async fn migrate<F>(&self, f: F) -> QueueResult<()>
    where
        F: FnOnce(&mut Connection) -> QueueResult<()> + Send + 'static,
    {
        let label = self.inner.label.clone();
        let path = self.inner.path.clone();
        self.with_conn(f).await.inspect_err(|e| {
            tracing::error!(
                pool = %label,
                path = %path.display(),
                error = %e,
                "schema setup failed; this service cannot enqueue or deliver anything"
            );
        })
    }
}

/// Open one connection with the settings every Deislet SQLite file uses.
fn open(path: &Path, label: &str) -> QueueResult<Connection> {
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
            "sqlite refused WAL and stayed in this journal mode; producers and consumers will now \
             block each other and pull latency will spike under load"
        );
    }
    // FULL, not NORMAL. A queue at-least-once promise rests on `Send`: a
    // Send that returns an id and is then lost to a host power failure is a
    // message that was accepted and never delivered, which is the one thing
    // the promise forbids. An ordinary commit under WAL is what makes that
    // promise durable, and FULL is one fsync per commit. `Ack` is different:
    // losing an acknowledgement only returns the message to its queue, and a
    // redelivery is inside the same at-least-once contract, so the cost is
    // paid on Send and accepted on Ack. The README records the measured
    // cost.
    conn.pragma_update(None, "synchronous", "FULL")?;
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

    /// The durability a queue owes a `Send` that returned an id: SQLite's
    /// `FULL` is 2, and `NORMAL` is 1.
    #[tokio::test]
    async fn synchronous_is_full() {
        let dir = TempDir::new().unwrap();
        let pool = pool(&dir);
        let synchronous: i64 = pool
            .with_conn(|c| Ok(c.query_row("PRAGMA synchronous", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(synchronous, 2);
    }

    #[tokio::test]
    async fn a_failing_transaction_rolls_back_and_keeps_the_pool_usable() {
        let dir = TempDir::new().unwrap();
        let pool = pool(&dir);
        pool.migrate(|c| {
            c.execute_batch("CREATE TABLE t(a INTEGER)")?;
            Ok(())
        })
        .await
        .unwrap();

        let err: QueueResult<()> = pool
            .with_tx(|tx| {
                tx.execute("INSERT INTO t(a) VALUES(1)", [])?;
                Err(QueueError::InvalidArgument("changed my mind".to_string()))
            })
            .await;
        assert!(matches!(err, Err(QueueError::InvalidArgument(_))));

        let n: i64 = pool
            .with_conn(|c| Ok(c.query_row("SELECT count(*) FROM t", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(n, 0, "the failed transaction must not have committed");
    }

    /// The whole reason [`SqlitePool::with_tx`] pins `IMMEDIATE`: a deferred
    /// read-then-write is refused outright when another connection commits in
    /// between, and no busy timeout can wait that out.
    #[tokio::test]
    async fn concurrent_read_then_write_transactions_all_commit() {
        let dir = TempDir::new().unwrap();
        let pool = pool(&dir);
        pool.migrate(|c| {
            c.execute_batch("CREATE TABLE t(a INTEGER NOT NULL)")?;
            Ok(())
        })
        .await
        .unwrap();
        pool.with_conn(|c| {
            c.execute("INSERT INTO t(a) VALUES(0)", [])?;
            Ok(())
        })
        .await
        .unwrap();

        let mut tasks = Vec::new();
        for _ in 0..16 {
            let p = pool.clone();
            tasks.push(tokio::spawn(async move {
                p.with_tx(|tx| {
                    let current: i64 = tx.query_row("SELECT max(a) FROM t", [], |r| r.get(0))?;
                    tx.execute("INSERT INTO t(a) VALUES(?1)", [current + 1])?;
                    Ok(())
                })
                .await
            }));
        }
        for t in tasks {
            t.await.unwrap().expect("no transaction may be refused");
        }

        let n: i64 = pool
            .with_conn(|c| Ok(c.query_row("SELECT count(*) FROM t", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(n, 17);
    }

    #[test]
    fn pool_size_is_never_zero() {
        assert_eq!(PoolSize::new(0).0, 1);
    }
}
