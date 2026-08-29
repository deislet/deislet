//! Runtime metrics, and their delivery to deis-telemetry.
//!
//! deis-telemetry has carried a `MetricService` since wave A and nothing ever
//! called it: the node shipped logs and not one number, so "how many requests
//! did this app serve, how many failed, how long did the isolate run" could
//! only be answered by reading log lines. This module records those numbers
//! and pushes them.
//!
//! # What is recorded
//!
//! Everything here is per application, because that is the dimension
//! deis-telemetry stores and every query filters on. The node dimension is
//! added when the samples are pushed (see [`MetricShipper`]), so the registry
//! itself does not need to know which node it is on.
//!
//! Counters are cumulative for the life of the process, the way a Prometheus
//! counter is: each push carries the current total rather than a delta, so a
//! push that fails costs nothing -- the next one carries the same total. The
//! consumer takes differences.
//!
//! # Why a process-wide registry
//!
//! Two of the numbers are recorded from places that have no handle to pass
//! anything down: the rate-limit cut-off happens inside V8's op-metrics
//! callback, and the timeout cut-off inside a detached timer task. Threading
//! a recorder into both would mean widening `IsolateSpec` and every
//! constructor between here and `main`. A registry reached by
//! [`recorder()`] is the standard shape for exactly this, and the recording
//! functions all take `&MetricsRegistry` so tests can use their own instance
//! instead of the global one.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::time::interval;
use tonic::transport::{Channel, Endpoint};
use tracing::{debug, error, info, warn};

use crate::config::LoggingConfig;

/// Generated client for deis-telemetry's `MetricService` (package
/// `deis.telemetry`).
///
/// The log shipper compiles the same file into its own module. Sharing one
/// copy would mean making that private module public, which is another
/// track's file; two copies of generated code cost a little binary size and
/// nothing else.
pub mod proto {
    tonic::include_proto!("deis.telemetry");
}

use proto::metric_service_client::MetricServiceClient;
use proto::{MetricSample, PushMetricsRequest};

/// Requests handed to an isolate, whatever the outcome.
pub const REQUESTS_TOTAL: &str = "deis_runtime_requests_total";
/// Requests that did not produce a successful response: the isolate failed,
/// was terminated, or the app answered 5xx.
pub const REQUEST_ERRORS_TOTAL: &str = "deis_runtime_request_errors_total";
/// Total milliseconds spent inside isolates.
pub const EXECUTION_MS_SUM: &str = "deis_runtime_isolate_execution_ms_sum";
/// Number of isolate executions the sum is over.
pub const EXECUTION_MS_COUNT: &str = "deis_runtime_isolate_execution_ms_count";
/// Longest single isolate execution since this node started.
pub const EXECUTION_MS_MAX: &str = "deis_runtime_isolate_execution_ms_max";
/// Executions cut off because the app exceeded its op dispatch rate limit.
pub const RATE_LIMIT_CUTOFFS_TOTAL: &str = "deis_runtime_rate_limit_cutoffs_total";
/// Executions cut off because the app exceeded its wall-clock timeout.
pub const TIMEOUT_CUTOFFS_TOTAL: &str = "deis_runtime_timeout_cutoffs_total";
/// Queue messages handed to an app's queue handler.
pub const QUEUE_MESSAGES_TOTAL: &str = "deis_runtime_queue_messages_total";
/// Queue messages the handler did not complete, and which were handed back.
pub const QUEUE_MESSAGE_FAILURES_TOTAL: &str = "deis_runtime_queue_message_failures_total";
/// Failed polls of deis-queue: the queue was unreachable or refused the call.
pub const QUEUE_POLL_ERRORS_TOTAL: &str = "deis_runtime_queue_poll_errors_total";

/// Identity of one counter: its name, the app it belongs to, and any extra
/// dimensions (only the queue name, today).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct SampleKey {
    name: &'static str,
    app_id: String,
    labels: BTreeMap<String, String>,
}

impl SampleKey {
    fn new(name: &'static str, app_id: &str) -> Self {
        Self {
            name,
            app_id: app_id.to_string(),
            labels: BTreeMap::new(),
        }
    }

    fn with_label(mut self, key: &str, value: &str) -> Self {
        self.labels.insert(key.to_string(), value.to_string());
        self
    }
}

/// How one request ended, as far as the metrics are concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestOutcome {
    /// The isolate produced a response the caller can serve.
    Ok,
    /// The isolate failed, was terminated, or answered 5xx.
    Error,
}

/// Process-wide counters, aggregated per application.
#[derive(Debug, Default)]
pub struct MetricsRegistry {
    counters: Mutex<HashMap<SampleKey, f64>>,
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn add(&self, key: SampleKey, delta: f64) {
        let mut counters = self.counters.lock().unwrap_or_else(|e| e.into_inner());
        *counters.entry(key).or_insert(0.0) += delta;
    }

    fn raise(&self, key: SampleKey, value: f64) {
        let mut counters = self.counters.lock().unwrap_or_else(|e| e.into_inner());
        let slot = counters.entry(key).or_insert(0.0);
        if value > *slot {
            *slot = value;
        }
    }

