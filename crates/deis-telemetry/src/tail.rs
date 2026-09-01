//! Live fan-out of freshly ingested records to `Tail` and `TailTraces`
//! subscribers.
//!
//! Two rules hold here, and the second one is why this is not a single ring
//! buffer that every application shares:
//!
//! 1. **A slow consumer must never slow down ingestion.** Whatever a follower
//!    cannot keep up with is thrown away rather than pushed back at the
//!    writer. The stored copy is never affected: what a stream misses is
//!    still in SQLite and comes back through `Query` or `QueryTraces`.
//! 2. **One application's volume must never cost another application its
//!    lines.** A shared buffer that evicts its oldest slot breaks this: an
//!    application logging in a loop walks the whole window forward, and a
//!    co-tenant's single line - the one an operator was watching a follow
//!    stream for - is gone before the stream reaches it. Nothing about that
//!    loss says whose lines went or why, so the evidence is missing and the
//!    reason is missing with it.
//!
//! # How the buffer is shared out
//!
//! Each open stream gets its own [`FairBuffer`] of `capacity` records, and
//! that buffer is divided evenly between the applications that have anything
//! in it *at that moment*:
//!
//! * one application producing, and it may hold the entire buffer - an idle
//!   co-tenant reserves nothing, so nothing is wasted;
//! * `k` applications producing, and each may hold `capacity / k`. A record
//!   pushed past that share evicts the **same application's** oldest record,
//!   never a co-tenant's;
//! * an application arriving shrinks every share at once, and whoever was
//!   over the new share gives the space back out of its own oldest records.
//!   Borrowing idle space is allowed and reclaiming it costs the borrower,
//!   which is the whole trade: capacity is never idle, and the bill for using
//!   it never lands on the application that was quiet.
//!
//! The consequence that matters: how long one application's record survives
//! in the buffer no longer depends on how fast any other application is
//! producing.
//!
//! Records are handed out round-robin across the applications holding them,
//! so a chatty one cannot make a quiet one wait behind its backlog either.
//!
//! # What a loss looks like from outside
//!
//! Every eviction is charged to the application whose record it was, on three
//! surfaces: the process-wide counter (`tail_entries_dropped_total`), the
//! per-application breakdown on `/metrics`
//! (`tail_entries_dropped_by_app_total{app_id="..."}`), and a `warn` line on
//! the stream that lost them naming the applications and the numbers. An
//! operator who finds a gap can therefore find out whose lines went missing
//! and how many, which is exactly what a bare "the consumer fell behind"
//! could not tell them.
//!
//! # What "one application" means here
//!
//! `app_id`, not the (application, environment) pair the storage services
//! partition on. That is this service's one application dimension - see the
//! crate documentation - so a preview environment producing hard can still
//! push its own production environment's live lines out. Both belong to one
//! author, which is the boundary this is drawn on.
//!
//! Logs and traces share the machinery and nothing else. [`LiveHub`] is
//! generic over what it carries, and each family brings its own counters, so
//! "log entries a follower lost" and "trace records a follower lost" stay two
//! numbers an operator can tell apart.

use std::collections::{BTreeMap, VecDeque};
use std::ops::Bound;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, mpsc, watch};
use tracing::{debug, info, warn};

use crate::stats::{AppDrops, ServiceStats};
use crate::store::{LogRecord, TraceRecord};

/// How many records one stream may have queued toward its client before the
/// pump stops pulling. Back-pressure stops here: it turns into eviction from
/// that stream's own buffer, never into a stalled writer.
const SUBSCRIBER_QUEUE: usize = 256;

/// Shortest gap between two loss reports on one stream.
///
/// A flood produces a loss on nearly every batch, and a log line per batch
/// would be a second flood inside this service's own output - the place an
/// operator goes to read about the first one. The counters are updated the
/// moment a record is evicted, so `/metrics` is never behind; only the
/// sentence is held back, and it carries the total that accumulated while it
/// waited.
const LOSS_REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// How many applications one loss report names before it stops listing them.
const LOSS_REPORT_APPS: usize = 8;

