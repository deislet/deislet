//! SQLite storage for logs and metric samples.
//!
//! Logs live in one table per UTC day (`logs_YYYYMMDD`). Day partitioning is
//! what makes retention cheap: expiring a day is a `DROP TABLE`, not a
//! row-by-row `DELETE` that would leave the file bloated and hold a write
//! lock for as long as the deletion runs.
//!
//! Metric samples live in a single time-series table, because they are small,
//! queried across long spans, and expired by value rather than by table.
//!
//! Every method here is synchronous and blocks the calling thread, so callers
//! on an async runtime must wrap them in `spawn_blocking`. Writes take the one
//! write connection; reads take a connection out of a small pool of their own,
//! which is what keeps a long query from holding up ingest.
//!
//! # A page is bounded in bytes, not only in rows
//!
//! Every page a query builds has to travel in one gRPC frame of at most
//! [`crate::MAX_GRPC_MESSAGE_BYTES`]. A page built by row count alone cannot
//! promise that: three hundred log lines of 60 KiB each are inside every
//! per-entry cap and still add up to 18 MiB, and tonic then refuses to encode
//! the response after the handler has already returned, so the caller sees
//! `OutOfRange` and this process logs nothing. So a page also stops at
//! [`MAX_PAGE_BYTES`], well under the frame, and reports `has_more` with a
//! cursor for the rest. Whatever limit a client asks for pages through.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, params_from_iter};
use tracing::{debug, info, warn};

use crate::MAX_GRPC_MESSAGE_BYTES;

/// Microseconds in one day, the width of a log partition.
pub const DAY_US: i64 = 86_400_000_000;

/// Most bytes of records one page may carry, whatever its row limit.
///
/// Half the frame: the estimate in [`WireSize`] is deliberately generous
/// rather than exact, and the response carries a cursor and framing on top of
/// the records, so the budget leaves room for both and a page can never reach
/// the ceiling tonic enforces. The same ratio deis-queue uses for a `Pull`.
pub const MAX_PAGE_BYTES: usize = MAX_GRPC_MESSAGE_BYTES / 2;

/// Most distinct day partitions one insert may create or touch.
///
/// Creating a partition is a `CREATE TABLE` plus its indexes inside the one
/// write transaction on the one write connection, and a batch of records
/// spread over hundreds of distinct days used to do exactly that, holding the
/// writer against every other producer for tens of seconds. The service
/// refuses stamps older than the retention window before a batch gets here,
/// which keeps a batch inside a handful of days; this is the belt under that,
/// so a bug or a configuration with a years-long retention can still not make
/// one insert build hundreds of tables.
pub const MAX_PARTITIONS_PER_BATCH: usize = 32;

/// An upper estimate of a record's size on the wire.
///
/// Protobuf spends a tag and a length prefix per string and per map entry,
/// and a few bytes per integer; the estimate charges more than that for each,
/// so the real encoding is never larger than the sum. Exactness is not the
/// point - [`MAX_PAGE_BYTES`] leaves half the frame spare - what matters is
/// that a page assembled against this estimate cannot outgrow the frame.
pub trait WireSize {
    fn wire_size(&self) -> usize;
}

/// What one string field costs at most: its bytes, a tag and a length varint.
fn string_cost(value: &str) -> usize {
    value.len() + 6
}

/// What one map entry costs at most: both strings, each with tag and length,
/// wrapped in an entry with a tag and length of its own.
fn pair_cost(key: &str, value: &str) -> usize {
    string_cost(key) + string_cost(value) + 6
}

/// The framing of one record inside a repeated field, plus its numeric
/// fields, which are at most eleven bytes each.
const RECORD_FIXED_COST: usize = 64;

/// Bytes still available in the page being built, and whether it was cut.
///
/// Cutting a page by bytes is never allowed to leave it empty: a single
/// record larger than the whole budget is returned on a page of its own,
/// because the alternative is a cursor that never advances. Such a record is
/// still under the frame, since every string that reaches storage is capped
/// at ingest.
struct PageBudget {
    remaining: usize,
    taken: usize,
    cut: bool,
}

impl PageBudget {
    fn new(bytes: usize) -> Self {
        Self {
            remaining: bytes,
            taken: 0,
            cut: false,
        }
    }

    /// Whether `record` fits on the page. Once this says no, the page is
    /// closed: the caller stops reading and reports the cut.
    fn admit<R: WireSize>(&mut self, record: &R) -> bool {
        let size = record.wire_size();
        if size > self.remaining && self.taken > 0 {
            self.cut = true;
            return false;
        }
        self.remaining = self.remaining.saturating_sub(size);
        self.taken += 1;
        true
    }

    fn is_closed(&self) -> bool {
        self.cut
    }
}

/// One stored log record. This is the storage-side shape; the proto type is
/// converted to and from it at the service boundary so storage does not
/// depend on generated code.
#[derive(Debug, Clone, PartialEq)]
pub struct LogRecord {
    pub timestamp_us: i64,
    /// `LogLevel` ordinal. Higher is more severe, which is what makes
    /// `min_level` a plain `>=` comparison.
    pub level: i32,
    /// Component that produced the entry, for example "deis-runtime". Not an
    /// application (that is `app_id`) and not a node: this service has no
    /// concept of nodes.
    pub component: String,
    pub app_id: String,
    pub message: String,
    /// Structured fields, kept ordered so the stored JSON is canonical.
    pub fields: BTreeMap<String, String>,
    pub trace_id: String,
    pub span_id: String,
}

/// One numeric sample of one series at one point in time.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricRecord {
    pub name: String,
    pub app_id: String,
    pub labels: BTreeMap<String, String>,
    pub value: f64,
    pub timestamp_us: i64,
}

/// One stored trace record: a single invocation of one application.
///
/// The storage-side shape, converted to and from the proto type at the
/// service boundary so storage does not depend on generated code. `kind` and
/// `outcome` are the proto enum ordinals, kept as integers so a value this
/// build does not know about survives a round trip instead of being flattened
/// to "unspecified".
#[derive(Debug, Clone, PartialEq)]
pub struct TraceRecord {
    /// The same id the invocation's log lines carry, which is what makes
    /// "show me this invocation's log" a `LogFilter { trace_id }` query.
    pub trace_id: String,
    pub started_us: i64,
    pub duration_us: i64,
    pub app_id: String,
    pub environment: String,
    pub node: String,
    /// `InvocationKind` ordinal.
    pub kind: i32,
    /// `InvocationOutcome` ordinal.
    pub outcome: i32,
    /// What the invocation was aimed at: a URL, a queue, a cron expression.
    pub target: String,
    pub method: String,
    pub status: i32,
    pub error: String,
    /// Extra dimensions, kept ordered so the stored JSON is canonical.
    pub attributes: BTreeMap<String, String>,
}

impl WireSize for LogRecord {
    fn wire_size(&self) -> usize {
        RECORD_FIXED_COST
            + string_cost(&self.component)
            + string_cost(&self.app_id)
            + string_cost(&self.message)
            + string_cost(&self.trace_id)
            + string_cost(&self.span_id)
            + self
                .fields
                .iter()
                .map(|(k, v)| pair_cost(k, v))
                .sum::<usize>()
    }
}

impl WireSize for MetricRecord {
    fn wire_size(&self) -> usize {
        RECORD_FIXED_COST
            + string_cost(&self.name)
            + string_cost(&self.app_id)
            + self
                .labels
                .iter()
                .map(|(k, v)| pair_cost(k, v))
                .sum::<usize>()
    }
}

impl WireSize for TraceRecord {
    fn wire_size(&self) -> usize {
        RECORD_FIXED_COST
            + string_cost(&self.trace_id)
            + string_cost(&self.app_id)
            + string_cost(&self.environment)
            + string_cost(&self.node)
            + string_cost(&self.target)
            + string_cost(&self.method)
            + string_cost(&self.error)
            + self
                .attributes
                .iter()
                .map(|(k, v)| pair_cost(k, v))
                .sum::<usize>()
    }
}

/// Filters for [`TelemetryStore::query_traces`]. `None` and an empty list
/// mean "no restriction"; the restrictions that are set combine with AND.
#[derive(Debug, Clone, Default)]
pub struct TraceFilter {
    pub app_id: Option<String>,
    pub environment: Option<String>,
    /// Inclusive lower bound on `started_us`.
    pub start_us: Option<i64>,
    /// Exclusive upper bound on `started_us`.
    pub end_us: Option<i64>,
    /// `InvocationKind` ordinals; empty matches every kind.
    pub kinds: Vec<i32>,
    /// `InvocationOutcome` ordinals; empty matches every outcome. This is
    /// how "the invocations that failed" is asked for.
    pub outcomes: Vec<i32>,
    pub trace_id: Option<String>,
    /// Only invocations that ran at least this long.
    pub min_duration_us: Option<i64>,
    /// Case-insensitive substring of `target`. SQLite's `lower()` only folds
    /// ASCII, so a non-ASCII target matches case-sensitively.
    pub target_contains: Option<String>,
}

/// Filters for [`TelemetryStore::query_logs`]. `None` means "no restriction"; the
/// restrictions that are set combine with AND.
#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    pub app_id: Option<String>,
    /// Inclusive lower bound.
    pub start_us: Option<i64>,
    /// Exclusive upper bound.
    pub end_us: Option<i64>,
    pub min_level: Option<i32>,
    pub trace_id: Option<String>,
    /// Case-insensitive substring of the message. SQLite's `lower()` only
    /// folds ASCII, so non-ASCII text matches case-sensitively.
    pub contains: Option<String>,
}

/// Filters for [`TelemetryStore::query_metrics`].
#[derive(Debug, Clone, Default)]
pub struct MetricFilter {
    pub name: Option<String>,
    pub app_id: Option<String>,
    /// Every pair must be present on the sample with the same value; the
    /// sample may carry labels beyond these.
    pub label_selector: BTreeMap<String, String>,
    pub start_us: Option<i64>,
    pub end_us: Option<i64>,
}

/// Position of the last row of a log page. Pagination is keyset based rather
/// than `OFFSET` based so that a page boundary stays stable while new logs
/// keep arriving at the newest end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogCursor {
    /// Days since the Unix epoch, identifying the partition.
    pub day: i64,
    pub timestamp_us: i64,
    pub row_id: i64,
}

impl LogCursor {
    /// Encode for the wire. The format is an implementation detail; callers
    /// must treat the string as opaque and hand it back unchanged.
    pub fn encode(&self) -> String {
        format!("{}:{}:{}", self.day, self.timestamp_us, self.row_id)
    }

    /// Decode a token produced by [`LogCursor::encode`].
    pub fn decode(raw: &str) -> Result<Self> {
        let mut parts = raw.split(':');
        let mut next = |field: &str| -> Result<i64> {
            parts
                .next()
                .with_context(|| format!("cursor is missing its {field} component"))?
                .parse::<i64>()
                .with_context(|| format!("cursor {field} component is not an integer"))
        };
        let day = next("day")?;
        let timestamp_us = next("timestamp")?;
        let row_id = next("row id")?;
        if parts.next().is_some() {
            anyhow::bail!("cursor has trailing components");
        }
        Ok(Self {
            day,
            timestamp_us,
            row_id,
        })
    }
}

/// One page of log records, newest first.
#[derive(Debug, Clone, Default)]
pub struct LogPage {
    pub records: Vec<LogRecord>,
    /// Position to resume from; `None` when the page is the last one.
    pub next_cursor: Option<LogCursor>,
    pub has_more: bool,
    /// The page holds fewer rows than asked for because the next row would
    /// have taken it past [`MAX_PAGE_BYTES`]. `has_more` is true whenever
    /// this is; it is reported apart so the caller can say why.
    pub cut_by_bytes: bool,
}

/// Position of the last row of a metric page. Like [`LogCursor`], keyset
/// based, and ordered by sample time rather than insert order: samples can
/// arrive late, and "newest" must mean newest sample, not newest write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricCursor {
    pub timestamp_us: i64,
    pub row_id: i64,
}

