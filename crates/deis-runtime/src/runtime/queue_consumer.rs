//! Queue consumption: one polling task per queue an application declares.
//!
//! # Why the runtime pulls
//!
//! deis-queue never pushes. Pushing would require the queue to know which
//! node runs which application, and the queue deliberately knows nothing
//! except the tenant -- one application in one environment
//! (`docs/architecture.zh.md` section 6). So each node polls for the
//! applications it runs, and the queue's lease is what stops two nodes from
//! handling the same message: a leased message is invisible to everybody else
//! until the lease lapses.
//!
//! # The loop
//!
//! Pull a batch, hand it to the app's queue handler, acknowledge what
//! succeeded and hand back what did not. An empty queue is the normal case,
//! not a failure, so the poller backs off instead of spinning: the wait
//! doubles from [`ConsumerSettings::idle_backoff_min`] up to
//! [`ConsumerSettings::idle_backoff_max`] while there is nothing to do, and
//! snaps back to the minimum the moment a pull returns work.
//!
//! # What `max_concurrency` means here, and what it does not
//!
//! It bounds how many batches this poller may have **outstanding** -- leased
//! from deis-queue and not yet settled -- and nothing else. It is not
//! parallelism, and on this platform it cannot be: one `(application,
//! environment)` is one isolate on one runtime thread on each node, so two
//! batches of the same application never execute at the same time. The
//! handler is serialised by the isolate no matter what this number says.
//!
//! What raising it does buy is real but narrow. A batch's life is pull ->
//! wait for the runtime thread -> run -> ack or nack, and only the third of
//! those is the isolate. With one batch outstanding the pull for the next one
//! does not even start until the previous batch has been acknowledged, so the
//! queue's round trips and the wait for the thread are dead time on a busy
//! queue. With two, the next batch is already leased and queued behind the
//! current one when the handler finishes.
//!
//! And it has a cost that grows with it: every outstanding batch has had its
//! lease running since it was pulled, and a batch waiting its turn on the
//! runtime thread spends that lease doing nothing. Past a point the lease
//! lapses while the batch is still queued, deis-queue hands the same messages
//! to somebody else, and the application processes them twice. That is why
//! the default is one, why [`MAX_OUTSTANDING_BATCHES`] is small, and why
//! [`spawn_with_registry`] says out loud what a value above one actually
//! bought.
//!
//! # Delivery guarantees the app has to live with
//!
//! At-least-once. A node that dies between handling a message and
//! acknowledging it will see that message again once the lease lapses, so an
//! application's queue handler must be idempotent. A handler that runs longer
//! than the lease has the same effect, which is why the lease is derived from
//! the app's execution timeout rather than fixed, and why a batch is given up
//! on before its lease lapses rather than acknowledged afterwards -- see
//! [`handle_batch`].

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::BoxFuture;
use tokio::sync::{oneshot, Semaphore};
use tokio::task::JoinSet;

use super::metrics::{recorder, MetricsRegistry};
use super::pool::DispatchError;
use super::queue_client::{AckOutcome, LeasedMessage, MessageHandle, NackOutcome, QueueClient};

/// What the app's queue handler did with a batch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchOutcome {
    /// Ids the handler did not complete. Everything else in the batch counts
    /// as handled and is acknowledged.
    pub failed: Vec<String>,
}

impl BatchOutcome {
    /// Every message in the batch failed -- what an invocation that threw, or
    /// never ran, amounts to.
    pub fn all_failed(messages: &[LeasedMessage]) -> Self {
        Self {
            failed: messages.iter().map(|m| m.id.clone()).collect(),
        }
    }
}

/// How a batch is handed to the application.
///
/// A function rather than a direct call into the pool so that the loop can be
/// tested without a V8 isolate: the pool supplies a closure that posts the
/// batch to the thread the app's isolate lives on.
pub type BatchInvoker = Arc<
    dyn Fn(String, Vec<LeasedMessage>) -> BoxFuture<'static, anyhow::Result<BatchOutcome>>
        + Send
        + Sync,
>;

