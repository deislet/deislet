//! Live fan-out of freshly ingested records to `Tail` and `TailTraces`
//! subscribers.
//!
//! The rule this module exists to enforce: **a slow consumer must never slow
//! down ingestion.** Everything is pushed into a bounded broadcast buffer,
//! which overwrites its oldest slot when full. A consumer that falls behind
//! loses records - for that consumer only - and the loss is counted and
//! logged rather than propagated back to the writer.
//!
//! The stored copy is never affected: whatever a stream misses is still in
//! SQLite and comes back through `Query` or `QueryTraces`.
//!
//! Logs and traces share the machinery and nothing else. [`LiveHub`] is
//! generic over what it carries, and each family brings its own counters, so
//! "log entries a slow follower missed" and "trace records a slow follower
//! missed" stay two numbers an operator can tell apart.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, info, warn};

use crate::stats::ServiceStats;
use crate::store::{LogRecord, TraceRecord};

/// How many records one stream may have queued toward its client before the
/// pump stops pulling. Back-pressure stops here: it turns into broadcast lag
/// (dropped records for this stream), never into a stalled writer.
const SUBSCRIBER_QUEUE: usize = 256;

/// Which live records a subscriber wants.
///
/// Implemented per family rather than shared, because the two families are
/// filtered on different things: a log stream narrows by severity, a trace
/// stream narrows by what the platform decided about the invocation.
pub trait LiveFilter<T>: Send + 'static {
    fn matches(&self, record: &T) -> bool;

    /// How the filter reads in this stream's log lines, so an operator can
    /// tell two open streams apart.
    fn describe(&self) -> String;
}

/// Which live log entries a subscriber wants. `None` means "no restriction".
#[derive(Debug, Clone, Default)]
pub struct TailFilter {
    pub app_id: Option<String>,
    /// Minimum `LogLevel` ordinal.
    pub min_level: Option<i32>,
}

impl LiveFilter<LogRecord> for TailFilter {
    fn matches(&self, record: &LogRecord) -> bool {
        if let Some(app_id) = &self.app_id
            && &record.app_id != app_id
        {
            return false;
        }
        if let Some(min_level) = self.min_level
            && record.level < min_level
        {
            return false;
        }
        true
    }

    fn describe(&self) -> String {
        format!(
            "app_id={} min_level={}",
            self.app_id.as_deref().unwrap_or("*"),
            self.min_level.unwrap_or(0)
        )
    }
}

/// Which live trace records a subscriber wants. An empty list means "every
/// value of that field", which is what makes `TailTraces` with nothing set
/// follow every invocation on the node.
#[derive(Debug, Clone, Default)]
pub struct TraceTailFilter {
    pub app_id: Option<String>,
    pub environment: Option<String>,
    /// `InvocationKind` ordinals.
    pub kinds: Vec<i32>,
    /// `InvocationOutcome` ordinals. This is how a follower asks to be shown
    /// only the invocations that failed.
    pub outcomes: Vec<i32>,
}

impl LiveFilter<TraceRecord> for TraceTailFilter {
    fn matches(&self, record: &TraceRecord) -> bool {
        if let Some(app_id) = &self.app_id
            && &record.app_id != app_id
        {
            return false;
        }
        if let Some(environment) = &self.environment
            && &record.environment != environment
        {
            return false;
        }
        if !self.kinds.is_empty() && !self.kinds.contains(&record.kind) {
            return false;
        }
        if !self.outcomes.is_empty() && !self.outcomes.contains(&record.outcome) {
            return false;
        }
        true
    }

    fn describe(&self) -> String {
        format!(
            "app_id={} environment={} kinds={:?} outcomes={:?}",
            self.app_id.as_deref().unwrap_or("*"),
            self.environment.as_deref().unwrap_or("*"),
            self.kinds,
            self.outcomes
        )
    }
}

/// The counters one hub keeps, named by projection out of [`ServiceStats`].
///
/// Passing the accessors rather than the numbers is what lets one hub
/// implementation serve two families without either family's counters
/// standing in for the other's.
#[derive(Clone, Copy)]
pub struct HubMeters {
    pub streams_started: fn(&ServiceStats) -> &AtomicU64,
    pub streams_open: fn(&ServiceStats) -> &AtomicU64,
    pub entries_dropped: fn(&ServiceStats) -> &AtomicU64,
}

