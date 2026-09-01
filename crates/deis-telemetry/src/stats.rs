//! Process-local counters describing what this service itself has done.
//!
//! These are exported next to the stored series on `/metrics` so an operator
//! can tell "no logs are showing up" (nothing was pushed) apart from "logs are
//! being pushed and rejected" (bad timestamps, oversized batches) without
//! reading the service's own log file.
//!
//! They describe this process only. Nothing here says anything about the
//! health of any other component.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tokio::sync::watch;
use tracing::{error, info};

/// Relaxed ordering is right for every counter here: they are read for
/// reporting, never to establish happens-before between threads.
const ORD: Ordering = Ordering::Relaxed;

/// Which kind of Prometheus series a number is. A counter only ever grows; a
/// gauge can move in both directions. Naming it wrong makes `rate()` lie, so
/// the kind is declared next to the counter instead of being decided at
/// render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    Gauge,
}

impl MetricKind {
    /// The token the exposition format expects on a `# TYPE` line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
        }
    }
}

/// Label value standing in for every application past [`MAX_TRACKED_APPS`].
///
/// The parentheses are what make it safe: an application id is
/// `[A-Za-z0-9_-]` everywhere the platform mints one, so no real application
/// can land on this series by accident. A producer is free to send whatever
/// string it likes as an `app_id`, so a deliberately named one still could -
/// that costs an operator a misattributed line in one bucket, never a wrong
/// total.
pub const OVERFLOW_APP: &str = "(other)";

/// How many applications a per-application breakdown names individually.
///
/// Every named application is one more series on `/metrics`, and the id comes
/// off the wire from whatever pushed the record, so an unbounded map is an
/// unbounded scrape - a node shipping ten thousand distinct ids would grow
/// this process's memory and then break the scrape for every other series on
/// the page. Past the cap the counting continues under [`OVERFLOW_APP`], so
/// the sum still adds up and only the attribution is lost.
pub const MAX_TRACKED_APPS: usize = 64;

/// Which direction a storage call went, so that a success in one direction
/// cannot vouch for the other.
///
/// The distinction is the whole point. The commonest way for this service to
/// break is a disk with no space left, and on that disk every read still
/// works: a scrape reads the metric table happily while every log write is
/// refused. A single "the store is fine" flag would be set back to fine by
/// the next scrape, once a second, and the failure would be invisible again
/// between one push and the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// An insert, a delete, or a retention sweep.
    Write,
    /// A query, or the read behind the Prometheus export.
    Read,
}

impl Access {
    fn as_str(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Read => "read",
        }
    }
}

/// What the last storage call in each direction did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreState {
    /// `true` while the last write succeeded.
    pub writable: bool,
    /// `true` while the last read succeeded.
    pub readable: bool,
}

impl StoreState {
    /// Whether this service can do its job at all. Ingest is the job, so a
    /// store that cannot be written to is not serving whatever its reads do.
    pub fn is_serving(self) -> bool {
        self.writable && self.readable
    }
}

/// Whether the database underneath this service is answering, in each
/// direction.
///
/// Every storage call reports its outcome here, and the answer drives two
/// things: the `store_writable` and `store_readable` gauges on `/metrics`,
/// and the standard `grpc.health.v1` status of all three services (see
/// [`crate::server::start`]). Before this existed, the health service was set
/// to `SERVING` once at startup and never touched again, so a process whose
/// disk had filled or whose database had been restored unwritable went on
/// answering every probe with `SERVING` while dropping every log line, trace
/// and sample it was handed. A collector that reports healthy while
/// collecting nothing is worse than one that reports broken: nothing
/// downstream has any other way to find out.
///
/// One failure is enough to say so, and one success **in the same direction**
/// is enough to take it back. That can flap, and flapping is the honest
/// answer while a disk is hovering at full - the alternative, a threshold, is
/// a window during which the service is known to be losing data and still
/// saying `SERVING`.
///
/// The three services share one database, so there is one state and not three.
#[derive(Debug)]
pub struct StoreHealth {
    /// A `watch` rather than two `AtomicBool`s because the health task has to
    /// be woken by a change; a poll loop would put a delay between the failure
    /// and the probe telling the truth about it.
    state: watch::Sender<StoreState>,
}

impl Default for StoreHealth {
    fn default() -> Self {
        Self::new()
    }
}

impl StoreHealth {
    /// Starts able to do both: nothing has failed yet, and the store was
    /// opened successfully or the process would not be running.
    pub fn new() -> Self {
        Self {
            state: watch::channel(StoreState {
                writable: true,
                readable: true,
            })
            .0,
        }
    }