    /// One isolate execution of a request: its outcome and how long the
    /// isolate ran.
    pub fn record_request(&self, app_id: &str, duration: Duration, outcome: RequestOutcome) {
        let millis = duration.as_secs_f64() * 1000.0;
        self.add(SampleKey::new(REQUESTS_TOTAL, app_id), 1.0);
        if outcome == RequestOutcome::Error {
            self.add(SampleKey::new(REQUEST_ERRORS_TOTAL, app_id), 1.0);
        }
        self.add(SampleKey::new(EXECUTION_MS_SUM, app_id), millis);
        self.add(SampleKey::new(EXECUTION_MS_COUNT, app_id), 1.0);
        self.raise(SampleKey::new(EXECUTION_MS_MAX, app_id), millis);
    }

    /// An execution terminated for exceeding the app's op dispatch rate
    /// limit. The request it belonged to fails.
    pub fn record_rate_limit_cutoff(&self, app_id: &str) {
        self.add(SampleKey::new(RATE_LIMIT_CUTOFFS_TOTAL, app_id), 1.0);
    }

    /// An execution terminated for exceeding the app's wall-clock timeout.
    /// The request it belonged to fails.
    pub fn record_timeout_cutoff(&self, app_id: &str) {
        self.add(SampleKey::new(TIMEOUT_CUTOFFS_TOTAL, app_id), 1.0);
    }

    /// One batch handed to an app's queue handler.
    pub fn record_queue_batch(&self, app_id: &str, queue: &str, delivered: u64, failed: u64) {
        self.add(
            SampleKey::new(QUEUE_MESSAGES_TOTAL, app_id).with_label("queue", queue),
            delivered as f64,
        );
        if failed > 0 {
            self.add(
                SampleKey::new(QUEUE_MESSAGE_FAILURES_TOTAL, app_id).with_label("queue", queue),
                failed as f64,
            );
        }
    }

    /// A poll of deis-queue that did not come back.
    pub fn record_queue_poll_error(&self, app_id: &str, queue: &str) {
        self.add(
            SampleKey::new(QUEUE_POLL_ERRORS_TOTAL, app_id).with_label("queue", queue),
            1.0,
        );
    }

    /// Current value of one counter for one application, summed across label
    /// sets (so a queue counter is the total over every queue).
    pub fn value(&self, name: &str, app_id: &str) -> f64 {
        let counters = self.counters.lock().unwrap_or_else(|e| e.into_inner());
        counters
            .iter()
            .filter(|(key, _)| key.name == name && key.app_id == app_id)
            .map(|(_, value)| *value)
            .sum()
    }

    /// Everything recorded so far, as samples stamped with `timestamp_us`.
    ///
    /// Nothing is reset: the values are cumulative, so a push that fails
    /// loses nothing.
    pub fn snapshot(&self, timestamp_us: i64) -> Vec<MetricSample> {
        let counters = self.counters.lock().unwrap_or_else(|e| e.into_inner());
        let mut keys: Vec<&SampleKey> = counters.keys().collect();
        keys.sort();
        keys.into_iter()
            .map(|key| MetricSample {
                name: key.name.to_string(),
                app_id: key.app_id.clone(),
                labels: key.labels.clone().into_iter().collect(),
                value: counters[key],
                timestamp_us,
            })
            .collect()
    }

    /// How many distinct counters are being kept.
    pub fn len(&self) -> usize {
        self.counters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The registry every isolate and every consumer in this process records into.
pub fn recorder() -> &'static Arc<MetricsRegistry> {
    static REGISTRY: OnceLock<Arc<MetricsRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Arc::new(MetricsRegistry::new()))
}

/// Where and how often metrics are pushed.
///
/// There is no `[metrics]` section in the node's TOML yet: the destination is
/// the same deis-telemetry instance the logs go to, so the address and the
/// on/off switch are taken from `[logging]`. Splitting them into their own
/// section is a change to `config.rs`, which this track does not own.
#[derive(Debug, Clone)]
pub struct MetricShipperConfig {
    /// deis-telemetry address, e.g. `http://127.0.0.1:8092`.
    pub endpoint: Option<String>,
    /// Whether to push at all.
    pub enabled: bool,
    /// Seconds between pushes.
    pub interval_secs: u64,
}

impl Default for MetricShipperConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            enabled: false,
            interval_secs: 15,
        }
    }
}

impl From<&LoggingConfig> for MetricShipperConfig {
    fn from(config: &LoggingConfig) -> Self {
        Self {
            endpoint: config.endpoint.clone(),
            enabled: config.enabled,
            // Metrics are cumulative totals, not events, so they are pushed on
            // their own slower cadence than logs: a missed push costs nothing
            // and the next one carries the same numbers.
            interval_secs: (config.flush_interval_secs * 3).max(10),
        }
    }
}

/// How long one dial to deis-telemetry may take before the push is treated as
/// failed and retried on the next tick.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one `PushMetrics` call may take, so a wedged telemetry node
/// cannot stall the shipping task indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