/// Fan-out point between the ingest path and every open stream of one family.
pub struct LiveHub<T> {
    tx: broadcast::Sender<Arc<T>>,
    /// Flipped once on shutdown. Without it a client that is following would
    /// keep its stream - and therefore the whole gRPC server's graceful
    /// shutdown - open forever, because a follow stream has no natural end.
    stop: watch::Sender<bool>,
    stats: Arc<ServiceStats>,
    meters: HubMeters,
    /// What the records are called in this hub's log lines.
    family: &'static str,
}

/// The hub `LogService.Tail` streams from.
pub type TailHub = LiveHub<LogRecord>;

/// The hub `TraceService.TailTraces` streams from.
pub type TraceHub = LiveHub<TraceRecord>;

impl TailHub {
    /// `capacity` is how many entries the shared live buffer holds. Bigger
    /// buffers tolerate slower consumers at the cost of memory.
    pub fn new(capacity: usize, stats: Arc<ServiceStats>) -> Self {
        Self::with_meters(
            capacity,
            stats,
            "log entry",
            HubMeters {
                streams_started: |s| &s.tail_streams_started,
                streams_open: |s| &s.tail_streams_open,
                entries_dropped: |s| &s.tail_entries_dropped,
            },
        )
    }
}

impl TraceHub {
    /// A hub for trace records. Traces arrive at one record per invocation
    /// rather than one per log line, so this buffer is normally configured
    /// larger than the log one.
    pub fn new(capacity: usize, stats: Arc<ServiceStats>) -> Self {
        Self::with_meters(
            capacity,
            stats,
            "trace record",
            HubMeters {
                streams_started: |s| &s.trace_tail_streams_started,
                streams_open: |s| &s.trace_tail_streams_open,
                entries_dropped: |s| &s.trace_tail_entries_dropped,
            },
        )
    }
}

impl<T: Send + Sync + 'static> LiveHub<T> {
    fn with_meters(
        capacity: usize,
        stats: Arc<ServiceStats>,
        family: &'static str,
        meters: HubMeters,
    ) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        let (stop, _) = watch::channel(false);
        Self {
            tx,
            stop,
            stats,
            meters,
            family,
        }
    }

    /// End every open stream. Called on shutdown; following must not be able
    /// to hold the service open.
    pub fn close(&self) {
        let open = self.subscriber_count();
        self.stop.send_replace(true);
        if open > 0 {
            info!(
                streams = open,
                family = self.family,
                "closing follow streams because the service is shutting down; \
                 followers must reconnect after the restart"
            );
        }
    }

    /// Hand freshly stored records to every open stream.
    ///
    /// Never blocks and never fails: with no subscribers the records are
    /// discarded, and with a slow subscriber the buffer overwrites its oldest
    /// slot, which that subscriber later observes as a drop.
    pub fn publish(&self, records: &[Arc<T>]) {
        if self.tx.receiver_count() == 0 {
            return;
        }
        for record in records {
            // The only error is "no receivers left", which is not a problem.
            let _ = self.tx.send(Arc::clone(record));
        }
    }

    /// How many streams are currently attached.
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Open a stream. `label` identifies the subscriber in log lines.
    pub fn subscribe<F: LiveFilter<T>>(&self, label: String, filter: F) -> mpsc::Receiver<Arc<T>> {
        let (tx, rx) = mpsc::channel(SUBSCRIBER_QUEUE);
        let stats = Arc::clone(&self.stats);
        let live = self.tx.subscribe();
        let stop = self.stop.subscribe();
        tokio::spawn(pump(
            label,
            filter,
            live,
            tx,
            stop,
            stats,
            self.meters,
            self.family,
        ));
        rx
    }
}

