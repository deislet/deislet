//! gRPC surface: [`LogService`] (ingest, query, live follow), [`TraceService`]
//! (per-invocation records: ingest, query, live follow) and [`MetricService`]
//! (sample ingest and query). All three are served on the same port and share
//! one database.
//!
//! The service stores what it is given and gives it back. It never evaluates
//! whether anything it stored is healthy, never raises an alert and never
//! calls another component. Whoever wants a judgement makes it themselves,
//! and liveness is always decided by a direct health check against the
//! component in question - never through this service, which would make an
//! outage here look like an outage everywhere.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;

use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, warn};

use crate::auth::{Owner, ReadScope, grant_of};
use crate::identity;
use crate::now_us;
// The generated traits carry the same names as the types implementing them,
// which is the point: proto service and Rust type line up. They are aliased
// only so both can be named in this file.
use crate::proto::log_service_server::LogService as LogServiceRpc;
use crate::proto::metric_service_server::MetricService as MetricServiceRpc;
use crate::proto::trace_service_server::TraceService as TraceServiceRpc;
use crate::proto::{
    LogEntry, MetricSample, PushLogsRequest, PushLogsResponse, PushMetricsRequest,
    PushMetricsResponse, PushTracesRequest, PushTracesResponse, QueryMetricsRequest,
    QueryMetricsResponse, QueryRequest, QueryResponse, QueryTracesRequest, QueryTracesResponse,
    TailRequest, TailTracesRequest, TraceRecord as ProtoTraceRecord,
};
use crate::stats::{Access, ServiceStats};
use crate::store::{
    DAY_US, LogCursor, LogFilter, LogRecord, MAX_PAGE_BYTES, MAX_PARTITIONS_PER_BATCH,
    MetricCursor, MetricFilter, MetricRecord, TelemetryStore, TraceCursor, TraceFilter,
    TraceRecord, day_of,
};
use crate::tail::{LiveFilter, TailFilter, TailHub, TraceHub, TraceTailFilter};

/// Longest message body kept verbatim. Anything longer is stored cut short:
/// a runaway `console.log` must not be able to bloat the database, and a
/// truncated line is still worth more than a rejected one.
pub const MAX_LOG_MESSAGE_BYTES: usize = 64 * 1024;

/// Longest identifier-like string accepted: `app_id`, `component`,
/// `trace_id`, `span_id`, a trace's `environment`, `node` and `method`, a
/// metric's `name`. These name things, so they are refused rather than cut -
/// a cut identifier would file the record under a name nothing else uses -
/// and nothing legitimate comes anywhere near a kilobyte.
pub const MAX_IDENTIFIER_BYTES: usize = 1024;

/// Longest key of a structured field, trace attribute or metric label. A key
/// cannot be cut without risking a collision with another key, so a longer
/// one refuses the record.
pub const MAX_KEY_BYTES: usize = 256;

/// Longest value of a structured field or trace attribute kept verbatim;
/// longer ones are cut and marked, like an oversized message.
pub const MAX_FIELD_VALUE_BYTES: usize = 16 * 1024;

/// Most bytes of structured fields or attributes - keys and values together,
/// after values were cut - one record may carry. Beyond this the record is
/// refused: with every value already capped, only a record built to be large
/// gets here.
pub const MAX_FIELDS_BYTES: usize = 64 * 1024;

/// Most bytes of labels one metric sample may carry. Labels name the series,
/// so a sample over this is refused rather than have its labels cut into a
/// series nobody asked for - which the assertion under it guarantees: no
/// single label value can be cut and still fit under the total.
pub const MAX_LABELS_BYTES: usize = 4 * 1024;
const _: () = assert!(MAX_LABELS_BYTES <= MAX_FIELD_VALUE_BYTES);

// Together these bound what one stored record can weigh: a log entry at
// roughly the message cap plus the fields cap, a trace at its attributes cap
// plus the target and error caps. Both are far under the page budget, which
// is what lets a page cut by bytes always carry at least one record and still
// fit the frame. Only `message`, `target` and `error` were capped before;
// everything else went to the database verbatim, and a single ten-megabyte
// field value made every page that included it impossible to encode.
const _: () =
    assert!(MAX_LOG_MESSAGE_BYTES + MAX_FIELDS_BYTES + 8 * MAX_IDENTIFIER_BYTES < MAX_PAGE_BYTES);

/// Marker appended to a message that was cut short, so a reader never mistakes
/// the truncation for the end of the original line.
const TRUNCATION_MARKER: &str = "...[truncated by deis-telemetry]";

/// How far ahead of the server clock a timestamp may be before the entry is
/// treated as malformed. Accepting far-future stamps would create partitions
/// that retention never reaches and that sort ahead of everything real.
const MAX_FUTURE_SKEW_US: i64 = 3_600_000_000;

/// Highest defined `LogLevel` ordinal (`FATAL`).
const MAX_LOG_LEVEL: i32 = 6;

/// Highest defined `InvocationKind` ordinal (`WEBSOCKET_MESSAGE`).
const MAX_INVOCATION_KIND: i32 = 4;

/// Highest defined `InvocationOutcome` ordinal (`CPU_LIMIT`).
const MAX_INVOCATION_OUTCOME: i32 = 7;

/// Longest `target` kept verbatim. A URL is attacker-supplied - a query
/// string can be megabytes - and one record per invocation multiplies
/// whatever a single one costs, so it is cut rather than stored whole.
pub const MAX_TRACE_TARGET_BYTES: usize = 2 * 1024;

/// Longest `error` kept verbatim. A stack trace is worth keeping; a stack
/// trace repeated for every invocation of a failing application is not.
pub const MAX_TRACE_ERROR_BYTES: usize = 8 * 1024;

/// Page-size policy, taken from the configuration.
#[derive(Debug, Clone, Copy)]
pub struct QueryLimits {
    pub default_limit: u32,
    pub max_limit: u32,
}

/// The retention policy, as ingest needs to know it.
///
/// A record older than what the next sweep would delete is refused at the
/// door. Storing it first would be worse on both counts that matter: the
/// data is gone within the hour either way, and getting it there creates a
/// day partition - a table and its indexes, inside the one write transaction
/// on the one write connection - per distinct day the batch names. One batch
/// of records spread over a few thousand past days held the writer, and with
/// it every other producer's push, for tens of seconds.
///
/// The one legitimate source of such stamps is a node whose clock is wrong.
/// Refusing is what serves that operator: the push answer names the stamp
/// and the window in `error_message`, so the producer can log it, and the
/// alternative - filing the record under a stamp it did not carry - would
/// hide the wrong clock behind data that looks right and is not. A producer
/// that is late by less than the window, a node that was offline for a day,
/// is still taken.
#[derive(Debug, Clone, Copy)]
pub struct Retention {
    pub log_days: u32,
    pub trace_days: u32,
    pub metric_days: u32,
}

/// The stamps one push accepts, worked out once per call from the server
/// clock and the retention for that kind of record.
#[derive(Debug, Clone, Copy)]
struct Window {
    /// Server receive time; what an entry without a stamp gets.
    now_us: i64,
    /// Earliest stamp the next retention sweep would still keep.
    oldest_us: i64,
    retention_days: u32,
    /// "log", "trace" or "metric", for the refusal text.
    kind: &'static str,
}

impl Window {
    fn new(now_us: i64, retention_days: u32, kind: &'static str) -> Self {
        // Retention keeps `retention_days` whole UTC days counting today, so
        // the oldest kept instant is the start of the oldest kept day.
        let oldest_day = day_of(now_us) - i64::from(retention_days) + 1;
        Self {
            now_us,
            oldest_us: oldest_day.saturating_mul(DAY_US),
            retention_days,
            kind,
        }
    }

    /// The stamp to store for `stamp`, or why it cannot be stored.
    fn accept(&self, stamp: i64, stamped: &mut u64) -> Result<i64, TimestampFault> {
        if stamp == 0 {
            *stamped += 1;
            return Ok(self.now_us);
        }
        if stamp < 0 || stamp > self.now_us + MAX_FUTURE_SKEW_US {
            return Err(TimestampFault::Future(stamp));
        }
        if stamp < self.oldest_us {
            return Err(TimestampFault::Past {
                stamp,
                retention_days: self.retention_days,
                kind: self.kind,
            });
        }
        Ok(stamp)
    }
}