/// The queue operations a poller needs.
///
/// [`QueueClient`] is the only real implementation; the indirection exists so
/// the loop -- which is where the interesting decisions live: what gets
/// acknowledged, what gets handed back, when to back off -- can be tested
/// without a running deis-queue.
pub trait QueuePort: Send + Sync + 'static {
    fn pull_messages<'a>(
        &'a self,
        queue: &'a str,
        max_messages: u32,
        lease_seconds: u32,
    ) -> BoxFuture<'a, anyhow::Result<Vec<LeasedMessage>>>;

    /// Handles, not ids: answering for a message means proving the lease is
    /// still this consumer's. See [`LeasedMessage::handle`].
    fn ack_messages<'a>(
        &'a self,
        queue: &'a str,
        handles: Vec<MessageHandle>,
    ) -> BoxFuture<'a, anyhow::Result<AckOutcome>>;

    /// `refund_attempt` gives the delivery attempts the server counted at the
    /// pull back again; see [`QueueClient::nack`] for why only a refusal that
    /// never reached the application may do that.
    fn nack_messages<'a>(
        &'a self,
        queue: &'a str,
        handles: Vec<MessageHandle>,
        delay_seconds: u32,
        refund_attempt: bool,
    ) -> BoxFuture<'a, anyhow::Result<NackOutcome>>;
}

impl QueuePort for QueueClient {
    fn pull_messages<'a>(
        &'a self,
        queue: &'a str,
        max_messages: u32,
        lease_seconds: u32,
    ) -> BoxFuture<'a, anyhow::Result<Vec<LeasedMessage>>> {
        Box::pin(self.pull(queue, max_messages, lease_seconds))
    }

    fn ack_messages<'a>(
        &'a self,
        queue: &'a str,
        handles: Vec<MessageHandle>,
    ) -> BoxFuture<'a, anyhow::Result<AckOutcome>> {
        Box::pin(self.ack(queue, handles))
    }

    fn nack_messages<'a>(
        &'a self,
        queue: &'a str,
        handles: Vec<MessageHandle>,
        delay_seconds: u32,
        refund_attempt: bool,
    ) -> BoxFuture<'a, anyhow::Result<NackOutcome>> {
        Box::pin(self.nack(
            queue,
            handles,
            delay_seconds,
            refund_attempt,
        ))
    }
}

/// Timing knobs for one poller.
#[derive(Debug, Clone)]
pub struct ConsumerSettings {
    /// Messages to lease per pull.
    pub batch_size: u32,
    /// How long the leases last. Must exceed the handler's worst-case run
    /// time or the queue will hand the same messages to somebody else while
    /// they are still being processed.
    pub lease_seconds: u32,
    /// Wait after the first empty pull.
    pub idle_backoff_min: Duration,
    /// Longest wait between polls of an idle queue.
    pub idle_backoff_max: Duration,
    /// How long a failed message waits before it is handed out again.
    pub retry_delay_seconds: u32,
    /// Batches this poller may hold at once: leased, waiting for the runtime
    /// thread, running, or being settled.
    ///
    /// Never parallelism -- see the module docs. One is the default and means
    /// exactly what this loop did before the setting existed: nothing is
    /// pulled until the previous batch has been acknowledged.
    pub max_concurrency: u32,
}

/// Messages leased per pull when the manifest does not say.
pub const DEFAULT_BATCH_SIZE: u32 = 10;

/// Batches outstanding at once when the manifest does not say.
///
/// One, so an application that declares nothing behaves exactly as this
/// consumer did before the setting existed. Anything higher is a trade an
/// application has to make deliberately, because what it spends is lease time
/// on messages it is not working on yet.
pub const DEFAULT_MAX_CONCURRENCY: u32 = 1;

/// The most batches this node will hold for one queue, whatever a manifest
/// asks for.
///
/// Small on purpose, and not because of memory. Each outstanding batch is
/// `batch_size` messages whose lease is already running while they wait for a
/// runtime thread they share with every other application on it, so the
/// number of messages at risk of a lapsed lease is this times the batch size:
/// eight batches of ten is eighty messages that may be redelivered if the
/// thread is busy. Beyond a small number the extra batches are not overlap,
/// they are queue depth with an expiry on it.
pub const MAX_OUTSTANDING_BATCHES: u32 = 8;