    /// A storage call in `access` succeeded.
    pub fn record_success(&self, access: Access) {
        let changed = self.state.send_if_modified(|state| match access {
            Access::Write if !state.writable => {
                state.writable = true;
                true
            }
            Access::Read if !state.readable => {
                state.readable = true;
                true
            }
            _ => false,
        });
        if changed {
            info!(
                direction = access.as_str(),
                "the telemetry database is taking {}s again; whatever was refused while it was \
                 not is not recoverable",
                access.as_str()
            );
        }
    }

    /// A storage call in `access` failed, or did not complete.
    ///
    /// `operation` is the caller's own name for what it was doing, so the line
    /// says which surface noticed first.
    pub fn record_failure(&self, access: Access, operation: &str) {
        let changed = self.state.send_if_modified(|state| match access {
            Access::Write if state.writable => {
                state.writable = false;
                true
            }
            Access::Read if state.readable => {
                state.readable = false;
                true
            }
            _ => false,
        });
        if !changed {
            return;
        }
        match access {
            Access::Write => error!(
                operation,
                "the telemetry database stopped accepting writes; every push is refused from \
                 here on, so the logs, traces and metric samples produced meanwhile are lost for \
                 good. This process now reports NOT_SERVING on grpc.health.v1 and \
                 deis_telemetry_store_writable reads 0"
            ),
            Access::Read => error!(
                operation,
                "the telemetry database stopped answering reads; queries and the Prometheus \
                 export fail from here on, so stored data is still there but nothing can get at \
                 it. This process now reports NOT_SERVING on grpc.health.v1 and \
                 deis_telemetry_store_readable reads 0"
            ),
        }
    }

    /// What the last call in each direction did. `1` or `0` on `/metrics`.
    pub fn state(&self) -> StoreState {
        *self.state.borrow()
    }

    /// A receiver that is woken whenever either direction changes.
    pub fn subscribe(&self) -> watch::Receiver<StoreState> {
        self.state.subscribe()
    }
}

/// Losses broken down by the application whose records were thrown away.
///
/// A single total answers "did anything go missing". Only the breakdown
/// answers "whose, and how much" - the question an operator actually has when
/// a follow stream skipped the lines they were watching for, and the reason
/// this exists next to the plain counters above.
#[derive(Debug, Default)]
pub struct AppDrops {
    inner: Mutex<AppDropsState>,
}

#[derive(Debug, Default)]
struct AppDropsState {
    /// Per-application totals, at most [`MAX_TRACKED_APPS`] entries.
    named: BTreeMap<String, u64>,
    /// Everything counted after the cap was reached.
    overflow: u64,
}

impl AppDrops {
    /// Charge `n` losses to `app_id`.
    ///
    /// A poisoned lock is recovered from rather than propagated: this is
    /// accounting on the ingest path, and a panic here would take down a push
    /// that has already been stored successfully.
    pub fn add(&self, app_id: &str, n: u64) {
        if n == 0 {
            return;
        }
        let mut state = match self.inner.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(slot) = state.named.get_mut(app_id) {
            *slot = slot.saturating_add(n);
        } else if state.named.len() < MAX_TRACKED_APPS {
            state.named.insert(app_id.to_string(), n);
        } else {
            state.overflow = state.overflow.saturating_add(n);
        }
    }

    /// Every application this has counted for, in export order, with the
    /// overflow bucket last and only when it has anything in it.
    pub fn snapshot(&self) -> Vec<(String, u64)> {
        let state = match self.inner.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut out: Vec<(String, u64)> = state
            .named
            .iter()
            .map(|(app, n)| (app.clone(), *n))
            .collect();
        if state.overflow > 0 {
            out.push((OVERFLOW_APP.to_string(), state.overflow));
        }
        out
    }

    /// What this has counted for one application. Reporting only; the export
    /// walks [`AppDrops::snapshot`].
    pub fn get(&self, app_id: &str) -> u64 {
        let state = match self.inner.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.named.get(app_id).copied().unwrap_or(0)
    }
}