/// Why a stamp was refused.
#[derive(Debug)]
enum TimestampFault {
    /// Negative, or more than an hour ahead of the server clock.
    Future(i64),
    /// Older than what the next retention sweep keeps.
    Past {
        stamp: i64,
        retention_days: u32,
        kind: &'static str,
    },
}

impl TimestampFault {
    fn describe(&self, field: &str) -> String {
        match self {
            Self::Future(stamp) => {
                format!(
                    "{field} {stamp} is negative or more than an hour ahead of the server clock"
                )
            }
            Self::Past {
                stamp,
                retention_days,
                kind,
            } => format!(
                "{field} {stamp} is older than the {retention_days}-day {kind} retention window \
                 and would be deleted by the next sweep, so it is refused rather than stored; \
                 check the producer's clock"
            ),
        }
    }
}

/// Why a record's strings cannot be stored as sent.
#[derive(Debug)]
enum SizeFault {
    /// An identifier over [`MAX_IDENTIFIER_BYTES`].
    Identifier { field: &'static str, len: usize },
    /// A field, attribute or label key over [`MAX_KEY_BYTES`].
    Key { len: usize },
    /// Fields, attributes or labels over their total cap, after values
    /// were cut.
    Total { bytes: usize, cap: usize },
}

impl SizeFault {
    fn describe(&self, what: &str) -> String {
        match self {
            Self::Identifier { field, len } => {
                format!("{field} is {len} bytes, the limit is {MAX_IDENTIFIER_BYTES}")
            }
            Self::Key { len } => {
                format!("a {what} key is {len} bytes, the limit is {MAX_KEY_BYTES}")
            }
            Self::Total { bytes, cap } => {
                format!("{what} total {bytes} bytes, the limit is {cap}")
            }
        }
    }
}

/// Refuse an identifier-like string that is too long to be one.
fn check_identifier(field: &'static str, value: &str) -> Result<(), SizeFault> {
    if value.len() > MAX_IDENTIFIER_BYTES {
        return Err(SizeFault::Identifier {
            field,
            len: value.len(),
        });
    }
    Ok(())
}

/// Bound a map of free-form strings: keys are refused over
/// [`MAX_KEY_BYTES`], values cut at [`MAX_FIELD_VALUE_BYTES`], and the whole
/// map refused over `cap` once the values are cut.
fn bound_map(
    pairs: impl IntoIterator<Item = (String, String)>,
    cap: usize,
    truncated: &mut u64,
) -> Result<BTreeMap<String, String>, SizeFault> {
    let mut out = BTreeMap::new();
    let mut total = 0usize;
    for (key, value) in pairs {
        if key.len() > MAX_KEY_BYTES {
            return Err(SizeFault::Key { len: key.len() });
        }
        let value = truncate_to(value, MAX_FIELD_VALUE_BYTES, truncated);
        total += key.len() + value.len();
        if total > cap {
            return Err(SizeFault::Total { bytes: total, cap });
        }
        out.insert(key, value);
    }
    Ok(out)
}

/// Keep a batch inside [`MAX_PARTITIONS_PER_BATCH`] distinct days.
///
/// Records on the oldest days beyond that count are dropped and counted as
/// rejected; the newest days win, because a producer that is behind catches
/// up from the newest end least badly. With stamps already refused outside
/// the retention window this only triggers under a retention longer than a
/// month, so the store's own refusal of such a batch is never reached.
fn cap_distinct_days<T>(records: &mut Vec<T>, day: impl Fn(&T) -> i64) -> usize {
    let mut days: Vec<i64> = records.iter().map(&day).collect();
    days.sort_unstable_by(|a, b| b.cmp(a));
    days.dedup();
    if days.len() <= MAX_PARTITIONS_PER_BATCH {
        return 0;
    }
    let oldest_kept = days[MAX_PARTITIONS_PER_BATCH - 1];
    let before = records.len();
    records.retain(|record| day(record) >= oldest_kept);
    before - records.len()
}

impl QueryLimits {
    /// Resolve the page size a caller asked for. Zero means "server default";
    /// anything above the ceiling is clamped rather than refused, so a client
    /// asking for too much still gets a usable answer. The error is a plain
    /// message so this stays independent of the transport.
    fn resolve(&self, requested: i32) -> Result<usize, String> {
        if requested < 0 {
            return Err(format!("limit must not be negative, got {requested}"));
        }
        let requested = requested as u32;
        let limit = if requested == 0 {
            self.default_limit
        } else {
            requested.min(self.max_limit)
        };
        Ok(limit as usize)
    }
}

/// Serves `deis.telemetry.LogService`.
#[derive(Clone)]
pub struct LogService {
    store: TelemetryStore,
    hub: Arc<TailHub>,
    stats: Arc<ServiceStats>,
    limits: QueryLimits,
    retention: Retention,
}

impl LogService {
    pub fn new(
        store: TelemetryStore,
        hub: Arc<TailHub>,
        stats: Arc<ServiceStats>,
        limits: QueryLimits,
        retention: Retention,
    ) -> Self {
        Self {
            store,
            hub,
            stats,
            limits,
            retention,
        }
    }
}

/// Serves `deis.telemetry.MetricService`.
///
/// It shares the database and the counters with [`LogService`] but keeps no
/// tail hub: samples have no live-follow surface, they are read back with
/// `QueryMetrics` or scraped from the HTTP endpoint.
#[derive(Clone)]
pub struct MetricService {
    store: TelemetryStore,
    stats: Arc<ServiceStats>,
    limits: QueryLimits,
    retention: Retention,
}

impl MetricService {
    pub fn new(
        store: TelemetryStore,
        stats: Arc<ServiceStats>,
        limits: QueryLimits,
        retention: Retention,
    ) -> Self {
        Self {
            store,
            stats,
            limits,
            retention,
        }
    }
}

/// Serves `deis.telemetry.TraceService`.
///
/// One record per invocation, which is a different volume from logs and a
/// different consumer: `deis trace` and the request panel read this, while
/// `deis tail` reads [`LogService`]. It shares the database and the counters
/// with both, and keeps its own live hub so a slow trace follower cannot cost
/// a log follower its entries.
#[derive(Clone)]
pub struct TraceService {
    store: TelemetryStore,
    hub: Arc<TraceHub>,
    stats: Arc<ServiceStats>,
    limits: QueryLimits,
    retention: Retention,
}

impl TraceService {
    pub fn new(
        store: TelemetryStore,
        hub: Arc<TraceHub>,
        stats: Arc<ServiceStats>,
        limits: QueryLimits,
        retention: Retention,
    ) -> Self {
        Self {
            store,
            hub,
            stats,
            limits,
            retention,
        }
    }
}

/// Why a record the capability does not cover is dropped.
const NOT_GRANTED: &str = "the presented capability does not allow pushing records of this \
                           application environment";

/// Whether a record whose owner resolved to `owner` may be handed to a reader
/// holding `scope`. A record whose owner cannot be worked out is shown only
/// to a reader who may see everything.
fn visible(scope: &ReadScope, owner: Result<Owner, String>) -> bool {
    match owner {
        Ok(owner) => scope.admits(&owner),
        Err(_) => matches!(scope, ReadScope::All),
    }
}

/// A live filter narrowed to what the reader's capability may see. Applied
/// before the caller's own filter, so a stream never carries a record the
/// same reader could not have queried.
struct Scoped<F> {
    filter: F,
    scope: ReadScope,
}

impl LiveFilter<LogRecord> for Scoped<TailFilter> {
    fn matches(&self, record: &LogRecord) -> bool {
        visible(&self.scope, identity::log(record)) && self.filter.matches(record)
    }

    fn describe(&self) -> String {
        self.filter.describe()
    }
}

impl LiveFilter<TraceRecord> for Scoped<TraceTailFilter> {
    fn matches(&self, record: &TraceRecord) -> bool {
        visible(&self.scope, identity::trace(record)) && self.filter.matches(record)
    }