/// A record that can be shared out by application.
///
/// The one thing [`FairBuffer`] needs to know about what it carries: which
/// application's share a record comes out of.
pub trait LiveRecord: Send + Sync + 'static {
    fn app_id(&self) -> &str;
}

impl LiveRecord for LogRecord {
    fn app_id(&self) -> &str {
        &self.app_id
    }
}

impl LiveRecord for TraceRecord {
    fn app_id(&self) -> &str {
        &self.app_id
    }
}

/// Which live records a subscriber wants.
///
/// Implemented per family rather than shared, because the two families are
/// filtered on different things: a log stream narrows by severity, a trace
/// stream narrows by what the platform decided about the invocation.
///
/// `Sync` because the filter is applied on the ingest path, by whichever task
/// is publishing, rather than inside the stream's own task. Applying it there
/// is what keeps a stream that follows one application from being charged for
/// another application's volume at all.
pub trait LiveFilter<T>: Send + Sync + 'static {
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
    /// The same losses as `entries_dropped`, split by the application whose
    /// records were thrown away.
    pub dropped_by_app: fn(&ServiceStats) -> &AppDrops,
}

/// Take a lock, recovering from a poisoned one.
///
/// Nothing in this module panics while holding one of these locks, so a
/// poisoned lock means an unrelated panic elsewhere in the same task. Turning
/// that into a second panic on the ingest path would fail a push whose
/// records are already committed to storage, which is a worse outcome than
/// carrying on with a buffer that is merely in some intermediate state.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Records thrown away, and who they belonged to.
#[derive(Debug, Default)]
struct Losses {
    total: u64,
    by_app: BTreeMap<String, u64>,
}

