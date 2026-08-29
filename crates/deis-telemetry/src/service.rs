//! gRPC surface: [`LogService`] (ingest, query, live follow) and
//! [`MetricService`] (sample ingest and query). Both are served on the same
//! port and share one database.
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
use crate::proto::{
    LogEntry, MetricSample, PushLogsRequest, PushLogsResponse, PushMetricsRequest,
    PushMetricsResponse, QueryMetricsRequest, QueryMetricsResponse, QueryRequest, QueryResponse,
    TailRequest,
};
use crate::stats::ServiceStats;
use crate::store::{
    LogCursor, LogFilter, LogRecord, MetricCursor, MetricFilter, MetricRecord, TelemetryStore,
};
use crate::tail::{TailFilter, TailHub};

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

/// Run a blocking storage call off the reactor.
///
/// `op` names the operation so a failure says which one failed; the caller
/// gets a generic `internal` status because the detail belongs in our log,
/// not in a client's error string.
async fn blocking<T, F>(op: &'static str, f: F) -> Result<T, Status>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => {
            error!(operation = op, error = ?e, "storage call failed, the request is answered with an error");
            Err(Status::internal(format!("{op} failed")))
        }
        Err(e) => {
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
    let message = if entry.message.len() > MAX_LOG_MESSAGE_BYTES {
        *truncated += 1;
        let mut cut = MAX_LOG_MESSAGE_BYTES;
        while cut > 0 && !entry.message.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}{TRUNCATION_MARKER}", &entry.message[..cut])
    } else {
        entry.message
    };
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
            let stored = blocking("store log batch", move || {
                store.insert_logs(&records)?;
                Ok(records)
            })
            .await?;
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
        let page = blocking("query logs", move || {
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
            blocking("store metric batch", move || store.insert_metrics(&records)).await?;
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
        let page = blocking("query metrics", move || {
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