/// Slack added to the app's execution timeout when deriving the lease.
///
/// It covers the pull and the acknowledgement, and *some* of the wait before
/// the isolate thread gets to the batch. That wait has no ceiling of its own:
/// several applications share one runtime thread, so a batch can sit in that
/// thread's channel behind other applications' requests for as long as they
/// take. The slack is therefore a guess, and the thing that actually keeps the
/// lease honest is the deadline in [`handle_batch`], which gives up on a batch
/// while the lease is still valid instead of acknowledging it afterwards.
const LEASE_SLACK: Duration = Duration::from_secs(30);

/// The lease this consumer will not ask beyond.
///
/// It is deis-queue's *default* ceiling, not a protocol constant:
/// `max_lease_secs` is configurable there, and a request above whatever it is
/// set to is silently clamped rather than refused (deis-queue's
/// `clamp_lease`). So this value only keeps the request sane; what was
/// actually granted is read back off the messages, see [`granted_lease`].
const MAX_LEASE_SECONDS: u32 = 3600;

/// How much of a lease is kept back for settling the batch: reporting the
/// outcome and the ack or nack round trip.
///
/// A batch handed back at the instant the lease lapses is a batch deis-queue
/// has already given to somebody else.
const SETTLE_RESERVE: Duration = Duration::from_secs(5);

impl ConsumerSettings {
    /// Settings for an app whose isolate is bounded by `execution_timeout`.
    pub fn for_app(
        batch_size: Option<u32>,
        max_concurrency: Option<u32>,
        execution_timeout: Duration,
    ) -> Self {
        let lease = execution_timeout + LEASE_SLACK;
        Self {
            batch_size: batch_size
                .filter(|size| *size > 0)
                .unwrap_or(DEFAULT_BATCH_SIZE),
            lease_seconds: (lease.as_secs() as u32).clamp(1, MAX_LEASE_SECONDS),
            idle_backoff_min: Duration::from_secs(1),
            idle_backoff_max: Duration::from_secs(30),
            retry_delay_seconds: 10,
            max_concurrency: max_concurrency
                .filter(|outstanding| *outstanding > 0)
                .unwrap_or(DEFAULT_MAX_CONCURRENCY),
        }
    }
}

impl Default for ConsumerSettings {
    fn default() -> Self {
        Self::for_app(None, None, Duration::from_secs(30))
    }
}

/// A running poller. Dropping the handle does not stop the task; call
/// [`ConsumerHandle::stop`] so the current batch is finished and
/// acknowledged first.
pub struct ConsumerHandle {
    app_id: String,
    queue: String,
    stop: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl ConsumerHandle {
    /// Ask the poller to finish the batch it is on and exit.
    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if self.join.await.is_err() {
            tracing::warn!(
                app_id = %self.app_id,
                queue = %self.queue,
                "Queue consumer for app {} queue {} did not shut down cleanly; any messages it \
                 held stay leased until deis-queue expires them and hands them out again",
                self.app_id,
                self.queue
            );
        }
    }

    pub fn queue(&self) -> &str {
        &self.queue
    }
}

/// Start polling one queue for one application.
pub fn spawn(
    app_id: String,
    queue: String,
    client: Arc<dyn QueuePort>,
    settings: ConsumerSettings,
    invoke: BatchInvoker,
) -> ConsumerHandle {
    spawn_with_registry(app_id, queue, client, settings, invoke, recorder().clone())
}

