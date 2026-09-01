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
    LogCursor, LogFilter, LogRecord, MetricCursor, MetricFilter, MetricRecord, TelemetryStore,
    TraceCursor, TraceFilter, TraceRecord,
};
use crate::tail::{TailFilter, TailHub, TraceHub, TraceTailFilter};

/// Longest message body kept verbatim. Anything longer is stored cut short:
/// a runaway `console.log` must not be able to bloat the database, and a
/// truncated line is still worth more than a rejected one.
pub const MAX_LOG_MESSAGE_BYTES: usize = 64 * 1024;

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
}

impl LogService {
    pub fn new(
        store: TelemetryStore,
        hub: Arc<TailHub>,
        stats: Arc<ServiceStats>,
        limits: QueryLimits,
    ) -> Self {
        Self {
            store,
            hub,
            stats,
            limits,
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
}

impl MetricService {
    pub fn new(store: TelemetryStore, stats: Arc<ServiceStats>, limits: QueryLimits) -> Self {
        Self {
            store,
            stats,
            limits,
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
}

impl TraceService {
    pub fn new(
        store: TelemetryStore,
        hub: Arc<TraceHub>,
        stats: Arc<ServiceStats>,
        limits: QueryLimits,
    ) -> Self {
        Self {
            store,
            hub,
            stats,
            limits,
        }
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
    Timestamp(i64),
    Level(i32),
}

impl Rejection {
    fn describe(&self) -> String {
        match self {
            Self::Timestamp(ts) => {
                format!(
                    "timestamp_us {ts} is negative or more than an hour ahead of the server clock"
                )
            }
            Self::Level(level) => {
                format!("level {level} is outside the LogLevel range 0..={MAX_LOG_LEVEL}")
            }
        }
    }
}

/// Convert a wire entry into a storable record, or say why it cannot be.
///
/// `now_us` is the server receive time, used to stamp entries that arrived
/// without one - a missing timestamp is worth repairing, a wrong one is not.
fn to_record(
    entry: LogEntry,
    now_us: i64,
    truncated: &mut u64,
    stamped: &mut u64,
) -> Result<LogRecord, Rejection> {
    let timestamp_us = if entry.timestamp_us == 0 {
        *stamped += 1;
        now_us
    } else if entry.timestamp_us < 0 || entry.timestamp_us > now_us + MAX_FUTURE_SKEW_US {
        return Err(Rejection::Timestamp(entry.timestamp_us));
    } else {
        entry.timestamp_us
    };
    if !(0..=MAX_LOG_LEVEL).contains(&entry.level) {
        return Err(Rejection::Level(entry.level));
    }
    let message = truncate_to(entry.message, MAX_LOG_MESSAGE_BYTES, truncated);
    Ok(LogRecord {
        timestamp_us,
        level: entry.level,
        component: entry.component,
        app_id: entry.app_id,
        message,
        fields: entry.fields.into_iter().collect(),
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

fn to_metric_record(sample: MetricSample, now_us: i64) -> Result<MetricRecord, String> {
    if sample.name.is_empty() {
        return Err("metric name is empty".to_string());
    }
    if !sample.value.is_finite() {
        // SQLite has no way to store NaN or an infinity in a REAL column - it
        // silently turns them into NULL - so they are refused at the door
        // instead of coming back as a read error later.
        return Err(format!("value {} is not finite", sample.value));
    }
    let timestamp_us = if sample.timestamp_us == 0 {
        now_us
    } else if sample.timestamp_us < 0 || sample.timestamp_us > now_us + MAX_FUTURE_SKEW_US {
        return Err(format!(
            "timestamp_us {} is negative or more than an hour ahead of the server clock",
            sample.timestamp_us
        ));
    } else {
        sample.timestamp_us
    };
    Ok(MetricRecord {
        name: sample.name,
        app_id: sample.app_id,
        labels: sample.labels.into_iter().collect(),
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

/// Turn an empty string into "no filter".
fn optional(value: String) -> Option<String> {
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
        let request = request.into_inner();
        ServiceStats::incr(&self.stats.log_pushes);
        let received = request.entries.len();
        if received == 0 {
            debug!("received an empty log batch, nothing to store");
            return Ok(Response::new(PushLogsResponse::default()));
        }

        let now = now_us();
        let mut records = Vec::with_capacity(received);
        let mut truncated = 0u64;
        let mut stamped = 0u64;
        let mut rejected = 0u64;
        let mut first_rejection: Option<String> = None;
        for entry in request.entries {
            match to_record(entry, now, &mut truncated, &mut stamped) {
                Ok(record) => records.push(record),
                Err(reason) => {
                    rejected += 1;
                    if first_rejection.is_none() {
                        first_rejection = Some(reason.describe());
                    }
                }
            }
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
                cap_bytes = MAX_LOG_MESSAGE_BYTES,
                "log messages exceeded the size cap and are stored cut short, the tail of those messages is lost"
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
        let request = request.into_inner();
        ServiceStats::incr(&self.stats.log_queries);
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
        let page = blocking(&self.stats, Access::Read, "query logs", move || {
            store.query_logs(&filter, limit, cursor)
        })
        .await?;
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
        let peer = request
            .remote_addr()
            .map(|addr| addr.to_string())
            .unwrap_or_else(|| "unknown-peer".to_string());
        let request = request.into_inner();
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
        let rx = self.hub.subscribe(label, filter);
        let stream = ReceiverStream::new(rx).map(|record| Ok(to_entry(&record)));
        Ok(Response::new(Box::pin(stream) as Self::TailStream))
    }
}

/// Why a trace record could not be stored.
#[derive(Debug)]
enum TraceRejection {
    Timestamp(i64),
    Duration(i64),
    Kind(i32),
    Outcome(i32),
}

impl TraceRejection {
    fn describe(&self) -> String {
        match self {
            Self::Timestamp(ts) => format!(
                "started_us {ts} is negative or more than an hour ahead of the server clock"
            ),
            Self::Duration(us) => format!("duration_us {us} is negative"),
            Self::Kind(kind) => {
                format!("kind {kind} is outside the InvocationKind range 0..={MAX_INVOCATION_KIND}")
            }
            Self::Outcome(outcome) => format!(
                "outcome {outcome} is outside the InvocationOutcome range \
                 0..={MAX_INVOCATION_OUTCOME}"
            ),
        }
    }
}

/// Convert a wire trace record into a storable one, or say why it cannot be.
///
/// A record with no `started_us` is stamped with the server receive time, the
/// same repair a log entry gets: a producer whose clock is unset is still
/// telling the truth about what happened, only not about when.
fn to_trace_record(
    record: ProtoTraceRecord,
    now_us: i64,
    truncated: &mut u64,
    stamped: &mut u64,
) -> Result<TraceRecord, TraceRejection> {
    let started_us = if record.started_us == 0 {
        *stamped += 1;
        now_us
    } else if record.started_us < 0 || record.started_us > now_us + MAX_FUTURE_SKEW_US {
        return Err(TraceRejection::Timestamp(record.started_us));
    } else {
        record.started_us
    };
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
        attributes: record.attributes.into_iter().collect(),
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

        let now = now_us();
        let mut records = Vec::with_capacity(received);
        let mut truncated = 0u64;
        let mut stamped = 0u64;
        let mut rejected = 0u64;
        let mut first_rejection: Option<String> = None;
        for record in request.records {
            match to_trace_record(record, now, &mut truncated, &mut stamped) {
                Ok(record) => records.push(record),
                Err(reason) => {
                    rejected += 1;
                    if first_rejection.is_none() {
                        first_rejection = Some(reason.describe());
                    }
                }
            }
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
                "trace targets or error messages exceeded their size cap and are stored cut \
                 short; the tail of those strings is lost"
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
        let request = request.into_inner();
        ServiceStats::incr(&self.stats.trace_queries);
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
        let page = blocking(&self.stats, Access::Read, "query traces", move || {
            store.query_traces(&filter, limit, cursor)
        })
        .await?;
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
        let peer = request
            .remote_addr()
            .map(|addr| addr.to_string())
            .unwrap_or_else(|| "unknown-peer".to_string());
        let request = request.into_inner();
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
        let rx = self.hub.subscribe(label, filter);
        let stream = ReceiverStream::new(rx).map(|record| Ok(to_proto_trace(&record)));
        Ok(Response::new(Box::pin(stream) as Self::TailTracesStream))
    }
}

#[tonic::async_trait]
impl MetricServiceRpc for MetricService {
    async fn push_metrics(
        &self,
        request: Request<PushMetricsRequest>,
    ) -> Result<Response<PushMetricsResponse>, Status> {
        let request = request.into_inner();
        ServiceStats::incr(&self.stats.metric_pushes);
        let received = request.samples.len();
        if received == 0 {
            debug!("received an empty metric batch, nothing to store");
            return Ok(Response::new(PushMetricsResponse::default()));
        }

        let now = now_us();
        let mut records = Vec::with_capacity(received);
        let mut rejected = 0u64;
        let mut first_rejection: Option<String> = None;
        for sample in request.samples {
            match to_metric_record(sample, now) {
                Ok(record) => records.push(record),
                Err(reason) => {
                    rejected += 1;
                    if first_rejection.is_none() {
                        first_rejection = Some(reason);
                    }
                }
            }
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
        let request = request.into_inner();
        ServiceStats::incr(&self.stats.metric_queries);
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
        let page = blocking(&self.stats, Access::Read, "query metrics", move || {
            store.query_metrics(&filter, limit, cursor)
        })
        .await?;
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
        let record = to_record(e, 42, &mut truncated, &mut stamped).expect("must be accepted");
        assert_eq!(record.timestamp_us, 42);
        assert_eq!(stamped, 1);
    }

    #[test]
    fn far_future_timestamp_is_rejected() {
        let mut truncated = 0;
        let mut stamped = 0;
        let mut e = entry("hello");
        e.timestamp_us = 10_000_000_000;
        let err = to_record(e, 1_000, &mut truncated, &mut stamped).expect_err("must be rejected");
        assert!(err.describe().contains("ahead of the server clock"));
    }

    #[test]
    fn out_of_range_level_is_rejected() {
        let mut truncated = 0;
        let mut stamped = 0;
        let mut e = entry("hello");
        e.level = 99;
        assert!(to_record(e, 1_700_000_000_000_001, &mut truncated, &mut stamped).is_err());
    }

    #[test]
    fn oversized_message_is_truncated_not_dropped() {
        let mut truncated = 0;
        let mut stamped = 0;
        let long = "x".repeat(MAX_LOG_MESSAGE_BYTES + 500);
        let record = to_record(
            entry(&long),
            1_700_000_000_000_001,
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
            1_700_000_000_000_001,
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
        assert!(to_metric_record(sample, 10).is_err());
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
        assert!(to_metric_record(sample, 10).is_err());
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
        let stored =
            to_trace_record(record, 42, &mut truncated, &mut stamped).expect("must be accepted");
        assert_eq!(stored.started_us, 42);
        assert_eq!(stamped, 1);
    }

    #[test]
    fn a_negative_duration_is_refused() {
        // It would sort ahead of every real invocation under a duration floor
        // and would read as a clock that ran backwards.
        let mut record = proto_trace();
        record.duration_us = -1;
        let err = to_trace_record(record, 1_700_000_000_000_001, &mut 0, &mut 0)
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
        assert!(to_trace_record(kind, 1_700_000_000_000_001, &mut 0, &mut 0).is_err());

        let mut outcome = proto_trace();
        outcome.outcome = 99;
        assert!(to_trace_record(outcome, 1_700_000_000_000_001, &mut 0, &mut 0).is_err());
    }

    #[test]
    fn an_oversized_target_is_cut_short_not_dropped() {
        // A URL is attacker-supplied and there is one trace per invocation,
        // so the record is kept and the tail of the string is not.
        let mut record = proto_trace();
        record.target = "x".repeat(MAX_TRACE_TARGET_BYTES + 500);
        let mut truncated = 0;
        let stored = to_trace_record(record, 1_700_000_000_000_001, &mut truncated, &mut 0)
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
}
