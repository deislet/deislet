#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::runtime::metrics::{MetricsRegistry, QUEUE_MESSAGES_TOTAL, QUEUE_POLL_ERRORS_TOTAL};
    use crate::runtime::queue_client::{AckOutcome, LeasedMessage, MessageHandle, NackOutcome};
    use std::sync::{Arc, Mutex};
    use tokio::sync::watch;

    /// Queue name plus message ids, one entry per batch the invoker saw.
    type SeenBatches = Arc<Mutex<Vec<(String, Vec<String>)>>>;

    fn message(id: &str) -> LeasedMessage {
        LeasedMessage {
            id: id.to_string(),
            body: format!("body-of-{id}").into_bytes(),
            attempts: 1,
            lease_expires_at: 0,
            created_at: 0,
            // A real deis-queue issues a fresh one per delivery; the tests
            // only need it to be the token that comes back with the id.
            lease_token: format!("lease-of-{id}"),
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
        /// The lease tokens the consumer presented with the ids above. An
        /// answer that arrived without one would be refused by a real
        /// deis-queue and the message redelivered.
        ack_tokens: Vec<String>,
        nack_tokens: Vec<String>,
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
            handles: Vec<MessageHandle>,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<AckOutcome>> {
            let count = handles.len() as i32;
            {
                let mut journal = self.journal.lock().unwrap();
                journal
                    .ack_tokens
                    .extend(handles.iter().map(|handle| handle.lease_token.clone()));
                journal
                    .acked
                    .extend(handles.into_iter().map(|handle| handle.id));
            }
            Box::pin(async move {
                Ok(AckOutcome {
                    acknowledged: count,
                    stale_leases: 0,
                })
            })
        }

        fn nack_messages<'a>(
            &'a self,
            _queue: &'a str,
            handles: Vec<MessageHandle>,
            delay_seconds: u32,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<NackOutcome>> {
            let count = handles.len() as i32;
            {
                let mut journal = self.journal.lock().unwrap();
                journal
                    .nack_tokens
                    .extend(handles.iter().map(|handle| handle.lease_token.clone()));
                journal
                    .nacked
                    .extend(handles.into_iter().map(|handle| handle.id));
                journal.nack_delay = delay_seconds;
            }
            Box::pin(async move {
                Ok(NackOutcome {
                    released: count,
                    dead_lettered: 0,
                    stale_leases: 0,
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
            max_concurrency: 1,
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
        let settings = ConsumerSettings::for_app(None, None, std::time::Duration::from_secs(30));
        assert!(settings.lease_seconds > 30);
        assert_eq!(settings.batch_size, DEFAULT_BATCH_SIZE);
    }

    #[test]
    fn a_declared_batch_size_is_honoured_and_a_zero_is_not() {
        assert_eq!(
            ConsumerSettings::for_app(Some(25), None, std::time::Duration::from_secs(30))
                .batch_size,
            25
        );
        assert_eq!(
            ConsumerSettings::for_app(Some(0), None, std::time::Duration::from_secs(30)).batch_size,
            DEFAULT_BATCH_SIZE,
            "asking for zero messages per pull would consume nothing at all"
        );
    }

    #[test]
    fn an_enormous_timeout_still_asks_for_a_lease_the_queue_accepts() {
        let settings =
            ConsumerSettings::for_app(None, None, std::time::Duration::from_secs(86_400));
        assert!(settings.lease_seconds <= 3600);
    }

    /// An invoker that holds every batch it is handed until the test lets go,
    /// and publishes how many it is holding.
    ///
    /// This is the only way "outstanding" is observable from outside: a batch
    /// that has been handed to the application and has not come back is
    /// exactly a batch the poller is still holding a lease on.
    fn holding_invoker(
        handed: watch::Sender<usize>,
        release: watch::Receiver<bool>,
    ) -> BatchInvoker {
        Arc::new(move |_queue, _messages| {
            let handed = handed.clone();
            let mut release = release.clone();
            Box::pin(async move {
                handed.send_modify(|count| *count += 1);
                loop {
                    if *release.borrow_and_update() {
                        break;
                    }
                    if release.changed().await.is_err() {
                        break;
                    }
                }
                Ok(BatchOutcome::default())
            })
        })
    }

    /// `max_concurrency` above one lets the poller lease the next batch while
    /// the previous one is still with the application.
    ///
    /// Two batches in the invoker's hands at the same moment is the whole
    /// claim, and it is deliberately not a claim about parallel *execution*:
    /// on a node the invoker posts the batch to the one runtime thread the
    /// application lives on, so the second batch waits its turn there. What is
    /// bought is that it is already leased and queued when the first finishes,
    /// instead of costing a fresh round trip to deis-queue.
    #[tokio::test]
    async fn two_outstanding_batches_are_held_at_once_when_the_manifest_asks_for_two() {
        let (queue, _journal) = FakeQueue::with_batches(vec![
            vec![message("m1")],
            vec![message("m2")],
            vec![message("m3")],
        ]);
        let (handed_tx, mut handed) = watch::channel(0usize);
        let (release_tx, release_rx) = watch::channel(false);

        let handle = spawn_with_registry(
            "demo".to_string(),
            "jobs".to_string(),
            queue.clone(),
            ConsumerSettings {
                max_concurrency: 2,
                ..settings()
            },
            holding_invoker(handed_tx, release_rx),
            Arc::new(MetricsRegistry::new()),
        );

        // Waits on the real event: the test proceeds the instant the second
        // batch is handed over, and hangs on the condition rather than racing
        // a timer if it never is.
        while *handed.borrow_and_update() < 2 {
            handed
                .changed()
                .await
                .expect("the invoker outlives the wait");
        }

        // Never a third, because the third permit does not exist.
        assert_eq!(*handed.borrow(), 2);

        release_tx
            .send(true)
            .expect("the invoker is still listening");
        handle.stop().await;
    }

    /// The default is one, and one means exactly the sequential loop this
    /// consumer had before the setting existed.
    ///
    /// The claim is a negative -- nothing else is pulled while the first batch
    /// is out -- so it needs a bound to be judged against; the timeout below
    /// is that bound and not a coordination sleep. A consumer that ignored
    /// `max_concurrency` would hand over the second batch immediately and fail
    /// this long before the window ran out.
    #[tokio::test]
    async fn one_outstanding_batch_is_the_default_and_nothing_is_pulled_behind_it() {
        let (queue, _journal) =
            FakeQueue::with_batches(vec![vec![message("m1")], vec![message("m2")]]);
        let (handed_tx, mut handed) = watch::channel(0usize);
        let (release_tx, release_rx) = watch::channel(false);

        let handle = spawn_with_registry(
            "demo".to_string(),
            "jobs".to_string(),
            queue.clone(),
            settings(),
            holding_invoker(handed_tx, release_rx),
            Arc::new(MetricsRegistry::new()),
        );

        while *handed.borrow_and_update() < 1 {
            handed
                .changed()
                .await
                .expect("the invoker outlives the wait");
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(500), handed.changed())
                .await
                .is_err(),
            "a second batch was handed over while the first was still outstanding"
        );

        // Released, so the stop below does not have to abandon a batch.
        release_tx
            .send(true)
            .expect("the invoker is still listening");
        handle.stop().await;
    }

    /// Every batch still in flight is settled before the poller reports that
    /// it stopped.
    ///
    /// Abandoning them would leave the messages leased until deis-queue
    /// expired the leases and handed them out again -- the same work done
    /// twice for an application that was only being unloaded.
    #[tokio::test]
    async fn stopping_waits_for_every_outstanding_batch_to_be_settled() {
        // Exactly as many batches as the poller may hold, so what is
        // acknowledged after the stop can only be the two it was holding.
        let (queue, journal) =
            FakeQueue::with_batches(vec![vec![message("m1")], vec![message("m2")]]);
        let (handed_tx, mut handed) = watch::channel(0usize);
        let (release_tx, release_rx) = watch::channel(false);

        let handle = spawn_with_registry(
            "demo".to_string(),
            "jobs".to_string(),
            queue.clone(),
            ConsumerSettings {
                max_concurrency: 2,
                ..settings()
            },
            holding_invoker(handed_tx, release_rx),
            Arc::new(MetricsRegistry::new()),
        );

        while *handed.borrow_and_update() < 2 {
            handed
                .changed()
                .await
                .expect("the invoker outlives the wait");
        }

        // Let both finish and stop in the same breath: `stop` has to wait for
        // the two batches it is holding, not only for the loop to notice.
        release_tx
            .send(true)
            .expect("the invoker is still listening");
        handle.stop().await;

        // Sorted: two batches settling at once have no order between them,
        // and the claim is that both were settled, not which went first.
        let mut acked = journal.lock().unwrap().acked.clone();
        acked.sort();
        assert_eq!(
            acked,
            vec!["m1".to_string(), "m2".to_string()],
            "both outstanding batches must be acknowledged before the poller reports it stopped"
        );
    }

    /// A manifest asking for more outstanding batches than this node allows is
    /// held to the node's number, the way every other limit is.
    #[test]
    fn a_manifest_cannot_ask_for_more_outstanding_batches_than_the_node_allows() {
        // `for_app` takes the declaration as written; the clamp happens where
        // the application can be named in the log, which is `spawn`.
        let asked = ConsumerSettings::for_app(
            None,
            Some(MAX_OUTSTANDING_BATCHES + 40),
            std::time::Duration::from_secs(30),
        );
        assert_eq!(asked.max_concurrency, MAX_OUTSTANDING_BATCHES + 40);

        // Absent and zero both mean "the runtime decides", which is one.
        assert_eq!(
            ConsumerSettings::for_app(None, None, std::time::Duration::from_secs(30))
                .max_concurrency,
            DEFAULT_MAX_CONCURRENCY
        );
        assert_eq!(
            ConsumerSettings::for_app(None, Some(0), std::time::Duration::from_secs(30))
                .max_concurrency,
            DEFAULT_MAX_CONCURRENCY
        );
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