/// Same as [`spawn`], with the metrics registry chosen by the caller so tests
/// can observe their own instead of the process-wide one.
pub fn spawn_with_registry(
    app_id: String,
    queue: String,
    client: Arc<dyn QueuePort>,
    settings: ConsumerSettings,
    invoke: BatchInvoker,
    metrics: Arc<MetricsRegistry>,
) -> ConsumerHandle {
    let (stop_tx, stop_rx) = oneshot::channel();
    let task_app_id = app_id.clone();
    let task_queue = queue.clone();

    // Clamped here rather than in `for_app` so that the number an application
    // asked for and the number in force appear in the same line: `for_app` is
    // also what every test and default goes through, and it does not know
    // which application it is building settings for.
    let mut settings = settings;
    if settings.max_concurrency > MAX_OUTSTANDING_BATCHES {
        tracing::warn!(
            app_id = %app_id,
            queue = %queue,
            requested = settings.max_concurrency,
            applied = MAX_OUTSTANDING_BATCHES,
            batch_size = settings.batch_size,
            "App {} asks to hold {} batches of queue {} at once; this node allows {}. Every \
             outstanding batch has its lease running while it waits for a runtime thread, so \
             {} batches of {} messages is already {} messages that can be redelivered if the \
             thread is busy",
            app_id,
            settings.max_concurrency,
            queue,
            MAX_OUTSTANDING_BATCHES,
            MAX_OUTSTANDING_BATCHES,
            settings.batch_size,
            MAX_OUTSTANDING_BATCHES * settings.batch_size
        );
        settings.max_concurrency = MAX_OUTSTANDING_BATCHES;
    }

    // The concurrency half of this line says what the number means, because
    // the name it arrives under -- `maxConcurrency` in a project file -- reads
    // as parallelism on every other platform and is not parallelism here. An
    // operator reading the log has to be able to see that without opening the
    // source.
    tracing::info!(
        app_id = %app_id,
        queue = %queue,
        batch_size = settings.batch_size,
        lease_seconds = settings.lease_seconds,
        max_concurrency = settings.max_concurrency,
        "Consuming queue {} for app {}: up to {} messages per pull, {}s leases, at most {} \
         batch(es) outstanding at once. Outstanding, not parallel: this app is one isolate on \
         one thread, so its handler runs one batch at a time however many are held",
        queue,
        app_id,
        settings.batch_size,
        settings.lease_seconds,
        settings.max_concurrency
    );

    let join = tokio::spawn(async move {
        run(
            task_app_id,
            task_queue,
            client,
            settings,
            invoke,
            metrics,
            stop_rx,
        )
        .await;
    });

    ConsumerHandle {
        app_id,
        queue,
        stop: Some(stop_tx),
        join,
    }
}