impl MetricCursor {
    /// Encode for the wire; opaque to callers.
    pub fn encode(&self) -> String {
        format!("{}:{}", self.timestamp_us, self.row_id)
    }

    /// Decode a token produced by [`MetricCursor::encode`].
    pub fn decode(raw: &str) -> Result<Self> {
        let mut parts = raw.split(':');
        let mut next = |field: &str| -> Result<i64> {
            parts
                .next()
                .with_context(|| format!("cursor is missing its {field} component"))?
                .parse::<i64>()
                .with_context(|| format!("cursor {field} component is not an integer"))
        };
        let timestamp_us = next("timestamp")?;
        let row_id = next("row id")?;
        if parts.next().is_some() {
            anyhow::bail!("cursor has trailing components");
        }
        Ok(Self {
            timestamp_us,
            row_id,
        })
    }
}

/// One page of metric samples, newest first.
#[derive(Debug, Clone, Default)]
pub struct MetricPage {
    pub records: Vec<MetricRecord>,
    /// Position to resume from; `None` when the page is the last one.
    pub next_cursor: Option<MetricCursor>,
    pub has_more: bool,
    /// Cut short at [`MAX_PAGE_BYTES`]; see [`LogPage::cut_by_bytes`].
    pub cut_by_bytes: bool,
}

/// Position of the last row of a trace page. Keyset based, like the other
/// two, and ordered by start time: an invocation is filed under the day it
/// started, so a long one never lands in a partition its start is not in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceCursor {
    /// Days since the Unix epoch, identifying the partition.
    pub day: i64,
    pub started_us: i64,
    pub row_id: i64,
}

impl TraceCursor {
    /// Encode for the wire; opaque to callers.
    pub fn encode(&self) -> String {
        format!("{}:{}:{}", self.day, self.started_us, self.row_id)
    }

    /// Decode a token produced by [`TraceCursor::encode`].
    pub fn decode(raw: &str) -> Result<Self> {
        let mut parts = raw.split(':');
        let mut next = |field: &str| -> Result<i64> {
            parts
                .next()
                .with_context(|| format!("cursor is missing its {field} component"))?
                .parse::<i64>()
                .with_context(|| format!("cursor {field} component is not an integer"))
        };
        let day = next("day")?;
        let started_us = next("start time")?;
        let row_id = next("row id")?;
        if parts.next().is_some() {
            anyhow::bail!("cursor has trailing components");
        }
        Ok(Self {
            day,
            started_us,
            row_id,
        })
    }
}

/// One page of trace records, newest first.
#[derive(Debug, Clone, Default)]
pub struct TracePage {
    pub records: Vec<TraceRecord>,
    /// Position to resume from; `None` when the page is the last one.
    pub next_cursor: Option<TraceCursor>,
    pub has_more: bool,
    /// Cut short at [`MAX_PAGE_BYTES`]; see [`LogPage::cut_by_bytes`].
    pub cut_by_bytes: bool,
}

/// A log partition removed by a retention sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedPartition {
    pub table: String,
    pub day: i64,
    pub rows: i64,
}

/// What a retention sweep removed. Reported so the caller can log it: dropped
/// data is unrecoverable and must never disappear silently.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepOutcome {
    pub dropped_partitions: Vec<DroppedPartition>,
    pub dropped_trace_partitions: Vec<DroppedPartition>,
    pub deleted_metric_samples: usize,
}

impl SweepOutcome {
    /// True when the sweep removed nothing, so the caller can stay quiet.
    pub fn is_empty(&self) -> bool {
        self.dropped_partitions.is_empty()
            && self.dropped_trace_partitions.is_empty()
            && self.deleted_metric_samples == 0
    }

    /// Total rows removed from log partitions.
    pub fn dropped_log_rows(&self) -> i64 {
        self.dropped_partitions.iter().map(|p| p.rows).sum()
    }

    /// Total rows removed from trace partitions.
    pub fn dropped_trace_rows(&self) -> i64 {
        self.dropped_trace_partitions.iter().map(|p| p.rows).sum()
    }
}

/// How many read connections the pool may open.
///
/// Reads run on `spawn_blocking` threads, so this also bounds how many of
/// those a burst of queries can occupy. Four is enough that a dashboard
/// query, a `deis tail` client's first look and a scrape do not queue behind
/// one another, and small enough that a flood of callers cannot make this
/// process open a SQLite handle per caller.
const READER_CONNECTIONS: usize = 4;

/// How long a connection waits for a competing writer before giving up.
/// WAL keeps readers out of the writer's way, so hitting this means two
/// writers met; failing after five seconds surfaces a wedged writer as an
/// error instead of a request that never returns.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Handle to the telemetry database. Cheap to clone; clones share the same
/// connections.
///
/// Writes go through one connection, because SQLite admits one writer at a
/// time whatever we do, and reads are served from a small pool of their own.
/// That split is what makes WAL worth having: a `contains` query scanning
/// several days runs on a read connection while `PushLogs` keeps committing
/// on the write connection. Behind a single shared connection the two would
/// serialise, and one slow log query would stall ingest for as long as it
/// ran - the observability service dropping the very logs someone is looking
/// at.
#[derive(Clone)]
pub struct TelemetryStore {
    inner: Arc<Inner>,
}

struct Inner {
    /// The one connection that writes. Locked for the length of a statement
    /// or a transaction; callers must already be off the async reactor.
    writer: Mutex<Connection>,
    /// Connections used for reads. `None` for an in-memory database, where a
    /// second connection would open a second, empty database - there reads
    /// share the write connection and do serialise with it.
    readers: Option<ReaderPool>,
}