impl Losses {
    fn charge(&mut self, app_id: &str, n: u64) {
        if n == 0 {
            return;
        }
        self.total = self.total.saturating_add(n);
        match self.by_app.get_mut(app_id) {
            Some(slot) => *slot = slot.saturating_add(n),
            None => {
                self.by_app.insert(app_id.to_string(), n);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.total == 0
    }

    fn absorb(&mut self, other: Losses) {
        for (app_id, n) in other.by_app {
            self.charge(&app_id, n);
        }
    }

    /// The breakdown as one field value: `app=n app=n`, heaviest loser first.
    ///
    /// Truncated because the list is as long as the number of applications on
    /// the node, and a log line that grows without limit is one a reader
    /// stops being able to use.
    fn describe(&self) -> String {
        let mut ranked: Vec<(&String, &u64)> = self.by_app.iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let mut out = String::new();
        for (app_id, n) in ranked.iter().take(LOSS_REPORT_APPS) {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&format!("{}={n}", app_id.escape_debug()));
        }
        if ranked.len() > LOSS_REPORT_APPS {
            out.push_str(&format!(" (+{} more)", ranked.len() - LOSS_REPORT_APPS));
        }
        out
    }
}

/// A bounded live buffer that shares its capacity out between the
/// applications currently holding records in it.
///
/// One of these belongs to one open stream. Records enter it already filtered
/// for that stream, and leave it round-robin across applications.
struct FairBuffer<T> {
    capacity: usize,
    /// One queue per application that has anything buffered right now. An
    /// application with nothing in flight is not in here at all, which is
    /// what makes it cost its co-tenants nothing.
    queues: BTreeMap<String, VecDeque<Arc<T>>>,
    /// Which application was served last, so the next pop starts after it.
    served_last: Option<String>,
    len: usize,
}

impl<T> FairBuffer<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            queues: BTreeMap::new(),
            served_last: None,
            len: 0,
        }
    }

    /// What one application may hold right now.
    ///
    /// Never zero: an application is always allowed its newest record, even
    /// when there are more applications producing than the buffer has slots.
    /// The buffer then holds one per application rather than the configured
    /// capacity, which overshoots by at most the number of applications and
    /// is the right way round - the alternative is a stream that shows
    /// nothing at all once enough applications are busy.
    fn share(&self) -> usize {
        (self.capacity / self.queues.len().max(1)).max(1)
    }

    /// Buffer one record for `app_id`, evicting whatever no longer fits.
    fn push(&mut self, app_id: &str, record: Arc<T>, losses: &mut Losses) {
        let newcomer = !self.queues.contains_key(app_id);
        self.queues
            .entry(app_id.to_string())
            .or_default()
            .push_back(record);
        self.len += 1;
        if newcomer {
            // A new application shrinks every share at once, so everyone
            // over the new one gives space back - out of their own oldest
            // records. This is the moment borrowed capacity is reclaimed,
            // and it is billed to the borrower.
            self.enforce_all(losses);
        } else {
            let share = self.share();
            self.enforce_one(app_id, share, losses);
        }
    }

    fn enforce_all(&mut self, losses: &mut Losses) {
        let share = self.share();
        let mut removed = 0usize;
        for (app_id, queue) in self.queues.iter_mut() {
            let over = queue.len().saturating_sub(share);
            if over == 0 {
                continue;
            }
            queue.drain(..over);
            removed += over;
            losses.charge(app_id, over as u64);
        }
        self.len -= removed;
    }

    fn enforce_one(&mut self, app_id: &str, share: usize, losses: &mut Losses) {
        let Some(queue) = self.queues.get_mut(app_id) else {
            return;
        };
        let over = queue.len().saturating_sub(share);
        if over == 0 {
            return;
        }
        queue.drain(..over);
        self.len -= over;
        losses.charge(app_id, over as u64);
    }

    /// Take the next record, round-robin across applications.
    ///
    /// Starting after whoever was served last is what stops a chatty
    /// application's backlog from making a quiet one wait: fairness in the
    /// buffer would be worth little if the drain order undid it.
    fn pop(&mut self) -> Option<Arc<T>> {
        let key = self.next_key()?;
        let queue = self
            .queues
            .get_mut(&key)
            .expect("the key was just read out of this map");
        let record = queue
            .pop_front()
            .expect("a queue is removed by the pop that empties it, so none is ever empty here");
        if queue.is_empty() {
            self.queues.remove(&key);
        }
        self.len -= 1;
        self.served_last = Some(key);
        Some(record)
    }

    fn next_key(&self) -> Option<String> {
        let after = match &self.served_last {
            Some(last) => self
                .queues
                .range::<str, _>((Bound::Excluded(last.as_str()), Bound::Unbounded))
                .next()
                .map(|(key, _)| key.clone()),
            None => None,
        };
        // Wrapping round to the first application is the other half of the
        // round-robin, and also the path taken when the one served last has
        // emptied and left the map.
        after.or_else(|| self.queues.keys().next().cloned())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.len
    }
}

/// One open stream, from the ingest path's point of view.
struct Subscriber<T> {
    /// Identifies the stream in log lines.
    label: String,
    filter: Box<dyn LiveFilter<T>>,
    buffer: Mutex<FairBuffer<T>>,
    /// Losses this stream has counted but not yet said out loud.
    unreported: Mutex<Losses>,
    /// Raised when something lands in `buffer`.
    ready: Notify,
}

/// Every open stream of one family. Shared between the hub and each stream's
/// own task, which is what lets a stream take itself out of the list when it
/// ends without needing a handle on the hub.
type Roster<T> = Arc<Mutex<Vec<Arc<Subscriber<T>>>>>;