/// One exported series that carries labels, which [`SelfMetric`] cannot: its
/// values are owned strings read off the wire rather than constants.
#[derive(Debug, Clone)]
pub struct LabelledMetric {
    /// Name without the `deis_telemetry_` prefix, which the renderer adds.
    pub name: &'static str,
    pub kind: MetricKind,
    pub help: &'static str,
    pub labels: Vec<(&'static str, String)>,
    pub value: u64,
}

/// One of this process's own numbers, ready to be written out: how it is
/// called on `/metrics`, what kind of series it is, what it means, and what
/// it currently reads.
#[derive(Debug, Clone, Copy)]
pub struct SelfMetric {
    /// Name without the `deis_telemetry_` prefix, which the renderer adds.
    pub name: &'static str,
    pub kind: MetricKind,
    pub help: &'static str,
    pub value: u64,
}

/// Declare the counters once.
///
/// Each line gives the field, its Prometheus kind, its exported name and its
/// help text, and that one line produces the struct field, its initialiser,
/// its documentation and its entry in [`ServiceStats::snapshot`]. `/metrics`
/// renders nothing but that snapshot, so a counter cannot be added without
/// also being exported - the earlier hand-kept list silently lost one.
macro_rules! service_stats {
    ($( $field:ident : $kind:ident = $name:literal, $help:literal ; )+) => {
        #[derive(Debug)]
        pub struct ServiceStats {
            $(
                #[doc = $help]
                pub $field: AtomicU64,
            )+
            started_at: Instant,
            /// Whether the database is answering. Not a counter: it is the
            /// one number here that goes back down, and it is what the
            /// gRPC health status follows.
            pub store_health: StoreHealth,
            /// Live log entries thrown away, by the application that owned
            /// them. Exported alongside the plain total by
            /// [`ServiceStats::labelled_snapshot`].
            pub tail_entries_dropped_by_app: AppDrops,
            /// The same breakdown for trace records.
            pub trace_tail_entries_dropped_by_app: AppDrops,
        }

        impl ServiceStats {
            pub fn new() -> Self {
                Self {
                    $( $field: AtomicU64::new(0), )+
                    started_at: Instant::now(),
                    store_health: StoreHealth::new(),
                    tail_entries_dropped_by_app: AppDrops::default(),
                    trace_tail_entries_dropped_by_app: AppDrops::default(),
                }
            }

            /// Every number this process keeps, in export order.
            ///
            /// This is the only enumeration of the set; the renderer walks it
            /// rather than repeating it.
            pub fn snapshot(&self) -> Vec<SelfMetric> {
                vec![
                    // Uptime is the one exported number that is computed
                    // rather than counted, so it is not a field above.
                    SelfMetric {
                        name: "uptime_seconds",
                        kind: MetricKind::Counter,
                        help: "Seconds since this deis-telemetry process started",
                        value: self.uptime_seconds(),
                    },
                    // Read off `StoreHealth` rather than counted, like
                    // uptime above. These are the two series that answer "is
                    // the pipeline up right now" instead of "how much has
                    // happened since start", which is the question an operator
                    // has first. Two, not one, because a full disk refuses
                    // every write while answering every read.
                    SelfMetric {
                        name: "store_writable",
                        kind: MetricKind::Gauge,
                        help: "1 while the last write to the telemetry database succeeded, 0 \
                               after one failed; while this reads 0 every push is refused and \
                               every log line, trace and sample produced is lost for good",
                        value: u64::from(self.store_health.state().writable),
                    },
                    SelfMetric {
                        name: "store_readable",
                        kind: MetricKind::Gauge,
                        help: "1 while the last read of the telemetry database succeeded, 0 \
                               after one failed; while this reads 0 queries and this very page \
                               fail, and stored data cannot be got at",
                        value: u64::from(self.store_health.state().readable),
                    },
                    $(
                        SelfMetric {
                            name: $name,
                            kind: MetricKind::$kind,
                            help: $help,
                            value: Self::get(&self.$field),
                        },
                    )+
                ]
            }
        }
    };
}

service_stats! {
    storage_failures: Counter =
        "storage_failures_total",
        "Storage calls that failed or did not complete, over ingest, queries, the retention \
         sweep and the Prometheus export. The single number that moves whatever is wrong \
         underneath: every path into the database is counted here, so a full disk or an \
         unwritable file shows up even on a surface nobody happened to be watching";
    logs_lost: Counter =
        "logs_lost_total",
        "Well-formed log entries this service accepted from a producer and then failed to \
         write; unlike logs_rejected_total there was nothing wrong with them, and they will \
         never appear in Query or Tail";
    traces_lost: Counter =
        "traces_lost_total",
        "Well-formed trace records this service accepted and then failed to write; those \
         invocations happened and will never appear in QueryTraces or TailTraces";
    metric_samples_lost: Counter =
        "metric_samples_lost_total",
        "Well-formed metric samples this service accepted and then failed to write; the series \
         they belonged to shows a gap in QueryMetrics and on /metrics";
    retention_sweeps_failed: Counter =
        "retention_sweeps_failed_total",
        "Retention sweeps that failed or did not complete; expired logs, traces and samples \
         stay on disk and the database keeps growing until a sweep succeeds";
    metric_scrapes: Counter = "metric_scrapes_total", "Prometheus scrapes served on /metrics";
    metric_samples_dropped_from_export: Counter =
        "metric_samples_dropped_from_export_total",
        "Stored metric samples left out of a /metrics page because they could not be rendered: \
         a name with no character legal in the exposition format, or a histogram bucket whose \
         `le` label names no bound. They are still in storage and still come back from \
         QueryMetrics; it is the scraped series that has the gap";
    metric_labels_dropped_from_export: Counter =
        "metric_labels_dropped_from_export_total",
        "Labels left off a line on a /metrics page: a name with no legal character, or one that \
         collides with an earlier label after sanitising. The sample is exported without that \
         label, because repeating a label name makes a scraper reject the whole page rather \
         than the one series";
    metric_scrapes_failed: Counter =
        "metric_scrapes_failed_total",
        "Prometheus scrapes answered with 500 because the stored samples could not be read; \
         the scraper records those as a failed scrape and every stored series shows a gap";
    log_pushes: Counter = "log_pushes_total", "PushLogs calls served";
    logs_accepted: Counter = "logs_accepted_total", "Log entries written to storage";
    logs_rejected: Counter =
        "logs_rejected_total", "Log entries refused as malformed and never stored";
    logs_truncated: Counter =
        "logs_truncated_total", "Log entries stored with their message cut short";
    log_queries: Counter = "log_queries_total", "Query calls served";
    tail_streams_started: Counter = "tail_streams_started_total", "Tail streams opened since start";
    tail_streams_open: Gauge = "tail_streams_open", "Tail streams currently open";
    tail_entries_dropped: Counter =
        "tail_entries_dropped_total",
        "Live log entries thrown away before a follower read them, over every open stream; \
         storage is unaffected and the entries stay queryable";
    trace_pushes: Counter = "trace_pushes_total", "PushTraces calls served";
    traces_accepted: Counter = "traces_accepted_total", "Trace records written to storage";
    traces_rejected: Counter =
        "traces_rejected_total", "Trace records refused as malformed and never stored";
    traces_truncated: Counter =
        "traces_truncated_total",
        "Trace records stored with an oversized target or error message cut short";
    trace_queries: Counter = "trace_queries_total", "QueryTraces calls served";
    traces_sampled_out: Counter =
        "traces_sampled_out_total",
        "Invocations a producer reported as deliberately not recorded; sampling policy, not loss";
    traces_dropped_by_producer: Counter =
        "traces_dropped_by_producer_total",
        "Trace records a producer built and then lost because its own buffer was full";
    trace_tail_streams_started: Counter =
        "trace_tail_streams_started_total", "TailTraces streams opened since start";
    trace_tail_streams_open: Gauge =
        "trace_tail_streams_open", "TailTraces streams currently open";
    trace_tail_entries_dropped: Counter =
        "trace_tail_entries_dropped_total",
        "Live trace records thrown away before a follower read them, over every open stream; \
         storage is unaffected and the records stay queryable";
    metric_pushes: Counter = "metric_pushes_total", "PushMetrics calls served";
    metric_samples_accepted: Counter =
        "metric_samples_accepted_total", "Metric samples written to storage";
    metric_samples_rejected: Counter =
        "metric_samples_rejected_total", "Metric samples refused as malformed and never stored";
    metric_queries: Counter = "metric_queries_total", "QueryMetrics calls served";
    retention_partitions_dropped: Counter =
        "retention_partitions_dropped_total", "Expired daily log partitions dropped since start";
    retention_log_rows_dropped: Counter =
        "retention_log_rows_dropped_total", "Log rows lost with expired partitions since start";
    retention_metric_samples_dropped: Counter =
        "retention_metric_samples_dropped_total", "Metric samples deleted by retention since start";
    retention_trace_partitions_dropped: Counter =
        "retention_trace_partitions_dropped_total",
        "Expired daily trace partitions dropped since start";
    retention_trace_rows_dropped: Counter =
        "retention_trace_rows_dropped_total",
        "Trace records lost with expired partitions since start";
}

impl Default for ServiceStats {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceStats {
    /// Add `n` to a counter.
    pub fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, ORD);
    }