/// The polling loop.
///
/// One permit per batch this poller is allowed to hold, taken *before* the
/// pull and given back when the batch has been settled. Taking it first is
/// the point: a lease starts running the moment deis-queue hands the messages
/// over, so pulling a batch this poller has nowhere to put would spend that
/// lease waiting for a permit.
///
/// At `max_concurrency = 1` -- the default, and what every application that
/// declares nothing gets -- this is exactly the sequential loop it replaced:
/// the single permit is only free again once the previous batch has been
/// acknowledged or handed back.
async fn run(
    app_id: String,
    queue: String,
    client: Arc<dyn QueuePort>,
    settings: ConsumerSettings,
    invoke: BatchInvoker,
    metrics: Arc<MetricsRegistry>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut backoff = settings.idle_backoff_min;
    // The shortfall is reported once per poller: it is a configuration fact,
    // not an event, and repeating it every poll would bury the batches that
    // actually failed.
    let mut lease_shortfall_reported = false;

    let permits = Arc::new(Semaphore::new(settings.max_concurrency as usize));
    // Batches handed off and not yet settled. Drained before this task
    // returns, so `ConsumerHandle::stop` still means "every batch this poller
    // holds is finished and answered for" rather than only the last one.
    let mut outstanding: JoinSet<()> = JoinSet::new();

    loop {
        // Reaped without waiting, so a finished batch stops being tracked at
        // the top of the next round. Nothing is reported here: the task
        // itself has already logged whatever happened to its batch.
        while outstanding.try_join_next().is_some() {}

        // Checked before the pull as well as during the wait, so a stop that
        // arrives while a batch is being handled takes effect at the top of
        // the next round instead of starting one more.
        match stop.try_recv() {
            // A dropped sender means the handle went away without a stop;
            // there is nobody left to consume for, so exit rather than poll
            // forever on behalf of an app that is gone.
            Ok(()) | Err(oneshot::error::TryRecvError::Closed) => break,
            Err(oneshot::error::TryRecvError::Empty) => {}
        }

        // Waiting for a permit is also a place a stop has to be able to
        // arrive: with every batch outstanding and a slow runtime thread,
        // this is where the poller spends its time.
        let permit = tokio::select! {
            permit = permits.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                // The semaphore is never closed while this loop owns it, so
                // this is unreachable; treated as a stop rather than an
                // unwrap, because polling on with no bound on outstanding
                // batches is the one behaviour that must not happen.
                Err(_) => break,
            },
            _ = &mut stop => break,
        };

        let messages = match client
            .pull_messages(&queue, settings.batch_size, settings.lease_seconds)
            .await
        {
            Ok(messages) => messages,
            Err(e) => {
                metrics.record_queue_poll_error(&app_id, &queue);
                tracing::warn!(
                    app_id = %app_id,
                    queue = %queue,
                    error = %e,
                    backoff_secs = backoff.as_secs(),
                    "Polling queue {} for app {} failed; nothing is being consumed and messages \
                     accumulate in deis-queue. Retrying in {}s",
                    queue,
                    app_id,
                    backoff.as_secs()
                );
                drop(permit);
                if wait(&mut stop, backoff).await == Wake::Stopped {
                    break;
                }
                backoff = next_backoff(backoff, settings.idle_backoff_max);
                continue;
            }
        };

        if messages.is_empty() {
            drop(permit);
            if wait(&mut stop, backoff).await == Wake::Stopped {
                break;
            }
            backoff = next_backoff(backoff, settings.idle_backoff_max);
            continue;
        }

        backoff = settings.idle_backoff_min;
        if !lease_shortfall_reported {
            lease_shortfall_reported =
                report_lease_shortfall(&app_id, &queue, &settings, &messages);
        }

        let task_app_id = app_id.clone();
        let task_queue = queue.clone();
        let task_client = client.clone();
        let task_settings = settings.clone();
        let task_invoke = invoke.clone();
        let task_metrics = metrics.clone();
        outstanding.spawn(async move {
            handle_batch(
                &task_app_id,
                &task_queue,
                task_client.as_ref(),
                &task_settings,
                &task_invoke,
                &task_metrics,
                messages,
            )
            .await;
            // Held until the batch is settled -- acknowledged or handed back
            // -- and not merely until the handler returned, because a batch
            // whose ack is still in flight is still this poller's.
            drop(permit);
        });
    }

    // Every batch still in flight is answered for before this poller reports
    // that it stopped. Leaving them would abort the tasks mid-ack: the
    // messages would stay leased until deis-queue expired them and then be
    // handed out again, which is work done twice for an application that was
    // merely being unloaded.
    // Reaped once more first: `len` counts tasks that have finished and not
    // yet been taken out, and a line that named those as still being waited
    // for would send an operator looking for a batch that is already settled.
    while outstanding.try_join_next().is_some() {}
    let still_held = outstanding.len();
    if still_held > 0 {
        tracing::debug!(
            app_id = %app_id,
            queue = %queue,
            batches = still_held,
            "Waiting for {} outstanding batch(es) of queue {} to be settled before app {} stops \
             consuming",
            still_held,
            queue,
            app_id
        );
    }
    while outstanding.join_next().await.is_some() {}

    tracing::info!(
        app_id = %app_id,
        queue = %queue,
        "Stopped consuming queue {} for app {}",
        queue,
        app_id
    );
}

