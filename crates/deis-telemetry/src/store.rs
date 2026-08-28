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
//! Every method here is synchronous and takes the connection lock. Callers on
//! an async runtime must wrap them in `spawn_blocking` so a slow query cannot
//! stall the reactor.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, params_from_iter};
use tracing::{debug, info, warn};

/// Microseconds in one day, the width of a log partition.
const DAY_US: i64 = 86_400_000_000;

/// One stored log record. This is the storage-side shape; the proto type is
/// converted to and from it at the service boundary so storage does not
/// depend on generated code.
#[derive(Debug, Clone, PartialEq)]
pub struct LogRecord {
    pub timestamp_us: i64,
    /// `LogLevel` ordinal. Higher is more severe, which is what makes
    /// `min_level` a plain `>=` comparison.
    pub level: i32,
    pub source: String,
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

/// Filters for [`Store::query_logs`]. `None` means "no restriction"; the
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

/// Filters for [`Store::query_metrics`].
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
    pub deleted_metric_samples: usize,
}

impl SweepOutcome {
    /// True when the sweep removed nothing, so the caller can stay quiet.
    pub fn is_empty(&self) -> bool {
        self.dropped_partitions.is_empty() && self.deleted_metric_samples == 0
    }

    /// Total rows removed from log partitions.
    pub fn dropped_log_rows(&self) -> i64 {
        self.dropped_partitions.iter().map(|p| p.rows).sum()
    }
}

/// Handle to the telemetry database. Cheap to clone; clones share one
/// connection guarded by a mutex.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    /// Open (creating if needed) the database under `data_dir`.
    pub fn open(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("cannot create data dir {}", data_dir.display()))?;
        let path = data_dir.join("telemetry.sqlite");
        let conn = Connection::open(&path)
            .with_context(|| format!("cannot open database {}", path.display()))?;
        let store = Self::from_connection(conn)?;
        info!(path = %path.display(), "telemetry database ready");
        Ok(store)
    }

    /// Open an in-memory database. Used by tests; the data dies with the
    /// handle.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory().context("cannot open in-memory db")?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // WAL keeps the ingest path from blocking readers: a `deis tail`
        // backfill or a dashboard query must not stall log pushes.
        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .context("cannot set journal_mode")?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            // In-memory databases refuse WAL. That is fine, but say so:
            // on a file database it would mean reduced read concurrency.
            debug!(
                journal_mode,
                "database is not in WAL mode, readers and writers will contend"
            );
        }
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("cannot set synchronous")?;
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
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A poisoned lock means another thread panicked mid-statement. The
        // connection itself is still usable and SQLite rolls back any open
        // transaction, so recovering beats taking the whole service down.
        self.conn.lock().unwrap_or_else(|poisoned| {
            warn!("telemetry database lock was poisoned by a panicking thread, recovering");
            poisoned.into_inner()
        })
    }

    /// Append log records, creating day partitions as needed.
    ///
    /// Returns the number of rows written. The whole call is one transaction,
    /// so a batch either lands entirely or not at all.
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

        let mut conn = self.conn();
        let tx = conn.transaction().context("cannot begin log insert")?;
        let mut written = 0usize;
        for (day, day_records) in by_day {
            let table = partition_table(day)?;
            create_partition(&tx, &table)?;
            let sql = format!(
                "INSERT INTO {table}
                     (timestamp_us, level, source, app_id, message, fields, trace_id, span_id)
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
                    record.source,
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
    /// whether an older page exists, and never returns that extra row.
    pub fn query_logs(
        &self,
        filter: &LogFilter,
        limit: usize,
        cursor: Option<LogCursor>,
    ) -> Result<LogPage> {
        let limit = limit.max(1);
        let conn = self.conn();
        let mut partitions = list_partitions(&conn)?;
        partitions.sort_unstable_by(|a, b| b.cmp(a));

        let mut collected: Vec<(LogCursor, LogRecord)> = Vec::new();
        let want = limit + 1;
        for day in partitions {
            if collected.len() >= want {
                break;
            }
            if !partition_intersects(day, filter) {
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
            let table = partition_table(day)?;
            let rows = query_partition(
                &conn,
                &table,
                day,
                filter,
                partition_cursor,
                want - collected.len(),
            )?;
            collected.extend(rows);
        }

        let has_more = collected.len() > limit;
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
        })
    }

    /// Append metric samples. One transaction for the whole batch.
    pub fn insert_metrics(&self, records: &[MetricRecord]) -> Result<usize> {
        if records.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn();
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
    pub fn query_metrics(
        &self,
        filter: &MetricFilter,
        limit: usize,
        cursor: Option<MetricCursor>,
    ) -> Result<MetricPage> {
        let limit = limit.max(1);
        let conn = self.conn();
        let (sql, args) = build_metric_query(filter, cursor);
        let mut stmt = prepare_stmt(&conn, &sql)?;
        let mut rows = stmt
            .query(params_from_iter(args.iter()))
            .context("cannot run metric query")?;

        let mut collected: Vec<(MetricCursor, MetricRecord)> = Vec::new();
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
            collected.push((
                MetricCursor {
                    timestamp_us,
                    row_id: id,
                },
                MetricRecord {
                    name: row.get(1)?,
                    app_id: row.get(2)?,
                    labels,
                    value: row.get(4)?,
                    timestamp_us,
                },
            ));
        }

        let has_more = collected.len() > limit;
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
        })
    }

    /// Latest sample of every series that reported at or after `since_us`.
    ///
    /// This is what `/metrics` exports. Series older than the window are left
    /// out on purpose: exporting a stale value would make a component that
    /// stopped reporting look like it is still reporting.
    pub fn latest_metric_samples(&self, since_us: i64) -> Result<Vec<MetricRecord>> {
        let conn = self.conn();
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
    ) -> Result<SweepOutcome> {
        let today = day_of(now_us);
        let oldest_log_day = today - i64::from(log_retention_days) + 1;
        let metric_cutoff_us = (today - i64::from(metric_retention_days) + 1) * DAY_US;

        let conn = self.conn();
        let mut outcome = SweepOutcome::default();
        for day in list_partitions(&conn)? {
            if day >= oldest_log_day {
                continue;
            }
            let table = partition_table(day)?;
            let rows: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .with_context(|| format!("cannot count rows of {table}"))?;
            conn.execute(&format!("DROP TABLE {table}"), [])
                .with_context(|| format!("cannot drop expired partition {table}"))?;
            outcome
                .dropped_partitions
                .push(DroppedPartition { table, day, rows });
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
        Ok(list_partitions(&self.conn())?.len())
    }

    /// Number of stored metric samples.
    pub fn metric_sample_count(&self) -> Result<i64> {
        self.conn()
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .context("cannot count metric samples")
    }
}

