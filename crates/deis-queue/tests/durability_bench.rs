//! Measurement, not a test: send/ack throughput of the queue's SQLite open
//! path under `WAL + synchronous=NORMAL` versus `WAL + synchronous=FULL`.
//!
//! R5 (the durability review) required the queue to run `FULL` so that a
//! `Send` that returned an id survives a host power failure, and required
//! the cost of that choice to be measured, not asserted. The numbers from a
//! run of this file are recorded in `README.md` under *Durability and its
//! cost*, with the date and the machine it was measured on; rerun it after
//! a platform change and refresh that section rather than trust the old
//! numbers.
//!
//! Run it with:
//!
//! ```bash
//! cargo nextest run -p deis-queue -- --ignored
//! ```
//!
//! The file lands in the process temp directory (`TMPDIR`), so the
//! measurement is only meaningful where that is a local filesystem with a
//! real fsync. On a host whose `/tmp` is tmpfs, point `TMPDIR` at a real
//! disk first: every fsync would otherwise be a no-op and both columns
//! would read fast for the wrong reason.
//!
//! What one iteration does, and why it is a fair proxy for the service:
//! a single-message `Send` is one `INSERT` into `messages` in an
//! `IMMEDIATE` transaction, committed on its own - exactly `send_one` here
//! against the same table and index the service creates. An `Ack` is one
//! `UPDATE` in its own transaction - `ack_one`. The body is 1 KiB, the size
//! a polled node's request usually is; bigger bodies move both columns
//! equally because the setting under test changes the fsync count, not the
//! bytes copied.

use rusqlite::{Connection, TransactionBehavior, params};
use std::path::Path;
use std::time::{Duration, Instant};

const SENDS: usize = 2000;
const WARMUP: usize = 200;
const BODY: &[u8] = &[0u8; 1024];

/// The `messages` table and its index, byte for byte what `queue.rs`
/// creates at startup, so the benchmark measures the same b-tree the
/// service keeps.
const SCHEMA: &str = "CREATE TABLE messages (
                         tenant         TEXT    NOT NULL,
                         queue          TEXT    NOT NULL,
                         id             TEXT    NOT NULL,
                         body           BLOB    NOT NULL,
                         attempts       INTEGER NOT NULL DEFAULT 0,
                         visible_at     INTEGER NOT NULL,
                         created_at     INTEGER NOT NULL,
                         lease_token    TEXT    NOT NULL DEFAULT '',
                         lease_generation TEXT   NOT NULL DEFAULT '',
                         PRIMARY KEY (tenant, queue, id)
                     );
                     CREATE INDEX messages_ready
                         ON messages(tenant, queue, visible_at);";

/// Open a file the way `sqlite.rs::open` does, taking the `synchronous`
/// value as the one variable under test.
fn open(path: &Path, synchronous: &str) -> Connection {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let conn = Connection::open(path).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        mode.to_lowercase(),
        "wal",
        "the benchmark measures WAL settings"
    );
    conn.pragma_update(None, "synchronous", synchronous)
        .unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn
}

fn send_one(conn: &mut Connection, i: usize) {
    let id = format!("m{i:034}");
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    tx.execute(
        "INSERT INTO messages (tenant, queue, id, body, attempts, visible_at, created_at)
         VALUES ('bench', 'q', ?1, ?2, 0, 0, 0)",
        params![id, BODY],
    )
    .unwrap();
    tx.commit().unwrap();
}

fn ack_one(conn: &mut Connection, i: usize) {
    let id = format!("m{i:034}");
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    tx.execute(
        "UPDATE messages
            SET attempts = attempts + 1
          WHERE tenant = 'bench' AND queue = 'q' AND id = ?1",
        [id],
    )
    .unwrap();
    tx.commit().unwrap();
}

/// Warm up, then time `SENDS` single-message sends and `SENDS` acks, one
/// committed transaction each, and return both rates in operations per
/// second.
fn measure(synchronous: &str, dir: &Path) -> (f64, f64) {
    let mut conn = open(&dir.join("bench.sqlite"), synchronous);
    for i in 0..WARMUP {
        send_one(&mut conn, i);
    }

    let started = Instant::now();
    for i in 0..SENDS {
        send_one(&mut conn, WARMUP + i);
    }
    let send_rate = SENDS as f64 / started.elapsed().as_secs_f64();

    let started = Instant::now();
    for i in 0..SENDS {
        ack_one(&mut conn, WARMUP + i);
    }
    let ack_rate = SENDS as f64 / started.elapsed().as_secs_f64();

    (send_rate, ack_rate)
}

#[test]
#[ignore = "a measurement, not an assertion: the numbers are recorded in README.md, refresh them here instead of editing the README by hand"]
fn durability_benchmark_wal_normal_vs_full() {
    let dir = tempfile::tempdir().unwrap();
    let normal_dir = tempfile::tempdir_in(dir.path()).unwrap();
    let full_dir = tempfile::tempdir_in(dir.path()).unwrap();
    let (normal_send, normal_ack) = measure("NORMAL", normal_dir.path());
    let (full_send, full_ack) = measure("FULL", full_dir.path());

    eprintln!(
        "[durability bench] WAL + synchronous=NORMAL: send {normal_send:.0} ops/s, ack {normal_ack:.0} ops/s"
    );
    eprintln!(
        "[durability bench] WAL + synchronous=FULL:   send {full_send:.0} ops/s, ack {full_ack:.0} ops/s"
    );
}
