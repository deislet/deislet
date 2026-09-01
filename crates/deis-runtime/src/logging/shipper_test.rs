#[cfg(test)]
mod tests {
    use crate::config::LoggingConfig;
    use crate::logging::buffer::LogBuffer;
    use crate::logging::shipper::{
        flush_once, micros_since_epoch, proto_level, run_shipper_loop, FlushReport, LogShipper,
        LogSink, PushOutcome, ShipperConfig, ShipperState,
    };
    use crate::logging::types::{LogEntry, LogLevel};
    use crate::telemetry::TelemetryCredential;
    use anyhow::Result;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::time::{Duration, UNIX_EPOCH};
    use tokio::sync::oneshot;

    use crate::logging::shipper::proto::LogLevel as ProtoLogLevel;
    use crate::logging::shipper::to_proto;

    /// A sink whose answers are scripted, so the retry and queueing rules can
    /// be exercised without a live deis-telemetry.
    #[derive(Default)]
    struct ScriptedSink {
        /// One flag per push, front first: `false` makes that push fail. Once
        /// the script runs out every further push succeeds.
        script: VecDeque<bool>,

        /// How many entries of each successful push the server "rejects".
        rejected_per_push: i32,

        /// Everything that was handed over, in order.
        pushes: Vec<Vec<LogEntry>>,
    }

    impl ScriptedSink {
        fn failing(times: usize) -> Self {
            Self {
                script: std::iter::repeat_n(false, times).collect(),
                ..Default::default()
            }
        }

        fn always_failing() -> Self {
            // Long enough that no test exhausts it.
            Self::failing(1_000)
        }

        fn shipped(&self) -> usize {
            self.pushes.iter().map(Vec::len).sum()
        }
    }

    impl LogSink for ScriptedSink {
        async fn push(&mut self, entries: &[LogEntry]) -> Result<PushOutcome> {
            let succeeds = self.script.pop_front().unwrap_or(true);
            if !succeeds {
                return Err(anyhow::anyhow!("deis-telemetry is unreachable"));
            }

            self.pushes.push(entries.to_vec());
            Ok(PushOutcome {
                accepted: entries.len() as i32 - self.rejected_per_push,
                rejected: self.rejected_per_push,
                message: if self.rejected_per_push > 0 {
                    "entry is malformed".to_string()
                } else {
                    String::new()
                },
            })
        }
    }

    fn test_config(batch_size: usize, max_pending_entries: usize) -> ShipperConfig {
        ShipperConfig {
            // Never dialled: every test drives a ScriptedSink directly.
            endpoint: Some("http://127.0.0.1:1".to_string()),
            credential: TelemetryCredential::default(),
            flush_interval_secs: 1,
            batch_size,
            max_retries: 1,
            enabled: true,
            max_pending_entries,
        }
    }

    fn fill(buffer: &LogBuffer, count: usize) {
        for i in 0..count {
            buffer.push(LogEntry::new(
                "node-1".to_string(),
                LogLevel::Info,
                format!("message {i}"),
            ));
        }
    }

    #[tokio::test]
    async fn a_successful_flush_hands_over_the_batch_and_empties_the_queue() {
        let buffer = LogBuffer::new(100);
        fill(&buffer, 5);

        let config = test_config(10, 100);
        let mut state = ShipperState::new(&buffer);
        let mut sink = ScriptedSink::default();

        let report = flush_once(&buffer, &config, &mut state, &mut sink).await;

        assert_eq!(report.drained, 5);
        assert_eq!(report.shipped, 5);
        assert_eq!(report.retained, 0);
        assert_eq!(sink.shipped(), 5);
        assert!(buffer.is_empty());
    }

    #[tokio::test]
    async fn a_failed_flush_keeps_every_entry_queued() {
        // The regression this guards: the shipper used to drain the buffer and
        // only then attempt the send, so a failed push destroyed the batch --
        // exactly the moment logs are worth the most. Nothing may be lost here.
        let buffer = LogBuffer::new(100);
        fill(&buffer, 5);

        let config = test_config(10, 100);
        let mut state = ShipperState::new(&buffer);
        let mut sink = ScriptedSink::always_failing();

        let report = flush_once(&buffer, &config, &mut state, &mut sink).await;

        assert_eq!(report.shipped, 0, "nothing was accepted");
        assert_eq!(report.retained, 5, "all five must still be queued");
        assert_eq!(state.pending.len(), 5);
        assert_eq!(sink.shipped(), 0);
    }

    #[tokio::test]
    async fn entries_held_through_an_outage_ship_once_it_ends() {
        let buffer = LogBuffer::new(100);
        fill(&buffer, 3);

        let config = test_config(10, 100);
        let mut state = ShipperState::new(&buffer);
        let mut sink = ScriptedSink::failing(1);

        flush_once(&buffer, &config, &mut state, &mut sink).await;
        assert_eq!(state.pending.len(), 3, "held over the failed flush");

        let report = flush_once(&buffer, &config, &mut state, &mut sink).await;
        assert_eq!(report.shipped, 3);
        assert!(state.pending.is_empty());

        let delivered: Vec<String> = sink.pushes[0]
            .iter()
            .map(|entry| entry.message.clone())
            .collect();
        assert_eq!(delivered, vec!["message 0", "message 1", "message 2"]);
    }