/// Days since the Unix epoch that `timestamp_us` falls in, in UTC.
pub fn day_of(timestamp_us: i64) -> i64 {
    timestamp_us.div_euclid(DAY_US)
}

/// Table name of the partition holding `day`.
fn partition_table(day: i64) -> Result<String> {
    let seconds = day
        .checked_mul(86_400)
        .with_context(|| format!("day {day} is out of range"))?;
    let date = chrono::DateTime::from_timestamp(seconds, 0)
        .with_context(|| format!("day {day} is not a representable date"))?
        .date_naive();
    Ok(format!("logs_{}", date.format("%Y%m%d")))
}

/// Inverse of [`partition_table`], for reading the partition list back out of
/// `sqlite_master`.
fn day_from_partition(table: &str) -> Option<i64> {
    let suffix = table.strip_prefix("logs_")?;
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
             source       TEXT NOT NULL,
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

fn list_partitions(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'logs_%'")
        .context("cannot list log partitions")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .context("cannot read log partition list")?;
    let mut days = Vec::new();
    for row in rows {
        let name = row.context("cannot read log partition name")?;
        match day_from_partition(&name) {
            Some(day) => days.push(day),
            None => warn!(
                table = %name,
                "table looks like a log partition but its name is not a date, \
                 it is ignored by queries and by retention"
            ),
        }
    }
    Ok(days)
}

/// Whether a partition can hold any row inside the filter's time window.
fn partition_intersects(day: i64, filter: &LogFilter) -> bool {
    let start = day * DAY_US;
    let end = start + DAY_US;
    if let Some(filter_start) = filter.start_us
        && end <= filter_start
    {
        return false;
    }
    if let Some(filter_end) = filter.end_us
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
        "SELECT id, timestamp_us, level, source, app_id, message, fields, trace_id, span_id
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

fn query_partition(
    conn: &Connection,
    table: &str,
    day: i64,
    filter: &LogFilter,
    cursor: Option<LogCursor>,
    limit: usize,
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
                    source: row.get(3)?,
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
        out.push(row.with_context(|| format!("cannot read row of {table}"))?);
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
            source: "runtime".into(),
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

    #[test]
    fn partition_names_are_dates() {
        assert_eq!(partition_table(day_of(BASE_US)).unwrap(), "logs_20260829");
        assert_eq!(day_from_partition("logs_20260829"), Some(day_of(BASE_US)));
        assert_eq!(day_from_partition("metrics"), None);
        assert_eq!(day_from_partition("logs_not_a_date"), None);
    }

    #[test]
    fn day_boundaries_split_partitions() {
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        assert!(partition_intersects(day_of(BASE_US), &filter));
        assert!(!partition_intersects(day_of(BASE_US) - 1, &filter));
        assert!(!partition_intersects(day_of(BASE_US) + 1, &filter));
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
        let store = Store::open_in_memory().unwrap();
        store
            .insert_logs(&[
                record(BASE_US - 5 * DAY_US, "shop", 3, "long gone"),
                record(BASE_US - 2 * DAY_US, "shop", 3, "still wanted"),
                record(BASE_US + 1, "shop", 3, "fresh"),
            ])
            .unwrap();
        assert_eq!(store.partition_count().unwrap(), 3);

        // Three days of retention keeps today and the two days before it.
        let outcome = store.sweep_retention(BASE_US, 3, 30).unwrap();
        assert_eq!(outcome.dropped_partitions.len(), 1);
        assert_eq!(outcome.dropped_partitions[0].rows, 1);
        assert_eq!(outcome.dropped_log_rows(), 1);
        assert_eq!(store.partition_count().unwrap(), 2);

        let page = store.query_logs(&LogFilter::default(), 10, None).unwrap();
        assert_eq!(messages(&page), vec!["fresh", "still wanted"]);

        // A second sweep with nothing expired reports nothing.
        assert!(store.sweep_retention(BASE_US, 3, 30).unwrap().is_empty());
    }

    #[test]
    fn retention_deletes_expired_metric_samples() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_metrics(&[
                metric("requests", "shop", &[], 1.0, BASE_US - 5 * DAY_US),
                metric("requests", "shop", &[], 2.0, BASE_US),
            ])
            .unwrap();
        let outcome = store.sweep_retention(BASE_US, 30, 2).unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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

    /// Ask SQLite how it intends to run a statement.
    fn explain(store: &Store, sql: &str, args: &[SqlValue]) -> String {
        let conn = store.conn();
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("the plan query must prepare");
        let rows = stmt
            .query_map(params_from_iter(args.iter()), |row| row.get::<_, String>(3))
            .expect("the plan must run");
        rows.map(|r| r.expect("plan row"))
            .collect::<Vec<_>>()
            .join(" | ")
    }

    #[test]
    fn filtering_by_app_and_time_uses_an_index() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_logs(&[record(BASE_US + 1, "shop", 3, "indexed")])
            .unwrap();
        let table = partition_table(day_of(BASE_US)).unwrap();
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
        let store = Store::open_in_memory().unwrap();
        store
            .insert_logs(&[record(BASE_US + 1, "shop", 3, "indexed")])
            .unwrap();
        let table = partition_table(day_of(BASE_US)).unwrap();
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
        let store = Store::open_in_memory().unwrap();
        store
            .insert_logs(&[record(BASE_US + 1, "shop", 3, "indexed")])
            .unwrap();
        let table = partition_table(day_of(BASE_US)).unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
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
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.insert_logs(&[]).unwrap(), 0);
        assert_eq!(store.insert_metrics(&[]).unwrap(), 0);
        assert_eq!(store.partition_count().unwrap(), 0);
    }

    #[test]
    fn a_query_over_an_empty_database_is_not_an_error() {
        let store = Store::open_in_memory().unwrap();
        let page = store.query_logs(&LogFilter::default(), 10, None).unwrap();
        assert!(page.records.is_empty());
        assert!(!page.has_more);
    }

    #[test]
    fn data_survives_reopening_the_file() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(dir.path()).unwrap();
            store
                .insert_logs(&[record(BASE_US + 1, "shop", 3, "persisted")])
                .unwrap();
        }
        let reopened = Store::open(dir.path()).unwrap();
        let page = reopened
            .query_logs(&LogFilter::default(), 10, None)
            .unwrap();
        assert_eq!(messages(&page), vec!["persisted"]);
    }
}