impl TelemetryStore {
    /// Open (creating if needed) the database under `data_dir`.
    pub fn open(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("cannot create data dir {}", data_dir.display()))?;
        let path = data_dir.join("telemetry.sqlite");
        let writer = open_connection(&path, Role::Writer)?;
        let store = Self::with_schema(writer, Some(ReaderPool::new(path.clone())))?;
        info!(
            path = %path.display(),
            reader_connections = READER_CONNECTIONS,
            "telemetry database ready"
        );
        Ok(store)
    }

    /// Open an in-memory database. Used by tests; the data dies with the
    /// handle, and reads and writes share the one connection.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("cannot open in-memory db")?;
        Self::with_schema(conn, None)
    }

    fn with_schema(conn: Connection, readers: Option<ReaderPool>) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS metrics (
                 id           INTEGER PRIMARY KEY,
                 name         TEXT NOT NULL,
                 app_id       TEXT NOT NULL,
                 labels       TEXT NOT NULL,
                 value        REAL NOT NULL,
                 timestamp_us INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS ix_metrics_series_time
                 ON metrics (name, app_id, timestamp_us DESC, id DESC);
             CREATE INDEX IF NOT EXISTS ix_metrics_time
                 ON metrics (timestamp_us DESC, id DESC);",
        )
        .context("cannot create metrics schema")?;
        Ok(Self {
            inner: Arc::new(Inner {
                writer: Mutex::new(conn),
                readers,
            }),
        })
    }

    /// The write connection. Everything that changes the database goes
    /// through here, one caller at a time.
    fn writer(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A poisoned lock means another thread panicked mid-statement. The
        // connection itself is still usable and SQLite rolls back any open
        // transaction, so recovering beats taking the whole service down.
        self.inner.writer.lock().unwrap_or_else(|poisoned| {
            warn!(
                "telemetry write connection lock was poisoned by a panicking thread, recovering; \
                 the interrupted transaction was rolled back and ingest continues"
            );
            poisoned.into_inner()
        })
    }

    /// Run `f` on a connection reserved for reading.
    ///
    /// On a file database this is a pooled connection of its own, so the read
    /// never waits for - and never delays - an in-flight write.
    fn with_reader<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        match &self.inner.readers {
            Some(pool) => {
                let reader = pool.checkout()?;
                // One deferred transaction around the whole call, so every
                // statement in it sees the same snapshot. A log query lists
                // the partitions and then reads them one by one; without this
                // a retention sweep committing in between would drop a
                // partition the query had already decided to read, and the
                // query would fail with "no such table" instead of returning
                // the rows it had. The transaction only reads, so it never
                // holds the writer up.
                let tx = reader
                    .unchecked_transaction()
                    .context("cannot open a read transaction")?;
                let out = f(&tx);
                // Dropping the transaction rolls it back, which for a
                // read-only transaction just releases the snapshot.
                drop(tx);
                out
            }
            None => f(&self.writer()),
        }
    }

    /// Callers currently waiting for a read connection. Test-only view of
    /// the number the pool reports in its log line.
    #[cfg(test)]
    fn readers_waiting(&self) -> usize {
        self.inner.readers.as_ref().map_or(0, ReaderPool::waiting)
    }

    /// Append log records, creating day partitions as needed.
    ///
    /// Returns the number of rows written. The whole call is one transaction,
    /// so a batch either lands entirely or not at all. A batch spread over
    /// more than [`MAX_PARTITIONS_PER_BATCH`] distinct days is refused before
    /// the transaction opens; the service keeps batches inside the retention
    /// window, so reaching this is a bug upstream, not a producer's doing.
    pub fn insert_logs(&self, records: &[LogRecord]) -> Result<usize> {
        if records.is_empty() {
            return Ok(0);
        }
        // Group by day first: a batch that straddles midnight touches two
        // partitions, and each partition is prepared once.
        let mut by_day: BTreeMap<i64, Vec<&LogRecord>> = BTreeMap::new();
        for record in records {
            by_day
                .entry(day_of(record.timestamp_us))
                .or_default()
                .push(record);
        }
        check_partition_span("log", by_day.len())?;

        let mut conn = self.writer();
        let tx = conn.transaction().context("cannot begin log insert")?;
        let mut written = 0usize;
        for (day, day_records) in by_day {
            let table = partition_table(LOG_PARTITIONS, day)?;
            create_partition(&tx, &table)?;
            let sql = format!(
                "INSERT INTO {table}
                     (timestamp_us, level, component, app_id, message, fields, trace_id, span_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
            );
            let mut stmt = tx
                .prepare_cached(&sql)
                .with_context(|| format!("cannot prepare insert into {table}"))?;
            for record in day_records {
                let fields =
                    serde_json::to_string(&record.fields).context("cannot serialize log fields")?;
                stmt.execute(rusqlite::params![
                    record.timestamp_us,
                    record.level,
                    record.component,
                    record.app_id,
                    record.message,
                    fields,
                    record.trace_id,
                    record.span_id,
                ])
                .with_context(|| format!("cannot insert log into {table}"))?;
                written += 1;
            }
        }
        tx.commit().context("cannot commit log insert")?;
        Ok(written)
    }

    /// Read one page of logs, newest first.
    ///
    /// `limit` is the page size; the store fetches one extra row to decide
    /// whether an older page exists, and never returns that extra row. The
    /// page also stops at [`MAX_PAGE_BYTES`], whatever `limit` says; a page
    /// cut that way reports `has_more` and a cursor like any other.
    pub fn query_logs(
        &self,
        filter: &LogFilter,
        limit: usize,
        cursor: Option<LogCursor>,
    ) -> Result<LogPage> {
        let limit = limit.max(1);
        let (mut collected, cut_by_bytes) = self.with_reader(|conn| {
            let mut partitions = list_partitions(conn, LOG_PARTITIONS)?;
            partitions.sort_unstable_by(|a, b| b.cmp(a));

            let mut collected: Vec<(LogCursor, LogRecord)> = Vec::new();
            let mut budget = PageBudget::new(MAX_PAGE_BYTES);
            let want = limit + 1;
            for day in partitions {
                if collected.len() >= want || budget.is_closed() {
                    break;
                }
                if !day_intersects(day, filter.start_us, filter.end_us) {
                    continue;
                }
                // The cursor points into one partition. Newer partitions were
                // already returned on earlier pages; the cursor's own partition
                // resumes mid-table, older ones start from their newest row.
                let partition_cursor = match cursor {
                    Some(c) if day > c.day => continue,
                    Some(c) if day == c.day => Some(c),
                    _ => None,
                };
                let table = partition_table(LOG_PARTITIONS, day)?;
                let rows = query_partition(
                    conn,
                    &table,
                    day,
                    filter,
                    partition_cursor,
                    want - collected.len(),
                    &mut budget,
                )?;
                collected.extend(rows);
            }
            Ok((collected, budget.is_closed()))
        })?;

        // A page closed by bytes has already seen a row it did not take, so
        // there is more behind it whatever the count says.
        let has_more = cut_by_bytes || collected.len() > limit;
        collected.truncate(limit);
        let next_cursor = if has_more {
            collected.last().map(|(cursor, _)| *cursor)
        } else {
            None
        };
        Ok(LogPage {
            records: collected.into_iter().map(|(_, record)| record).collect(),
            next_cursor,
            has_more,
            cut_by_bytes,
        })
    }

    /// Append trace records, creating day partitions as needed.
    ///
    /// Returns the number of rows written. The whole call is one transaction,
    /// so a batch either lands entirely or not at all, and the same
    /// [`MAX_PARTITIONS_PER_BATCH`] ceiling as [`Self::insert_logs`] applies.
    pub fn insert_traces(&self, records: &[TraceRecord]) -> Result<usize> {
        if records.is_empty() {
            return Ok(0);
        }
        // Filed under the day the invocation started, not the day it ended:
        // that is the timestamp every query orders and bounds by, so a record
        // must live in the partition that timestamp points at.
        let mut by_day: BTreeMap<i64, Vec<&TraceRecord>> = BTreeMap::new();
        for record in records {
            by_day
                .entry(day_of(record.started_us))
                .or_default()
                .push(record);
        }
        check_partition_span("trace", by_day.len())?;

        let mut conn = self.writer();
        let tx = conn.transaction().context("cannot begin trace insert")?;
        let mut written = 0usize;
        for (day, day_records) in by_day {
            let table = partition_table(TRACE_PARTITIONS, day)?;
            create_trace_partition(&tx, &table)?;
            let sql = format!(
                "INSERT INTO {table}
                     (trace_id, started_us, duration_us, app_id, environment, node,
                      kind, outcome, target, method, status, error, attributes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
            );
            let mut stmt = tx
                .prepare_cached(&sql)
                .with_context(|| format!("cannot prepare insert into {table}"))?;
            for record in day_records {
                let attributes = serde_json::to_string(&record.attributes)
                    .context("cannot serialize trace attributes")?;
                stmt.execute(rusqlite::params![
                    record.trace_id,
                    record.started_us,
                    record.duration_us,
                    record.app_id,
                    record.environment,
                    record.node,
                    record.kind,
                    record.outcome,
                    record.target,
                    record.method,
                    record.status,
                    record.error,
                    attributes,
                ])
                .with_context(|| format!("cannot insert trace into {table}"))?;
                written += 1;
            }
        }
        tx.commit().context("cannot commit trace insert")?;
        Ok(written)
    }

    /// Read one page of traces, newest invocation first.
    ///
    /// `limit` is the page size; the store fetches one extra row to decide
    /// whether an older page exists, and never returns that extra row. Like
    /// [`Self::query_logs`], the page also stops at [`MAX_PAGE_BYTES`].
    pub fn query_traces(
        &self,
        filter: &TraceFilter,
        limit: usize,
        cursor: Option<TraceCursor>,
    ) -> Result<TracePage> {
        let limit = limit.max(1);
        let (mut collected, cut_by_bytes) = self.with_reader(|conn| {
            let mut partitions = list_partitions(conn, TRACE_PARTITIONS)?;
            partitions.sort_unstable_by(|a, b| b.cmp(a));

            let mut collected: Vec<(TraceCursor, TraceRecord)> = Vec::new();
            let mut budget = PageBudget::new(MAX_PAGE_BYTES);
            let want = limit + 1;
            for day in partitions {
                if collected.len() >= want || budget.is_closed() {
                    break;
                }
                if !day_intersects(day, filter.start_us, filter.end_us) {
                    continue;
                }
                let partition_cursor = match cursor {
                    Some(c) if day > c.day => continue,
                    Some(c) if day == c.day => Some(c),
                    _ => None,
                };
                let table = partition_table(TRACE_PARTITIONS, day)?;
                let rows = query_trace_partition(
                    conn,
                    &table,
                    day,
                    filter,
                    partition_cursor,
                    want - collected.len(),
                    &mut budget,
                )?;
                collected.extend(rows);
            }
            Ok((collected, budget.is_closed()))
        })?;

        let has_more = cut_by_bytes || collected.len() > limit;
        collected.truncate(limit);
        let next_cursor = if has_more {
            collected.last().map(|(cursor, _)| *cursor)
        } else {
            None
        };
        Ok(TracePage {
            records: collected.into_iter().map(|(_, record)| record).collect(),
            next_cursor,
            has_more,
            cut_by_bytes,
        })
    }

    /// Append metric samples. One transaction for the whole batch.
    pub fn insert_metrics(&self, records: &[MetricRecord]) -> Result<usize> {
        if records.is_empty() {
            return Ok(0);
        }
        let mut conn = self.writer();
        let tx = conn.transaction().context("cannot begin metric insert")?;
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO metrics (name, app_id, labels, value, timestamp_us)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .context("cannot prepare metric insert")?;
            for record in records {
                let labels =
                    serde_json::to_string(&record.labels).context("cannot serialize labels")?;
                stmt.execute(rusqlite::params![
                    record.name,
                    record.app_id,
                    labels,
                    record.value,
                    record.timestamp_us,
                ])
                .context("cannot insert metric sample")?;
            }
        }
        tx.commit().context("cannot commit metric insert")?;
        Ok(records.len())
    }

    /// Read one page of metric samples, newest first.
    ///
    /// Like the other two, the page stops at [`MAX_PAGE_BYTES`] as well as at
    /// `limit`: a sample carries its labels, and a thousand samples with
    /// large label sets are a page the frame cannot hold.
    pub fn query_metrics(
        &self,
        filter: &MetricFilter,
        limit: usize,
        cursor: Option<MetricCursor>,
    ) -> Result<MetricPage> {
        let limit = limit.max(1);
        let (mut collected, cut_by_bytes) = self.with_reader(|conn| {
            let (sql, args) = build_metric_query(filter, cursor);
            let mut stmt = prepare_stmt(conn, &sql)?;
            let mut rows = stmt
                .query(params_from_iter(args.iter()))
                .context("cannot run metric query")?;

            let mut collected: Vec<(MetricCursor, MetricRecord)> = Vec::new();
            let mut budget = PageBudget::new(MAX_PAGE_BYTES);
            while collected.len() <= limit {
                let Some(row) = rows.next().context("cannot read metric row")? else {
                    break;
                };
                let id: i64 = row.get(0)?;
                let timestamp_us: i64 = row.get(5)?;
                let labels_json: String = row.get(3)?;
                let labels: BTreeMap<String, String> = serde_json::from_str(&labels_json)
                    .unwrap_or_else(|e| {
                        warn!(
                            metric_row = id,
                            error = %e,
                            "metric labels are not valid JSON, sample is reported without labels"
                        );
                        BTreeMap::new()
                    });
                if !filter
                    .label_selector
                    .iter()
                    .all(|(k, v)| labels.get(k).is_some_and(|found| found == v))
                {
                    continue;
                }
                let record = MetricRecord {
                    name: row.get(1)?,
                    app_id: row.get(2)?,
                    labels,
                    value: row.get(4)?,
                    timestamp_us,
                };
                if !budget.admit(&record) {
                    break;
                }
                collected.push((
                    MetricCursor {
                        timestamp_us,
                        row_id: id,
                    },
                    record,
                ));
            }
            Ok((collected, budget.is_closed()))
        })?;

        let has_more = cut_by_bytes || collected.len() > limit;
        collected.truncate(limit);
        let next_cursor = if has_more {
            collected.last().map(|(cursor, _)| *cursor)
        } else {
            None
        };
        Ok(MetricPage {
            records: collected.into_iter().map(|(_, record)| record).collect(),
            next_cursor,
            has_more,
            cut_by_bytes,
        })
    }

    /// Latest sample of every series that reported at or after `since_us`.
    ///
    /// This is what `/metrics` exports. Series older than the window are left
    /// out on purpose: exporting a stale value would make a component that
    /// stopped reporting look like it is still reporting.
    pub fn latest_metric_samples(&self, since_us: i64) -> Result<Vec<MetricRecord>> {
        self.with_reader(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT name, app_id, labels, value, MAX(timestamp_us)
                 FROM metrics
                 WHERE timestamp_us >= ?1
                 GROUP BY name, app_id, labels
                 ORDER BY name, app_id, labels",
                )
                .context("cannot prepare metric export")?;
            let rows = stmt
                .query_map([since_us], |row| {
                    let labels_json: String = row.get(2)?;
                    Ok(MetricRecord {
                        name: row.get(0)?,
                        app_id: row.get(1)?,
                        labels: serde_json::from_str(&labels_json).unwrap_or_default(),
                        value: row.get(3)?,
                        timestamp_us: row.get(4)?,
                    })
                })
                .context("cannot run metric export")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("cannot read exported metric row")?);
            }
            Ok(out)
        })
    }

    /// Drop log partitions and metric samples that fell out of retention.
    ///
    /// `now_us` is passed in rather than read from the clock so tests can age
    /// data deterministically.
    pub fn sweep_retention(
        &self,
        now_us: i64,
        log_retention_days: u32,
        metric_retention_days: u32,
        trace_retention_days: u32,
    ) -> Result<SweepOutcome> {
        let today = day_of(now_us);
        let metric_cutoff_us = (today - i64::from(metric_retention_days) + 1) * DAY_US;

        // Dropping a table and deleting rows are writes, so this takes the
        // write connection; queries keep running on the read pool meanwhile.
        let conn = self.writer();
        let mut outcome = SweepOutcome::default();
        for (prefix, retention_days, dropped) in [
            (
                LOG_PARTITIONS,
                log_retention_days,
                &mut outcome.dropped_partitions,
            ),
            (
                TRACE_PARTITIONS,
                trace_retention_days,
                &mut outcome.dropped_trace_partitions,
            ),
        ] {
            let oldest_day = today - i64::from(retention_days) + 1;
            for day in list_partitions(&conn, prefix)? {
                if day >= oldest_day {
                    continue;
                }
                let table = partition_table(prefix, day)?;
                let rows: i64 = conn
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })
                    .with_context(|| format!("cannot count rows of {table}"))?;
                conn.execute(&format!("DROP TABLE {table}"), [])
                    .with_context(|| format!("cannot drop expired partition {table}"))?;
                dropped.push(DroppedPartition { table, day, rows });
            }
        }
        let deleted = conn
            .execute(
                "DELETE FROM metrics WHERE timestamp_us < ?1",
                [metric_cutoff_us],
            )
            .context("cannot delete expired metric samples")?;
        outcome.deleted_metric_samples = deleted;
        Ok(outcome)
    }

    /// Number of log partitions currently on disk. Used by the self-metrics
    /// export and by tests.
    pub fn partition_count(&self) -> Result<usize> {
        self.with_reader(|conn| Ok(list_partitions(conn, LOG_PARTITIONS)?.len()))
    }

    /// Number of trace partitions currently on disk.
    pub fn trace_partition_count(&self) -> Result<usize> {
        self.with_reader(|conn| Ok(list_partitions(conn, TRACE_PARTITIONS)?.len()))
    }

    /// Number of stored metric samples.
    pub fn metric_sample_count(&self) -> Result<i64> {
        self.with_reader(|conn| {
            conn.query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
                .context("cannot count metric samples")
        })
    }
}

/// What a connection is opened for. Only the writer sets the file-wide
/// pragmas; a reader that tried would have to wait for the writer to be idle.
#[derive(Debug, Clone, Copy)]
enum Role {
    Writer,
    Reader,
}