/// Hand one batch to the app and settle it with the queue.
///
/// The hand-off is bounded by the lease, not by the app's execution timeout.
/// Those are different budgets: the timeout starts when the isolate begins to
/// run the batch, while the lease has been running since the pull, and between
/// the two the batch waits its turn in the queue of the runtime thread this
/// app shares with every other app on it. That wait has no ceiling. Without a
/// deadline here the poller would keep waiting past the lease and then
/// acknowledge messages deis-queue had already handed to the next puller: the
/// same work done twice, delivery attempts burnt, and eventually dead letters
/// for messages nothing was ever wrong with. The only trace would be an info
/// line on the queue side about acknowledging ids that were no longer queued.
async fn handle_batch(
    app_id: &str,
    queue: &str,
    client: &dyn QueuePort,
    settings: &ConsumerSettings,
    invoke: &BatchInvoker,
    metrics: &MetricsRegistry,
    messages: Vec<LeasedMessage>,
) {
    let delivered = messages.len();
    let budget = handling_budget(&messages, settings, now_unix());

    // Whether the attempts the queue counted for these deliveries may be
    // given back when they are handed back below. Attempts are charged at
    // the pull, so a batch the runtime refused at capacity -- the thread's
    // queue full, or the admission budget spent before the thread reached it
    // -- burned one attempt per message without the application ever seeing
    // the work. Refunding it is what keeps an overloaded node from dead
    // lettering messages nothing ever ran. Every other failure -- the
    // handler threw, the lease ran out -- is an attempt spent, and a refund
    // there would let a poison message outlive the ceiling that exists to
    // retire it.
    let (outcome, refund_attempt) =
        match tokio::time::timeout(budget, invoke(queue.to_string(), messages.clone())).await
        {
            Err(_) => (
                Err(anyhow::anyhow!(
                    "the batch was not finished within the {} second(s) its lease still had to \
                     run; it is handed back now rather than acknowledged after deis-queue has \
                     already given it to somebody else. The handler may still be running, so \
                     these messages can be processed twice -- queue handlers must be idempotent",
                    budget.as_secs()
                )),
                false,
            ),
            Ok(Err(e)) => {
                // Asked before the error is handed back into the tuple: the
                // downcast borrows it, and the tuple needs to own it.
                let refund_attempt = e
                    .downcast_ref::<DispatchError>()
                    .is_some_and(|error| matches!(error, DispatchError::Overloaded { .. }));
                (Err(e), refund_attempt)
            }
            Ok(Ok(completed)) => (Ok(completed), false),
        };

    // Settled by handle, not by id: the lease token in each handle is what
    // proves this consumer still holds the delivery it is answering for.
    let outcome = match outcome {
        Ok(completed) => completed,
        Err(e) => {
            // The handler never ran, threw, or is still running with the
            // lease about to lapse. Every message in the batch is unfinished;
            // hand them all back rather than losing them.
            tracing::warn!(
                app_id,
                queue,
                messages = delivered,
                refund_attempt,
                error = %e,
                "App {} failed to handle a batch of {} message(s) from queue {}; they are \
                 returned to deis-queue and will be delivered again, or dead lettered once \
                 their attempts run out",
                app_id,
                delivered,
                queue
            );
            BatchOutcome::all_failed(&messages)
        }
    };

    let failed: Vec<MessageHandle> = messages
        .iter()
        .filter(|message| outcome.failed.contains(&message.id))
        .map(LeasedMessage::handle)
        .collect();
    let handled: Vec<MessageHandle> = messages
        .iter()
        .filter(|message| !outcome.failed.contains(&message.id))
        .map(LeasedMessage::handle)
        .collect();

    metrics.record_queue_batch(app_id, queue, delivered as u64, failed.len() as u64);

    if !handled.is_empty() {
        let count = handled.len();
        match client.ack_messages(queue, handled).await {
            Ok(outcome) => report_stale_leases(app_id, queue, settings, outcome.stale_leases),
            Err(e) => tracing::warn!(
                app_id,
                queue,
                count,
                error = %e,
                "Acknowledging {} handled message(s) on queue {} failed; they stay leased and \
                 will be delivered a second time when the lease lapses, so the app's handler \
                 must be idempotent",
                count,
                queue
            ),
        }
    }

    if !failed.is_empty() {
        let count = failed.len();
        match client
            .nack_messages(queue, failed, settings.retry_delay_seconds, refund_attempt)
            .await
        {
            Ok(outcome) => {
                if outcome.dead_lettered > 0 {
                    tracing::warn!(
                        app_id,
                        queue,
                        dead_lettered = outcome.dead_lettered,
                        "{} message(s) from queue {} used up their delivery attempts and were \
                         dead lettered for app {}; they will never be delivered again and need \
                         an operator to look at them",
                        outcome.dead_lettered,
                        queue,
                        app_id
                    );
                }
                report_stale_leases(app_id, queue, settings, outcome.stale_leases);
            }
            Err(e) => tracing::warn!(
                app_id,
                queue,
                count,
                error = %e,
                "Returning {} unhandled message(s) to queue {} failed; they stay leased until \
                 deis-queue expires the lease, which delays the retry but does not lose them",
                count,
                queue
            ),
        }
    }
}

/// Say when deis-queue refused an answer because the lease had already lapsed.
///
/// This is not a transient hiccup: the message has been handed to another
/// consumer, so the work this node just finished is being done again right
/// now, and the count is the number of messages that happened to. The cause is
/// always the same one -- the handler takes longer than the lease -- so the
/// line names the lease it was given.
fn report_stale_leases(app_id: &str, queue: &str, settings: &ConsumerSettings, stale: i32) {
    if stale <= 0 {
        return;
    }
    tracing::warn!(
        app_id,
        queue,
        stale_leases = stale,
        lease_seconds = settings.lease_seconds,
        "{} message(s) on queue {} were answered too late: their lease had lapsed and \
         deis-queue had already given them to another consumer, so that work is being done \
         twice. The {}s lease is shorter than this app's handler actually takes -- raise the \
         app's execution timeout budget or make the batches smaller",
        stale,
        queue,
        settings.lease_seconds
    );
}

