//! Live fan-out of freshly ingested logs to `Tail` subscribers.
//!
//! The rule this module exists to enforce: **a slow tail consumer must never
//! slow down ingestion.** Everything is pushed into a bounded broadcast
//! buffer, which overwrites its oldest slot when full. A consumer that falls
//! behind loses entries - for that consumer only - and the loss is counted and
//! logged rather than propagated back to the writer.
//!
//! The stored copy is never affected: whatever a tail stream misses is still
//! in SQLite and comes back through `Query`.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, info, warn};

use crate::stats::ServiceStats;
use crate::store::LogRecord;

/// How many entries one tail stream may have queued toward its client before
/// the pump stops pulling. Back-pressure stops here: it turns into broadcast
/// lag (dropped entries for this stream), never into a stalled writer.
const SUBSCRIBER_QUEUE: usize = 256;

/// Which live entries a subscriber wants. `None` means "no restriction".
#[derive(Debug, Clone, Default)]
pub struct TailFilter {
    pub app_id: Option<String>,
    /// Minimum `LogLevel` ordinal.
    pub min_level: Option<i32>,
}

impl TailFilter {
    pub fn matches(&self, record: &LogRecord) -> bool {
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
}

/// Fan-out point between the ingest path and every open tail stream.
pub struct TailHub {
    tx: broadcast::Sender<Arc<LogRecord>>,
    /// Flipped once on shutdown. Without it a client that is following logs
    /// would keep its stream - and therefore the whole gRPC server's graceful
    /// shutdown - open forever, because a tail stream has no natural end.
    stop: watch::Sender<bool>,
    stats: Arc<ServiceStats>,
}

impl TailHub {
    /// `capacity` is how many entries the shared live buffer holds. Bigger
    /// buffers tolerate slower consumers at the cost of memory.
    pub fn new(capacity: usize, stats: Arc<ServiceStats>) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        let (stop, _) = watch::channel(false);
        Self { tx, stop, stats }
    }

    /// End every open tail stream. Called on shutdown; following logs must
    /// not be able to hold the service open.
    pub fn close(&self) {
        let open = self.subscriber_count();
        self.stop.send_replace(true);
        if open > 0 {
            info!(
                streams = open,
                "closing tail streams because the service is shutting down; \
                 followers must reconnect after the restart"
            );
        }
    }

    /// Hand freshly stored entries to every open tail stream.
    ///
    /// Never blocks and never fails: with no subscribers the entries are
    /// discarded, and with a slow subscriber the buffer overwrites its oldest
    /// slot, which that subscriber later observes as a drop.
    pub fn publish(&self, records: &[Arc<LogRecord>]) {
        if self.tx.receiver_count() == 0 {
            return;
        }
        for record in records {
            // The only error is "no receivers left", which is not a problem.
            let _ = self.tx.send(Arc::clone(record));
        }
    }

    /// How many tail streams are currently attached.
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Open a tail stream. `label` identifies the subscriber in log lines.
    pub fn subscribe(&self, label: String, filter: TailFilter) -> mpsc::Receiver<Arc<LogRecord>> {
        let (tx, rx) = mpsc::channel(SUBSCRIBER_QUEUE);
        let stats = Arc::clone(&self.stats);
        let live = self.tx.subscribe();
        let stop = self.stop.subscribe();
        tokio::spawn(pump(label, filter, live, tx, stop, stats));
        rx
    }
}

/// Move entries from the shared live buffer into one subscriber's queue,
/// applying that subscriber's filter and accounting for what it missed.
async fn pump(
    label: String,
    filter: TailFilter,
    mut live: broadcast::Receiver<Arc<LogRecord>>,
    out: mpsc::Sender<Arc<LogRecord>>,
    mut stop: watch::Receiver<bool>,
    stats: Arc<ServiceStats>,
) {
    if *stop.borrow() {
        debug!(
            subscriber = %label,
            "tail stream requested while the service is shutting down, closing it right away"
        );
        return;
    }
    ServiceStats::incr(&stats.tail_streams_started);
    ServiceStats::incr(&stats.tail_streams_open);
    info!(
        subscriber = %label,
        app_id = filter.app_id.as_deref().unwrap_or("*"),
        min_level = filter.min_level.unwrap_or(0),
        "tail stream opened"
    );

    let mut dropped_total: u64 = 0;
    let mut forwarded: u64 = 0;
    loop {
        let next = tokio::select! {
            _ = stop.changed() => {
                debug!(subscriber = %label, "service is shutting down, closing this tail stream");
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
                            "service is shutting down while this stream was backed up, closing it"
                        );
                        break;
                    }
                    sent = out.send(record) => sent,
                };
                if sent.is_err() {
                    debug!(
                        subscriber = %label,
                        "tail client disconnected, closing its stream"
                    );
                    break;
                }
                forwarded += 1;
            }
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                dropped_total = dropped_total.saturating_add(missed);
                ServiceStats::add(&stats.tail_entries_dropped, missed);
                warn!(
                    subscriber = %label,
                    missed,
                    dropped_total,
                    "tail consumer fell behind the live buffer; those entries are \
                     skipped for this stream only - ingestion was never slowed and \
                     the entries are still queryable from storage"
                );
            }
            Err(broadcast::error::RecvError::Closed) => {
                debug!(
                    subscriber = %label,
                    "live buffer closed, service is shutting down"
                );
                break;
            }
        }
    }

    ServiceStats::decr(&stats.tail_streams_open);
    info!(
        subscriber = %label,
        forwarded,
        dropped_total,
        "tail stream closed"
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