/// Move records from the shared live buffer into one subscriber's queue,
/// applying that subscriber's filter and accounting for what it missed.
#[allow(clippy::too_many_arguments)]
async fn pump<T, F>(
    label: String,
    filter: F,
    mut live: broadcast::Receiver<Arc<T>>,
    out: mpsc::Sender<Arc<T>>,
    mut stop: watch::Receiver<bool>,
    stats: Arc<ServiceStats>,
    meters: HubMeters,
    family: &'static str,
) where
    T: Send + Sync + 'static,
    F: LiveFilter<T>,
{
    if *stop.borrow() {
        debug!(
            subscriber = %label,
            family,
            "follow stream requested while the service is shutting down, closing it right away"
        );
        return;
    }
    ServiceStats::incr((meters.streams_started)(&stats));
    ServiceStats::incr((meters.streams_open)(&stats));
    info!(
        subscriber = %label,
        family,
        filter = %filter.describe(),
        "follow stream opened"
    );

    let mut dropped_total: u64 = 0;
    let mut forwarded: u64 = 0;
    loop {
        let next = tokio::select! {
            _ = stop.changed() => {
                debug!(subscriber = %label, family, "service is shutting down, closing this stream");
                break;
            }
            next = live.recv() => next,
        };
        match next {
            Ok(record) => {
                if !filter.matches(&record) {
                    continue;
                }
                // The send waits when this client's queue is full, so it has
                // to watch for shutdown as well: a client that stopped
                // reading altogether must not be able to hold the process
                // open by leaving the pump parked here.
                let sent = tokio::select! {
                    _ = stop.changed() => {
                        debug!(
                            subscriber = %label,
                            family,
                            "service is shutting down while this stream was backed up, closing it"
                        );
                        break;
                    }
                    sent = out.send(record) => sent,
                };
                if sent.is_err() {
                    debug!(
                        subscriber = %label,
                        family,
                        "follow client disconnected, closing its stream"
                    );
                    break;
                }
                forwarded += 1;
            }
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                dropped_total = dropped_total.saturating_add(missed);
                ServiceStats::add((meters.entries_dropped)(&stats), missed);
                warn!(
                    subscriber = %label,
                    family,
                    missed,
                    dropped_total,
                    "follow consumer fell behind the live buffer; those records are \
                     skipped for this stream only - ingestion was never slowed and \
                     the records are still queryable from storage"
                );
            }
            Err(broadcast::error::RecvError::Closed) => {
                debug!(
                    subscriber = %label,
                    family,
                    "live buffer closed, service is shutting down"
                );
                break;
            }
        }
    }

    ServiceStats::decr((meters.streams_open)(&stats));
    info!(
        subscriber = %label,
        family,
        forwarded,
        dropped_total,
        "follow stream closed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn record(app_id: &str, level: i32, message: &str) -> Arc<LogRecord> {
        Arc::new(LogRecord {
            timestamp_us: 1,
            level,
            component: "deis-runtime".into(),
            app_id: app_id.into(),
            message: message.into(),
            fields: BTreeMap::new(),
            trace_id: String::new(),
            span_id: String::new(),
        })
    }

    fn trace(app_id: &str, environment: &str, kind: i32, outcome: i32) -> Arc<TraceRecord> {
        Arc::new(TraceRecord {
            trace_id: "t".into(),
            started_us: 1,
            duration_us: 1,
            app_id: app_id.into(),
            environment: environment.into(),
            node: "node-a".into(),
            kind,
            outcome,
            target: "https://example.test/".into(),
            method: "GET".into(),
            status: 200,
            error: String::new(),
            attributes: BTreeMap::new(),
        })
    }

    #[test]
    fn filter_matches_on_app_and_level() {
        let filter = TailFilter {
            app_id: Some("shop".into()),
            min_level: Some(4),
        };
        assert!(filter.matches(&record("shop", 5, "boom")));
        assert!(!filter.matches(&record("shop", 3, "routine")));
        assert!(!filter.matches(&record("blog", 5, "boom")));
        assert!(TailFilter::default().matches(&record("blog", 1, "trace")));
    }

    #[test]
    fn a_trace_filter_narrows_to_the_invocations_that_failed() {
        // The question the trace surface exists for: show me the failures,
        // and nothing else, as they happen.
        let filter = TraceTailFilter {
            outcomes: vec![2, 3, 4, 5, 6],
            ..TraceTailFilter::default()
        };
        assert!(filter.matches(&trace("shop", "production", 1, 3)));
        assert!(!filter.matches(&trace("shop", "production", 1, 1)));
    }

    #[test]
    fn a_trace_filter_separates_environments_of_one_application() {
        let filter = TraceTailFilter {
            app_id: Some("shop".into()),
            environment: Some("preview".into()),
            ..TraceTailFilter::default()
        };
        assert!(filter.matches(&trace("shop", "preview", 1, 1)));
        assert!(!filter.matches(&trace("shop", "production", 1, 1)));
        assert!(!filter.matches(&trace("blog", "preview", 1, 1)));
    }

    #[test]
    fn an_empty_trace_filter_follows_every_invocation() {
        let filter = TraceTailFilter::default();
        assert!(filter.matches(&trace("shop", "production", 1, 1)));
        assert!(filter.matches(&trace("blog", "preview", 3, 4)));
    }

    #[tokio::test]
    async fn subscriber_receives_matching_entries_only() {
        let stats = Arc::new(ServiceStats::new());
        let hub = TailHub::new(64, Arc::clone(&stats));
        let mut rx = hub.subscribe(
            "test".into(),
            TailFilter {
                app_id: Some("shop".into()),
                min_level: None,
            },
        );
        // The pump subscribes inside a spawned task; wait until it is
        // attached so the publish below cannot race ahead of it.
        while hub.subscriber_count() == 0 {
            tokio::task::yield_now().await;
        }

        hub.publish(&[
            record("blog", 3, "ignored"),
            record("shop", 3, "kept"),
            record("shop", 5, "kept too"),
        ]);

        assert_eq!(rx.recv().await.expect("entry").message, "kept");
        assert_eq!(rx.recv().await.expect("entry").message, "kept too");
    }

    #[tokio::test]
    async fn a_trace_stream_forwards_only_the_outcomes_it_asked_for() {
        let stats = Arc::new(ServiceStats::new());
        let hub = TraceHub::new(64, Arc::clone(&stats));
        let mut rx = hub.subscribe(
            "test".into(),
            TraceTailFilter {
                outcomes: vec![3],
                ..TraceTailFilter::default()
            },
        );
        while hub.subscriber_count() == 0 {
            tokio::task::yield_now().await;
        }

        let mut cut_off = trace("shop", "production", 1, 3);
        Arc::get_mut(&mut cut_off).expect("sole owner").target = "https://shop.test/slow".into();
        hub.publish(&[trace("shop", "production", 1, 1), cut_off]);

        assert_eq!(
            rx.recv().await.expect("record").target,
            "https://shop.test/slow"
        );
    }

    #[tokio::test]
    async fn a_slow_trace_follower_does_not_spend_the_log_drop_counter() {
        // Two families, two numbers: an operator asking "did I lose logs?"
        // must not be answered with a trace follower's arithmetic.
        let stats = Arc::new(ServiceStats::new());
        let hub = TraceHub::new(4, Arc::clone(&stats));
        let live = hub.tx.subscribe();
        for _ in 0..10 {
            hub.publish(&[trace("shop", "production", 1, 1)]);
        }
        let (out, mut rx) = mpsc::channel(16);
        let stop = hub.stop.subscribe();
        let pump = tokio::spawn(pump(
            "slow".into(),
            TraceTailFilter::default(),
            live,
            out,
            stop,
            Arc::clone(&stats),
            HubMeters {
                streams_started: |s| &s.trace_tail_streams_started,
                streams_open: |s| &s.trace_tail_streams_open,
                entries_dropped: |s| &s.trace_tail_entries_dropped,
            },
            "trace record",
        ));

        for _ in 0..4 {
            rx.recv().await.expect("record");
        }
        assert_eq!(ServiceStats::get(&stats.trace_tail_entries_dropped), 6);
        assert_eq!(
            ServiceStats::get(&stats.tail_entries_dropped),
            0,
            "a trace follower's loss must never be reported as lost log entries"
        );

        drop(hub);
        pump.await
            .expect("pump task must finish when the hub is gone");
    }

    #[tokio::test]
    async fn closing_the_hub_ends_open_streams() {
        let stats = Arc::new(ServiceStats::new());
        let hub = TailHub::new(64, Arc::clone(&stats));
        let mut rx = hub.subscribe("test".into(), TailFilter::default());
        while hub.subscriber_count() == 0 {
            tokio::task::yield_now().await;
        }

        // Without this, a follower would keep the service from shutting down.
        hub.close();
        assert!(
            rx.recv().await.is_none(),
            "the stream must end rather than hang once the hub is closed"
        );
        while ServiceStats::get(&stats.tail_streams_open) != 0 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn a_backed_up_stream_still_closes_on_shutdown() {
        let stats = Arc::new(ServiceStats::new());
        let hub = TailHub::new(4096, Arc::clone(&stats));
        let mut rx = hub.subscribe("stuck".into(), TailFilter::default());
        // Wait for the pump task itself to start, not just for the
        // subscription to be registered.
        while ServiceStats::get(&stats.tail_streams_open) == 0 {
            tokio::task::yield_now().await;
        }
        // More than one client queue holds, with nobody reading.
        let batch_size = SUBSCRIBER_QUEUE + 50;
        let batch: Vec<Arc<LogRecord>> = (0..batch_size)
            .map(|i| record("shop", 3, &format!("entry {i}")))
            .collect();
        hub.publish(&batch);
        // A full queue is where the pump parks.
        while rx.len() < SUBSCRIBER_QUEUE {
            tokio::task::yield_now().await;
        }

        hub.close();
        // Draining reaches the end of the stream instead of hanging.
        let mut drained = 0;
        while rx.recv().await.is_some() {
            drained += 1;
        }
        // At least everything already queued is delivered. The pump may also
        // squeeze in a few more: once the queue drains, both the shutdown
        // signal and the pending send are ready and either may be taken
        // first. What matters is that the stream ends at all.
        assert!(
            (SUBSCRIBER_QUEUE..=batch_size).contains(&drained),
            "expected between {SUBSCRIBER_QUEUE} and {batch_size} entries, got {drained}"
        );
        while ServiceStats::get(&stats.tail_streams_open) != 0 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn a_stream_opened_during_shutdown_ends_immediately() {
        let stats = Arc::new(ServiceStats::new());
        let hub = TailHub::new(64, Arc::clone(&stats));
        hub.close();
        let mut rx = hub.subscribe("late".into(), TailFilter::default());
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn slow_subscriber_drops_are_counted_and_not_pushed_back() {
        let stats = Arc::new(ServiceStats::new());
        let hub = TailHub::new(4, Arc::clone(&stats));
        // Attach a receiver by hand and overrun it before the pump starts,
        // so the very first recv reports a deterministic lag instead of
        // depending on task scheduling.
        let live = hub.tx.subscribe();
        for i in 0..10 {
            hub.publish(&[record("shop", 3, &format!("entry {i}"))]);
        }
        let (out, mut rx) = mpsc::channel(16);
        let stop = hub.stop.subscribe();
        let pump = tokio::spawn(pump(
            "slow".into(),
            TailFilter::default(),
            live,
            out,
            stop,
            Arc::clone(&stats),
            HubMeters {
                streams_started: |s| &s.tail_streams_started,
                streams_open: |s| &s.tail_streams_open,
                entries_dropped: |s| &s.tail_entries_dropped,
            },
            "log entry",
        ));

        // Only the last 4 entries survived in the buffer; the other 6 were
        // overwritten while the writer carried on undisturbed.
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(rx.recv().await.expect("entry").message.clone());
        }
        assert_eq!(seen, vec!["entry 6", "entry 7", "entry 8", "entry 9"]);
        assert_eq!(ServiceStats::get(&stats.tail_entries_dropped), 6);

        drop(hub);
        pump.await
            .expect("pump task must finish when the hub is gone");
        assert_eq!(ServiceStats::get(&stats.tail_streams_open), 0);
    }
}