    #[tokio::test]
    async fn the_retry_queue_never_grows_past_its_ceiling() {
        // With telemetry down the queue must stop at its ceiling instead of
        // growing until the node runs out of memory.
        let buffer = LogBuffer::new(1_000);
        fill(&buffer, 100);

        let config = test_config(10, 25);
        let mut state = ShipperState::new(&buffer);
        let mut sink = ScriptedSink::always_failing();

        for _ in 0..20 {
            flush_once(&buffer, &config, &mut state, &mut sink).await;
            assert!(
                state.pending.len() <= config.max_pending_entries,
                "queue grew to {} past the {} ceiling",
                state.pending.len(),
                config.max_pending_entries
            );
        }

        assert_eq!(state.pending.len(), config.max_pending_entries);
    }

    #[tokio::test]
    async fn overflowed_buffer_entries_are_counted_as_lost() {
        // Once the queue is full the buffer starts discarding its oldest
        // entries. The flush must say how many, since nothing downstream can
        // tell those lines ever existed.
        let buffer = LogBuffer::new(10);
        let config = test_config(5, 5);
        let mut state = ShipperState::new(&buffer);
        let mut sink = ScriptedSink::always_failing();

        fill(&buffer, 5);
        flush_once(&buffer, &config, &mut state, &mut sink).await;
        assert_eq!(state.pending.len(), 5, "queue is now at its ceiling");

        // 20 more entries into an empty 10-entry buffer: the first 10 fill it
        // and each of the other 10 displaces an older one.
        fill(&buffer, 20);
        let report = flush_once(&buffer, &config, &mut state, &mut sink).await;

        assert_eq!(report.drained, 0, "the queue had no room");
        assert_eq!(report.dropped_by_buffer, 10);
    }

    #[tokio::test]
    async fn a_rejected_batch_is_dropped_rather_than_retried() {
        // A rejection is deterministic: the same entries would be rejected
        // again, so retrying them would wedge the queue forever.
        let buffer = LogBuffer::new(100);
        fill(&buffer, 4);

        let config = test_config(10, 100);
        let mut state = ShipperState::new(&buffer);
        let mut sink = ScriptedSink {
            rejected_per_push: 2,
            ..Default::default()
        };

        let report = flush_once(&buffer, &config, &mut state, &mut sink).await;

        assert_eq!(report.shipped, 4);
        assert!(state.pending.is_empty());
    }

    #[tokio::test]
    async fn a_flush_with_nothing_buffered_does_nothing() {
        let buffer = LogBuffer::new(100);
        let config = test_config(10, 100);
        let mut state = ShipperState::new(&buffer);
        let mut sink = ScriptedSink::always_failing();

        let report = flush_once(&buffer, &config, &mut state, &mut sink).await;

        assert_eq!(report, FlushReport::default());
        assert_eq!(sink.pushes.len(), 0);
    }

