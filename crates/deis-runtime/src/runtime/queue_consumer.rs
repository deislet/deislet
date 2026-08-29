//! Queue consumption: one polling task per queue an application declares.
//!
//! # Why the runtime pulls
//!
//! deis-queue never pushes. Pushing would require the queue to know which
//! node runs which application, and the queue deliberately knows nothing
//! except the app id (`docs/architecture.zh.md` section 6). So each node
//! polls for the applications it runs, and the queue's lease is what stops
//! two nodes from handling the same message: a leased message is invisible to
//! everybody else until the lease lapses.
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
//! # Delivery guarantees the app has to live with
//!
//! At-least-once. A node that dies between handling a message and
//! acknowledging it will see that message again once the lease lapses, so an
//! application's queue handler must be idempotent. A handler that runs longer
//! than the lease has the same effect, which is why the lease is derived from
//! the app's execution timeout rather than fixed.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::sync::oneshot;

use super::metrics::{recorder, MetricsRegistry};
use super::queue_client::{LeasedMessage, NackOutcome, QueueClient};

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

    fn ack_messages<'a>(
        &'a self,
        queue: &'a str,
        message_ids: Vec<String>,
    ) -> BoxFuture<'a, anyhow::Result<i32>>;

    fn nack_messages<'a>(
        &'a self,
        queue: &'a str,
        message_ids: Vec<String>,
        delay_seconds: u32,
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
        message_ids: Vec<String>,
    ) -> BoxFuture<'a, anyhow::Result<i32>> {
        Box::pin(self.ack(queue, message_ids))
    }

    fn nack_messages<'a>(
        &'a self,
        queue: &'a str,
        message_ids: Vec<String>,
        delay_seconds: u32,
    ) -> BoxFuture<'a, anyhow::Result<NackOutcome>> {
        Box::pin(self.nack(queue, message_ids, delay_seconds))
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
}

/// Messages leased per pull when the manifest does not say.
pub const DEFAULT_BATCH_SIZE: u32 = 10;

/// Slack added to the app's execution timeout when deriving the lease, to
/// cover the pull, the hand-off to the isolate thread and the acknowledgement.
const LEASE_SLACK: Duration = Duration::from_secs(30);

/// Upper bound deis-queue puts on a lease; asking for more is refused.
const MAX_LEASE_SECONDS: u32 = 3600;

impl ConsumerSettings {
    /// Settings for an app whose isolate is bounded by `execution_timeout`.
    pub fn for_app(batch_size: Option<u32>, execution_timeout: Duration) -> Self {
        let lease = execution_timeout + LEASE_SLACK;
        Self {
            batch_size: batch_size
                .filter(|size| *size > 0)
                .unwrap_or(DEFAULT_BATCH_SIZE),
            lease_seconds: (lease.as_secs() as u32).clamp(1, MAX_LEASE_SECONDS),
            idle_backoff_min: Duration::from_secs(1),
            idle_backoff_max: Duration::from_secs(30),
            retry_delay_seconds: 10,
        }
    }
}

impl Default for ConsumerSettings {
    fn default() -> Self {
        Self::for_app(None, Duration::from_secs(30))
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

    tracing::info!(
        app_id = %app_id,
        queue = %queue,
        batch_size = settings.batch_size,
        lease_seconds = settings.lease_seconds,
        "Consuming queue {} for app {}: up to {} messages per pull, {}s leases",
        queue,
        app_id,
        settings.batch_size,
        settings.lease_seconds
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

    loop {
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
                if wait(&mut stop, backoff).await == Wake::Stopped {
                    break;
                }
                backoff = next_backoff(backoff, settings.idle_backoff_max);
                continue;
            }
        };

        if messages.is_empty() {
            if wait(&mut stop, backoff).await == Wake::Stopped {
                break;
            }
            backoff = next_backoff(backoff, settings.idle_backoff_max);
            continue;
        }

        backoff = settings.idle_backoff_min;
        handle_batch(
            &app_id,
            &queue,
            client.as_ref(),
            &settings,
            &invoke,
            &metrics,
            messages,
        )
        .await;
    }

    tracing::info!(
        app_id = %app_id,
        queue = %queue,
        "Stopped consuming queue {} for app {}",
        queue,
        app_id
    );
}

/// Hand one batch to the app and settle it with the queue.
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
    let ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();

    let outcome = match invoke(queue.to_string(), messages.clone()).await {
        Ok(outcome) => outcome,
        Err(e) => {
            // The handler never ran, or threw. Every message in the batch is
            // unfinished; hand them all back rather than losing them.
            tracing::warn!(
                app_id,
                queue,
                messages = delivered,
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

    let failed: Vec<String> = ids
        .iter()
        .filter(|id| outcome.failed.contains(id))
        .cloned()
        .collect();
    let handled: Vec<String> = ids
        .iter()
        .filter(|id| !outcome.failed.contains(id))
        .cloned()
        .collect();

    metrics.record_queue_batch(app_id, queue, delivered as u64, failed.len() as u64);

    if !handled.is_empty() {
        let count = handled.len();
        if let Err(e) = client.ack_messages(queue, handled).await {
            tracing::warn!(
                app_id,
                queue,
                count,
                error = %e,
                "Acknowledging {} handled message(s) on queue {} failed; they stay leased and \
                 will be delivered a second time when the lease lapses, so the app's handler \
                 must be idempotent",
                count,
                queue
            );
        }
    }

    if !failed.is_empty() {
        let count = failed.len();
        match client
            .nack_messages(queue, failed, settings.retry_delay_seconds)
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