/// Fan-out point between the ingest path and every open stream of one family.
pub struct LiveHub<T> {
    roster: Roster<T>,
    /// Records one stream may buffer, before it is shared out between the
    /// applications producing them.
    capacity: usize,
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
    /// `capacity` is how many entries one open stream may hold. Bigger
    /// buffers tolerate slower consumers at the cost of memory, and the cost
    /// is per stream rather than per process: two followers watching the same
    /// entries share them and the second costs a pointer each, while two
    /// watching different applications hold different entries and the second
    /// costs a buffer's worth. Followers are operators, so there are few of
    /// them; the count is on `/metrics` as `tail_streams_open`.
    pub fn new(capacity: usize, stats: Arc<ServiceStats>) -> Self {
        Self::with_meters(
            capacity,
            stats,
            "log entry",
            HubMeters {
                streams_started: |s| &s.tail_streams_started,
                streams_open: |s| &s.tail_streams_open,
                entries_dropped: |s| &s.tail_entries_dropped,
                dropped_by_app: |s| &s.tail_entries_dropped_by_app,
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
                dropped_by_app: |s| &s.trace_tail_entries_dropped_by_app,
            },
        )
    }
}

impl<T: LiveRecord> LiveHub<T> {
    fn with_meters(
        capacity: usize,
        stats: Arc<ServiceStats>,
        family: &'static str,
        meters: HubMeters,
    ) -> Self {
        let (stop, _) = watch::channel(false);
        Self {
            roster: Arc::new(Mutex::new(Vec::new())),
            capacity: capacity.max(1),
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
    /// Never blocks and never fails. With no subscribers the records are
    /// discarded without being copied anywhere - a follow stream carries what
    /// arrives while it is open, so there is nothing to keep for a follower
    /// that is not there yet.
    pub fn publish(&self, records: &[Arc<T>]) {
        if records.is_empty() {
            return;
        }
        // Cloned out from under the lock so a stream ending mid-publish
        // cannot deadlock against a pump taking itself off the roster.
        let subscribers: Vec<Arc<Subscriber<T>>> = lock(&self.roster).clone();
        if subscribers.is_empty() {
            return;
        }
        for subscriber in &subscribers {
            let mut losses = Losses::default();
            let mut queued = false;
            {
                let mut buffer = lock(&subscriber.buffer);
                for record in records {
                    // Filtering here, rather than in the stream's own task,
                    // is what makes a stream following one application immune
                    // to every other application's volume: the records it
                    // does not want never take up any of its buffer.
                    if !subscriber.filter.matches(record) {
                        continue;
                    }
                    buffer.push(record.app_id(), Arc::clone(record), &mut losses);
                    queued = true;
                }
            }
            if !losses.is_empty() {
                // Counted the moment it happens, said out loud by the stream
                // itself: a scrape is never behind, even while the sentence
                // is being held back.
                ServiceStats::add((self.meters.entries_dropped)(&self.stats), losses.total);
                let by_app = (self.meters.dropped_by_app)(&self.stats);
                for (app_id, n) in &losses.by_app {
                    by_app.add(app_id, *n);
                }
                lock(&subscriber.unreported).absorb(losses);
            }
            if queued {
                subscriber.ready.notify_one();
            }
        }
    }

    /// How many streams are currently attached.
    pub fn subscriber_count(&self) -> usize {
        lock(&self.roster).len()
    }

    /// Open a stream. `label` identifies the subscriber in log lines.
    pub fn subscribe<F: LiveFilter<T>>(&self, label: String, filter: F) -> mpsc::Receiver<Arc<T>> {
        let (tx, rx) = mpsc::channel(SUBSCRIBER_QUEUE);
        let subscriber = Arc::new(Subscriber {
            label,
            filter: Box::new(filter),
            buffer: Mutex::new(FairBuffer::new(self.capacity)),
            unreported: Mutex::new(Losses::default()),
            ready: Notify::new(),
        });
        // On the roster before the task exists, so a batch published in the
        // same moment cannot slip past a stream that has been promised.
        lock(&self.roster).push(Arc::clone(&subscriber));
        tokio::spawn(pump(
            subscriber,
            Arc::clone(&self.roster),
            tx,
            self.stop.subscribe(),
            Arc::clone(&self.stats),
            self.meters,
            self.family,
        ));
        rx
    }
}

fn unregister<T>(roster: &Roster<T>, subscriber: &Arc<Subscriber<T>>) {
    lock(roster).retain(|open| !Arc::ptr_eq(open, subscriber));
}

/// Say what this stream lost, if anything, and if it is time to say it again.
///
/// Returns the running total so the closing line can carry it.
fn report_losses<T>(
    subscriber: &Subscriber<T>,
    family: &'static str,
    dropped_total: &mut u64,
    last_report: &mut Option<Instant>,
    force: bool,
) {
    if !force && last_report.is_some_and(|reported| reported.elapsed() < LOSS_REPORT_INTERVAL) {
        return;
    }
    let losses = std::mem::take(&mut *lock(&subscriber.unreported));
    if losses.is_empty() {
        return;
    }
    *dropped_total = dropped_total.saturating_add(losses.total);
    *last_report = Some(Instant::now());
    warn!(
        subscriber = %subscriber.label,
        family,
        dropped = losses.total,
        dropped_total = *dropped_total,
        by_app = %losses.describe(),
        "records were thrown away before this stream read them; each application holds its \
         own share of this stream's buffer, so every line counted here was pushed out by the \
         named application's own volume and no other application lost anything for it. The \
         records are still in storage and come back from a query"
    );
}

/// Move records from one stream's buffer into that stream's queue toward its
/// client, and account for what did not fit.
async fn pump<T: LiveRecord>(
    subscriber: Arc<Subscriber<T>>,
    roster: Roster<T>,
    out: mpsc::Sender<Arc<T>>,
    mut stop: watch::Receiver<bool>,
    stats: Arc<ServiceStats>,
    meters: HubMeters,
    family: &'static str,
) {
    if *stop.borrow() {
        debug!(
            subscriber = %subscriber.label,
            family,
            "follow stream requested while the service is shutting down, closing it right away"
        );
        unregister(&roster, &subscriber);
        return;
    }
    ServiceStats::incr((meters.streams_started)(&stats));
    ServiceStats::incr((meters.streams_open)(&stats));
    info!(
        subscriber = %subscriber.label,
        family,
        filter = %subscriber.filter.describe(),
        "follow stream opened"
    );

    let mut dropped_total: u64 = 0;
    let mut forwarded: u64 = 0;
    let mut last_report: Option<Instant> = None;
    'stream: loop {
        // Before the next records, not after: a reader should meet the notice
        // of a gap where the gap is, rather than further down the stream.
        report_losses(
            &subscriber,
            family,
            &mut dropped_total,
            &mut last_report,
            false,
        );

        loop {
            let next = lock(&subscriber.buffer).pop();
            let Some(record) = next else {
                break;
            };
            // The send waits when this client's queue is full, so it has to
            // watch for shutdown as well: a client that stopped reading
            // altogether must not be able to hold the process open by leaving
            // the pump parked here.
            let sent = tokio::select! {
                _ = stop.changed() => {
                    debug!(
                        subscriber = %subscriber.label,
                        family,
                        "service is shutting down while this stream was backed up, closing it"
                    );
                    break 'stream;
                }
                sent = out.send(record) => sent,
            };
            if sent.is_err() {
                debug!(
                    subscriber = %subscriber.label,
                    family,
                    "follow client disconnected, closing its stream"
                );
                break 'stream;
            }
            forwarded += 1;
        }

        tokio::select! {
            _ = stop.changed() => {
                debug!(subscriber = %subscriber.label, family, "service is shutting down, closing this stream");
                break 'stream;
            }
            // A notification raised while the drain above was running is kept
            // by `Notify`, so a record that arrived just as the buffer went
            // empty wakes this immediately rather than waiting for the next.
            _ = subscriber.ready.notified() => {}
        }
    }

    unregister(&roster, &subscriber);
    // Whatever is still unreported goes out now, whether or not the interval
    // has passed: a stream that ends must not take the count of what it lost
    // with it.
    report_losses(
        &subscriber,
        family,
        &mut dropped_total,
        &mut last_report,
        true,
    );
    ServiceStats::decr((meters.streams_open)(&stats));
    info!(
        subscriber = %subscriber.label,
        family,
        forwarded,
        dropped_total,
        "follow stream closed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A run of records from one noisy application, big enough to walk any
    /// buffer in these tests all the way round.
    fn flood(app_id: &str, count: usize) -> Vec<Arc<LogRecord>> {
        (0..count)
            .map(|i| record(app_id, 3, &format!("flood {i}")))
            .collect()
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

    // ---- how the buffer is shared out ----------------------------------

    #[test]
    fn one_application_on_its_own_may_hold_the_whole_buffer() {
        // The answer to "what does a heavy application get while everyone
        // else is idle": all of it. Reserving a share for applications that
        // are producing nothing would throw away lines to protect nobody.
        let mut buffer = FairBuffer::new(8);
        let mut losses = Losses::default();
        for i in 0..8 {
            buffer.push(
                "noisy",
                record("noisy", 3, &format!("entry {i}")),
                &mut losses,
            );
        }
        assert!(losses.is_empty(), "nothing should have been thrown away");
        assert_eq!(buffer.len(), 8);

        buffer.push("noisy", record("noisy", 3, "entry 8"), &mut losses);
        assert_eq!(losses.total, 1, "past the buffer it loses its own oldest");
        assert_eq!(losses.by_app.get("noisy"), Some(&1));
        assert_eq!(buffer.len(), 8);
    }

    #[test]
    fn borrowed_space_is_reclaimed_from_the_borrower_not_from_the_newcomer() {
        // The moment a quiet application starts producing, the heavy one's
        // share halves - and the space comes out of the heavy one's own
        // oldest records. The newcomer's line is what this exists to save,
        // so it must not be what pays for the reclaim.
        let mut buffer = FairBuffer::new(8);
        let mut warmup = Losses::default();
        for i in 0..8 {
            buffer.push(
                "noisy",
                record("noisy", 3, &format!("entry {i}")),
                &mut warmup,
            );
        }
        assert!(warmup.is_empty());

        let mut losses = Losses::default();
        buffer.push(
            "quiet",
            record("quiet", 3, "the line that mattered"),
            &mut losses,
        );
        assert_eq!(
            losses.by_app.get("noisy"),
            Some(&4),
            "noisy gives back half"
        );
        assert_eq!(
            losses.by_app.get("quiet"),
            None,
            "the newcomer pays nothing"
        );
        assert_eq!(buffer.len(), 5);
    }

    #[test]
    fn the_drain_alternates_so_a_backlog_cannot_make_a_quiet_application_wait() {
        // Fairness in the buffer would buy little if the drain order handed
        // the whole backlog over first: the quiet application's line would
        // still arrive last.
        let mut buffer = FairBuffer::new(100);
        let mut losses = Losses::default();
        for i in 0..3 {
            buffer.push(
                "noisy",
                record("noisy", 3, &format!("flood {i}")),
                &mut losses,
            );
        }
        buffer.push(
            "quiet",
            record("quiet", 3, "the line that mattered"),
            &mut losses,
        );
        assert!(losses.is_empty());

        let drained: Vec<String> = std::iter::from_fn(|| buffer.pop())
            .map(|r| r.message.clone())
            .collect();
        // One of the flood, then the quiet line, then the rest of the
        // backlog: the quiet application waits behind one record rather than
        // behind everything the noisy one had queued.
        assert_eq!(
            drained,
            vec![
                "flood 0".to_string(),
                "the line that mattered".to_string(),
                "flood 1".to_string(),
                "flood 2".to_string(),
            ]
        );
    }

    #[test]
    fn more_applications_than_slots_still_leaves_each_one_its_newest_record() {
        // A share of zero would mean a stream that shows nothing at all once
        // enough applications are busy, which is worse than a buffer that
        // holds one record per application and overshoots its capacity by the
        // number of applications.
        let mut buffer = FairBuffer::new(4);
        let mut losses = Losses::default();
        for i in 0..8 {
            let app = format!("app-{i}");
            buffer.push(&app, record(&app, 3, "only line"), &mut losses);
        }
        assert!(losses.is_empty());
        assert_eq!(buffer.len(), 8);
    }

    // ---- what the streams see ------------------------------------------

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

        hub.publish(&[
            record("blog", 3, "ignored"),
            record("shop", 3, "kept"),
            record("shop", 5, "kept too"),
        ]);

        assert_eq!(rx.recv().await.expect("entry").message, "kept");
        assert_eq!(rx.recv().await.expect("entry").message, "kept too");
    }

    #[tokio::test]
    async fn a_flood_from_one_application_does_not_delete_a_co_tenants_lines() {
        // The failure this module was rebuilt for. A stream following one
        // application used to lose that application's lines to a co-tenant's
        // volume, because both went through one shared window: the co-tenant
        // walked the window forward and the line the operator was waiting for
        // was gone before the stream reached it.
        let stats = Arc::new(ServiceStats::new());
        let hub = TailHub::new(8, Arc::clone(&stats));
        let mut rx = hub.subscribe(
            "quiet-follower".into(),
            TailFilter {
                app_id: Some("quiet".into()),
                min_level: None,
            },
        );

        hub.publish(&[record("quiet", 3, "the line that mattered")]);
        hub.publish(&flood("noisy", 1000));
        hub.publish(&[record("quiet", 3, "the line after")]);

        assert_eq!(
            rx.recv()
                .await
                .expect("the follower must receive something")
                .message,
            "the line that mattered"
        );
        assert_eq!(
            ServiceStats::get(&stats.tail_entries_dropped),
            0,
            "a stream following one application must not be charged for another's volume"
        );
    }

    #[tokio::test]
    async fn a_flood_spares_a_co_tenant_even_on_a_stream_following_everything() {
        // The same protection where no filter can provide it: this stream
        // wants both applications, so only the per-application share keeps
        // the quiet one's single line alive through a thousand of the other's.
        let stats = Arc::new(ServiceStats::new());
        let hub = TailHub::new(8, Arc::clone(&stats));
        let mut rx = hub.subscribe("everything".into(), TailFilter::default());

        let mut batch = vec![record("quiet", 3, "the line that mattered")];
        batch.extend(flood("noisy", 1000));
        hub.publish(&batch);

        // Capacity 8 shared between two applications: four of the flood's
        // newest, and the quiet application's only line.
        let mut seen = Vec::new();
        for _ in 0..5 {
            seen.push(rx.recv().await.expect("record").message.clone());
        }
        assert!(
            seen.contains(&"the line that mattered".to_string()),
            "the quiet application's line was evicted by the flood: {seen:?}"
        );
        assert_eq!(seen.iter().filter(|m| m.starts_with("flood ")).count(), 4);
    }

    #[tokio::test]
    async fn a_loss_is_charged_to_the_application_that_caused_it() {
        // What an operator reads afterwards. The total alone would say
        // something went missing; only the breakdown says whose volume did
        // it, which is the difference between a usable report and a shrug.
        let stats = Arc::new(ServiceStats::new());
        let hub = TailHub::new(8, Arc::clone(&stats));
        let _rx = hub.subscribe("everything".into(), TailFilter::default());

        let mut batch = vec![record("quiet", 3, "the line that mattered")];
        batch.extend(flood("noisy", 1000));
        hub.publish(&batch);

        assert_eq!(ServiceStats::get(&stats.tail_entries_dropped), 996);
        assert_eq!(stats.tail_entries_dropped_by_app.get("noisy"), 996);
        assert_eq!(
            stats.tail_entries_dropped_by_app.get("quiet"),
            0,
            "the quiet application lost nothing and must not be named as if it had"
        );
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

        let mut cut_off = trace("shop", "production", 1, 3);
        Arc::get_mut(&mut cut_off).expect("sole owner").target = "https://shop.test/slow".into();
        hub.publish(&[trace("shop", "production", 1, 1), cut_off]);

        assert_eq!(
            rx.recv().await.expect("record").target,
            "https://shop.test/slow"
        );
    }

    #[tokio::test]
    async fn traces_are_shared_out_between_applications_the_same_way() {
        // Traces arrive at one record per invocation and go through the same
        // hub, so an application under load pushes out records far faster
        // than a log flood does. The share is what keeps a co-tenant's
        // failing invocation visible while that happens.
        let stats = Arc::new(ServiceStats::new());
        let hub = TraceHub::new(8, Arc::clone(&stats));
        let mut rx = hub.subscribe("everything".into(), TraceTailFilter::default());

        let mut batch = vec![trace("quiet", "production", 1, 3)];
        batch.extend((0..1000).map(|_| trace("noisy", "production", 1, 1)));
        hub.publish(&batch);

        let mut apps = Vec::new();
        for _ in 0..5 {
            apps.push(rx.recv().await.expect("record").app_id.clone());
        }
        assert!(
            apps.contains(&"quiet".to_string()),
            "the quiet application's invocation was evicted by the flood: {apps:?}"
        );
        assert_eq!(stats.trace_tail_entries_dropped_by_app.get("noisy"), 996);
        assert_eq!(
            ServiceStats::get(&stats.tail_entries_dropped),
            0,
            "a trace follower's loss must never be reported as lost log entries"
        );
    }

    #[tokio::test]
    async fn a_slow_subscriber_loses_its_own_oldest_and_the_writer_is_untouched() {
        let stats = Arc::new(ServiceStats::new());
        let hub = TailHub::new(4, Arc::clone(&stats));
        let mut rx = hub.subscribe("slow".into(), TailFilter::default());

        // One publish call, so what the buffer keeps does not depend on when
        // the stream's own task happens to run.
        let batch: Vec<Arc<LogRecord>> = (0..10)
            .map(|i| record("shop", 3, &format!("entry {i}")))
            .collect();
        hub.publish(&batch);

        // Only the last four survived; the other six were thrown away while
        // the writer carried on undisturbed.
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(rx.recv().await.expect("entry").message.clone());
        }
        assert_eq!(seen, vec!["entry 6", "entry 7", "entry 8", "entry 9"]);
        assert_eq!(ServiceStats::get(&stats.tail_entries_dropped), 6);
        assert_eq!(stats.tail_entries_dropped_by_app.get("shop"), 6);
    }

    #[tokio::test]
    async fn a_slow_trace_follower_does_not_spend_the_log_drop_counter() {
        // Two families, two numbers: an operator asking "did I lose logs?"
        // must not be answered with a trace follower's arithmetic.
        let stats = Arc::new(ServiceStats::new());
        let hub = TraceHub::new(4, Arc::clone(&stats));
        let _rx = hub.subscribe("slow".into(), TraceTailFilter::default());

        let batch: Vec<Arc<TraceRecord>> =
            (0..10).map(|_| trace("shop", "production", 1, 1)).collect();
        hub.publish(&batch);

        assert_eq!(ServiceStats::get(&stats.trace_tail_entries_dropped), 6);
        assert_eq!(
            ServiceStats::get(&stats.tail_entries_dropped),
            0,
            "a trace follower's loss must never be reported as lost log entries"
        );
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
        // Wait for the stream's own task to start, not just for it to be on
        // the roster.
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
        while hub.subscriber_count() != 0 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn a_finished_stream_leaves_the_roster() {
        // A stream that stays on the roster after its client is gone would
        // make every later publish copy records into a buffer nobody drains.
        let stats = Arc::new(ServiceStats::new());
        let hub = TailHub::new(64, Arc::clone(&stats));
        let rx = hub.subscribe("brief".into(), TailFilter::default());
        assert_eq!(hub.subscriber_count(), 1);
        drop(rx);
        // The stream notices its client has gone on its next send.
        hub.publish(&[record("shop", 3, "nobody is listening")]);
        while hub.subscriber_count() != 0 {
            tokio::task::yield_now().await;
        }
    }
}