/// Open one connection on `path` with the settings this database needs.
fn open_connection(path: &Path, role: Role) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("cannot open database {}", path.display()))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .context("cannot set busy_timeout")?;
    if let Role::Writer = role {
        // WAL is what lets the read pool work at all: readers see a snapshot
        // while the writer commits, instead of the two locking each other out.
        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .context("cannot set journal_mode")?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            warn!(
                journal_mode,
                "sqlite refused WAL, so readers and the writer will lock each other out; \
                 log queries and log ingest will now block one another"
            );
        }
        // NORMAL trades an fsync per commit for one per checkpoint. Under WAL
        // that risks losing the last commits if the machine loses power, not
        // corrupting the file, which is the right trade for telemetry.
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("cannot set synchronous")?;
    }
    debug!(path = %path.display(), role = ?role, "opened a telemetry sqlite connection");
    Ok(conn)
}

/// A bounded set of connections used only for reads, opened on demand.
///
/// Checking one out blocks the calling thread while all of them are busy.
/// Callers are already on blocking threads, and waiting for a connection is
/// better than letting a burst of queries open an unbounded number of them.
struct ReaderPool {
    path: PathBuf,
    state: Mutex<PoolState>,
    /// Signalled whenever a connection goes back into the pool.
    returned: Condvar,
}

struct PoolState {
    idle: Vec<Connection>,
    /// How many more connections may still be opened before the ceiling.
    spare: usize,
    /// Callers parked waiting for a connection to come back.
    waiting: usize,
}

impl ReaderPool {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(PoolState {
                idle: Vec::new(),
                spare: READER_CONNECTIONS,
                waiting: 0,
            }),
            returned: Condvar::new(),
        }
    }

    /// A poisoned lock only means a previous holder panicked; the connection
    /// list itself is intact, so it is recovered rather than failing every
    /// later query on this pool.
    fn lock(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(|poisoned| {
            warn!("telemetry reader pool lock was poisoned by a panicking thread, recovering");
            poisoned.into_inner()
        })
    }

    /// How many callers are parked waiting for a connection right now. Only
    /// the tests read it; in production the same number goes into the log
    /// line above.
    #[cfg(test)]
    fn waiting(&self) -> usize {
        self.lock().waiting
    }

    fn checkout(&self) -> Result<PooledReader<'_>> {
        let mut state = self.lock();
        loop {
            if let Some(conn) = state.idle.pop() {
                return Ok(PooledReader {
                    pool: self,
                    conn: Some(conn),
                });
            }
            if state.spare > 0 {
                state.spare -= 1;
                drop(state);
                let conn = open_connection(&self.path, Role::Reader).inspect_err(|_| {
                    // Hand the slot back: a transient open failure must not
                    // shrink the pool for the rest of the process's life.
                    self.lock().spare += 1;
                    self.returned.notify_one();
                })?;
                return Ok(PooledReader {
                    pool: self,
                    conn: Some(conn),
                });
            }
            state.waiting += 1;
            debug!(
                connections = READER_CONNECTIONS,
                waiting = state.waiting,
                "every telemetry read connection is busy, this query waits for one to come \
                 back; sustained waiting here shows up as slow Query and /metrics responses"
            );
            state = self
                .returned
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.waiting -= 1;
        }
    }
}

/// A read connection checked out of the pool, returned when dropped - on a
/// panic as well, so a failing query cannot leak a connection.
struct PooledReader<'a> {
    pool: &'a ReaderPool,
    conn: Option<Connection>,
}

impl std::ops::Deref for PooledReader<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("the connection is taken only while dropping")
    }
}

impl Drop for PooledReader<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.lock().idle.push(conn);
            self.pool.returned.notify_one();
        }
    }
}

/// Days since the Unix epoch that `timestamp_us` falls in, in UTC.
pub fn day_of(timestamp_us: i64) -> i64 {
    timestamp_us.div_euclid(DAY_US)
}

/// Table-name prefix of the log partitions.
const LOG_PARTITIONS: &str = "logs_";

/// Table-name prefix of the trace partitions. Traces are partitioned by day
/// for the same reason logs are, and expire on their own shorter schedule.
const TRACE_PARTITIONS: &str = "traces_";

/// Refuse a batch that would touch more day partitions than one insert may.
fn check_partition_span(kind: &str, distinct_days: usize) -> Result<()> {
    if distinct_days > MAX_PARTITIONS_PER_BATCH {
        anyhow::bail!(
            "a {kind} batch spans {distinct_days} distinct days, more than the \
             {MAX_PARTITIONS_PER_BATCH} partitions one insert may touch; the batch is refused \
             whole rather than allowed to build that many tables inside one transaction"
        );
    }
    Ok(())
}

/// Table name of the partition of `prefix` holding `day`.
fn partition_table(prefix: &str, day: i64) -> Result<String> {
    let seconds = day
        .checked_mul(86_400)
        .with_context(|| format!("day {day} is out of range"))?;
    let date = chrono::DateTime::from_timestamp(seconds, 0)
        .with_context(|| format!("day {day} is not a representable date"))?
        .date_naive();
    Ok(format!("{prefix}{}", date.format("%Y%m%d")))
}

/// Inverse of [`partition_table`], for reading the partition list back out of
/// `sqlite_master`.
fn day_from_partition(prefix: &str, table: &str) -> Option<i64> {
    let suffix = table.strip_prefix(prefix)?;
    let date = chrono::NaiveDate::parse_from_str(suffix, "%Y%m%d").ok()?;
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1)?;
    Some(date.signed_duration_since(epoch).num_days())
}

fn create_partition(conn: &Connection, table: &str) -> Result<()> {
    // Indexes mirror the three query shapes the service serves: "this app
    // over this window" (the common one), "everything over this window", and
    // "this trace". Without them every query would scan the whole day.
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {table} (
             id           INTEGER PRIMARY KEY,
             timestamp_us INTEGER NOT NULL,
             level        INTEGER NOT NULL,
             component    TEXT NOT NULL,
             app_id       TEXT NOT NULL,
             message      TEXT NOT NULL,
             fields       TEXT NOT NULL,
             trace_id     TEXT NOT NULL,
             span_id      TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS ix_{table}_app_time
             ON {table} (app_id, timestamp_us DESC, id DESC);
         CREATE INDEX IF NOT EXISTS ix_{table}_time
             ON {table} (timestamp_us DESC, id DESC);
         CREATE INDEX IF NOT EXISTS ix_{table}_trace_time
             ON {table} (trace_id, timestamp_us DESC);"
    ))
    .with_context(|| format!("cannot create log partition {table}"))
}

fn create_trace_partition(conn: &Connection, table: &str) -> Result<()> {
    // The indexes mirror the four questions traces exist to answer: "this
    // application over this window", "everything over this window", "what
    // failed" - the one the review was written about, which without an
    // outcome index would scan a whole day of invocations - and "this one
    // invocation", which is also how a trace is joined to its log lines.
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {table} (
             id          INTEGER PRIMARY KEY,
             trace_id    TEXT NOT NULL,
             started_us  INTEGER NOT NULL,
             duration_us INTEGER NOT NULL,
             app_id      TEXT NOT NULL,
             environment TEXT NOT NULL,
             node        TEXT NOT NULL,
             kind        INTEGER NOT NULL,
             outcome     INTEGER NOT NULL,
             target      TEXT NOT NULL,
             method      TEXT NOT NULL,
             status      INTEGER NOT NULL,
             error       TEXT NOT NULL,
             attributes  TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS ix_{table}_app_time
             ON {table} (app_id, started_us DESC, id DESC);
         CREATE INDEX IF NOT EXISTS ix_{table}_time
             ON {table} (started_us DESC, id DESC);
         CREATE INDEX IF NOT EXISTS ix_{table}_outcome_time
             ON {table} (outcome, started_us DESC, id DESC);
         CREATE INDEX IF NOT EXISTS ix_{table}_trace
             ON {table} (trace_id, started_us DESC);"
    ))
    .with_context(|| format!("cannot create trace partition {table}"))
}

fn list_partitions(conn: &Connection, prefix: &str) -> Result<Vec<i64>> {
    // `_` is a single-character wildcard in LIKE, so the prefix is escaped:
    // an unescaped `traces_%` would also match a hypothetical `tracesX...`,
    // and an unescaped `logs_%` would match anything starting with `logs`.
    let pattern = format!("{}%", prefix.replace('_', "\\_"));
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name LIKE ?1 ESCAPE '\\'",
        )
        .context("cannot list day partitions")?;
    let rows = stmt
        .query_map([pattern], |row| row.get::<_, String>(0))
        .context("cannot read the day partition list")?;
    let mut days = Vec::new();
    for row in rows {
        let name = row.context("cannot read a day partition name")?;
        match day_from_partition(prefix, &name) {
            Some(day) => days.push(day),
            None => warn!(
                table = %name,
                prefix,
                "table looks like a day partition but its name is not a date, \
                 it is ignored by queries and by retention"
            ),
        }
    }
    Ok(days)
}

/// Whether a day partition can hold any row inside `[start_us, end_us)`.
fn day_intersects(day: i64, start_us: Option<i64>, end_us: Option<i64>) -> bool {
    let start = day * DAY_US;
    let end = start + DAY_US;
    if let Some(filter_start) = start_us
        && end <= filter_start
    {
        return false;
    }
    if let Some(filter_end) = end_us
        && start >= filter_end
    {
        return false;
    }
    true
}