    fn describe(&self) -> String {
        self.filter.describe()
    }
}

/// Run a blocking storage call off the reactor, and account for how it went.
///
/// `op` names the operation so a failure says which one failed; the caller
/// gets a generic `internal` status because the detail belongs in our log,
/// not in a client's error string.
///
/// **Every storage call in this file comes through here, and that is what
/// makes the accounting trustworthy.** Each of these failures used to produce
/// a line in this process's own log and move no counter at all, so an
/// operator watching `/metrics` while the database was refusing every write
/// saw `log_pushes_total` climbing, `logs_accepted_total` flat, and every
/// counter with "failed" or "rejected" in its name reading zero - the shape
/// of an idle service, not a broken one. Counting here rather than at each
/// call site also means a storage call added later cannot forget to be
/// counted.
async fn blocking<T, F>(
    stats: &ServiceStats,
    access: Access,
    op: &'static str,
    f: F,
) -> Result<T, Status>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(value)) => {
            stats.store_health.record_success(access);
            Ok(value)
        }
        Ok(Err(e)) => {
            ServiceStats::incr(&stats.storage_failures);
            stats.store_health.record_failure(access, op);
            error!(operation = op, error = ?e, "storage call failed, the request is answered with an error");
            Err(Status::internal(format!("{op} failed")))
        }
        Err(e) => {
            ServiceStats::incr(&stats.storage_failures);
            stats.store_health.record_failure(access, op);
            error!(operation = op, error = %e, "storage worker panicked or was cancelled, the request is answered with an error");
            Err(Status::internal(format!("{op} did not complete")))
        }
    }
}

/// Why an entry could not be stored.
#[derive(Debug)]
enum Rejection {
    Timestamp(TimestampFault),
    Level(i32),
    Size(SizeFault),
    /// Dropped because the batch named more distinct days than one insert
    /// may touch; see [`cap_distinct_days`].
    TooManyDays,
}

impl Rejection {
    fn describe(&self) -> String {
        match self {
            Self::Timestamp(fault) => fault.describe("timestamp_us"),
            Self::Level(level) => {
                format!("level {level} is outside the LogLevel range 0..={MAX_LOG_LEVEL}")
            }
            Self::Size(fault) => fault.describe("fields"),
            Self::TooManyDays => format!(
                "the batch spans more than {MAX_PARTITIONS_PER_BATCH} distinct days; entries on \
                 the oldest days beyond that were dropped"
            ),
        }
    }
}

/// Convert a wire entry into a storable record, or say why it cannot be.
///
/// `window` carries the server receive time, used to stamp entries that
/// arrived without one - a missing timestamp is worth repairing, a wrong one
/// is not - and the oldest stamp retention would still keep.
fn to_record(
    entry: LogEntry,
    window: &Window,
    truncated: &mut u64,
    stamped: &mut u64,
) -> Result<LogRecord, Rejection> {
    let timestamp_us = window
        .accept(entry.timestamp_us, stamped)
        .map_err(Rejection::Timestamp)?;
    if !(0..=MAX_LOG_LEVEL).contains(&entry.level) {
        return Err(Rejection::Level(entry.level));
    }
    for (field, value) in [
        ("app_id", &entry.app_id),
        ("component", &entry.component),
        ("trace_id", &entry.trace_id),
        ("span_id", &entry.span_id),
    ] {
        check_identifier(field, value).map_err(Rejection::Size)?;
    }
    let fields = bound_map(entry.fields, MAX_FIELDS_BYTES, truncated).map_err(Rejection::Size)?;
    let message = truncate_to(entry.message, MAX_LOG_MESSAGE_BYTES, truncated);
    Ok(LogRecord {
        timestamp_us,
        level: entry.level,
        component: entry.component,
        app_id: entry.app_id,
        message,
        fields,
        trace_id: entry.trace_id,
        span_id: entry.span_id,
    })
}

/// Cut `value` to `cap` bytes on a character boundary, marking the cut.
///
/// Storing a cut string beats refusing the record it belongs to: an operator
/// looking at a failed invocation is better served by most of the URL than by
/// no record at all.
fn truncate_to(value: String, cap: usize, truncated: &mut u64) -> String {
    if value.len() <= cap {
        return value;
    }
    *truncated += 1;
    let mut cut = cap;
    while cut > 0 && !value.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{TRUNCATION_MARKER}", &value[..cut])
}

fn to_entry(record: &LogRecord) -> LogEntry {
    LogEntry {
        timestamp_us: record.timestamp_us,
        level: record.level,
        component: record.component.clone(),
        app_id: record.app_id.clone(),
        message: record.message.clone(),
        fields: record
            .fields
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        trace_id: record.trace_id.clone(),
        span_id: record.span_id.clone(),
    }
}

fn to_metric_record(sample: MetricSample, window: &Window) -> Result<MetricRecord, String> {
    if sample.name.is_empty() {
        return Err("metric name is empty".to_string());
    }
    if !sample.value.is_finite() {
        // SQLite has no way to store NaN or an infinity in a REAL column - it
        // silently turns them into NULL - so they are refused at the door
        // instead of coming back as a read error later.
        return Err(format!("value {} is not finite", sample.value));
    }
    let mut stamped = 0u64;
    let timestamp_us = window
        .accept(sample.timestamp_us, &mut stamped)
        .map_err(|fault| fault.describe("timestamp_us"))?;
    for (field, value) in [("name", &sample.name), ("app_id", &sample.app_id)] {
        check_identifier(field, value).map_err(|fault| fault.describe("labels"))?;
    }
    // Labels name the series, so no label is ever cut and kept: the total
    // cap is below the per-value cut point (asserted below), so any value
    // long enough to be cut fails the total and refuses the whole sample.
    let mut cut = 0u64;
    let labels = bound_map(sample.labels, MAX_LABELS_BYTES, &mut cut)
        .map_err(|fault| fault.describe("labels"))?;
    Ok(MetricRecord {
        name: sample.name,
        app_id: sample.app_id,
        labels,
        value: sample.value,
        timestamp_us,
    })
}

fn to_metric_sample(record: &MetricRecord) -> MetricSample {
    MetricSample {
        name: record.name.clone(),
        app_id: record.app_id.clone(),
        labels: record
            .labels
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        value: record.value,
        timestamp_us: record.timestamp_us,
    }
}

/// Say, and count, that a page stopped at its byte budget short of the rows
/// the caller asked for. At `warn` on purpose: the page is correct and the
/// rest is behind `next_cursor`, but a client that takes a short page for
/// the end of the data is missing entries, and this line is how an operator
/// finds out that the limit they asked for is not the page size they get.
fn note_page_cut(stats: &ServiceStats, kind: &str, returned: usize, limit: usize, filter: &str) {
    ServiceStats::incr(&stats.query_pages_cut_by_bytes);
    warn!(
        kind,
        returned,
        limit,
        budget_bytes = MAX_PAGE_BYTES,
        filter,
        "a page was cut short by its byte budget before reaching the requested limit; the \
         rest is behind next_cursor and has_more is set. A client that reads only the first \
         page is missing records - it has to follow the cursor"
    );
}

impl LogService {
    fn note_page_cut(&self, kind: &str, returned: usize, limit: usize, filter: &str) {
        note_page_cut(&self.stats, kind, returned, limit, filter);
    }
}

impl TraceService {
    fn note_page_cut(&self, kind: &str, returned: usize, limit: usize, filter: &str) {
        note_page_cut(&self.stats, kind, returned, limit, filter);
    }
}

impl MetricService {
    fn note_page_cut(&self, kind: &str, returned: usize, limit: usize, filter: &str) {
        note_page_cut(&self.stats, kind, returned, limit, filter);
    }
}