struct RunningShipper {
    stop: oneshot::Sender<()>,
    finished: oneshot::Receiver<()>,
}

/// Background task that pushes [`MetricsRegistry`] contents to
/// deis-telemetry.
pub struct MetricShipper {
    registry: Arc<MetricsRegistry>,
    config: MetricShipperConfig,
    node_id: String,
    running: AsyncMutex<Option<RunningShipper>>,
}

impl MetricShipper {
    pub fn new(
        registry: Arc<MetricsRegistry>,
        config: MetricShipperConfig,
        node_id: String,
    ) -> Self {
        Self {
            registry,
            config,
            node_id,
            running: AsyncMutex::new(None),
        }
    }

    /// Ship the process-wide registry.
    pub fn for_process(config: MetricShipperConfig, node_id: String) -> Self {
        Self::new(recorder().clone(), config, node_id)
    }

    /// Spawn the pushing task.
    ///
    /// Does nothing when metrics are off or no address is configured; in both
    /// cases the numbers are still recorded, they just stay in this process.
    pub async fn start(&self) {
        if !self.config.enabled {
            info!(
                "Metric shipping is disabled; this node's request, error and execution-time \
                 counters stay in memory and no dashboard can see them"
            );
            return;
        }

        let Some(endpoint) = self.config.endpoint.clone() else {
            error!(
                "Metric shipping is enabled but no endpoint is configured; no metric will reach \
                 deis-telemetry. Set logging.endpoint to the telemetry address."
            );
            return;
        };

        let mut running = self.running.lock().await;
        if running.is_some() {
            warn!("Metric shipper is already running; ignoring the second start");
            return;
        }

        let channel = match Endpoint::from_shared(endpoint.clone())
            .map(|e| e.connect_timeout(CONNECT_TIMEOUT).timeout(REQUEST_TIMEOUT))
        {
            Ok(builder) => builder.connect_lazy(),
            Err(e) => {
                error!(
                    %endpoint,
                    error = %e,
                    "Metric endpoint is not a usable address; no metric will reach deis-telemetry"
                );
                return;
            }
        };

        let (stop_tx, stop_rx) = oneshot::channel();
        let (finished_tx, finished_rx) = oneshot::channel();

        info!(
            %endpoint,
            every_secs = self.config.interval_secs,
            "Metric shipper started; this node's counters will be queryable through deis-telemetry"
        );

        let registry = self.registry.clone();
        let interval_secs = self.config.interval_secs;
        let node_id = self.node_id.clone();
        tokio::spawn(async move {
            let mut client = MetricServiceClient::new(channel)
                .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
                .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES);
            let mut ticker = interval(Duration::from_secs(interval_secs));
            let mut stop = stop_rx;
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        push_once(&registry, &node_id, &mut client).await;
                    }
                    _ = &mut stop => break,
                }
            }
            // One last push so the final minute of a node's life is not
            // missing from the dashboard.
            push_once(&registry, &node_id, &mut client).await;
            let _ = finished_tx.send(());
        });

        *running = Some(RunningShipper {
            stop: stop_tx,
            finished: finished_rx,
        });
    }

    /// Stop the pushing task after one final push.
    pub async fn shutdown(&self) {
        let Some(handle) = self.running.lock().await.take() else {
            return;
        };
        let _ = handle.stop.send(());
        if handle.finished.await.is_err() {
            warn!(
                "Metric shipper task ended without reporting; the last counters may not have been \
                 pushed"
            );
        }
    }
}

/// Stamp every sample with the node it came from.
///
/// The proto has no node column -- deis-telemetry knows applications, not
/// nodes -- so the node travels as a label. Without it, two nodes running the
/// same app produce samples that cannot be told apart.
fn stamp_node(mut samples: Vec<MetricSample>, node_id: &str) -> Vec<MetricSample> {
    for sample in &mut samples {
        sample
            .labels
            .insert("node".to_string(), node_id.to_string());
    }
    samples
}

fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

async fn push_once(
    registry: &MetricsRegistry,
    node_id: &str,
    client: &mut MetricServiceClient<Channel>,
) {
    let samples = stamp_node(registry.snapshot(now_us()), node_id);
    if samples.is_empty() {
        return;
    }
    let count = samples.len();

    match client
        .push_metrics(PushMetricsRequest {
            samples,
            metadata: Default::default(),
        })
        .await
    {
        Ok(response) => {
            let response = response.into_inner();
            if response.rejected_count > 0 {
                warn!(
                    accepted = response.accepted_count,
                    rejected = response.rejected_count,
                    error = %response.error_message,
                    "deis-telemetry rejected some metric samples; those counters will be missing \
                     from queries until the next push carries them again"
                );
            } else {
                debug!(count, "Pushed metric samples to deis-telemetry");
            }
        }
        Err(status) => warn!(
            count,
            error = %status,
            "Pushing metrics to deis-telemetry failed; the counters are cumulative so nothing is \
             lost, but dashboards will show a gap until the next push succeeds"
        ),
    }
}

#[cfg(test)]
#[path = "metrics_test.rs"]
mod metrics_test;