    #[tokio::test]
    async fn a_batch_larger_than_batch_size_goes_out_in_pieces() {
        let buffer = LogBuffer::new(100);
        fill(&buffer, 7);

        let config = test_config(3, 100);
        let mut state = ShipperState::new(&buffer);
        let mut sink = ScriptedSink::default();

        for _ in 0..3 {
            flush_once(&buffer, &config, &mut state, &mut sink).await;
        }

        assert_eq!(
            sink.pushes.iter().map(Vec::len).collect::<Vec<_>>(),
            [3, 3, 1]
        );
        assert!(state.pending.is_empty());
        assert!(buffer.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_batch_is_retried_up_to_max_retries_before_being_left_queued() {
        // start_paused lets the backoff between attempts pass instantly, so
        // this asserts the retry count without waiting six real seconds.
        let buffer = LogBuffer::new(100);
        fill(&buffer, 2);

        let mut config = test_config(10, 100);
        config.max_retries = 3;

        let mut state = ShipperState::new(&buffer);
        // Two failures then success: the third attempt of the same flush wins.
        let mut sink = ScriptedSink::failing(2);

        let report = flush_once(&buffer, &config, &mut state, &mut sink).await;

        assert_eq!(report.shipped, 2);
        assert_eq!(sink.pushes.len(), 1, "only the winning attempt delivered");
    }

    #[tokio::test]
    async fn shutdown_flushes_what_is_still_buffered() {
        let buffer = Arc::new(LogBuffer::new(100));
        fill(&buffer, 6);

        let config = test_config(4, 100);
        let mut sink = ScriptedSink::default();
        let (stop_tx, stop_rx) = oneshot::channel();

        // Ask it to stop before the first tick, so the only thing that can
        // deliver these entries is the final drain.
        stop_tx.send(()).expect("loop has not started yet");
        run_shipper_loop(buffer.clone(), config, &mut sink, stop_rx).await;

        assert_eq!(sink.shipped(), 6, "shutdown must not abandon buffered logs");
        assert!(buffer.is_empty());
    }

    #[tokio::test]
    async fn shutdown_gives_up_when_telemetry_cannot_be_reached() {
        let buffer = Arc::new(LogBuffer::new(100));
        fill(&buffer, 6);

        let config = test_config(4, 100);
        let mut sink = ScriptedSink::always_failing();
        let (stop_tx, stop_rx) = oneshot::channel();
        stop_tx.send(()).expect("loop has not started yet");

        // Must return rather than retry forever, or the node would never exit.
        run_shipper_loop(buffer.clone(), config, &mut sink, stop_rx).await;

        assert_eq!(sink.shipped(), 0);
    }

    #[test]
    fn to_proto_carries_every_field_across() {
        let entry = LogEntry::new(
            "node-1".to_string(),
            LogLevel::Warn,
            "disk is filling up".to_string(),
        )
        .with_app_id("shop".to_string())
        .with_field("attempt".to_string(), "3".to_string())
        .with_trace("trace-1".to_string(), "span-1".to_string());

        let proto = to_proto(&entry);

        assert_eq!(proto.component, "deis-runtime");
        assert_eq!(
            proto.fields.get("node").map(String::as_str),
            Some("node-1"),
            "the node id has no proto column, so it must survive as a field"
        );
        assert_eq!(proto.app_id, "shop");
        assert_eq!(proto.message, "disk is filling up");
        assert_eq!(proto.level, ProtoLogLevel::Warn as i32);
        assert_eq!(proto.fields.get("attempt").map(String::as_str), Some("3"));
        assert_eq!(proto.trace_id, "trace-1");
        assert_eq!(proto.span_id, "span-1");
        assert!(proto.timestamp_us > 0);
    }

    #[test]
    fn an_entry_without_optional_fields_maps_to_empty_strings() {
        let entry = LogEntry::new("node-1".to_string(), LogLevel::Info, "hello".to_string());
        let proto = to_proto(&entry);

        assert_eq!(proto.app_id, "");
        assert_eq!(proto.trace_id, "");
        assert_eq!(proto.span_id, "");
    }

    #[test]
    fn every_level_maps_onto_the_wire_enum() {
        let pairs = [
            (LogLevel::Trace, ProtoLogLevel::Trace),
            (LogLevel::Debug, ProtoLogLevel::Debug),
            (LogLevel::Info, ProtoLogLevel::Info),
            (LogLevel::Warn, ProtoLogLevel::Warn),
            (LogLevel::Error, ProtoLogLevel::Error),
            (LogLevel::Fatal, ProtoLogLevel::Fatal),
        ];

        for (level, expected) in pairs {
            assert_eq!(proto_level(level), expected, "{level:?}");
        }
    }

    #[test]
    fn a_timestamp_before_the_epoch_becomes_zero_instead_of_failing() {
        // One entry with a strange timestamp must not cost the whole batch.
        let before_epoch = UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(micros_since_epoch(before_epoch), 0);
    }

    #[test]
    fn shipper_config_follows_the_logging_config() {
        let logging = LoggingConfig {
            enabled: true,
            endpoint: Some("http://127.0.0.1:8092".to_string()),
            auth_secret: Some("shared-with-the-collector".to_string()),
            buffer_size: 1,
            flush_interval_secs: 9,
            batch_size: 8,
            max_retries: 7,
            max_pending_entries: 6,
        };
        let config = ShipperConfig::from(&logging);

        assert!(config.enabled);
        assert_eq!(config.endpoint.as_deref(), Some("http://127.0.0.1:8092"));
        assert_eq!(config.flush_interval_secs, 9);
        assert_eq!(config.batch_size, 8);
        assert_eq!(config.max_retries, 7);
        assert_eq!(config.max_pending_entries, 6);
        assert!(
            config.credential.is_present(),
            "a configured auth_secret has to become a token the shipper actually carries; \
             without it deis-telemetry refuses every batch and the node retries forever"
        );

        let unauthenticated = ShipperConfig::from(&LoggingConfig {
            auth_secret: None,
            ..logging
        });
        assert!(
            !unauthenticated.credential.is_present(),
            "and no secret has to mean no header at all: an empty one is presented and refused \
             where none is served by a collector that requires none"
        );
    }

    #[tokio::test]
    async fn a_shipper_without_an_endpoint_starts_nothing() {
        let buffer = Arc::new(LogBuffer::new(10));
        fill(&buffer, 3);

        let shipper = LogShipper::new(
            buffer.clone(),
            ShipperConfig {
                enabled: true,
                endpoint: None,
                ..ShipperConfig::default()
            },
        );
        shipper.start().await;
        shipper.shutdown().await;

        // The entries stay put: nothing pretended to have shipped them.
        assert_eq!(buffer.len(), 3);
    }
}
