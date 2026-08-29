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
    fn execution_time_accumulates_and_keeps_the_peak() {
        let registry = MetricsRegistry::new();
        registry.record_request("demo", Duration::from_millis(10), RequestOutcome::Ok);
        registry.record_request("demo", Duration::from_millis(40), RequestOutcome::Ok);
        registry.record_request("demo", Duration::from_millis(20), RequestOutcome::Ok);

        assert_eq!(registry.value(EXECUTION_MS_COUNT, "demo"), 3.0);
        assert!((registry.value(EXECUTION_MS_SUM, "demo") - 70.0).abs() < 3.0);
        assert!(
            (registry.value(EXECUTION_MS_MAX, "demo") - 40.0).abs() < 3.0,
            "the peak must not be overwritten by a later, shorter run"
        );
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

    #[test]
    fn an_empty_registry_produces_no_samples() {
        assert!(MetricsRegistry::new().snapshot(1).is_empty());
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
            },
            "node-a".to_string(),
        );
        shipper.start().await;
        shipper.shutdown().await;
    }
}
