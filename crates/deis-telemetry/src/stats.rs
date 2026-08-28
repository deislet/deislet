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

#[derive(Debug)]
pub struct ServiceStats {
    /// PushLogs calls served.
    pub log_pushes: AtomicU64,
    /// Log entries written to storage.
    pub logs_accepted: AtomicU64,
    /// Log entries refused as malformed; these were never stored.
    pub logs_rejected: AtomicU64,
    /// Log entries whose message exceeded the cap and was cut short.
    pub logs_truncated: AtomicU64,
    /// Query calls served.
    pub log_queries: AtomicU64,
    /// Tail streams opened since start.
    pub tail_streams_started: AtomicU64,
    /// Tail streams currently open.
    pub tail_streams_open: AtomicU64,
    /// Entries a Tail subscriber missed because it could not keep up. The
    /// stored copy is unaffected; only the live view skipped them.
    pub tail_entries_dropped: AtomicU64,
    /// PushMetrics calls served.
    pub metric_pushes: AtomicU64,
    /// Metric samples written to storage.
    pub metric_samples_accepted: AtomicU64,
    /// Metric samples refused as malformed.
    pub metric_samples_rejected: AtomicU64,
    /// QueryMetrics calls served.
    pub metric_queries: AtomicU64,
    /// Log partitions dropped by retention since start.
    pub retention_partitions_dropped: AtomicU64,
    /// Log rows lost with those partitions.
    pub retention_log_rows_dropped: AtomicU64,
    /// Metric samples deleted by retention since start.
    pub retention_metric_samples_dropped: AtomicU64,
    started_at: Instant,
}

impl Default for ServiceStats {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceStats {
    pub fn new() -> Self {
        Self {
            log_pushes: AtomicU64::new(0),
            logs_accepted: AtomicU64::new(0),
            logs_rejected: AtomicU64::new(0),
            logs_truncated: AtomicU64::new(0),
            log_queries: AtomicU64::new(0),
            tail_streams_started: AtomicU64::new(0),
            tail_streams_open: AtomicU64::new(0),
            tail_entries_dropped: AtomicU64::new(0),
            metric_pushes: AtomicU64::new(0),
            metric_samples_accepted: AtomicU64::new(0),
            metric_samples_rejected: AtomicU64::new(0),
            metric_queries: AtomicU64::new(0),
            retention_partitions_dropped: AtomicU64::new(0),
            retention_log_rows_dropped: AtomicU64::new(0),
            retention_metric_samples_dropped: AtomicU64::new(0),
            started_at: Instant::now(),
        }
    }

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
    fn counters_accumulate() {
        let stats = ServiceStats::new();
        ServiceStats::add(&stats.logs_accepted, 40);
        ServiceStats::add(&stats.logs_accepted, 2);
        assert_eq!(ServiceStats::get(&stats.logs_accepted), 42);
    }
}
