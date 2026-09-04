#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::config::LoggingConfig;
    use std::time::Duration;

    fn sample<'a>(samples: &'a [proto::MetricSample], name: &str) -> &'a proto::MetricSample {
        samples
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no sample named {name}: {samples:?}"))
    }

    #[test]
    fn a_served_request_counts_once_and_is_not_an_error() {
        let registry = MetricsRegistry::new();
        registry.record_request("demo", Duration::from_millis(12), RequestOutcome::Ok);

        assert_eq!(registry.value(REQUESTS_TOTAL, "demo"), 1.0);
        assert_eq!(registry.value(REQUEST_ERRORS_TOTAL, "demo"), 0.0);
        assert_eq!(registry.value(EXECUTION_MS_COUNT, "demo"), 1.0);
        assert!((registry.value(EXECUTION_MS_SUM, "demo") - 12.0).abs() < 1.0);
    }

    #[test]
    fn a_failed_request_counts_in_both_totals() {
        // The error counter is a subset of the request counter, so a
        // dashboard can divide one by the other to get an error rate.
        let registry = MetricsRegistry::new();
        registry.record_request("demo", Duration::from_millis(5), RequestOutcome::Error);

        assert_eq!(registry.value(REQUESTS_TOTAL, "demo"), 1.0);
        assert_eq!(registry.value(REQUEST_ERRORS_TOTAL, "demo"), 1.0);
    }

    #[test]
    fn execution_time_accumulates() {
        let registry = MetricsRegistry::new();
        registry.record_request("demo", Duration::from_millis(10), RequestOutcome::Ok);
        registry.record_request("demo", Duration::from_millis(40), RequestOutcome::Ok);
        registry.record_request("demo", Duration::from_millis(20), RequestOutcome::Ok);

        assert_eq!(registry.value(EXECUTION_MS_COUNT, "demo"), 3.0);
        assert!((registry.value(EXECUTION_MS_SUM, "demo") - 70.0).abs() < 3.0);
    }

    /// The count in one latency bucket, read the way a scraper reads it.
    fn bucket(samples: &[proto::MetricSample], le: &str) -> f64 {
        samples
            .iter()
            .find(|s| {
                s.name == EXECUTION_MS_BUCKET && s.labels.iter().any(|(k, v)| k == "le" && v == le)
            })
            .unwrap_or_else(|| panic!("no bucket le={le}: {samples:?}"))
            .value
    }

    #[test]
    fn latency_lands_in_buckets_a_percentile_can_be_read_from() {
        // What the high-water gauge could not do: three fast requests and one
        // slow one must leave a distribution, not a single number pinned at
        // the slow one for the rest of the process's life.
        let registry = MetricsRegistry::new();
        registry.record_request("demo", Duration::from_millis(3), RequestOutcome::Ok);
        registry.record_request("demo", Duration::from_millis(4), RequestOutcome::Ok);
        registry.record_request("demo", Duration::from_millis(7), RequestOutcome::Ok);
        registry.record_request("demo", Duration::from_millis(400), RequestOutcome::Ok);

        let samples = registry.snapshot(1);
        assert_eq!(bucket(&samples, "1"), 0.0);
        assert_eq!(bucket(&samples, "5"), 2.0, "3ms and 4ms fall at or under 5");
        assert_eq!(bucket(&samples, "10"), 3.0);
        assert_eq!(bucket(&samples, "250"), 3.0);
        assert_eq!(bucket(&samples, "500"), 4.0);
        assert_eq!(bucket(&samples, "+Inf"), 4.0);
        assert_eq!(registry.value(EXECUTION_MS_COUNT, "demo"), 4.0);
    }

    #[test]
    fn one_slow_request_does_not_pin_the_numbers_for_ever() {
        // The defect this replaced: a 30-second timeout used to hold the
        // latency reading at 30 seconds until the process restarted. In a
        // histogram it is one observation in the top bucket and the fast
        // requests after it are still visible as fast.
        let registry = MetricsRegistry::new();
        registry.record_request("demo", Duration::from_secs(30), RequestOutcome::Error);
        for _ in 0..99 {
            registry.record_request("demo", Duration::from_millis(2), RequestOutcome::Ok);
        }

        let samples = registry.snapshot(1);
        assert_eq!(bucket(&samples, "2"), 99.0);
        assert_eq!(bucket(&samples, "+Inf"), 100.0);
        assert_eq!(
            bucket(&samples, "10000"),
            99.0,
            "the slow one is above 10s and must not be counted below it"
        );
    }

    #[test]
    fn every_bucket_exists_from_the_first_observation() {
        // A bucket that appears partway through makes rate() over it read as
        // a burst that never happened, so they are all created at once.
        let registry = MetricsRegistry::new();
        registry.record_request("demo", Duration::from_millis(1), RequestOutcome::Ok);

        let samples = registry.snapshot(1);
        let buckets = samples
            .iter()
            .filter(|s| s.name == EXECUTION_MS_BUCKET)
            .count();
        assert_eq!(
            buckets,
            EXECUTION_MS_BUCKETS.len() + 1,
            "every finite bound plus +Inf: {samples:?}"
        );
    }

    #[test]
    fn buckets_never_decrease_as_the_bound_rises() {
        // Cumulative counts are what histogram_quantile assumes; a bucket
        // that read lower than the one below it would make every quantile
        // nonsense.
        let registry = MetricsRegistry::new();
        for ms in [1, 3, 9, 30, 90, 300, 900, 3000] {
            registry.record_request("demo", Duration::from_millis(ms), RequestOutcome::Ok);
        }

        let samples = registry.snapshot(1);
        let mut previous = 0.0;
        for (_, label) in EXECUTION_MS_BUCKETS {
            let value = bucket(&samples, label);
            assert!(
                value >= previous,
                "bucket le={label} is {value}, below the bucket under it ({previous})"
            );
            previous = value;
        }
        assert!(bucket(&samples, "+Inf") >= previous);
    }

    #[test]
    fn one_app_never_shows_up_in_another_app_s_numbers() {
        let registry = MetricsRegistry::new();
        registry.record_request("a", Duration::from_millis(1), RequestOutcome::Ok);
        registry.record_request("b", Duration::from_millis(1), RequestOutcome::Error);

        assert_eq!(registry.value(REQUESTS_TOTAL, "a"), 1.0);
        assert_eq!(registry.value(REQUEST_ERRORS_TOTAL, "a"), 0.0);
        assert_eq!(registry.value(REQUEST_ERRORS_TOTAL, "b"), 1.0);
    }

    #[test]
    fn the_two_cut_offs_are_counted_separately() {
        // They are different faults with different fixes -- one is a runaway
        // handler, the other is an app hammering the op layer -- so a single
        // "terminated" counter would not tell an operator which happened.
        let registry = MetricsRegistry::new();
        registry.record_rate_limit_cutoff("demo");
        registry.record_timeout_cutoff("demo");
        registry.record_timeout_cutoff("demo");

        assert_eq!(registry.value(RATE_LIMIT_CUTOFFS_TOTAL, "demo"), 1.0);
        assert_eq!(registry.value(TIMEOUT_CUTOFFS_TOTAL, "demo"), 2.0);
    }

    #[test]
    fn queue_numbers_carry_the_queue_they_belong_to() {
        let registry = MetricsRegistry::new();
        registry.record_queue_batch("demo", "jobs", 5, 2);
        registry.record_queue_batch("demo", "emails", 3, 0);

        let samples = registry.snapshot(1);
        let jobs = samples
            .iter()
            .find(|s| s.name == QUEUE_MESSAGES_TOTAL && s.labels.get("queue").unwrap() == "jobs")
            .expect("a sample for the jobs queue");
        assert_eq!(jobs.value, 5.0);
        assert_eq!(jobs.app_id, "demo");

        let failures: Vec<_> = samples
            .iter()
            .filter(|s| s.name == QUEUE_MESSAGE_FAILURES_TOTAL)
            .collect();
        assert_eq!(
            failures.len(),
            1,
            "a batch with no failures records no failure counter"
        );
        assert_eq!(failures[0].value, 2.0);
    }

    #[test]
    fn a_failed_poll_is_counted_so_a_silent_consumer_is_visible() {
        let registry = MetricsRegistry::new();
        registry.record_queue_poll_error("demo", "jobs");
        assert_eq!(registry.value(QUEUE_POLL_ERRORS_TOTAL, "demo"), 1.0);
    }

    #[test]
    fn a_snapshot_carries_the_application_and_the_timestamp() {
        let registry = MetricsRegistry::new();
        registry.record_request("demo", Duration::from_millis(1), RequestOutcome::Ok);

        let samples = registry.snapshot(1_700_000_000_000_000);
        let requests = sample(&samples, REQUESTS_TOTAL);
        assert_eq!(requests.app_id, "demo");
        assert_eq!(requests.timestamp_us, 1_700_000_000_000_000);
    }

    #[test]
    fn a_snapshot_does_not_reset_the_counters() {
        // They are cumulative: a push that fails must cost nothing, because
        // the next push carries the same totals.
        let registry = MetricsRegistry::new();
        registry.record_request("demo", Duration::from_millis(1), RequestOutcome::Ok);
        registry.snapshot(1);
        registry.snapshot(2);
        assert_eq!(registry.value(REQUESTS_TOTAL, "demo"), 1.0);
    }

    /// The defect, stated as a test: a `_total` that a consumer cannot tell a
    /// restart from a decrease in.
    ///
    /// A node's counters live in memory and start at zero, so a restart makes
    /// every one of them smaller than it was. deis-telemetry keeps the newest
    /// sample of each series and re-exports it, so what a consumer is handed
    /// is one number that went backwards with nothing to say why -- measured
    /// against a running node, `deis_runtime_requests_total` for one tenant
    /// read 116, the node was restarted, six requests were served, and the
    /// same series read 6. Every push now carries the moment this run began,
    /// so a smaller total that arrives with a later start time is a restart
    /// and a smaller total that arrives with the same one is a fault.
    #[test]
    fn every_push_says_which_run_the_totals_belong_to() {
        let first = MetricsRegistry::new();
        first.record_request("demo", Duration::from_millis(1), RequestOutcome::Ok);

        let started = |registry: &MetricsRegistry| {
            registry
                .snapshot(1)
                .into_iter()
                .find(|sample| sample.name == PROCESS_START_TIME_SECONDS)
                .map(|sample| {
                    assert!(
                        sample.app_id.is_empty(),
                        "the start time is a fact about the process, not about a tenant"
                    );
                    sample.value
                })
                .expect("every snapshot carries the moment this run began")
        };

        let run = started(&first);
        assert!(run > 0.0, "the start time has to be a real moment: {run}");
        assert_eq!(
            run,
            started(&first),
            "the start time may not move between two pushes of one run, or every push \
             would read as a restart"
        );

        // A second registry is a second process, which is what a restart is.
        // Its totals begin again at zero and its start time is a later one,
        // which is the whole of what a consumer needs to tell the two apart.
        let second = MetricsRegistry::new();
        assert_eq!(second.value(REQUESTS_TOTAL, "demo"), 0.0);
        assert!(
            started(&second) >= run,
            "a later run has to carry a start time no earlier than the one before it"
        );
    }

    /// A node that has counted nothing still says it is counting.
    ///
    /// The scrape that most needs the start time is the one taken just after a
    /// restart: the old run's totals are still inside deis-telemetry's export
    /// window, and until this node serves its first request there would
    /// otherwise be nothing at all to say the producer behind them is gone.
    #[test]
    fn an_empty_registry_still_reports_when_it_began() {
        let samples = MetricsRegistry::new().snapshot(1);
        assert_eq!(
            samples
                .iter()
                .map(|sample| sample.name.as_str())
                .collect::<Vec<_>>(),
            vec![PROCESS_START_TIME_SECONDS],
            "an idle node reports its start time and nothing else"
        );
    }

    #[test]
    fn samples_are_stamped_with_the_node_they_came_from() {
        // deis-telemetry has no node column, so without this label two nodes
        // running the same app produce samples nobody can tell apart.
        let registry = MetricsRegistry::new();
        registry.record_request("demo", Duration::from_millis(1), RequestOutcome::Ok);

        let stamped = stamp_node(registry.snapshot(1), "node-a");
        assert_eq!(
            stamped[0].labels.get("node").map(String::as_str),
            Some("node-a")
        );
    }

    #[test]
    fn the_process_registry_is_one_instance() {
        assert!(std::sync::Arc::ptr_eq(recorder(), recorder()));
    }

    #[test]
    fn shipping_follows_the_telemetry_address_the_logs_use() {
        let mut logging = LoggingConfig {
            enabled: true,
            endpoint: Some("http://127.0.0.1:8092".to_string()),
            ..LoggingConfig::default()
        };
        logging.flush_interval_secs = 5;

        let config = MetricShipperConfig::from(&logging);
        assert!(config.enabled);
        assert_eq!(config.endpoint.as_deref(), Some("http://127.0.0.1:8092"));
        assert!(
            config.interval_secs >= logging.flush_interval_secs,
            "metrics are totals, not events; they go out no more often than logs"
        );
    }

    #[tokio::test]
    async fn a_shipper_with_shipping_off_starts_nothing() {
        let shipper = MetricShipper::new(
            std::sync::Arc::new(MetricsRegistry::new()),
            MetricShipperConfig::default(),
            "node-a".to_string(),
        );
        shipper.start().await;
        // Nothing to stop, and shutting down an unstarted shipper is a no-op
        // rather than a panic.
        shipper.shutdown().await;
    }

    #[tokio::test]
    async fn a_shipper_with_an_unusable_address_starts_nothing() {
        let shipper = MetricShipper::new(
            std::sync::Arc::new(MetricsRegistry::new()),
            MetricShipperConfig {
                endpoint: Some("not a url".to_string()),
                enabled: true,
                interval_secs: 1,
                ..MetricShipperConfig::default()
            },
            "node-a".to_string(),
        );
        shipper.start().await;
        shipper.shutdown().await;
    }

    /// A sample is charged to the tenant, but Prometheus has to see the two
    /// halves apart: otherwise `demo.production` and `demo.preview` look like two
    /// unrelated applications and no query can add them up.
    #[test]
    fn a_shipped_sample_carries_the_environment_as_a_label() {
        let registry = MetricsRegistry::new();
        registry.record_request("demo.preview", Duration::from_millis(5), RequestOutcome::Ok);

        let samples = registry.snapshot(1);
        let requests = samples
            .iter()
            .find(|s| s.name == REQUESTS_TOTAL)
            .expect("requests counter");

        assert_eq!(requests.app_id, "demo");
        assert_eq!(
            requests
                .labels
                .iter()
                .find(|(k, _)| *k == "environment")
                .map(|(_, v)| v.as_str()),
            Some("preview")
        );
    }

    // ---- invocation traces -------------------------------------------

    fn traced(registry: &MetricsRegistry) -> Vec<InvocationTrace> {
        registry.traces().drain(usize::MAX)
    }

    #[test]
    fn a_request_leaves_a_trace_that_says_what_it_was_and_how_it_ended() {
        // The question section 4.6 says the platform cannot answer: which
        // request failed, and what it was aimed at.
        let registry = MetricsRegistry::new();
        registry.record_request_with(
            "demo.production",
            Duration::from_millis(9),
            RequestOutcome::Error,
            RequestFacts {
                trace_id: "abc".into(),
                method: "POST".into(),
                url: "https://demo.test/checkout".into(),
                status: 500,
                error: "TypeError: cannot read properties of undefined".into(),
                platform_fault: false,
                cpu_time: Some(Duration::from_micros(8_750)),
            },
        );

        let traces = traced(&registry);
        assert_eq!(traces.len(), 1);
        let trace = &traces[0];
        assert_eq!(trace.kind, InvocationKind::HttpRequest);
        assert_eq!(trace.outcome, InvocationOutcome::ApplicationError);
        assert_eq!(trace.target, "https://demo.test/checkout");
        assert_eq!(trace.method, "POST");
        assert_eq!(trace.status, 500);
        assert_eq!(trace.trace_id, "abc");
        assert!(trace.error.starts_with("TypeError"));
        assert_eq!(
            trace
                .attributes
                .get(CPU_TIME_US_ATTRIBUTE)
                .map(String::as_str),
            Some("8750")
        );
        assert!(trace.duration >= Duration::from_millis(9));
    }

    #[test]
    fn a_cut_off_invocation_does_not_get_blamed_on_the_application() {
        // The single most useful thing an operator can be told, and the one
        // thing only this node knows: a killed isolate answers 500 exactly
        // like a handler that threw.
        for (cut_off, expected) in [
            (
                &MetricsRegistry::record_timeout_cutoff as &dyn Fn(&MetricsRegistry, &str),
                InvocationOutcome::ExecutionTimeout,
            ),
            (
                &MetricsRegistry::record_rate_limit_cutoff,
                InvocationOutcome::RateLimited,
            ),
            (
                &MetricsRegistry::record_cpu_cutoff,
                InvocationOutcome::CpuLimit,
            ),
            (
                &MetricsRegistry::record_memory_cutoff,
                InvocationOutcome::MemoryLimit,
            ),
        ] {
            let registry = MetricsRegistry::new();
            // The cut-off fires on the watchdog thread first; the request
            // path finds out when its script returns an error.
            cut_off(&registry, "demo");
            registry.record_request("demo", Duration::from_secs(30), RequestOutcome::Error);

            let traces = traced(&registry);
            assert_eq!(traces[0].outcome, expected, "got {:?}", traces[0]);
        }
    }

    #[test]
    fn a_cut_off_is_charged_to_the_invocation_it_stopped_and_no_later_one() {
        // A cut-off can also fire during a `waitUntil` tail, after its own
        // request was recorded. Pinning that stale cause on whichever request
        // fails next names the wrong invocation, which is worse than naming
        // none.
        let registry = MetricsRegistry::new();
        registry.record_timeout_cutoff("demo");
        // A one-microsecond invocation cannot be the one a cut-off recorded
        // before it stopped.
        registry.record_request("demo", Duration::from_micros(1), RequestOutcome::Error);

        let traces = traced(&registry);
        assert_eq!(traces[0].outcome, InvocationOutcome::ApplicationError);
    }

    #[test]
    fn a_platform_fault_is_not_reported_as_the_application_failing() {
        let registry = MetricsRegistry::new();
        registry.record_request_with(
            "demo",
            Duration::from_millis(1),
            RequestOutcome::Error,
            RequestFacts {
                error: "Module evaluation failed".into(),
                platform_fault: true,
                ..RequestFacts::default()
            },
        );
        assert_eq!(
            traced(&registry)[0].outcome,
            InvocationOutcome::InternalError
        );
    }

    #[test]
    fn every_kind_of_invocation_can_be_traced() {
        // A queue batch, a cron tick and a WebSocket message are invocations
        // too; a surface that only covered requests would leave the other
        // three kinds as invisible as they were before.
        let registry = MetricsRegistry::new();
        for (kind, target) in [
            (InvocationKind::QueueBatch, "orders"),
            (InvocationKind::CronTick, "*/5 * * * *"),
            (InvocationKind::WebsocketMessage, "/socket"),
        ] {
            let mut trace = InvocationTrace::new("demo.production", kind);
            trace.outcome = InvocationOutcome::Ok;
            trace.target = target.to_string();
            trace.duration = Duration::from_millis(3);
            registry.record_invocation(trace);
        }

        let traces = traced(&registry);
        assert_eq!(traces.len(), 3);
        assert_eq!(traces[0].kind, InvocationKind::QueueBatch);
        assert_eq!(traces[1].target, "*/5 * * * *");
        assert_eq!(traces[2].kind, InvocationKind::WebsocketMessage);
    }

    #[test]
    fn a_trace_splits_the_tenant_and_carries_the_node() {
        // Same rule as a metric sample: `demo.production` and `demo.preview`
        // are one application, and two nodes serving it must stay apart.
        let registry = MetricsRegistry::new();
        registry.record_request("demo.preview", Duration::from_millis(1), RequestOutcome::Ok);

        let record = traced(&registry)[0].to_proto("node-a");
        assert_eq!(record.app_id, "demo");
        assert_eq!(record.environment, "preview");
        assert_eq!(record.node, "node-a");
        assert_eq!(record.kind, InvocationKind::HttpRequest as i32);
        assert_eq!(record.outcome, InvocationOutcome::Ok as i32);
    }

    #[test]
    fn sampling_keeps_every_failure_whatever_the_rate() {
        // An operator asking "what is failing" is asking about exactly these
        // records; a sampled node that threw them away would answer with
        // silence.
        let registry = MetricsRegistry::new();
        registry.traces().configure(1_000, 0.0);
        for _ in 0..50 {
            registry.record_request("demo", Duration::from_millis(1), RequestOutcome::Ok);
        }
        for _ in 0..5 {
            registry.record_timeout_cutoff("demo");
            registry.record_request("demo", Duration::from_secs(30), RequestOutcome::Error);
        }

        let traces = traced(&registry);
        assert_eq!(
            traces.iter().filter(|t| t.is_failure()).count(),
            5,
            "every failure survives the lowest rate on offer: {traces:?}"
        );
        // Counting off keeps the first of every N, so one successful
        // invocation is recorded too: a node that recorded literally nothing
        // would look indistinguishable from a node nobody is calling.
        assert_eq!(traces.len(), 6);
        assert_eq!(registry.value(TRACES_SAMPLED_OUT_TOTAL, "demo"), 49.0);
        assert_eq!(registry.value(TRACES_RECORDED_TOTAL, "demo"), 6.0);
    }

    #[test]
    fn a_sample_rate_records_one_successful_invocation_in_n() {
        let registry = MetricsRegistry::new();
        registry.traces().configure(1_000, 0.1);
        for _ in 0..100 {
            registry.record_request("demo", Duration::from_millis(1), RequestOutcome::Ok);
        }

        let traces = traced(&registry);
        assert_eq!(traces.len(), 10, "one in ten, counted off, not drawn");
        assert_eq!(registry.value(TRACES_SAMPLED_OUT_TOTAL, "demo"), 90.0);
    }

    #[test]
    fn a_rate_of_one_records_everything() {
        let registry = MetricsRegistry::new();
        registry.traces().configure(1_000, 1.0);
        for _ in 0..20 {
            registry.record_request("demo", Duration::from_millis(1), RequestOutcome::Ok);
        }
        assert_eq!(traced(&registry).len(), 20);
    }

    #[test]
    fn an_overrun_buffer_drops_records_and_says_how_many() {
        // The requirement the review is explicit about: losing records is
        // allowed, losing them silently is not.
        let registry = MetricsRegistry::new();
        registry.traces().configure(4, 1.0);
        for _ in 0..10 {
            registry.record_request("demo", Duration::from_millis(1), RequestOutcome::Ok);
        }

        assert_eq!(registry.traces().pending(), 4);
        assert_eq!(registry.value(TRACES_RECORDED_TOTAL, "demo"), 10.0);
        assert_eq!(registry.value(TRACES_DROPPED_TOTAL, "demo"), 6.0);
        let gaps = registry.traces().take_gaps();
        assert_eq!(gaps.dropped, 6);
        assert_eq!(gaps.sampled_out, 0);
        assert!(
            registry.traces().take_gaps().is_empty(),
            "the counts are deltas: what one push reported the next must not repeat"
        );
    }

    #[test]
    fn a_failed_push_puts_the_batch_and_the_gap_counts_back() {
        // An invocation happened whether or not deis-telemetry was
        // reachable; losing the record of it to a transient outage is the
        // failure this whole surface exists to prevent.
        let registry = MetricsRegistry::new();
        registry.traces().configure(10, 1.0);
        for _ in 0..3 {
            registry.record_request("demo", Duration::from_millis(1), RequestOutcome::Ok);
        }
        let gaps = registry.traces().take_gaps();
        let batch = registry.traces().drain(10);
        assert_eq!(batch.len(), 3);
        assert_eq!(registry.traces().pending(), 0);

        registry.traces().restore_gaps(gaps);
        assert_eq!(registry.traces().requeue(batch), 3);
        assert_eq!(registry.traces().pending(), 3);
    }

    #[test]
    fn a_requeue_that_no_longer_fits_is_counted_as_dropped() {
        let registry = MetricsRegistry::new();
        registry.traces().configure(2, 1.0);
        let batch = vec![
            InvocationTrace::new("demo", InvocationKind::HttpRequest),
            InvocationTrace::new("demo", InvocationKind::HttpRequest),
            InvocationTrace::new("demo", InvocationKind::HttpRequest),
        ];
        assert_eq!(registry.traces().requeue(batch), 2);
        assert_eq!(registry.traces().take_gaps().dropped, 1);
    }

    #[test]
    fn a_sample_rate_out_of_range_is_refused_rather_than_obeyed() {
        // A typo must not silently turn tracing off, and must not divide by
        // zero either.
        assert_eq!(keep_one_in(1.0), 1);
        assert_eq!(keep_one_in(2.0), 1);
        assert_eq!(keep_one_in(0.5), 2);
        assert_eq!(keep_one_in(0.01), 100);
        assert_eq!(keep_one_in(0.0), 1_000_000);
        assert_eq!(keep_one_in(-1.0), 1_000_000);
        assert_eq!(keep_one_in(f64::NAN), 1);
    }

    #[test]
    fn the_trace_recorder_is_reachable_from_the_process_registry() {
        // The cut-off paths reach the recorder the same way they reach the
        // counters; a second global would be a second thing to keep in step.
        assert!(std::sync::Arc::ptr_eq(
            recorder().traces(),
            recorder().traces()
        ));
    }

    #[test]
    fn a_sample_recorded_under_a_bare_application_id_gets_no_environment_label() {
        let registry = MetricsRegistry::new();
        registry.record_rate_limit_cutoff("legacy-app");

        let samples = registry.snapshot(1);
        let cutoffs = samples
            .iter()
            .find(|s| s.name == RATE_LIMIT_CUTOFFS_TOTAL)
            .expect("cutoff counter");

        assert_eq!(cutoffs.app_id, "legacy-app");
        assert!(cutoffs.labels.iter().all(|(k, _)| *k != "environment"));
    }
}