/// Turn an empty string into "no filter".
fn optional(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

/// The same, borrowed, for checking a request before it is taken apart.
fn optional_ref(value: &str) -> Option<&str> {
    if value.is_empty() { None } else { Some(value) }
}

/// Turn a zero timestamp into "unbounded".
fn optional_time(value: i64) -> Option<i64> {
    if value == 0 { None } else { Some(value) }
}

/// Turn `LOG_LEVEL_UNSPECIFIED` into "no floor".
fn optional_level(value: i32) -> Option<i32> {
    if value <= 0 { None } else { Some(value) }
}

fn check_window(start_us: i64, end_us: i64) -> Result<(), String> {
    if start_us != 0 && end_us != 0 && start_us >= end_us {
        return Err(format!(
            "start_us {start_us} must be earlier than end_us {end_us}"
        ));
    }
    Ok(())
}

#[tonic::async_trait]
impl LogServiceRpc for LogService {
    type TailStream = Pin<Box<dyn Stream<Item = Result<LogEntry, Status>> + Send + 'static>>;

    async fn push_logs(
        &self,
        request: Request<PushLogsRequest>,
    ) -> Result<Response<PushLogsResponse>, Status> {
        let grant = grant_of(&request)?;
        let ingest = grant.ingest()?;
        let request = request.into_inner();
        ServiceStats::incr(&self.stats.log_pushes);
        let received = request.entries.len();
        if received == 0 {
            debug!("received an empty log batch, nothing to store");
            return Ok(Response::new(PushLogsResponse::default()));
        }

        let window = Window::new(now_us(), self.retention.log_days, "log");
        let mut records = Vec::with_capacity(received);
        let mut truncated = 0u64;
        let mut stamped = 0u64;
        let mut rejected = 0u64;
        let mut not_granted = 0u64;
        let mut first_rejection: Option<String> = None;
        for entry in request.entries {
            match to_record(entry, &window, &mut truncated, &mut stamped) {
                Ok(record) => match identity::log(&record) {
                    Ok(owner) if ingest.admits(&owner) => records.push(record),
                    Ok(_) => {
                        rejected += 1;
                        not_granted += 1;
                        first_rejection.get_or_insert_with(|| NOT_GRANTED.to_string());
                    }
                    Err(reason) => {
                        rejected += 1;
                        first_rejection.get_or_insert(reason);
                    }
                },
                Err(reason) => {
                    rejected += 1;
                    if first_rejection.is_none() {
                        first_rejection = Some(reason.describe());
                    }
                }
            }
        }
        if not_granted > 0 {
            warn!(
                not_granted,
                principal = %grant.principal(),
                "refused log records for application environments the pushing capability \
                 does not name; a node is issued ingest only for what it runs"
            );
        }
        let over_days = cap_distinct_days(&mut records, |record| day_of(record.timestamp_us));
        if over_days > 0 {
            rejected += over_days as u64;
            first_rejection.get_or_insert_with(|| Rejection::TooManyDays.describe());
        }

        if rejected > 0 {
            warn!(
                rejected,
                received,
                reason = first_rejection.as_deref().unwrap_or(""),
                "log entries were malformed and are dropped; they will never appear in Query or Tail"
            );
            ServiceStats::add(&self.stats.logs_rejected, rejected);
        }
        if truncated > 0 {
            warn!(
                truncated,
                message_cap_bytes = MAX_LOG_MESSAGE_BYTES,
                field_value_cap_bytes = MAX_FIELD_VALUE_BYTES,
                "log messages or field values exceeded their size cap and are stored cut short, \
                 the tail of those strings is lost"
            );
            ServiceStats::add(&self.stats.logs_truncated, truncated);
        }
        if stamped > 0 {
            debug!(
                stamped,
                "log entries arrived without a timestamp and were stamped with the server receive time"
            );
        }

        let accepted = records.len();
        if accepted > 0 {
            let store = self.store.clone();
            // The records come back out of the blocking task so they can be
            // fanned out without a second copy.
            let stored = match blocking(&self.stats, Access::Write, "store log batch", move || {
                store.insert_logs(&records)?;
                Ok(records)
            })
            .await
            {
                Ok(stored) => stored,
                Err(status) => {
                    // How many were lost, not just that something failed.
                    // These entries were well formed and accepted; nothing but
                    // this counter says they existed at all, because the write
                    // that would have made them queryable is the one that
                    // failed.
                    ServiceStats::add(&self.stats.logs_lost, accepted as u64);
                    error!(
                        lost = accepted,
                        received,
                        "a log batch could not be written; those entries are gone and will never \
                         appear in Query or Tail"
                    );
                    return Err(status);
                }
            };
            ServiceStats::add(&self.stats.logs_accepted, accepted as u64);
            // Fan out only after the write committed, so a tail consumer can
            // never see an entry that a later Query would not find.
            let live: Vec<Arc<LogRecord>> = stored.into_iter().map(Arc::new).collect();
            self.hub.publish(&live);
            debug!(
                accepted,
                rejected,
                tail_streams = self.hub.subscriber_count(),
                "stored a log batch"
            );
        }

        Ok(Response::new(PushLogsResponse {
            accepted_count: accepted as i32,
            rejected_count: rejected as i32,
            error_message: first_rejection.unwrap_or_default(),
        }))
    }

    async fn query(
        &self,
        request: Request<QueryRequest>,
    ) -> Result<Response<QueryResponse>, Status> {
        let scope = grant_of(&request)?.read_scope()?;
        let request = request.into_inner();
        ServiceStats::incr(&self.stats.log_queries);
        scope.check_app(optional_ref(&request.app_id))?;
        check_window(request.start_us, request.end_us).map_err(Status::invalid_argument)?;
        let limit = self
            .limits
            .resolve(request.limit)
            .map_err(Status::invalid_argument)?;
        let cursor = if request.cursor.is_empty() {
            None
        } else {
            Some(LogCursor::decode(&request.cursor).map_err(|e| {
                warn!(cursor = %request.cursor, error = ?e, "a client sent an unreadable page cursor, the request is refused");
                Status::invalid_argument("cursor is not a token produced by this service")
            })?)
        };
        let filter = LogFilter {
            app_id: optional(request.app_id),
            start_us: optional_time(request.start_us),
            end_us: optional_time(request.end_us),
            min_level: optional_level(request.min_level),
            trace_id: optional(request.trace_id),
            contains: optional(request.contains),
        };

        let store = self.store.clone();
        let describe = format!("{filter:?}");
        let mut page = blocking(&self.stats, Access::Read, "query logs", move || {
            store.query_logs(&filter, limit, cursor)
        })
        .await?;
        // Filtered after the page is cut, so the cursor still walks the
        // application's rows; a narrow reader may get a short page and a
        // cursor to the next one.
        page.records
            .retain(|record| visible(&scope, identity::log(record)));
        if page.cut_by_bytes {
            self.note_page_cut("log", page.records.len(), limit, &describe);
        }
        debug!(
            returned = page.records.len(),
            has_more = page.has_more,
            limit,
            filter = %describe,
            "answered a log query"
        );

        Ok(Response::new(QueryResponse {
            entries: page.records.iter().map(to_entry).collect(),
            next_cursor: page.next_cursor.map(|c| c.encode()).unwrap_or_default(),
            has_more: page.has_more,
        }))
    }

    // Every stream item carries a `tonic::Status` error slot, which clippy
    // flags as a large `Err` variant. The type is dictated by the generated
    // trait, so there is nothing to shrink here.
    #[allow(clippy::result_large_err)]
    async fn tail(
        &self,
        request: Request<TailRequest>,
    ) -> Result<Response<Self::TailStream>, Status> {
        let grant = grant_of(&request)?;
        let scope = grant.read_scope()?;
        let peer = request
            .remote_addr()
            .map(|addr| addr.to_string())
            .unwrap_or_else(|| "unknown-peer".to_string());
        let request = request.into_inner();
        scope.check_app(optional_ref(&request.app_id))?;
        let filter = TailFilter {
            app_id: optional(request.app_id),
            min_level: optional_level(request.min_level),
        };
        let label = format!("{peer}/{}", uuid::Uuid::new_v4().simple());
        info!(
            subscriber = %label,
            app_id = filter.app_id.as_deref().unwrap_or("*"),
            "opening a tail stream; only entries ingested from now on are streamed"
        );
        let rx = self.hub.subscribe(label, Scoped { filter, scope });
        let stream = ReceiverStream::new(rx).map(|record| Ok(to_entry(&record)));
        let stream = until_expiry(stream, grant.expires_at());
        Ok(Response::new(Box::pin(stream) as Self::TailStream))
    }
}

/// `items`, ended when the capability that opened the stream expires.
///
/// A follow stream is authorised once, when it opens, so without this a
/// reader whose capability ran out - or was revoked and not renewed - would
/// go on receiving lines for as long as the connection held. At `expires_at`
/// the stream ends with `Unauthenticated`, which tells the reader to come
/// back with a fresh capability rather than that the service went away.
fn until_expiry<T, S>(items: S, expires_at: u64) -> ReceiverStream<Result<T, Status>>
where
    T: Send + 'static,
    S: Stream<Item = Result<T, Status>> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move {
        let remaining = expires_at.saturating_sub(now_us().max(0) as u64 / 1_000_000);
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(remaining));
        tokio::pin!(deadline);
        tokio::pin!(items);
        loop {
            tokio::select! {
                _ = &mut deadline => {
                    let _ = tx
                        .send(Err(Status::unauthenticated(
                            "the capability this stream was opened with has expired; open it \
                             again with a fresh one",
                        )))
                        .await;
                    break;
                }
                _ = tx.closed() => break,
                item = items.next() => match item {
                    Some(item) => {
                        if tx.send(item).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
    });
    ReceiverStream::new(rx)
}

/// Why a trace record could not be stored.
#[derive(Debug)]
enum TraceRejection {
    Timestamp(TimestampFault),
    Duration(i64),
    Kind(i32),
    Outcome(i32),
    Size(SizeFault),
    /// See [`Rejection::TooManyDays`].
    TooManyDays,
}

impl TraceRejection {
    fn describe(&self) -> String {
        match self {
            Self::Timestamp(fault) => fault.describe("started_us"),
            Self::Duration(us) => format!("duration_us {us} is negative"),
            Self::Kind(kind) => {
                format!("kind {kind} is outside the InvocationKind range 0..={MAX_INVOCATION_KIND}")
            }
            Self::Outcome(outcome) => format!(
                "outcome {outcome} is outside the InvocationOutcome range \
                 0..={MAX_INVOCATION_OUTCOME}"
            ),
            Self::Size(fault) => fault.describe("attributes"),
            Self::TooManyDays => format!(
                "the batch spans more than {MAX_PARTITIONS_PER_BATCH} distinct days; records on \
                 the oldest days beyond that were dropped"
            ),
        }
    }
}

/// Convert a wire trace record into a storable one, or say why it cannot be.
///
/// A record with no `started_us` is stamped with the server receive time, the
/// same repair a log entry gets: a producer whose clock is unset is still
/// telling the truth about what happened, only not about when. One that is
/// set to before the retention window is refused, the same as a log entry.
fn to_trace_record(
    record: ProtoTraceRecord,
    window: &Window,
    truncated: &mut u64,
    stamped: &mut u64,
) -> Result<TraceRecord, TraceRejection> {
    let started_us = window
        .accept(record.started_us, stamped)
        .map_err(TraceRejection::Timestamp)?;
    // A negative duration would sort ahead of every real one under a
    // `min_duration_us` floor and would read as a clock that ran backwards.
    if record.duration_us < 0 {
        return Err(TraceRejection::Duration(record.duration_us));
    }
    if !(0..=MAX_INVOCATION_KIND).contains(&record.kind) {
        return Err(TraceRejection::Kind(record.kind));
    }
    if !(0..=MAX_INVOCATION_OUTCOME).contains(&record.outcome) {
        return Err(TraceRejection::Outcome(record.outcome));
    }
    for (field, value) in [
        ("trace_id", &record.trace_id),
        ("app_id", &record.app_id),
        ("environment", &record.environment),
        ("node", &record.node),
        ("method", &record.method),
    ] {
        check_identifier(field, value).map_err(TraceRejection::Size)?;
    }
    let attributes =
        bound_map(record.attributes, MAX_FIELDS_BYTES, truncated).map_err(TraceRejection::Size)?;
    Ok(TraceRecord {
        trace_id: record.trace_id,
        started_us,
        duration_us: record.duration_us,
        app_id: record.app_id,
        environment: record.environment,
        node: record.node,
        kind: record.kind,
        outcome: record.outcome,
        target: truncate_to(record.target, MAX_TRACE_TARGET_BYTES, truncated),
        method: record.method,
        status: record.status,
        error: truncate_to(record.error, MAX_TRACE_ERROR_BYTES, truncated),
        attributes,
    })
}

fn to_proto_trace(record: &TraceRecord) -> ProtoTraceRecord {
    ProtoTraceRecord {
        trace_id: record.trace_id.clone(),
        started_us: record.started_us,
        duration_us: record.duration_us,
        app_id: record.app_id.clone(),
        environment: record.environment.clone(),
        node: record.node.clone(),
        kind: record.kind,
        outcome: record.outcome,
        target: record.target.clone(),
        method: record.method.clone(),
        status: record.status,
        error: record.error.clone(),
        attributes: record
            .attributes
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    }
}

/// Drop the "unspecified" placeholder out of a repeated enum filter.
///
/// A client that sends nothing gets an empty list, which means "every value".
/// A client that sends `[UNSPECIFIED]` almost certainly meant the same thing
/// rather than "only records nobody could classify", and answering the latter
/// would silently return an empty page.
fn enum_selection(values: Vec<i32>) -> Vec<i32> {
    values.into_iter().filter(|value| *value > 0).collect()
}

#[tonic::async_trait]
impl TraceServiceRpc for TraceService {
    type TailTracesStream =
        Pin<Box<dyn Stream<Item = Result<ProtoTraceRecord, Status>> + Send + 'static>>;

    async fn push_traces(
        &self,
        request: Request<PushTracesRequest>,
    ) -> Result<Response<PushTracesResponse>, Status> {
        let grant = grant_of(&request)?;
        let ingest = grant.ingest()?;
        let request = request.into_inner();
        ServiceStats::incr(&self.stats.trace_pushes);

        // Counted before the early return: a producer whose sampling threw
        // the whole interval away still has to report the hole, and that push
        // carries no records at all.
        if request.sampled_out > 0 {
            ServiceStats::add(&self.stats.traces_sampled_out, request.sampled_out);
            debug!(
                sampled_out = request.sampled_out,
                "a producer reported invocations it deliberately did not record; \
                 the trace history has that many gaps by policy"
            );
        }
        if request.dropped > 0 {
            ServiceStats::add(&self.stats.traces_dropped_by_producer, request.dropped);
            warn!(
                dropped = request.dropped,
                "a producer reported trace records it built and then lost because its own \
                 buffer was full; those invocations happened and will never be queryable. \
                 Either the producer cannot reach this service or its trace buffer is too \
                 small for its traffic"
            );
        }

        let received = request.records.len();
        if received == 0 {
            return Ok(Response::new(PushTracesResponse::default()));
        }

        let window = Window::new(now_us(), self.retention.trace_days, "trace");
        let mut records = Vec::with_capacity(received);
        let mut truncated = 0u64;
        let mut stamped = 0u64;
        let mut rejected = 0u64;
        let mut not_granted = 0u64;
        let mut first_rejection: Option<String> = None;
        for record in request.records {
            match to_trace_record(record, &window, &mut truncated, &mut stamped) {
                Ok(record) => match identity::trace(&record) {
                    Ok(owner) if ingest.admits(&owner) => records.push(record),
                    Ok(_) => {
                        rejected += 1;
                        not_granted += 1;
                        first_rejection.get_or_insert_with(|| NOT_GRANTED.to_string());
                    }
                    Err(reason) => {
                        rejected += 1;
                        first_rejection.get_or_insert(reason);
                    }
                },
                Err(reason) => {
                    rejected += 1;
                    if first_rejection.is_none() {
                        first_rejection = Some(reason.describe());
                    }
                }
            }
        }
        if not_granted > 0 {
            warn!(
                not_granted,
                principal = %grant.principal(),
                "refused trace records for application environments the pushing capability \
                 does not name; a node is issued ingest only for what it runs"
            );
        }
        let over_days = cap_distinct_days(&mut records, |record| day_of(record.started_us));
        if over_days > 0 {
            rejected += over_days as u64;
            first_rejection.get_or_insert_with(|| TraceRejection::TooManyDays.describe());
        }

        if rejected > 0 {
            warn!(
                rejected,
                received,
                reason = first_rejection.as_deref().unwrap_or(""),
                "trace records were malformed and are dropped; those invocations will never \
                 appear in QueryTraces or TailTraces"
            );
            ServiceStats::add(&self.stats.traces_rejected, rejected);
        }
        if truncated > 0 {
            warn!(
                truncated,
                target_cap_bytes = MAX_TRACE_TARGET_BYTES,
                error_cap_bytes = MAX_TRACE_ERROR_BYTES,
                attribute_value_cap_bytes = MAX_FIELD_VALUE_BYTES,
                "trace targets, error messages or attribute values exceeded their size cap and \
                 are stored cut short; the tail of those strings is lost"
            );
            ServiceStats::add(&self.stats.traces_truncated, truncated);
        }
        if stamped > 0 {
            debug!(
                stamped,
                "trace records arrived without a start time and were stamped with the server \
                 receive time"
            );
        }

        let accepted = records.len();
        if accepted > 0 {
            let store = self.store.clone();
            let stored = match blocking(
                &self.stats,
                Access::Write,
                "store trace batch",
                move || {
                    store.insert_traces(&records)?;
                    Ok(records)
                },
            )
            .await
            {
                Ok(stored) => stored,
                Err(status) => {
                    ServiceStats::add(&self.stats.traces_lost, accepted as u64);
                    error!(
                        lost = accepted,
                        received,
                        "a trace batch could not be written; those invocations happened and will \
                         never appear in QueryTraces or TailTraces"
                    );
                    return Err(status);
                }
            };
            ServiceStats::add(&self.stats.traces_accepted, accepted as u64);
            // Fan out only after the write committed, so a follower can never
            // see a record a later QueryTraces would not find.
            let live: Vec<Arc<TraceRecord>> = stored.into_iter().map(Arc::new).collect();
            self.hub.publish(&live);
            debug!(
                accepted,
                rejected,
                trace_streams = self.hub.subscriber_count(),
                "stored a trace batch"
            );
        }

        Ok(Response::new(PushTracesResponse {
            accepted_count: accepted as i32,
            rejected_count: rejected as i32,
            error_message: first_rejection.unwrap_or_default(),
        }))
    }

    async fn query_traces(
        &self,
        request: Request<QueryTracesRequest>,
    ) -> Result<Response<QueryTracesResponse>, Status> {
        let scope = grant_of(&request)?.read_scope()?;
        let request = request.into_inner();
        ServiceStats::incr(&self.stats.trace_queries);
        scope.check_app(optional_ref(&request.app_id))?;
        check_window(request.start_us, request.end_us).map_err(Status::invalid_argument)?;
        if request.min_duration_us < 0 {
            return Err(Status::invalid_argument(format!(
                "min_duration_us must not be negative, got {}",
                request.min_duration_us
            )));
        }
        let limit = self
            .limits
            .resolve(request.limit)
            .map_err(Status::invalid_argument)?;
        let cursor = if request.cursor.is_empty() {
            None
        } else {
            Some(TraceCursor::decode(&request.cursor).map_err(|e| {
                warn!(cursor = %request.cursor, error = ?e, "a client sent an unreadable trace page cursor, the request is refused");
                Status::invalid_argument("cursor is not a token produced by this service")
            })?)
        };
        let filter = TraceFilter {
            app_id: optional(request.app_id),
            environment: optional(request.environment),
            start_us: optional_time(request.start_us),
            end_us: optional_time(request.end_us),
            kinds: enum_selection(request.kinds),
            outcomes: enum_selection(request.outcomes),
            trace_id: optional(request.trace_id),
            min_duration_us: optional_time(request.min_duration_us),
            target_contains: optional(request.target_contains),
        };

        let store = self.store.clone();
        let describe = format!("{filter:?}");
        let mut page = blocking(&self.stats, Access::Read, "query traces", move || {
            store.query_traces(&filter, limit, cursor)
        })
        .await?;
        page.records
            .retain(|record| visible(&scope, identity::trace(record)));
        if page.cut_by_bytes {
            self.note_page_cut("trace", page.records.len(), limit, &describe);
        }
        debug!(
            returned = page.records.len(),
            has_more = page.has_more,
            limit,
            filter = %describe,
            "answered a trace query"
        );

        Ok(Response::new(QueryTracesResponse {
            records: page.records.iter().map(to_proto_trace).collect(),
            next_cursor: page.next_cursor.map(|c| c.encode()).unwrap_or_default(),
            has_more: page.has_more,
        }))
    }

    // Same generated-trait constraint as `LogService::tail`: the stream item
    // carries a `tonic::Status`, which clippy reads as a large `Err`.
    #[allow(clippy::result_large_err)]
    async fn tail_traces(
        &self,
        request: Request<TailTracesRequest>,
    ) -> Result<Response<Self::TailTracesStream>, Status> {
        let grant = grant_of(&request)?;
        let scope = grant.read_scope()?;
        let peer = request
            .remote_addr()
            .map(|addr| addr.to_string())
            .unwrap_or_else(|| "unknown-peer".to_string());
        let request = request.into_inner();
        scope.check_app(optional_ref(&request.app_id))?;
        let filter = TraceTailFilter {
            app_id: optional(request.app_id),
            environment: optional(request.environment),
            kinds: enum_selection(request.kinds),
            outcomes: enum_selection(request.outcomes),
        };
        let label = format!("{peer}/{}", uuid::Uuid::new_v4().simple());
        info!(
            subscriber = %label,
            app_id = filter.app_id.as_deref().unwrap_or("*"),
            "opening a trace stream; only invocations recorded from now on are streamed"
        );
        let rx = self.hub.subscribe(label, Scoped { filter, scope });
        let stream = ReceiverStream::new(rx).map(|record| Ok(to_proto_trace(&record)));
        let stream = until_expiry(stream, grant.expires_at());
        Ok(Response::new(Box::pin(stream) as Self::TailTracesStream))
    }
}

#[tonic::async_trait]
impl MetricServiceRpc for MetricService {
    async fn push_metrics(
        &self,
        request: Request<PushMetricsRequest>,
    ) -> Result<Response<PushMetricsResponse>, Status> {
        let grant = grant_of(&request)?;
        let ingest = grant.ingest()?;
        let request = request.into_inner();
        ServiceStats::incr(&self.stats.metric_pushes);
        let received = request.samples.len();
        if received == 0 {
            debug!("received an empty metric batch, nothing to store");
            return Ok(Response::new(PushMetricsResponse::default()));
        }

        let window = Window::new(now_us(), self.retention.metric_days, "metric");
        let mut records = Vec::with_capacity(received);
        let mut rejected = 0u64;
        let mut not_granted = 0u64;
        let mut first_rejection: Option<String> = None;
        for sample in request.samples {
            match to_metric_record(sample, &window) {
                Ok(record) => match identity::metric(&record) {
                    Ok(owner) if ingest.admits(&owner) => records.push(record),
                    Ok(_) => {
                        rejected += 1;
                        not_granted += 1;
                        first_rejection.get_or_insert_with(|| NOT_GRANTED.to_string());
                    }
                    Err(reason) => {
                        rejected += 1;
                        first_rejection.get_or_insert(reason);
                    }
                },
                Err(reason) => {
                    rejected += 1;
                    if first_rejection.is_none() {
                        first_rejection = Some(reason);
                    }
                }
            }
        }
        if not_granted > 0 {
            warn!(
                not_granted,
                principal = %grant.principal(),
                "refused metric records for application environments the pushing capability \
                 does not name; a node is issued ingest only for what it runs"
            );
        }
        if rejected > 0 {
            warn!(
                rejected,
                received,
                reason = first_rejection.as_deref().unwrap_or(""),
                "metric samples were malformed and are dropped; they will appear in neither QueryMetrics nor /metrics"
            );
            ServiceStats::add(&self.stats.metric_samples_rejected, rejected);
        }

        let accepted = records.len();
        if accepted > 0 {
            let store = self.store.clone();
            if let Err(status) = blocking(
                &self.stats,
                Access::Write,
                "store metric batch",
                move || store.insert_metrics(&records),
            )
            .await
            {
                ServiceStats::add(&self.stats.metric_samples_lost, accepted as u64);
                error!(
                    lost = accepted,
                    received,
                    "a metric batch could not be written; those series show a gap in \
                     QueryMetrics and on /metrics"
                );
                return Err(status);
            }
            ServiceStats::add(&self.stats.metric_samples_accepted, accepted as u64);
            debug!(accepted, rejected, "stored a metric batch");
        }

        Ok(Response::new(PushMetricsResponse {
            accepted_count: accepted as i32,
            rejected_count: rejected as i32,
            error_message: first_rejection.unwrap_or_default(),
        }))
    }

    async fn query_metrics(
        &self,
        request: Request<QueryMetricsRequest>,
    ) -> Result<Response<QueryMetricsResponse>, Status> {
        let scope = grant_of(&request)?.read_scope()?;
        let request = request.into_inner();
        ServiceStats::incr(&self.stats.metric_queries);
        scope.check_app(optional_ref(&request.app_id))?;
        check_window(request.start_us, request.end_us).map_err(Status::invalid_argument)?;
        let limit = self
            .limits
            .resolve(request.limit)
            .map_err(Status::invalid_argument)?;
        let cursor = if request.cursor.is_empty() {
            None
        } else {
            Some(MetricCursor::decode(&request.cursor).map_err(|e| {
                warn!(cursor = %request.cursor, error = ?e, "a client sent an unreadable metric page cursor, the request is refused");
                Status::invalid_argument("cursor is not a token produced by this service")
            })?)
        };
        let filter = MetricFilter {
            name: optional(request.name),
            app_id: optional(request.app_id),
            label_selector: request
                .label_selector
                .into_iter()
                .collect::<BTreeMap<_, _>>(),
            start_us: optional_time(request.start_us),
            end_us: optional_time(request.end_us),
        };

        let store = self.store.clone();
        let describe = format!("{filter:?}");
        let mut page = blocking(&self.stats, Access::Read, "query metrics", move || {
            store.query_metrics(&filter, limit, cursor)
        })
        .await?;
        page.records
            .retain(|record| visible(&scope, identity::metric(record)));
        if page.cut_by_bytes {
            self.note_page_cut("metric", page.records.len(), limit, &describe);
        }
        debug!(
            returned = page.records.len(),
            has_more = page.has_more,
            limit,
            filter = %describe,
            "answered a metric query"
        );

        Ok(Response::new(QueryMetricsResponse {
            samples: page.records.iter().map(to_metric_sample).collect(),
            next_cursor: page.next_cursor.map(|c| c.encode()).unwrap_or_default(),
            has_more: page.has_more,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window around `now_us` with a week of log retention.
    fn window(now_us: i64) -> Window {
        Window::new(now_us, 7, "log")
    }

    fn entry(message: &str) -> LogEntry {
        LogEntry {
            timestamp_us: 1_700_000_000_000_000,
            level: 3,
            component: "deis-runtime".into(),
            app_id: "shop".into(),
            message: message.into(),
            fields: Default::default(),
            trace_id: String::new(),
            span_id: String::new(),
        }
    }

    #[test]
    fn missing_timestamp_is_stamped_on_arrival() {
        let mut truncated = 0;
        let mut stamped = 0;
        let mut e = entry("hello");
        e.timestamp_us = 0;
        let record =
            to_record(e, &window(42), &mut truncated, &mut stamped).expect("must be accepted");
        assert_eq!(record.timestamp_us, 42);
        assert_eq!(stamped, 1);
    }

    #[test]
    fn far_future_timestamp_is_rejected() {
        let mut truncated = 0;
        let mut stamped = 0;
        let mut e = entry("hello");
        e.timestamp_us = 10_000_000_000;
        let err = to_record(e, &window(1_000), &mut truncated, &mut stamped)
            .expect_err("must be rejected");
        assert!(err.describe().contains("ahead of the server clock"));
    }

    #[test]
    fn out_of_range_level_is_rejected() {
        let mut truncated = 0;
        let mut stamped = 0;
        let mut e = entry("hello");
        e.level = 99;
        assert!(
            to_record(
                e,
                &window(1_700_000_000_000_001),
                &mut truncated,
                &mut stamped
            )
            .is_err()
        );
    }

    #[test]
    fn oversized_message_is_truncated_not_dropped() {
        let mut truncated = 0;
        let mut stamped = 0;
        let long = "x".repeat(MAX_LOG_MESSAGE_BYTES + 500);
        let record = to_record(
            entry(&long),
            &window(1_700_000_000_000_001),
            &mut truncated,
            &mut stamped,
        )
        .expect("must be accepted");
        assert_eq!(truncated, 1);
        assert!(record.message.ends_with(TRUNCATION_MARKER));
        assert_eq!(
            record.message.len(),
            MAX_LOG_MESSAGE_BYTES + TRUNCATION_MARKER.len()
        );
    }

    #[test]
    fn truncation_respects_character_boundaries() {
        let mut truncated = 0;
        let mut stamped = 0;
        // A three-byte character straddling the cap must not be split.
        let long = "\u{4e2d}".repeat(MAX_LOG_MESSAGE_BYTES);
        let record = to_record(
            entry(&long),
            &window(1_700_000_000_000_001),
            &mut truncated,
            &mut stamped,
        )
        .expect("must be accepted");
        assert!(record.message.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn non_finite_metric_values_are_rejected() {
        let sample = MetricSample {
            name: "latency".into(),
            app_id: "shop".into(),
            labels: Default::default(),
            value: f64::NAN,
            timestamp_us: 1,
        };
        assert!(to_metric_record(sample, &window(10)).is_err());
    }

    #[test]
    fn nameless_metric_is_rejected() {
        let sample = MetricSample {
            name: String::new(),
            app_id: "shop".into(),
            labels: Default::default(),
            value: 1.0,
            timestamp_us: 1,
        };
        assert!(to_metric_record(sample, &window(10)).is_err());
    }

    fn proto_trace() -> ProtoTraceRecord {
        ProtoTraceRecord {
            trace_id: "abc".into(),
            started_us: 1_700_000_000_000_000,
            duration_us: 1_500,
            app_id: "shop".into(),
            environment: "production".into(),
            node: "node-a".into(),
            kind: 1,
            outcome: 1,
            target: "https://shop.test/checkout".into(),
            method: "POST".into(),
            status: 200,
            error: String::new(),
            attributes: Default::default(),
        }
    }

    #[test]
    fn a_trace_without_a_start_time_is_stamped_on_arrival() {
        let mut truncated = 0;
        let mut stamped = 0;
        let mut record = proto_trace();
        record.started_us = 0;
        let stored = to_trace_record(record, &window(42), &mut truncated, &mut stamped)
            .expect("must be accepted");
        assert_eq!(stored.started_us, 42);
        assert_eq!(stamped, 1);
    }

    #[test]
    fn a_negative_duration_is_refused() {
        // It would sort ahead of every real invocation under a duration floor
        // and would read as a clock that ran backwards.
        let mut record = proto_trace();
        record.duration_us = -1;
        let err = to_trace_record(record, &window(1_700_000_000_000_001), &mut 0, &mut 0)
            .expect_err("must be rejected");
        assert!(err.describe().contains("duration_us"));
    }

    #[test]
    fn an_unknown_kind_or_outcome_is_refused() {
        // An out-of-range ordinal would come back out of storage as a value
        // no reader can name, and a query filtering on outcome would silently
        // skip it.
        let mut kind = proto_trace();
        kind.kind = 99;
        assert!(to_trace_record(kind, &window(1_700_000_000_000_001), &mut 0, &mut 0).is_err());

        let mut outcome = proto_trace();
        outcome.outcome = 99;
        assert!(to_trace_record(outcome, &window(1_700_000_000_000_001), &mut 0, &mut 0).is_err());
    }

    #[test]
    fn an_oversized_target_is_cut_short_not_dropped() {
        // A URL is attacker-supplied and there is one trace per invocation,
        // so the record is kept and the tail of the string is not.
        let mut record = proto_trace();
        record.target = "x".repeat(MAX_TRACE_TARGET_BYTES + 500);
        let mut truncated = 0;
        let stored = to_trace_record(
            record,
            &window(1_700_000_000_000_001),
            &mut truncated,
            &mut 0,
        )
        .expect("must be accepted");
        assert_eq!(truncated, 1);
        assert!(stored.target.ends_with(TRUNCATION_MARKER));
        assert_eq!(
            stored.target.len(),
            MAX_TRACE_TARGET_BYTES + TRUNCATION_MARKER.len()
        );
    }

    #[test]
    fn an_unspecified_enum_in_a_filter_means_no_restriction() {
        // A client that sends the zero value almost certainly meant "every
        // outcome"; answering "only the ones nobody could classify" would
        // hand back an empty page and look like an outage.
        assert!(enum_selection(vec![0]).is_empty());
        assert_eq!(enum_selection(vec![0, 2, 3]), vec![2, 3]);
    }

    #[test]
    fn limits_clamp_instead_of_refusing() {
        let limits = QueryLimits {
            default_limit: 100,
            max_limit: 1000,
        };
        assert_eq!(limits.resolve(0).unwrap(), 100);
        assert_eq!(limits.resolve(10).unwrap(), 10);
        assert_eq!(limits.resolve(1_000_000).unwrap(), 1000);
        assert!(limits.resolve(-1).is_err());
    }

    #[test]
    fn inverted_time_window_is_refused() {
        assert!(check_window(100, 10).is_err());
        assert!(check_window(0, 10).is_ok());
        assert!(check_window(100, 0).is_ok());
    }

    /// A stamp `days` whole days before `now`, at the same time of day.
    fn days_before(now: i64, days: i64) -> i64 {
        now - days * DAY_US
    }

    #[test]
    fn a_stamp_older_than_the_retention_window_is_refused_and_says_why() {
        // Noon on a day well inside the epoch, so the window's day arithmetic
        // is exercised for real rather than saturating at zero.
        let now = 1_787_961_600_000_000 + DAY_US / 2;
        let window = Window::new(now, 7, "log");

        // Eight days back is behind the seven kept; six days back is inside.
        let mut old = entry("late");
        old.timestamp_us = days_before(now, 8);
        let err = to_record(old, &window, &mut 0, &mut 0).expect_err("must be refused");
        let reason = err.describe();
        assert!(reason.contains("7-day log retention window"), "{reason}");
        assert!(reason.contains("clock"), "{reason}");

        let mut recent = entry("in time");
        recent.timestamp_us = days_before(now, 6);
        assert!(to_record(recent, &window, &mut 0, &mut 0).is_ok());

        // The oldest kept day starts at midnight, and the whole of it counts.
        let mut edge = entry("edge");
        edge.timestamp_us = (day_of(now) - 6) * DAY_US;
        assert!(to_record(edge, &window, &mut 0, &mut 0).is_ok());
        let mut over = entry("over");
        over.timestamp_us = (day_of(now) - 6) * DAY_US - 1;
        assert!(to_record(over, &window, &mut 0, &mut 0).is_err());

        // Traces and metrics run on their own retention.
        let traces = Window::new(now, 3, "trace");
        let mut old = proto_trace();
        old.started_us = days_before(now, 4);
        let err = to_trace_record(old, &traces, &mut 0, &mut 0).expect_err("must be refused");
        assert!(
            err.describe().contains("3-day trace retention"),
            "{}",
            err.describe()
        );

        let metrics = Window::new(now, 30, "metric");
        let old = MetricSample {
            name: "latency".into(),
            app_id: "shop".into(),
            labels: Default::default(),
            value: 1.0,
            timestamp_us: days_before(now, 31),
        };
        let err = to_metric_record(old, &metrics).expect_err("must be refused");
        assert!(err.contains("30-day metric retention"), "{err}");
    }

    #[test]
    fn a_batch_over_the_day_cap_keeps_the_newest_days() {
        let mut records: Vec<LogRecord> = (0..(MAX_PARTITIONS_PER_BATCH as i64 + 8))
            .map(|d| LogRecord {
                timestamp_us: 1_787_961_600_000_000 - d * DAY_US,
                level: 3,
                component: "deis-runtime".into(),
                app_id: "clock".into(),
                message: "x".into(),
                fields: Default::default(),
                trace_id: String::new(),
                span_id: String::new(),
            })
            .collect();
        let dropped = cap_distinct_days(&mut records, |r| day_of(r.timestamp_us));
        assert_eq!(dropped, 8);
        assert_eq!(records.len(), MAX_PARTITIONS_PER_BATCH);
        let oldest = records.iter().map(|r| r.timestamp_us).min().unwrap();
        assert_eq!(
            oldest,
            1_787_961_600_000_000 - (MAX_PARTITIONS_PER_BATCH as i64 - 1) * DAY_US,
            "the newest days survive, the oldest go"
        );

        // Under the cap nothing moves.
        let mut few = records.clone();
        assert_eq!(cap_distinct_days(&mut few, |r| day_of(r.timestamp_us)), 0);
        assert_eq!(few.len(), records.len());
    }

    #[test]
    fn oversized_identifiers_and_keys_refuse_the_record_and_values_are_cut() {
        // The fixture stamps entries at 1_700_000_000_000_000; a window a
        // microsecond later keeps them inside retention.
        let w = window(1_700_000_000_000_001);

        // An identifier over the cap names nothing real; the record goes.
        let mut e = entry("id");
        e.app_id = "a".repeat(MAX_IDENTIFIER_BYTES + 1);
        let err = to_record(e, &w, &mut 0, &mut 0).expect_err("must be refused");
        assert!(err.describe().contains("app_id"), "{}", err.describe());
        for set in [
            |e: &mut LogEntry| e.component = "c".repeat(MAX_IDENTIFIER_BYTES + 1),
            |e: &mut LogEntry| e.trace_id = "t".repeat(MAX_IDENTIFIER_BYTES + 1),
            |e: &mut LogEntry| e.span_id = "s".repeat(MAX_IDENTIFIER_BYTES + 1),
        ] {
            let mut e = entry("id");
            set(&mut e);
            assert!(to_record(e, &w, &mut 0, &mut 0).is_err());
        }

        // A key over the cap cannot be cut without colliding; the record goes.
        let mut e = entry("key");
        e.fields
            .insert("k".repeat(MAX_KEY_BYTES + 1), "v".to_string());
        let err = to_record(e, &w, &mut 0, &mut 0).expect_err("must be refused");
        assert!(err.describe().contains("key"), "{}", err.describe());

        // A value over the cap is cut and marked, and counted as truncated.
        let mut e = entry("value");
        e.fields
            .insert("big".to_string(), "v".repeat(MAX_FIELD_VALUE_BYTES + 100));
        let mut truncated = 0;
        let record = to_record(e, &w, &mut truncated, &mut 0).expect("must be kept");
        assert_eq!(truncated, 1);
        assert!(record.fields["big"].ends_with(TRUNCATION_MARKER));
        assert_eq!(
            record.fields["big"].len(),
            MAX_FIELD_VALUE_BYTES + TRUNCATION_MARKER.len()
        );

        // Many capped values still add up; past the total the record goes.
        let mut e = entry("total");
        for i in 0..(MAX_FIELDS_BYTES / MAX_FIELD_VALUE_BYTES + 1) {
            e.fields
                .insert(format!("f{i}"), "v".repeat(MAX_FIELD_VALUE_BYTES));
        }
        let err = to_record(e, &w, &mut 0, &mut 0).expect_err("must be refused");
        assert!(
            err.describe().contains("fields total"),
            "{}",
            err.describe()
        );

        // The same rules hold for a trace's identifiers and attributes.
        let mut t = proto_trace();
        t.method = "M".repeat(MAX_IDENTIFIER_BYTES + 1);
        assert!(to_trace_record(t, &w, &mut 0, &mut 0).is_err());
        let mut t = proto_trace();
        t.attributes
            .insert("big".to_string(), "v".repeat(MAX_FIELD_VALUE_BYTES + 100));
        let mut truncated = 0;
        let stored = to_trace_record(t, &w, &mut truncated, &mut 0).expect("must be kept");
        assert_eq!(truncated, 1);
        assert!(stored.attributes["big"].ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn metric_labels_are_refused_over_their_cap_never_cut() {
        // Labels name the series. A cut label would file the sample under a
        // series nobody asked for, so an oversized one refuses the sample.
        let w = window(1_787_961_600_000_000);
        let sample = |labels: Vec<(&str, String)>| MetricSample {
            name: "latency".into(),
            app_id: "shop".into(),
            labels: labels
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            value: 1.0,
            timestamp_us: 1_787_961_600_000_000,
        };
        let err = to_metric_record(
            sample(vec![("route", "r".repeat(MAX_LABELS_BYTES + 1))]),
            &w,
        )
        .expect_err("must be refused");
        assert!(err.contains("labels"), "{err}");
        // Long enough to be cut as a field value; as a label it still fails
        // the total, so it is never stored cut.
        let err = to_metric_record(
            sample(vec![("route", "r".repeat(MAX_FIELD_VALUE_BYTES + 1))]),
            &w,
        )
        .expect_err("must be refused");
        assert!(err.contains("labels total"), "{err}");
        let kept =
            to_metric_record(sample(vec![("route", "/a".to_string())]), &w).expect("must be kept");
        assert_eq!(kept.labels["route"], "/a");

        let mut named = sample(vec![]);
        named.name = "n".repeat(MAX_IDENTIFIER_BYTES + 1);
        assert!(to_metric_record(named, &w).is_err());
    }
}