/// How much shorter than requested a granted lease may be before it is worth a
/// log line.
///
/// The lease is stamped on the server before the batch travels back and
/// expiry is whole seconds, so a small shortfall is normal rather than a sign
/// of anything.
const LEASE_SHORTFALL_TOLERANCE: Duration = Duration::from_secs(5);

/// Unix seconds: the clock deis-queue stamps lease expiry with.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// How long the server's lease on this batch still has to run.
///
/// Read off the messages rather than assumed: deis-queue clamps a lease
/// request above its own configured `max_lease_secs` instead of refusing it,
/// so what was asked for and what was granted are not always the same number.
/// The earliest expiry in the batch is the one that counts -- from that
/// instant on, that message belongs to whoever pulls next.
///
/// `None` when no message carries an expiry; a real deis-queue always does,
/// so this is the shape of a test double rather than of a server, and the
/// caller falls back to the lease it asked for.
fn granted_lease(messages: &[LeasedMessage], now: i64) -> Option<Duration> {
    let earliest = messages
        .iter()
        .map(|message| message.lease_expires_at)
        .filter(|expiry| *expiry > 0)
        .min()?;
    Some(Duration::from_secs(
        earliest.saturating_sub(now).max(0) as u64
    ))
}

/// How long the app may hold this batch: what is left of the lease, less what
/// settling it will cost.
///
/// Floored at one second, so a batch whose lease is already spent when it
/// arrives is still offered to the app -- the work may well succeed, and the
/// message is going to be redelivered either way.
fn handling_budget(messages: &[LeasedMessage], settings: &ConsumerSettings, now: i64) -> Duration {
    let lease = granted_lease(messages, now)
        .unwrap_or_else(|| Duration::from_secs(u64::from(settings.lease_seconds)));
    lease
        .saturating_sub(SETTLE_RESERVE)
        .max(Duration::from_secs(1))
}

/// Report a lease materially shorter than the one asked for, once.
///
/// deis-queue clamps an over-long request to its configured `max_lease_secs`
/// and logs that on its own side at debug level, so without this the node
/// would keep planning around a lease it does not have: handlers running past
/// the real expiry, their messages quietly handed to the next puller, and the
/// duplicates showing up as work done twice. Returns whether anything was
/// reported.
fn report_lease_shortfall(
    app_id: &str,
    queue: &str,
    settings: &ConsumerSettings,
    messages: &[LeasedMessage],
) -> bool {
    let Some(granted) = granted_lease(messages, now_unix()) else {
        return false;
    };
    let asked = Duration::from_secs(u64::from(settings.lease_seconds));
    if granted + LEASE_SHORTFALL_TOLERANCE >= asked {
        return false;
    }

    tracing::warn!(
        app_id,
        queue,
        asked_seconds = settings.lease_seconds,
        granted_seconds = granted.as_secs(),
        "deis-queue leased queue {} for {}s, not the {}s this node asked for -- it caps leases at \
         its own max_lease_secs rather than refusing the request. Batches are given up on at the \
         shorter deadline, so a handler of app {} that needs longer will have its messages \
         delivered a second time; raise max_lease_secs on deis-queue or lower this app's \
         execution timeout",
        queue,
        granted.as_secs(),
        settings.lease_seconds,
        app_id
    );
    true
}

#[derive(Debug, PartialEq, Eq)]
enum Wake {
    Elapsed,
    Stopped,
}

/// Wait out a backoff, cutting it short if the poller is asked to stop.
async fn wait(stop: &mut oneshot::Receiver<()>, duration: Duration) -> Wake {
    tokio::select! {
        _ = tokio::time::sleep(duration) => Wake::Elapsed,
        _ = stop => Wake::Stopped,
    }
}

fn next_backoff(current: Duration, max: Duration) -> Duration {
    (current * 2).min(max)
}

#[cfg(test)]
#[path = "queue_consumer_test.rs"]
mod queue_consumer_test;
