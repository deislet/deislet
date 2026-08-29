#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::runtime::metrics::{MetricsRegistry, QUEUE_MESSAGES_TOTAL, QUEUE_POLL_ERRORS_TOTAL};
    use crate::runtime::queue_client::{LeasedMessage, NackOutcome};
    use std::sync::{Arc, Mutex};
    use tokio::sync::watch;

    /// Queue name plus message ids, one entry per batch the invoker saw.
    type SeenBatches = Arc<Mutex<Vec<(String, Vec<String>)>>>;

    fn message(id: &str) -> LeasedMessage {
        LeasedMessage {
            id: id.to_string(),
            body: format!("body-of-{id}"),
            attempts: 1,
            lease_expires_at: 0,
            created_at: 0,
        }
    }

    /// The same message, leased until `seconds` from now -- the shape a real
    /// deis-queue hands back.
    fn message_leased_for(id: &str, seconds: i64) -> LeasedMessage {
        LeasedMessage {
            lease_expires_at: now_unix() + seconds,
            ..message(id)
        }
    }

    /// What the fake queue was asked to do.
    #[derive(Debug, Default)]
    struct Journal {
        acked: Vec<String>,
        nacked: Vec<String>,
        nack_delay: u32,
    }

    /// A queue that hands out one scripted batch per pull and records how the
    /// consumer settled it. Standing in for deis-queue keeps the interesting
    /// half -- what gets acknowledged and what gets handed back -- testable
    /// without a live server.
    struct FakeQueue {
        batches: Mutex<Vec<Vec<LeasedMessage>>>,
        journal: Arc<Mutex<Journal>>,
        /// Number of pulls so far, published so a test can wait for the
        /// consumer to reach a point instead of sleeping and hoping.
        pulls: watch::Sender<usize>,
        fail_pull: bool,
    }

    impl FakeQueue {
        fn with_batches(batches: Vec<Vec<LeasedMessage>>) -> (Arc<Self>, Arc<Mutex<Journal>>) {
            let journal = Arc::new(Mutex::new(Journal::default()));
            let queue = Arc::new(Self {
                batches: Mutex::new(batches.into_iter().rev().collect()),
                journal: journal.clone(),
                pulls: watch::channel(0).0,
                fail_pull: false,
            });
            (queue, journal)
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                batches: Mutex::new(Vec::new()),
                journal: Arc::new(Mutex::new(Journal::default())),
                pulls: watch::channel(0).0,
                fail_pull: true,
            })
        }

        fn pull_count(&self) -> usize {
            *self.pulls.borrow()
        }

        fn watch_pulls(&self) -> watch::Receiver<usize> {
            self.pulls.subscribe()
        }
    }

    impl QueuePort for FakeQueue {
        fn pull_messages<'a>(
            &'a self,
            _queue: &'a str,
            _max_messages: u32,
            _lease_seconds: u32,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<Vec<LeasedMessage>>> {
            self.pulls.send_modify(|count| *count += 1);
            Box::pin(async move {
                if self.fail_pull {
                    anyhow::bail!("queue unreachable");
                }
                Ok(self.batches.lock().unwrap().pop().unwrap_or_default())
            })
        }

        fn ack_messages<'a>(
            &'a self,
            _queue: &'a str,
            message_ids: Vec<String>,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<i32>> {
            let count = message_ids.len() as i32;
            self.journal.lock().unwrap().acked.extend(message_ids);
            Box::pin(async move { Ok(count) })
        }

        fn nack_messages<'a>(
            &'a self,
            _queue: &'a str,
            message_ids: Vec<String>,
            delay_seconds: u32,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<NackOutcome>> {
            let count = message_ids.len() as i32;
            {
                let mut journal = self.journal.lock().unwrap();
                journal.nacked.extend(message_ids);
                journal.nack_delay = delay_seconds;
            }
            Box::pin(async move {
                Ok(NackOutcome {
                    released: count,
                    dead_lettered: 0,
                })
            })
        }
    }

    fn settings() -> ConsumerSettings {
        ConsumerSettings {
            batch_size: 10,
            lease_seconds: 60,
            idle_backoff_min: std::time::Duration::from_millis(5),
            idle_backoff_max: std::time::Duration::from_millis(20),
            retry_delay_seconds: 7,
        }
    }

    /// An invoker that reports the given ids as failed and records the
    /// batches it was handed.
    fn invoker(failed: Vec<&str>, seen: SeenBatches) -> BatchInvoker {
        let failed: Vec<String> = failed.into_iter().map(String::from).collect();
        Arc::new(move |queue, messages| {
            let failed = failed.clone();
            let seen = seen.clone();
            Box::pin(async move {
                seen.lock()
                    .unwrap()
                    .push((queue, messages.iter().map(|m| m.id.clone()).collect()));
                Ok(BatchOutcome { failed })
            })
        })
    }

    #[tokio::test]
    async fn a_handled_batch_is_acknowledged_and_never_delivered_again() {
        let (queue, journal) = FakeQueue::with_batches(vec![vec![message("m1"), message("m2")]]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let metrics = Arc::new(MetricsRegistry::new());
        let mut pulls = queue.watch_pulls();

        let handle = spawn_with_registry(
            "demo".to_string(),
            "jobs".to_string(),
            queue.clone(),
            settings(),
            invoker(vec![], seen.clone()),
            metrics.clone(),
        );

        // The consumer is done with the batch once it has come back for more
        // work, which is also when the acknowledgement has already been sent.
        wait_for_pulls(&mut pulls, 2).await;
        handle.stop().await;

        let journal = journal.lock().unwrap();
        assert_eq!(journal.acked, vec!["m1".to_string(), "m2".to_string()]);
        assert!(journal.nacked.is_empty());
        assert_eq!(seen.lock().unwrap()[0].0, "jobs");
        assert_eq!(metrics.value(QUEUE_MESSAGES_TOTAL, "demo"), 2.0);
    }

    #[tokio::test]
    async fn a_failed_message_goes_back_and_the_rest_are_acknowledged() {
        // Half a batch failing must not cost the other half: acknowledging
        // everything would lose the failure, nacking everything would
        // redeliver work that already succeeded.
        let (queue, journal) =
            FakeQueue::with_batches(vec![vec![message("ok1"), message("bad"), message("ok2")]]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut pulls = queue.watch_pulls();

        let handle = spawn_with_registry(
            "demo".to_string(),
            "jobs".to_string(),
            queue.clone(),
            settings(),
            invoker(vec!["bad"], seen),
            Arc::new(MetricsRegistry::new()),
        );
        wait_for_pulls(&mut pulls, 2).await;
        handle.stop().await;

        let journal = journal.lock().unwrap();
        assert_eq!(journal.acked, vec!["ok1".to_string(), "ok2".to_string()]);
        assert_eq!(journal.nacked, vec!["bad".to_string()]);
        assert_eq!(journal.nack_delay, 7, "the configured retry delay is used");
    }

    #[tokio::test]
    async fn a_handler_that_throws_returns_the_whole_batch() {
        let (queue, journal) = FakeQueue::with_batches(vec![vec![message("m1"), message("m2")]]);
        let mut pulls = queue.watch_pulls();
        let invoke: BatchInvoker = Arc::new(|_queue, _messages| {
            Box::pin(async move { anyhow::bail!("handler exploded") })
        });

        let handle = spawn_with_registry(
            "demo".to_string(),
            "jobs".to_string(),
            queue.clone(),
            settings(),
            invoke,
            Arc::new(MetricsRegistry::new()),
        );
        wait_for_pulls(&mut pulls, 2).await;
        handle.stop().await;

        let journal = journal.lock().unwrap();
        assert!(journal.acked.is_empty(), "nothing was handled");
        assert_eq!(journal.nacked, vec!["m1".to_string(), "m2".to_string()]);
    }

    #[tokio::test]
    async fn an_empty_queue_is_polled_again_rather_than_treated_as_a_failure() {
        let (queue, journal) = FakeQueue::with_batches(vec![vec![], vec![message("m1")]]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut pulls = queue.watch_pulls();

        let handle = spawn_with_registry(
            "demo".to_string(),
            "jobs".to_string(),
            queue.clone(),
            settings(),
            invoker(vec![], seen),
            Arc::new(MetricsRegistry::new()),
        );
        // Pull 1 comes back empty, pull 2 carries the message, and by pull 3
        // it has been handled and acknowledged.
        wait_for_pulls(&mut pulls, 3).await;
        handle.stop().await;

        assert_eq!(journal.lock().unwrap().acked, vec!["m1".to_string()]);
    }

    #[tokio::test]
    async fn a_queue_that_cannot_be_reached_is_counted_and_retried() {
        let queue = FakeQueue::failing();
        let metrics = Arc::new(MetricsRegistry::new());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut pulls = queue.watch_pulls();

        let handle = spawn_with_registry(
            "demo".to_string(),
            "jobs".to_string(),
            queue.clone(),
            settings(),
            invoker(vec![], seen),
            metrics.clone(),
        );
        wait_for_pulls(&mut pulls, 2).await;
        handle.stop().await;

        assert!(
            metrics.value(QUEUE_POLL_ERRORS_TOTAL, "demo") >= 1.0,
            "a consumer that cannot reach its queue must be visible in metrics, not just in logs"
        );
    }

    #[tokio::test]
    async fn stopping_ends_the_poller() {
        let (queue, _journal) = FakeQueue::with_batches(vec![]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut pulls = queue.watch_pulls();
        let handle = spawn_with_registry(
            "demo".to_string(),
            "jobs".to_string(),
            queue.clone(),
            settings(),
            invoker(vec![], seen),
            Arc::new(MetricsRegistry::new()),
        );
        wait_for_pulls(&mut pulls, 1).await;
        // Returns only once the task has actually finished; a hang here would
        // mean an unloaded app keeps leasing messages forever.
        handle.stop().await;
        let before = queue.pull_count();
        tokio::task::yield_now().await;
        assert_eq!(queue.pull_count(), before);
    }

    #[tokio::test]
    async fn a_batch_that_outlives_its_lease_is_handed_back_instead_of_acknowledged() {
        // The hand-off to the isolate thread has no ceiling of its own: the
        // batch waits behind every other app on that thread. Waiting for it
        // regardless and then acknowledging would acknowledge messages
        // deis-queue had already given to the next puller -- the work is done
        // twice, the delivery attempts are spent, and messages nothing was
        // ever wrong with end up dead lettered. So the poller gives up while
        // the lease is still valid and hands the batch back.
        //
        // The lease here is short enough that the deadline is one second (the
        // floor), and the invoker never answers at all.
        let (queue, journal) = FakeQueue::with_batches(vec![vec![message_leased_for("stuck", 2)]]);
        let mut pulls = queue.watch_pulls();
        let never: BatchInvoker = Arc::new(|_queue, _messages| {
            Box::pin(async move {
                std::future::pending::<()>().await;
                unreachable!("this invoker never answers")
            })
        });

        let handle = spawn_with_registry(
            "demo".to_string(),
            "jobs".to_string(),
            queue.clone(),
            settings(),
            never,
            Arc::new(MetricsRegistry::new()),
        );
        wait_for_pulls(&mut pulls, 2).await;
        handle.stop().await;

        let journal = journal.lock().unwrap();
        assert!(
            journal.acked.is_empty(),
            "nothing may be acknowledged while the handler is still running"
        );
        assert_eq!(
            journal.nacked,
            vec!["stuck".to_string()],
            "the batch has to go back to deis-queue before the lease lapses"
        );
    }

    #[test]
    fn the_handling_budget_comes_from_the_lease_the_server_granted() {
        // deis-queue caps a lease above its configured max_lease_secs instead
        // of refusing it, so what was asked for is not what is held.
        let asked = ConsumerSettings {
            lease_seconds: 600,
            ..settings()
        };
        let clamped = vec![message_leased_for("m1", 100)];

        let budget = handling_budget(&clamped, &asked, now_unix());
        assert!(
            budget <= std::time::Duration::from_secs(100),
            "the budget must follow the granted lease, not the requested one: {budget:?}"
        );
        assert!(
            budget >= std::time::Duration::from_secs(90),
            "and it must not throw away most of a lease that is nearly all usable: {budget:?}"
        );
    }

    #[test]
    fn the_earliest_expiry_in_a_batch_is_the_one_that_counts() {
        // Once the first message's lease lapses it is somebody else's, even
        // though the rest of the batch is still held.
        let messages = vec![
            message_leased_for("later", 300),
            message_leased_for("soonest", 60),
        ];
        let budget = handling_budget(&messages, &settings(), now_unix());
        assert!(budget <= std::time::Duration::from_secs(60), "{budget:?}");
    }

    #[test]
    fn a_batch_whose_lease_is_already_spent_is_still_offered_to_the_app() {
        // The work may well succeed, and the message is going to be
        // redelivered either way; refusing to try would only waste it.
        let messages = vec![message_leased_for("stale", -30)];
        assert_eq!(
            handling_budget(&messages, &settings(), now_unix()),
            std::time::Duration::from_secs(1)
        );
    }

    #[test]
    fn a_lease_the_server_shortened_is_reported_once() {
        let asked = ConsumerSettings {
            lease_seconds: 600,
            ..settings()
        };
        assert!(
            report_lease_shortfall("demo", "jobs", &asked, &[message_leased_for("m1", 100)]),
            "a lease six times shorter than requested has to be visible"
        );
        assert!(
            !report_lease_shortfall("demo", "jobs", &asked, &[message_leased_for("m1", 599)]),
            "a second of slack in a granted lease is normal and must not be reported"
        );
        assert!(
            !report_lease_shortfall("demo", "jobs", &asked, &[message("m1")]),
            "a message carrying no expiry says nothing about the lease"
        );
    }

    #[test]
    fn the_lease_outlasts_the_app_s_execution_timeout() {
        // A lease shorter than the handler means deis-queue hands the same
        // message to somebody else while it is still being processed.
        let settings = ConsumerSettings::for_app(None, std::time::Duration::from_secs(30));
        assert!(settings.lease_seconds > 30);
        assert_eq!(settings.batch_size, DEFAULT_BATCH_SIZE);
    }

    #[test]
    fn a_declared_batch_size_is_honoured_and_a_zero_is_not() {
        assert_eq!(
            ConsumerSettings::for_app(Some(25), std::time::Duration::from_secs(30)).batch_size,
            25
        );
        assert_eq!(
            ConsumerSettings::for_app(Some(0), std::time::Duration::from_secs(30)).batch_size,
            DEFAULT_BATCH_SIZE,
            "asking for zero messages per pull would consume nothing at all"
        );
    }

    #[test]
    fn an_enormous_timeout_still_asks_for_a_lease_the_queue_accepts() {
        let settings = ConsumerSettings::for_app(None, std::time::Duration::from_secs(86_400));
        assert!(settings.lease_seconds <= 3600);
    }

    /// Wait until the consumer has pulled at least `n` times.
    ///
    /// Driven by the fake queue's own signal rather than by a sleep: the test
    /// proceeds the instant the consumer gets there, and a consumer that
    /// never does makes the test hang on the real event instead of racing a
    /// timer.
    async fn wait_for_pulls(pulls: &mut watch::Receiver<usize>, n: usize) {
        while *pulls.borrow_and_update() < n {
            pulls
                .changed()
                .await
                .expect("the fake queue outlives the wait");
        }
    }
}
