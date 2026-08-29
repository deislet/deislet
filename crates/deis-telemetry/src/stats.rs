//! Process-local counters describing what this service itself has done.
//!
//! These are exported next to the stored series on `/metrics` so an operator
//! can tell "no logs are showing up" (nothing was pushed) apart from "logs are
//! being pushed and rejected" (bad timestamps, oversized batches) without
//! reading the service's own log file.
//!
//! They describe this process only. Nothing here says anything about the
//! health of any other component.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

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
        }

        impl ServiceStats {
            pub fn new() -> Self {
                Self {
                    $( $field: AtomicU64::new(0), )+
                    started_at: Instant::now(),
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
        "Entries skipped for slow tail consumers; storage is unaffected";
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
    fn counters_accumulate() {
        let stats = ServiceStats::new();
        ServiceStats::add(&stats.logs_accepted, 40);
        ServiceStats::add(&stats.logs_accepted, 2);
        assert_eq!(ServiceStats::get(&stats.logs_accepted), 42);
    }
}