    /// Add one to a counter.
    pub fn incr(counter: &AtomicU64) {
        counter.fetch_add(1, ORD);
    }

    /// Subtract one from a gauge, saturating at zero so a bookkeeping slip
    /// cannot wrap the value around to a huge number.
    pub fn decr(gauge: &AtomicU64) {
        let _ = gauge.fetch_update(ORD, ORD, |v| Some(v.saturating_sub(1)));
    }

    /// Read a counter.
    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(ORD)
    }

    /// Seconds since the service started.
    pub fn uptime_seconds(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    /// Every number this process keeps that is broken down by application.
    ///
    /// Kept apart from [`ServiceStats::snapshot`] because these series carry a
    /// label and an owned value, and because the set is not fixed: an
    /// application appears here only once something of its was actually
    /// thrown away. The totals in `snapshot` stay authoritative - they count
    /// every loss, including the ones past [`MAX_TRACKED_APPS`] that this
    /// breakdown can only name as [`OVERFLOW_APP`].
    pub fn labelled_snapshot(&self) -> Vec<LabelledMetric> {
        let families: [(&'static str, &'static str, &AppDrops); 2] = [
            (
                "tail_entries_dropped_by_app_total",
                "Live log entries thrown away before a follower read them, by the application \
                 that produced them",
                &self.tail_entries_dropped_by_app,
            ),
            (
                "trace_tail_entries_dropped_by_app_total",
                "Live trace records thrown away before a follower read them, by the \
                 application that produced them",
                &self.trace_tail_entries_dropped_by_app,
            ),
        ];
        let mut out = Vec::new();
        for (name, help, drops) in families {
            for (app_id, value) in drops.snapshot() {
                out.push(LabelledMetric {
                    name,
                    kind: MetricKind::Counter,
                    help,
                    labels: vec![("app_id", app_id)],
                    value,
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gauge_does_not_wrap_below_zero() {
        let stats = ServiceStats::new();
        ServiceStats::decr(&stats.tail_streams_open);
        assert_eq!(ServiceStats::get(&stats.tail_streams_open), 0);
        ServiceStats::incr(&stats.tail_streams_open);
        ServiceStats::incr(&stats.tail_streams_open);
        ServiceStats::decr(&stats.tail_streams_open);
        assert_eq!(ServiceStats::get(&stats.tail_streams_open), 1);
    }

    #[test]
    fn exported_names_are_unique_and_legal() {
        // A repeated name would make two different numbers land on the same
        // Prometheus series; an illegal character would make the scrape
        // unparseable, and both would take the whole /metrics page with them.
        let stats = ServiceStats::new();
        let mut seen = std::collections::BTreeSet::new();
        for metric in stats.snapshot() {
            assert!(
                seen.insert(metric.name),
                "{} is exported twice",
                metric.name
            );
            assert!(!metric.help.is_empty(), "{} has no help text", metric.name);
            assert!(
                metric
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "{} is not a legal metric name",
                metric.name
            );
        }
    }

    #[test]
    fn a_breakdown_names_applications_until_the_cap_then_counts_the_rest_together() {
        // The id comes off the wire, so an unbounded map is an unbounded
        // scrape and unbounded memory. Past the cap the attribution is lost
        // on purpose - the total must not be.
        let drops = AppDrops::default();
        for i in 0..MAX_TRACKED_APPS {
            drops.add(&format!("app-{i:03}"), 1);
        }
        drops.add("app-999", 5);
        drops.add("app-998", 2);
        // A cap already reached does not stop an application already named
        // from being counted further.
        drops.add("app-000", 3);

        let snapshot = drops.snapshot();
        assert_eq!(snapshot.len(), MAX_TRACKED_APPS + 1);
        assert_eq!(drops.get("app-000"), 4);
        assert_eq!(drops.get("app-999"), 0, "past the cap nothing is named");
        assert_eq!(
            snapshot.last(),
            Some(&(OVERFLOW_APP.to_string(), 7)),
            "everything past the cap still adds up in one bucket"
        );
        let total: u64 = snapshot.iter().map(|(_, n)| n).sum();
        assert_eq!(total, MAX_TRACKED_APPS as u64 + 10);
    }

    #[test]
    fn a_breakdown_starts_empty_rather_than_at_zero_for_everything() {
        let drops = AppDrops::default();
        drops.add("shop", 0);
        assert!(
            drops.snapshot().is_empty(),
            "an application that lost nothing must not appear"
        );
    }

    #[test]
    fn counters_accumulate() {
        let stats = ServiceStats::new();
        ServiceStats::add(&stats.logs_accepted, 40);
        ServiceStats::add(&stats.logs_accepted, 2);
        assert_eq!(ServiceStats::get(&stats.logs_accepted), 42);
    }
}