/// Build the statement for one partition. Separate from execution so a test
/// can hand the exact SQL to `EXPLAIN QUERY PLAN` and prove the indexes are
/// used instead of taking it on trust.
fn build_partition_query(
    table: &str,
    filter: &LogFilter,
    cursor: Option<LogCursor>,
    limit: usize,
) -> (String, Vec<SqlValue>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut args: Vec<SqlValue> = Vec::new();
    if let Some(app_id) = &filter.app_id {
        clauses.push(format!("app_id = ?{}", args.len() + 1));
        args.push(SqlValue::Text(app_id.clone()));
    }
    if let Some(start) = filter.start_us {
        clauses.push(format!("timestamp_us >= ?{}", args.len() + 1));
        args.push(SqlValue::Integer(start));
    }
    if let Some(end) = filter.end_us {
        clauses.push(format!("timestamp_us < ?{}", args.len() + 1));
        args.push(SqlValue::Integer(end));
    }
    if let Some(min_level) = filter.min_level {
        clauses.push(format!("level >= ?{}", args.len() + 1));
        args.push(SqlValue::Integer(i64::from(min_level)));
    }
    if let Some(trace_id) = &filter.trace_id {
        clauses.push(format!("trace_id = ?{}", args.len() + 1));
        args.push(SqlValue::Text(trace_id.clone()));
    }
    if let Some(needle) = &filter.contains {
        clauses.push(format!(
            "instr(lower(message), lower(?{})) > 0",
            args.len() + 1
        ));
        args.push(SqlValue::Text(needle.clone()));
    }
    if let Some(cursor) = cursor {
        // Strictly older than the cursor row, with the row id breaking ties
        // between entries stamped at the same microsecond.
        let ts_slot = args.len() + 1;
        let id_slot = args.len() + 2;
        clauses.push(format!(
            "(timestamp_us < ?{ts_slot} OR (timestamp_us = ?{ts_slot} AND id < ?{id_slot}))"
        ));
        args.push(SqlValue::Integer(cursor.timestamp_us));
        args.push(SqlValue::Integer(cursor.row_id));
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    let limit_slot = args.len() + 1;
    args.push(SqlValue::Integer(limit as i64));
    let sql = format!(
        "SELECT id, timestamp_us, level, component, app_id, message, fields, trace_id, span_id
         FROM {table}{where_sql}
         ORDER BY timestamp_us DESC, id DESC
         LIMIT ?{limit_slot}"
    );
    (sql, args)
}

/// Build the metric statement.
///
/// There is no `LIMIT`: label selection happens in Rust, so the caller stops
/// stepping once its page is full. The ordering matches the indexes, so
/// SQLite streams rows in order instead of sorting the whole table first.
fn build_metric_query(
    filter: &MetricFilter,
    cursor: Option<MetricCursor>,
) -> (String, Vec<SqlValue>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut args: Vec<SqlValue> = Vec::new();
    if let Some(name) = &filter.name {
        clauses.push(format!("name = ?{}", args.len() + 1));
        args.push(SqlValue::Text(name.clone()));
    }
    if let Some(app_id) = &filter.app_id {
        clauses.push(format!("app_id = ?{}", args.len() + 1));
        args.push(SqlValue::Text(app_id.clone()));
    }
    if let Some(start) = filter.start_us {
        clauses.push(format!("timestamp_us >= ?{}", args.len() + 1));
        args.push(SqlValue::Integer(start));
    }
    if let Some(end) = filter.end_us {
        clauses.push(format!("timestamp_us < ?{}", args.len() + 1));
        args.push(SqlValue::Integer(end));
    }
    if let Some(cursor) = cursor {
        let ts_slot = args.len() + 1;
        let id_slot = args.len() + 2;
        clauses.push(format!(
            "(timestamp_us < ?{ts_slot} OR (timestamp_us = ?{ts_slot} AND id < ?{id_slot}))"
        ));
        args.push(SqlValue::Integer(cursor.timestamp_us));
        args.push(SqlValue::Integer(cursor.row_id));
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    let sql = format!(
        "SELECT id, name, app_id, labels, value, timestamp_us
         FROM metrics{where_sql}
         ORDER BY timestamp_us DESC, id DESC"
    );
    (sql, args)
}

/// Read up to `limit` rows of one partition, stopping early once `budget`
/// is spent. Rows are stepped one at a time, so a partition of large rows
/// costs the page's budget in memory and no more.
fn query_partition(
    conn: &Connection,
    table: &str,
    day: i64,
    filter: &LogFilter,
    cursor: Option<LogCursor>,
    limit: usize,
    budget: &mut PageBudget,
) -> Result<Vec<(LogCursor, LogRecord)>> {
    let (sql, args) = build_partition_query(table, filter, cursor, limit);
    let mut stmt = prepare_stmt(conn, &sql)?;
    let rows = stmt
        .query_map(params_from_iter(args.iter()), |row| {
            let id: i64 = row.get(0)?;
            let timestamp_us: i64 = row.get(1)?;
            let fields_json: String = row.get(6)?;
            Ok((
                LogCursor {
                    day,
                    timestamp_us,
                    row_id: id,
                },
                LogRecord {
                    timestamp_us,
                    level: row.get(2)?,
                    component: row.get(3)?,
                    app_id: row.get(4)?,
                    message: row.get(5)?,
                    fields: serde_json::from_str(&fields_json).unwrap_or_else(|e| {
                        warn!(
                            log_row = id,
                            error = %e,
                            "log fields are not valid JSON, entry is returned without fields"
                        );
                        BTreeMap::new()
                    }),
                    trace_id: row.get(7)?,
                    span_id: row.get(8)?,
                },
            ))
        })
        .with_context(|| format!("cannot query log partition {table}"))?;
    let mut out = Vec::new();
    for row in rows {
        let (cursor, record) = row.with_context(|| format!("cannot read row of {table}"))?;
        if !budget.admit(&record) {
            break;
        }
        out.push((cursor, record));
    }
    Ok(out)
}

/// Build the statement for one trace partition. Separate from execution for
/// the same reason the log one is: a test can hand the exact SQL to
/// `EXPLAIN QUERY PLAN` and prove the indexes are used.
fn build_trace_partition_query(
    table: &str,
    filter: &TraceFilter,
    cursor: Option<TraceCursor>,
    limit: usize,
) -> (String, Vec<SqlValue>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut args: Vec<SqlValue> = Vec::new();
    if let Some(app_id) = &filter.app_id {
        clauses.push(format!("app_id = ?{}", args.len() + 1));
        args.push(SqlValue::Text(app_id.clone()));
    }
    if let Some(environment) = &filter.environment {
        clauses.push(format!("environment = ?{}", args.len() + 1));
        args.push(SqlValue::Text(environment.clone()));
    }
    if let Some(start) = filter.start_us {
        clauses.push(format!("started_us >= ?{}", args.len() + 1));
        args.push(SqlValue::Integer(start));
    }
    if let Some(end) = filter.end_us {
        clauses.push(format!("started_us < ?{}", args.len() + 1));
        args.push(SqlValue::Integer(end));
    }
    for (column, wanted) in [("kind", &filter.kinds), ("outcome", &filter.outcomes)] {
        if wanted.is_empty() {
            continue;
        }
        let slots: Vec<String> = wanted
            .iter()
            .map(|value| {
                args.push(SqlValue::Integer(i64::from(*value)));
                format!("?{}", args.len())
            })
            .collect();
        clauses.push(format!("{column} IN ({})", slots.join(", ")));
    }
    if let Some(trace_id) = &filter.trace_id {
        clauses.push(format!("trace_id = ?{}", args.len() + 1));
        args.push(SqlValue::Text(trace_id.clone()));
    }
    if let Some(floor) = filter.min_duration_us {
        clauses.push(format!("duration_us >= ?{}", args.len() + 1));
        args.push(SqlValue::Integer(floor));
    }
    if let Some(needle) = &filter.target_contains {
        clauses.push(format!(
            "instr(lower(target), lower(?{})) > 0",
            args.len() + 1
        ));
        args.push(SqlValue::Text(needle.clone()));
    }
    if let Some(cursor) = cursor {
        let ts_slot = args.len() + 1;
        let id_slot = args.len() + 2;
        clauses.push(format!(
            "(started_us < ?{ts_slot} OR (started_us = ?{ts_slot} AND id < ?{id_slot}))"
        ));
        args.push(SqlValue::Integer(cursor.started_us));
        args.push(SqlValue::Integer(cursor.row_id));
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    let limit_slot = args.len() + 1;
    args.push(SqlValue::Integer(limit as i64));
    let sql = format!(
        "SELECT id, trace_id, started_us, duration_us, app_id, environment, node,
                kind, outcome, target, method, status, error, attributes
         FROM {table}{where_sql}
         ORDER BY started_us DESC, id DESC
         LIMIT ?{limit_slot}"
    );
    (sql, args)
}

/// The trace twin of [`query_partition`], under the same byte budget.
fn query_trace_partition(
    conn: &Connection,
    table: &str,
    day: i64,
    filter: &TraceFilter,
    cursor: Option<TraceCursor>,
    limit: usize,
    budget: &mut PageBudget,
) -> Result<Vec<(TraceCursor, TraceRecord)>> {
    let (sql, args) = build_trace_partition_query(table, filter, cursor, limit);
    let mut stmt = prepare_stmt(conn, &sql)?;
    let rows = stmt
        .query_map(params_from_iter(args.iter()), |row| {
            let id: i64 = row.get(0)?;
            let started_us: i64 = row.get(2)?;
            let attributes_json: String = row.get(13)?;
            Ok((
                TraceCursor {
                    day,
                    started_us,
                    row_id: id,
                },
                TraceRecord {
                    trace_id: row.get(1)?,
                    started_us,
                    duration_us: row.get(3)?,
                    app_id: row.get(4)?,
                    environment: row.get(5)?,
                    node: row.get(6)?,
                    kind: row.get(7)?,
                    outcome: row.get(8)?,
                    target: row.get(9)?,
                    method: row.get(10)?,
                    status: row.get(11)?,
                    error: row.get(12)?,
                    attributes: serde_json::from_str(&attributes_json).unwrap_or_else(|e| {
                        warn!(
                            trace_row = id,
                            error = %e,
                            "trace attributes are not valid JSON, the record is returned without them"
                        );
                        BTreeMap::new()
                    }),
                },
            ))
        })
        .with_context(|| format!("cannot query trace partition {table}"))?;
    let mut out = Vec::new();
    for row in rows {
        let (cursor, record) = row.with_context(|| format!("cannot read row of {table}"))?;
        if !budget.admit(&record) {
            break;
        }
        out.push((cursor, record));
    }
    Ok(out)
}

fn prepare_stmt<'a>(conn: &'a Connection, sql: &str) -> Result<rusqlite::Statement<'a>> {
    conn.prepare(sql)
        .with_context(|| format!("cannot prepare statement: {sql}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-29 00:00:00 UTC, the base for the timestamps below.
    const BASE_US: i64 = 1_787_961_600_000_000;

    fn record(ts: i64, app: &str, level: i32, message: &str) -> LogRecord {
        LogRecord {
            timestamp_us: ts,
            level,
            component: "deis-runtime".into(),
            app_id: app.into(),
            message: message.into(),
            fields: BTreeMap::new(),
            trace_id: String::new(),
            span_id: String::new(),
        }
    }

    fn messages(page: &LogPage) -> Vec<&str> {
        page.records.iter().map(|r| r.message.as_str()).collect()
    }

    /// A read in flight must not stop ingest. With one shared connection this
    /// blocks until the read finishes, so the bounded wait below turns the
    /// stall into a failure instead of a hung test run.
    #[test]
    fn a_read_in_flight_does_not_block_ingest() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("temp dir");
        let store = TelemetryStore::open(dir.path()).expect("store must open");
        store
            .insert_logs(&[record(BASE_US, "shop", 3, "seed")])
            .expect("seed insert must succeed");

        let (reading, release) = (mpsc::channel::<()>(), mpsc::channel::<()>());
        let (ready_tx, ready_rx) = reading;
        let (release_tx, release_rx) = release;
        let reader = {
            let store = store.clone();
            std::thread::spawn(move || {
                store.with_reader(|conn| {
                    let _: i64 =
                        conn.query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| row.get(0))?;
                    let _ = ready_tx.send(());
                    // Hold the connection until the writer is done. A dropped
                    // sender means the test already failed; stop either way.
                    let _ = release_rx.recv();
                    Ok(())
                })
            })
        };
        ready_rx.recv().expect("the reader must start");

        let (written_tx, written_rx) = mpsc::channel();
        let writer = {
            let store = store.clone();
            std::thread::spawn(move || {
                let written = store.insert_logs(&[record(BASE_US + 1, "shop", 3, "during read")]);
                let _ = written_tx.send(written);
            })
        };
        let written = written_rx
            .recv_timeout(Duration::from_secs(10))
            .expect(
                "ingest must proceed while a read is in flight, it waited on the reader instead",
            )
            .expect("the insert itself must succeed");
        assert_eq!(written, 1);

        let _ = release_tx.send(());
        reader
            .join()
            .expect("reader thread must not panic")
            .expect("the read must succeed");
        writer.join().expect("writer thread must not panic");
    }

    /// The read pool is bounded, so a caller that arrives when every
    /// connection is busy has to wait for one to come back. It must actually
    /// come back - a leaked connection or a missed wake-up would wedge every
    /// later query on this process for good.
    #[test]
    fn a_waiting_reader_is_served_once_a_connection_returns() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("temp dir");
        let store = TelemetryStore::open(dir.path()).expect("store must open");

        // Occupy every connection in the pool.
        let mut release = Vec::new();
        let mut holders = Vec::new();
        let (held_tx, held_rx) = mpsc::channel();
        for _ in 0..READER_CONNECTIONS {
            let (release_tx, release_rx) = mpsc::channel::<()>();
            release.push(release_tx);
            let store = store.clone();
            let held_tx = held_tx.clone();
            holders.push(std::thread::spawn(move || {
                store.with_reader(|_| {
                    let _ = held_tx.send(());
                    let _ = release_rx.recv();
                    Ok(())
                })
            }));
        }
        for _ in 0..READER_CONNECTIONS {
            held_rx.recv().expect("every pooled reader must start");
        }

        let (served_tx, served_rx) = mpsc::channel();
        let waiting = {
            let store = store.clone();
            std::thread::spawn(move || {
                let count = store.partition_count();
                let _ = served_tx.send(count);
            })
        };

        // Do not release anything until that caller is genuinely parked on
        // the pool; otherwise it would just find an idle connection and the
        // hand-off below would never be exercised.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while store.readers_waiting() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the extra reader never queued for a connection"
            );
            std::thread::yield_now();
        }

        // Free one connection; the waiting caller must pick it up.
        release.pop().expect("a holder to release").send(()).ok();
        served_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("a waiting reader must be woken when a connection is returned")
            .expect("the query itself must succeed");

        for tx in release {
            let _ = tx.send(());
        }
        for holder in holders {
            holder
                .join()
                .expect("holder thread must not panic")
                .expect("the held read must succeed");
        }
        waiting.join().expect("waiting thread must not panic");
    }

    /// Retention drops whole tables while queries are running. A query that
    /// has already listed the partitions must still be able to read them,
    /// instead of failing halfway with "no such table".
    #[test]
    fn a_query_keeps_its_snapshot_while_retention_drops_a_partition() {
        use std::sync::mpsc;

        let dir = tempfile::tempdir().expect("temp dir");
        let store = TelemetryStore::open(dir.path()).expect("store must open");
        store
            .insert_logs(&[
                record(BASE_US - DAY_US, "shop", 3, "yesterday"),
                record(BASE_US, "shop", 3, "today"),
            ])
            .expect("insert must succeed");
        let expiring =
            partition_table(LOG_PARTITIONS, day_of(BASE_US - DAY_US)).expect("partition name");

        let (listed_tx, listed_rx) = mpsc::channel();
        let (swept_tx, swept_rx) = mpsc::channel::<()>();
        let reader = {
            let store = store.clone();
            let table = expiring.clone();
            std::thread::spawn(move || {
                store.with_reader(|conn| {
                    let _ = listed_tx.send(list_partitions(conn, LOG_PARTITIONS)?.len());
                    // Wait for the sweep to commit, then read the partition
                    // this query already committed to reading.
                    let _ = swept_rx.recv();
                    let rows: i64 =
                        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                            row.get(0)
                        })?;
                    Ok(rows)
                })
            })
        };
        assert_eq!(listed_rx.recv().expect("the reader must list"), 2);

        let outcome = store
            .sweep_retention(BASE_US, 1, 30, 30)
            .expect("sweep must succeed");
        assert_eq!(outcome.dropped_partitions.len(), 1);
        let _ = swept_tx.send(());

        let rows = reader
            .join()
            .expect("reader thread must not panic")
            .expect("the query must keep reading the snapshot it started on");
        assert_eq!(rows, 1);
    }

    #[test]
    fn partition_names_are_dates() {
        assert_eq!(
            partition_table(LOG_PARTITIONS, day_of(BASE_US)).unwrap(),
            "logs_20260829"
        );
        assert_eq!(
            day_from_partition(LOG_PARTITIONS, "logs_20260829"),
            Some(day_of(BASE_US))
        );
        assert_eq!(day_from_partition(LOG_PARTITIONS, "metrics"), None);
        assert_eq!(day_from_partition(LOG_PARTITIONS, "logs_not_a_date"), None);
    }

    #[test]
    fn day_boundaries_split_partitions() {
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_logs(&[
                record(BASE_US - 1, "shop", 3, "yesterday"),
                record(BASE_US, "shop", 3, "today"),
            ])
            .unwrap();
        assert_eq!(store.partition_count().unwrap(), 2);
    }

    #[test]
    fn query_returns_newest_first() {
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_logs(&[
                record(BASE_US + 1, "shop", 3, "first"),
                record(BASE_US + 2, "shop", 3, "second"),
                record(BASE_US + 3, "shop", 3, "third"),
            ])
            .unwrap();
        let page = store.query_logs(&LogFilter::default(), 10, None).unwrap();
        assert_eq!(messages(&page), vec!["third", "second", "first"]);
        assert!(!page.has_more);
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn fields_survive_a_round_trip() {
        let store = TelemetryStore::open_in_memory().unwrap();
        let mut original = record(BASE_US + 1, "shop", 4, "with fields");
        original.fields.insert("route".into(), "/api".into());
        original.trace_id = "trace-1".into();
        original.span_id = "span-1".into();
        store.insert_logs(std::slice::from_ref(&original)).unwrap();
        let page = store.query_logs(&LogFilter::default(), 10, None).unwrap();
        assert_eq!(page.records, vec![original]);
    }

    #[test]
    fn filters_narrow_the_result() {
        let store = TelemetryStore::open_in_memory().unwrap();
        let mut traced = record(BASE_US + 4, "shop", 5, "checkout EXPLODED");
        traced.trace_id = "trace-9".into();
        store
            .insert_logs(&[
                record(BASE_US + 1, "shop", 2, "debug noise"),
                record(BASE_US + 2, "blog", 5, "other app"),
                record(BASE_US + 3, "shop", 3, "routine"),
                traced,
            ])
            .unwrap();

        let by_app = store
            .query_logs(
                &LogFilter {
                    app_id: Some("shop".into()),
                    ..Default::default()
                },
                10,
                None,
            )
            .unwrap();
        assert_eq!(
            messages(&by_app),
            vec!["checkout EXPLODED", "routine", "debug noise"]
        );

        let by_level = store
            .query_logs(
                &LogFilter {
                    min_level: Some(5),
                    ..Default::default()
                },
                10,
                None,
            )
            .unwrap();
        assert_eq!(messages(&by_level), vec!["checkout EXPLODED", "other app"]);

        let by_trace = store
            .query_logs(
                &LogFilter {
                    trace_id: Some("trace-9".into()),
                    ..Default::default()
                },
                10,
                None,
            )
            .unwrap();
        assert_eq!(messages(&by_trace), vec!["checkout EXPLODED"]);

        // Substring search folds ASCII case.
        let by_text = store
            .query_logs(
                &LogFilter {
                    contains: Some("exploded".into()),
                    ..Default::default()
                },
                10,
                None,
            )
            .unwrap();
        assert_eq!(messages(&by_text), vec!["checkout EXPLODED"]);

        let by_window = store
            .query_logs(
                &LogFilter {
                    start_us: Some(BASE_US + 2),
                    end_us: Some(BASE_US + 4),
                    ..Default::default()
                },
                10,
                None,
            )
            .unwrap();
        assert_eq!(messages(&by_window), vec!["routine", "other app"]);
    }

    #[test]
    fn pages_walk_backwards_without_gaps_or_repeats() {
        let store = TelemetryStore::open_in_memory().unwrap();
        let batch: Vec<LogRecord> = (0..10)
            .map(|i| record(BASE_US + i, "shop", 3, &format!("entry {i}")))
            .collect();
        store.insert_logs(&batch).unwrap();

        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = store.query_logs(&LogFilter::default(), 3, cursor).unwrap();
            seen.extend(page.records.iter().map(|r| r.message.clone()));
            cursor = page.next_cursor;
            if !page.has_more {
                break;
            }
        }
        let expected: Vec<String> = (0..10).rev().map(|i| format!("entry {i}")).collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn entries_sharing_a_timestamp_do_not_repeat_across_pages() {
        let store = TelemetryStore::open_in_memory().unwrap();
        // Three entries stamped at the same microsecond: only the row id can
        // break the tie, which is what the cursor carries.
        let batch: Vec<LogRecord> = (0..3)
            .map(|i| record(BASE_US, "shop", 3, &format!("same instant {i}")))
            .collect();
        store.insert_logs(&batch).unwrap();

        let first = store.query_logs(&LogFilter::default(), 2, None).unwrap();
        assert_eq!(messages(&first), vec!["same instant 2", "same instant 1"]);
        assert!(first.has_more);
        let second = store
            .query_logs(&LogFilter::default(), 2, first.next_cursor)
            .unwrap();
        assert_eq!(messages(&second), vec!["same instant 0"]);
        assert!(!second.has_more);
    }

    #[test]
    fn pages_cross_day_partitions() {
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_logs(&[
                record(BASE_US - DAY_US - 1, "shop", 3, "two days ago"),
                record(BASE_US - 1, "shop", 3, "yesterday"),
                record(BASE_US + 1, "shop", 3, "today"),
            ])
            .unwrap();
        assert_eq!(store.partition_count().unwrap(), 3);

        let first = store.query_logs(&LogFilter::default(), 2, None).unwrap();
        assert_eq!(messages(&first), vec!["today", "yesterday"]);
        assert!(first.has_more);
        let second = store
            .query_logs(&LogFilter::default(), 2, first.next_cursor)
            .unwrap();
        assert_eq!(messages(&second), vec!["two days ago"]);
        assert!(!second.has_more);
    }

    #[test]
    fn a_time_window_skips_partitions_entirely() {
        let filter = LogFilter {
            start_us: Some(BASE_US),
            end_us: Some(BASE_US + DAY_US),
            ..Default::default()
        };
        assert!(day_intersects(
            day_of(BASE_US),
            filter.start_us,
            filter.end_us
        ));
        assert!(!day_intersects(
            day_of(BASE_US) - 1,
            filter.start_us,
            filter.end_us
        ));
        assert!(!day_intersects(
            day_of(BASE_US) + 1,
            filter.start_us,
            filter.end_us
        ));
    }

    #[test]
    fn cursor_round_trips_and_rejects_junk() {
        let cursor = LogCursor {
            day: 20_000,
            timestamp_us: BASE_US,
            row_id: 42,
        };
        assert_eq!(LogCursor::decode(&cursor.encode()).unwrap(), cursor);
        assert!(LogCursor::decode("").is_err());
        assert!(LogCursor::decode("1:2").is_err());
        assert!(LogCursor::decode("1:2:3:4").is_err());
        assert!(LogCursor::decode("a:b:c").is_err());
    }

    #[test]
    fn retention_drops_whole_expired_partitions() {
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_logs(&[
                record(BASE_US - 5 * DAY_US, "shop", 3, "long gone"),
                record(BASE_US - 2 * DAY_US, "shop", 3, "still wanted"),
                record(BASE_US + 1, "shop", 3, "fresh"),
            ])
            .unwrap();
        assert_eq!(store.partition_count().unwrap(), 3);

        // Three days of retention keeps today and the two days before it.
        let outcome = store.sweep_retention(BASE_US, 3, 30, 30).unwrap();
        assert_eq!(outcome.dropped_partitions.len(), 1);
        assert_eq!(outcome.dropped_partitions[0].rows, 1);
        assert_eq!(outcome.dropped_log_rows(), 1);
        assert_eq!(store.partition_count().unwrap(), 2);

        let page = store.query_logs(&LogFilter::default(), 10, None).unwrap();
        assert_eq!(messages(&page), vec!["fresh", "still wanted"]);

        // A second sweep with nothing expired reports nothing.
        assert!(
            store
                .sweep_retention(BASE_US, 3, 30, 30)
                .unwrap()
                .is_empty()
        );
    }

    /// Kind ordinals, mirroring `InvocationKind` in the proto.
    const KIND_HTTP: i32 = 1;
    const KIND_QUEUE: i32 = 2;
    /// Outcome ordinals, mirroring `InvocationOutcome` in the proto.
    const OUTCOME_OK: i32 = 1;
    const OUTCOME_APP_ERROR: i32 = 2;
    const OUTCOME_TIMEOUT: i32 = 3;

    fn trace(started_us: i64, app: &str, outcome: i32, target: &str) -> TraceRecord {
        TraceRecord {
            trace_id: format!("t-{started_us}"),
            started_us,
            duration_us: 1_000,
            app_id: app.to_string(),
            environment: "production".to_string(),
            node: "node-a".to_string(),
            kind: KIND_HTTP,
            outcome,
            target: target.to_string(),
            method: "GET".to_string(),
            status: if outcome == OUTCOME_OK { 200 } else { 500 },
            error: String::new(),
            attributes: BTreeMap::new(),
        }
    }

    fn targets(page: &TracePage) -> Vec<&str> {
        page.records.iter().map(|r| r.target.as_str()).collect()
    }

    #[test]
    fn traces_come_back_newest_first_with_their_attributes() {
        let store = TelemetryStore::open_in_memory().unwrap();
        let mut first = trace(BASE_US + 1, "shop", OUTCOME_OK, "/a");
        first
            .attributes
            .insert("retry".to_string(), "0".to_string());
        store
            .insert_traces(&[first, trace(BASE_US + 2, "shop", OUTCOME_OK, "/b")])
            .unwrap();

        let page = store
            .query_traces(&TraceFilter::default(), 10, None)
            .unwrap();
        assert_eq!(targets(&page), vec!["/b", "/a"]);
        assert_eq!(
            page.records[1].attributes.get("retry").map(String::as_str),
            Some("0")
        );
        assert!(!page.has_more);
    }

    #[test]
    fn the_question_the_trace_surface_exists_for_is_one_query() {
        // "Show me the requests that failed in the last hour" -- a window and
        // an outcome selection, answered without reading a single log line.
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_traces(&[
                trace(BASE_US + 1, "shop", OUTCOME_OK, "/healthy"),
                trace(BASE_US + 2, "shop", OUTCOME_APP_ERROR, "/threw"),
                trace(BASE_US + 3, "shop", OUTCOME_TIMEOUT, "/hung"),
                trace(BASE_US + 4, "blog", OUTCOME_APP_ERROR, "/other-app"),
            ])
            .unwrap();

        let filter = TraceFilter {
            app_id: Some("shop".to_string()),
            start_us: Some(BASE_US),
            outcomes: vec![OUTCOME_APP_ERROR, OUTCOME_TIMEOUT],
            ..TraceFilter::default()
        };
        let page = store.query_traces(&filter, 10, None).unwrap();
        assert_eq!(targets(&page), vec!["/hung", "/threw"]);
    }

    #[test]
    fn a_cut_off_is_distinguishable_from_an_application_error() {
        // The single most useful thing an operator can be told, and the whole
        // reason the outcome is stored rather than inferred from the status.
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_traces(&[
                trace(BASE_US + 1, "shop", OUTCOME_APP_ERROR, "/threw"),
                trace(BASE_US + 2, "shop", OUTCOME_TIMEOUT, "/hung"),
            ])
            .unwrap();

        let filter = TraceFilter {
            outcomes: vec![OUTCOME_TIMEOUT],
            ..TraceFilter::default()
        };
        let page = store.query_traces(&filter, 10, None).unwrap();
        assert_eq!(targets(&page), vec!["/hung"]);
    }

    #[test]
    fn a_trace_can_be_found_by_the_id_its_log_lines_carry() {
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_traces(&[
                trace(BASE_US + 1, "shop", OUTCOME_OK, "/a"),
                trace(BASE_US + 2, "shop", OUTCOME_OK, "/b"),
            ])
            .unwrap();

        let filter = TraceFilter {
            trace_id: Some(format!("t-{}", BASE_US + 2)),
            ..TraceFilter::default()
        };
        let page = store.query_traces(&filter, 10, None).unwrap();
        assert_eq!(targets(&page), vec!["/b"]);
    }

    #[test]
    fn traces_filter_on_kind_environment_duration_and_target() {
        let store = TelemetryStore::open_in_memory().unwrap();
        let mut slow = trace(BASE_US + 1, "shop", OUTCOME_OK, "/checkout");
        slow.duration_us = 5_000_000;
        let mut batch = trace(BASE_US + 2, "shop", OUTCOME_OK, "orders");
        batch.kind = KIND_QUEUE;
        let mut preview = trace(BASE_US + 3, "shop", OUTCOME_OK, "/checkout");
        preview.environment = "preview".to_string();
        store.insert_traces(&[slow, batch, preview]).unwrap();

        let by_kind = store
            .query_traces(
                &TraceFilter {
                    kinds: vec![KIND_QUEUE],
                    ..TraceFilter::default()
                },
                10,
                None,
            )
            .unwrap();
        assert_eq!(targets(&by_kind), vec!["orders"]);

        let by_environment = store
            .query_traces(
                &TraceFilter {
                    environment: Some("preview".to_string()),
                    ..TraceFilter::default()
                },
                10,
                None,
            )
            .unwrap();
        assert_eq!(by_environment.records.len(), 1);

        let by_duration = store
            .query_traces(
                &TraceFilter {
                    min_duration_us: Some(2_000_000),
                    ..TraceFilter::default()
                },
                10,
                None,
            )
            .unwrap();
        assert_eq!(by_duration.records.len(), 1);
        assert_eq!(by_duration.records[0].duration_us, 5_000_000);

        let by_target = store
            .query_traces(
                &TraceFilter {
                    target_contains: Some("CHECK".to_string()),
                    ..TraceFilter::default()
                },
                10,
                None,
            )
            .unwrap();
        assert_eq!(by_target.records.len(), 2);
    }

    #[test]
    fn trace_pages_cross_day_partitions() {
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_traces(&[
                trace(BASE_US - DAY_US, "shop", OUTCOME_OK, "/yesterday"),
                trace(BASE_US + 1, "shop", OUTCOME_OK, "/today"),
            ])
            .unwrap();
        assert_eq!(store.trace_partition_count().unwrap(), 2);

        let first = store
            .query_traces(&TraceFilter::default(), 1, None)
            .unwrap();
        assert_eq!(targets(&first), vec!["/today"]);
        assert!(first.has_more);

        let second = store
            .query_traces(&TraceFilter::default(), 1, first.next_cursor)
            .unwrap();
        assert_eq!(targets(&second), vec!["/yesterday"]);
        assert!(!second.has_more);
    }

    #[test]
    fn traces_expire_on_their_own_shorter_schedule() {
        // Traces are higher volume than logs, so they are kept for fewer days
        // and the two sweeps must not be able to take each other's data.
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_logs(&[record(BASE_US - 4 * DAY_US, "shop", 3, "old log")])
            .unwrap();
        store
            .insert_traces(&[
                trace(BASE_US - 4 * DAY_US, "shop", OUTCOME_OK, "/old"),
                trace(BASE_US + 1, "shop", OUTCOME_OK, "/fresh"),
            ])
            .unwrap();

        let outcome = store.sweep_retention(BASE_US, 7, 30, 2).unwrap();
        assert_eq!(outcome.dropped_trace_partitions.len(), 1);
        assert_eq!(outcome.dropped_trace_rows(), 1);
        assert!(
            outcome.dropped_partitions.is_empty(),
            "the log partition is inside its own seven-day window and must survive"
        );
        assert_eq!(store.partition_count().unwrap(), 1);

        let page = store
            .query_traces(&TraceFilter::default(), 10, None)
            .unwrap();
        assert_eq!(targets(&page), vec!["/fresh"]);
    }

    #[test]
    fn the_failed_invocation_query_uses_an_index() {
        // Without the outcome index this scans a whole day of invocations,
        // and a day of invocations is the volume traces exist to survive.
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_traces(&[trace(BASE_US + 1, "shop", OUTCOME_APP_ERROR, "/threw")])
            .unwrap();
        let table = partition_table(TRACE_PARTITIONS, day_of(BASE_US)).unwrap();
        let (sql, args) = build_trace_partition_query(
            &table,
            &TraceFilter {
                outcomes: vec![OUTCOME_APP_ERROR],
                start_us: Some(BASE_US),
                end_us: Some(BASE_US + DAY_US),
                ..TraceFilter::default()
            },
            None,
            10,
        );
        let plan = explain(&store, &sql, &args);
        assert!(
            plan.contains(&format!("USING INDEX ix_{table}_outcome_time")),
            "the failed-invocation query must not scan the day: {plan}"
        );
    }

    #[test]
    fn the_trace_id_join_between_a_trace_and_its_log_lines_uses_an_index() {
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_traces(&[trace(BASE_US + 1, "shop", OUTCOME_OK, "/a")])
            .unwrap();
        let table = partition_table(TRACE_PARTITIONS, day_of(BASE_US)).unwrap();
        let (sql, args) = build_trace_partition_query(
            &table,
            &TraceFilter {
                trace_id: Some("t".to_string()),
                ..TraceFilter::default()
            },
            None,
            10,
        );
        let plan = explain(&store, &sql, &args);
        assert!(
            plan.contains(&format!("USING INDEX ix_{table}_trace")),
            "looking one invocation up by id must not scan the day: {plan}"
        );
    }

    #[test]
    fn retention_deletes_expired_metric_samples() {
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_metrics(&[
                metric("requests", "shop", &[], 1.0, BASE_US - 5 * DAY_US),
                metric("requests", "shop", &[], 2.0, BASE_US),
            ])
            .unwrap();
        let outcome = store.sweep_retention(BASE_US, 30, 2, 30).unwrap();
        assert_eq!(outcome.deleted_metric_samples, 1);
        assert_eq!(store.metric_sample_count().unwrap(), 1);
    }

    fn metric(name: &str, app: &str, labels: &[(&str, &str)], value: f64, ts: i64) -> MetricRecord {
        MetricRecord {
            name: name.into(),
            app_id: app.into(),
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            value,
            timestamp_us: ts,
        }
    }

    #[test]
    fn metric_query_filters_and_pages() {
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_metrics(&[
                metric("requests", "shop", &[("route", "/a")], 1.0, BASE_US + 1),
                metric("requests", "shop", &[("route", "/b")], 2.0, BASE_US + 2),
                metric("requests", "blog", &[("route", "/a")], 3.0, BASE_US + 3),
                metric("latency", "shop", &[], 4.0, BASE_US + 4),
            ])
            .unwrap();

        let by_name = store
            .query_metrics(
                &MetricFilter {
                    name: Some("requests".into()),
                    ..Default::default()
                },
                10,
                None,
            )
            .unwrap();
        assert_eq!(by_name.records.len(), 3);
        // Newest first.
        assert_eq!(by_name.records[0].value, 3.0);

        let by_label = store
            .query_metrics(
                &MetricFilter {
                    name: Some("requests".into()),
                    label_selector: [("route".to_string(), "/a".to_string())]
                        .into_iter()
                        .collect(),
                    ..Default::default()
                },
                10,
                None,
            )
            .unwrap();
        assert_eq!(by_label.records.len(), 2);
        assert!(by_label.records.iter().all(|r| r.labels["route"] == "/a"));

        let first = store
            .query_metrics(&MetricFilter::default(), 2, None)
            .unwrap();
        assert!(first.has_more);
        let second = store
            .query_metrics(&MetricFilter::default(), 2, first.next_cursor)
            .unwrap();
        assert_eq!(second.records.len(), 2);
        assert!(!second.has_more);
        assert_eq!(second.records[0].value, 2.0);
    }

    #[test]
    fn export_takes_the_latest_sample_inside_the_window() {
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_metrics(&[
                metric("requests", "shop", &[], 1.0, BASE_US + 1),
                metric("requests", "shop", &[], 7.0, BASE_US + 2),
                metric("stale", "shop", &[], 9.0, BASE_US - DAY_US),
            ])
            .unwrap();
        let exported = store.latest_metric_samples(BASE_US).unwrap();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].name, "requests");
        assert_eq!(exported[0].value, 7.0);
    }

    /// Ask SQLite how it intends to run a statement. This goes through the
    /// same read path a query does, so the plan is the one queries get.
    fn explain(store: &TelemetryStore, sql: &str, args: &[SqlValue]) -> String {
        store
            .with_reader(|conn| {
                let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                let rows =
                    stmt.query_map(params_from_iter(args.iter()), |row| row.get::<_, String>(3))?;
                let mut steps = Vec::new();
                for row in rows {
                    steps.push(row?);
                }
                Ok(steps.join(" | "))
            })
            .expect("the plan query must run")
    }

    #[test]
    fn filtering_by_app_and_time_uses_an_index() {
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_logs(&[record(BASE_US + 1, "shop", 3, "indexed")])
            .unwrap();
        let table = partition_table(LOG_PARTITIONS, day_of(BASE_US)).unwrap();
        let filter = LogFilter {
            app_id: Some("shop".into()),
            start_us: Some(BASE_US),
            end_us: Some(BASE_US + DAY_US),
            ..Default::default()
        };
        let (sql, args) = build_partition_query(&table, &filter, None, 10);
        let plan = explain(&store, &sql, &args);
        assert!(
            plan.contains(&format!("USING INDEX ix_{table}_app_time")),
            "the app+time query must not scan the day: {plan}"
        );
        assert!(
            !plan.contains("TEMP B-TREE"),
            "the index must supply the ordering too: {plan}"
        );
    }

    #[test]
    fn a_query_without_an_app_still_uses_the_time_index() {
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_logs(&[record(BASE_US + 1, "shop", 3, "indexed")])
            .unwrap();
        let table = partition_table(LOG_PARTITIONS, day_of(BASE_US)).unwrap();
        let (sql, args) = build_partition_query(&table, &LogFilter::default(), None, 10);
        let plan = explain(&store, &sql, &args);
        assert!(
            plan.contains(&format!("USING INDEX ix_{table}_time")),
            "the newest-first walk must follow the time index: {plan}"
        );
        assert!(
            !plan.contains("TEMP B-TREE"),
            "no sort should be needed: {plan}"
        );
    }

    #[test]
    fn a_trace_lookup_uses_the_trace_index() {
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_logs(&[record(BASE_US + 1, "shop", 3, "indexed")])
            .unwrap();
        let table = partition_table(LOG_PARTITIONS, day_of(BASE_US)).unwrap();
        let filter = LogFilter {
            trace_id: Some("trace-1".into()),
            ..Default::default()
        };
        let (sql, args) = build_partition_query(&table, &filter, None, 10);
        let plan = explain(&store, &sql, &args);
        assert!(
            plan.contains(&format!("USING INDEX ix_{table}_trace_time")),
            "a trace lookup must not scan the day: {plan}"
        );
    }

    #[test]
    fn metric_queries_use_the_series_index() {
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_metrics(&[metric("requests", "shop", &[], 1.0, BASE_US)])
            .unwrap();
        let filter = MetricFilter {
            name: Some("requests".into()),
            app_id: Some("shop".into()),
            start_us: Some(BASE_US),
            ..Default::default()
        };
        let (sql, args) = build_metric_query(&filter, None);
        let plan = explain(&store, &sql, &args);
        assert!(
            plan.contains("USING INDEX ix_metrics_series_time"),
            "a series query must not scan the table: {plan}"
        );
        assert!(
            !plan.contains("TEMP B-TREE"),
            "no sort should be needed: {plan}"
        );
    }

    #[test]
    fn metric_pages_are_ordered_by_sample_time_not_insert_order() {
        let store = TelemetryStore::open_in_memory().unwrap();
        // The later sample is written first, as happens when a producer
        // flushes a backlog after a newer batch already landed.
        store
            .insert_metrics(&[
                metric("requests", "shop", &[], 9.0, BASE_US + 100),
                metric("requests", "shop", &[], 1.0, BASE_US + 1),
            ])
            .unwrap();
        let page = store
            .query_metrics(&MetricFilter::default(), 10, None)
            .unwrap();
        assert_eq!(
            page.records.iter().map(|r| r.value).collect::<Vec<_>>(),
            vec![9.0, 1.0]
        );
    }

    #[test]
    fn metric_cursor_round_trips_and_rejects_junk() {
        let cursor = MetricCursor {
            timestamp_us: BASE_US,
            row_id: 7,
        };
        assert_eq!(MetricCursor::decode(&cursor.encode()).unwrap(), cursor);
        assert!(MetricCursor::decode("1").is_err());
        assert!(MetricCursor::decode("1:2:3").is_err());
        assert!(MetricCursor::decode("x:y").is_err());
    }

    #[test]
    fn empty_batches_do_nothing() {
        let store = TelemetryStore::open_in_memory().unwrap();
        assert_eq!(store.insert_logs(&[]).unwrap(), 0);
        assert_eq!(store.insert_metrics(&[]).unwrap(), 0);
        assert_eq!(store.partition_count().unwrap(), 0);
    }

    #[test]
    fn a_query_over_an_empty_database_is_not_an_error() {
        let store = TelemetryStore::open_in_memory().unwrap();
        let page = store.query_logs(&LogFilter::default(), 10, None).unwrap();
        assert!(page.records.is_empty());
        assert!(!page.has_more);
    }

    #[test]
    fn data_survives_reopening_the_file() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = TelemetryStore::open(dir.path()).unwrap();
            store
                .insert_logs(&[record(BASE_US + 1, "shop", 3, "persisted")])
                .unwrap();
        }
        let reopened = TelemetryStore::open(dir.path()).unwrap();
        let page = reopened
            .query_logs(&LogFilter::default(), 10, None)
            .unwrap();
        assert_eq!(messages(&page), vec!["persisted"]);
    }

    /// A record whose message is `kib` KiB, so a page of them is sized by
    /// bytes long before it is sized by rows.
    fn big_record(ts: i64, kib: usize) -> LogRecord {
        record(ts, "chatty", 3, &"x".repeat(kib * 1024))
    }

    #[test]
    fn a_log_page_stops_at_the_byte_budget_and_the_cursor_resumes_behind_it() {
        // 150 lines of 60 KiB are 9 MiB, over the 8 MiB budget and under
        // the 16 MiB frame twice over once split. Asked for 200 rows, the
        // page must close early, say so, and hand back a cursor that walks
        // the rest without a gap or a repeat.
        let store = TelemetryStore::open_in_memory().unwrap();
        let batch: Vec<LogRecord> = (0..150).map(|i| big_record(BASE_US + i, 60)).collect();
        store.insert_logs(&batch).unwrap();

        let first = store.query_logs(&LogFilter::default(), 200, None).unwrap();
        assert!(first.cut_by_bytes, "9 MiB of rows cannot be one page");
        assert!(first.has_more);
        assert!(first.next_cursor.is_some());
        assert!(
            first.records.len() < 150 && !first.records.is_empty(),
            "got {}",
            first.records.len()
        );
        let bytes: usize = first.records.iter().map(WireSize::wire_size).sum();
        assert!(bytes <= MAX_PAGE_BYTES, "{bytes} bytes on one page");

        let mut seen: Vec<i64> = first.records.iter().map(|r| r.timestamp_us).collect();
        let mut cursor = first.next_cursor;
        while let Some(c) = cursor {
            let page = store
                .query_logs(&LogFilter::default(), 200, Some(c))
                .unwrap();
            seen.extend(page.records.iter().map(|r| r.timestamp_us));
            cursor = page.next_cursor;
            if !page.has_more {
                assert!(!page.cut_by_bytes, "the last page was not cut");
            }
        }
        let expected: Vec<i64> = (0..150).rev().map(|i| BASE_US + i).collect();
        assert_eq!(
            seen, expected,
            "paging must reach every row once, newest first"
        );
    }

    #[test]
    fn a_byte_cut_page_crosses_partitions_the_same_way() {
        // The budget is per page, not per partition: a page that is cut
        // inside an older partition must resume there, not skip the rest of
        // that day.
        let store = TelemetryStore::open_in_memory().unwrap();
        let mut batch: Vec<LogRecord> = (0..40).map(|i| big_record(BASE_US + i, 60)).collect();
        batch.extend((0..120).map(|i| big_record(BASE_US - DAY_US + i, 60)));
        store.insert_logs(&batch).unwrap();
        assert_eq!(store.partition_count().unwrap(), 2);

        let mut seen = 0usize;
        let mut cursor = None;
        loop {
            let page = store
                .query_logs(&LogFilter::default(), 1000, cursor)
                .unwrap();
            seen += page.records.len();
            cursor = page.next_cursor;
            if !page.has_more {
                break;
            }
        }
        assert_eq!(seen, 160);
    }

    #[test]
    fn a_single_record_over_the_budget_still_travels_on_a_page_of_its_own() {
        // The store cannot refuse to hand back a row it holds, and a page
        // that carried nothing would come with a cursor that never moves.
        // Such a row cannot arrive through the service, whose caps keep a
        // record far under the budget; this guards the store's own promise.
        let store = TelemetryStore::open_in_memory().unwrap();
        store
            .insert_logs(&[
                big_record(BASE_US + 1, (MAX_PAGE_BYTES / 1024) + 1),
                record(BASE_US + 2, "chatty", 3, "small"),
            ])
            .unwrap();
        let first = store.query_logs(&LogFilter::default(), 10, None).unwrap();
        assert_eq!(messages(&first), vec!["small"]);
        assert!(first.cut_by_bytes && first.has_more);
        let second = store
            .query_logs(&LogFilter::default(), 10, first.next_cursor)
            .unwrap();
        assert_eq!(
            second.records.len(),
            1,
            "the oversized row is delivered alone"
        );
        assert!(!second.has_more);
    }

    #[test]
    fn a_trace_page_stops_at_the_byte_budget_too() {
        let store = TelemetryStore::open_in_memory().unwrap();
        let batch: Vec<TraceRecord> = (0..150)
            .map(|i| {
                let mut t = trace(BASE_US + i, "shop", OUTCOME_OK, "/big");
                t.attributes
                    .insert("payload".to_string(), "y".repeat(60 * 1024));
                t
            })
            .collect();
        store.insert_traces(&batch).unwrap();

        let first = store
            .query_traces(&TraceFilter::default(), 200, None)
            .unwrap();
        assert!(first.cut_by_bytes && first.has_more);
        assert!(first.records.len() < 150 && !first.records.is_empty());

        let mut seen = first.records.len();
        let mut cursor = first.next_cursor;
        while let Some(c) = cursor {
            let page = store
                .query_traces(&TraceFilter::default(), 200, Some(c))
                .unwrap();
            seen += page.records.len();
            cursor = page.next_cursor;
        }
        assert_eq!(seen, 150);
    }

    #[test]
    fn a_metric_page_stops_at_the_byte_budget_too() {
        let store = TelemetryStore::open_in_memory().unwrap();
        let big = "z".repeat(60 * 1024);
        let batch: Vec<MetricRecord> = (0..150)
            .map(|i| metric("requests", "shop", &[("payload", &big)], 1.0, BASE_US + i))
            .collect();
        store.insert_metrics(&batch).unwrap();

        let first = store
            .query_metrics(&MetricFilter::default(), 200, None)
            .unwrap();
        assert!(first.cut_by_bytes && first.has_more);
        assert!(first.records.len() < 150 && !first.records.is_empty());

        let mut seen = first.records.len();
        let mut cursor = first.next_cursor;
        while let Some(c) = cursor {
            let page = store
                .query_metrics(&MetricFilter::default(), 200, Some(c))
                .unwrap();
            seen += page.records.len();
            cursor = page.next_cursor;
        }
        assert_eq!(seen, 150);
    }

    #[test]
    fn a_batch_over_the_partition_cap_is_refused_before_it_opens_a_transaction() {
        // One record per day for one day more than an insert may touch. The
        // batch is refused whole and no table is created: the point of the
        // cap is that the writer is never held for that many CREATE TABLEs.
        let store = TelemetryStore::open_in_memory().unwrap();
        let days = MAX_PARTITIONS_PER_BATCH as i64 + 1;
        let logs: Vec<LogRecord> = (0..days)
            .map(|d| record(BASE_US - d * DAY_US, "clock", 3, "past"))
            .collect();
        let err = store.insert_logs(&logs).unwrap_err();
        assert!(err.to_string().contains("distinct days"), "{err}");
        assert_eq!(store.partition_count().unwrap(), 0);

        let traces: Vec<TraceRecord> = (0..days)
            .map(|d| trace(BASE_US - d * DAY_US, "clock", OUTCOME_OK, "/past"))
            .collect();
        let err = store.insert_traces(&traces).unwrap_err();
        assert!(err.to_string().contains("distinct days"), "{err}");
        assert_eq!(store.trace_partition_count().unwrap(), 0);

        // Exactly the cap is fine.
        store.insert_logs(&logs[1..]).unwrap();
        assert_eq!(store.partition_count().unwrap(), MAX_PARTITIONS_PER_BATCH);
    }
}
