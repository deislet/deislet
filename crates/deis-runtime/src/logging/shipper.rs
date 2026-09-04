//! Ships this node's application logs to deis-telemetry.
//!
//! The in-process [`LogBuffer`] collects every `target: "app"` event. This task
//! wakes on a fixed interval, takes a batch out of the buffer, and pushes it to
//! deis-telemetry's `LogService.PushLogs`.
//!
//! # What happens when telemetry is down
//!
//! A batch is only removed from the retry queue once deis-telemetry has
//! answered. A failed push leaves the batch queued and it goes out again on the
//! next tick, so an outage delays logs instead of destroying them. The queue is
//! bounded by [`ShipperConfig::max_pending_entries`]: once it is full the buffer
//! stops being drained, the buffer itself starts dropping its oldest entries to
//! make room for new ones, and every flush logs at warn how many were lost. The
//! node keeps serving traffic throughout -- log delivery is never allowed to
//! become a reason requests fail.

use anyhow::{Context, Result};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{oneshot, Mutex};
use tokio::time::{interval, sleep};
use tonic::transport::{Channel, Endpoint};
use tracing::{debug, error, info, warn};

use super::buffer::LogBuffer;
use super::types::{LogEntry, LogLevel};
use crate::config::LoggingConfig;
use crate::telemetry::{report_posture, TelemetryCredential};

/// Generated client for deis-telemetry's `LogService` (package
/// `deis.telemetry`).
pub mod proto {
    tonic::include_proto!("deis.telemetry");
}

use proto::log_service_client::LogServiceClient;
use proto::{LogEntry as ProtoLogEntry, LogLevel as ProtoLogLevel, PushLogsRequest};

/// How long one dial to deis-telemetry may take before it is treated as a
/// failure and retried.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one `PushLogs` call may take. Without this a hung telemetry node
/// would stall the shipper indefinitely and the buffer would silently fill.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling on the exponential backoff between retries of one batch, so a long
/// outage does not push the next attempt minutes away.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Value of the proto's `component` field for everything this crate ships.
const COMPONENT: &str = "deis-runtime";

/// Structured field carrying the node id, which the proto has no column for.
const NODE_FIELD: &str = "node";

/// Structured field carrying the environment half of a canonical tenant.
///
/// Most entries have already passed through `logging::layer`, which splits
/// `shop.preview` into `app_id = shop` and this field. `LogEntry` is public,
/// though, and direct callers can put the tenant into `app_id`. Normalising
/// again at the wire boundary makes both paths obey the collector contract.
const ENVIRONMENT_FIELD: &str = "environment";

/// Log shipper configuration.
///
/// Built from [`LoggingConfig`]; see `crate::config` for what each value means
/// and which values are refused at startup.
#[derive(Debug, Clone)]
pub struct ShipperConfig {
    /// deis-telemetry `LogService` address. `None` disables shipping just as
    /// `enabled = false` does, and is refused at startup when `enabled` is on.
    pub endpoint: Option<String>,

    /// The capability token this node presents on every push, derived from
    /// `logging.auth_secret`. Absent is correct against a collector that has
    /// no `auth_secret`, and fatal to log delivery against one that has; see
    /// [`crate::telemetry`].
    pub credential: TelemetryCredential,

    /// Seconds between flush attempts.
    pub flush_interval_secs: u64,

    /// Maximum entries sent in one `PushLogs` call.
    pub batch_size: usize,

    /// Attempts per batch before it is left queued for the next flush.
    pub max_retries: u32,

    /// Whether shipping runs at all.
    pub enabled: bool,

    /// Ceiling on the retry queue.
    pub max_pending_entries: usize,
}

impl Default for ShipperConfig {
    fn default() -> Self {
        Self::from(&LoggingConfig::default())
    }
}

impl From<&LoggingConfig> for ShipperConfig {
    fn from(config: &LoggingConfig) -> Self {
        Self {
            endpoint: config.endpoint.clone(),
            credential: TelemetryCredential::from_secret(config.auth_secret.as_deref()),
            flush_interval_secs: config.flush_interval_secs,
            batch_size: config.batch_size,
            max_retries: config.max_retries,
            enabled: config.enabled,
            max_pending_entries: config.max_pending_entries,
        }
    }
}

/// Handle on a running shipper task, used to drain and stop it.
struct RunningShipper {
    stop: oneshot::Sender<()>,
    finished: oneshot::Receiver<()>,
}

/// Background task that moves buffered logs to deis-telemetry.
pub struct LogShipper {
    buffer: Arc<LogBuffer>,
    config: ShipperConfig,
    running: Mutex<Option<RunningShipper>>,
}

impl LogShipper {
    /// Create a shipper. Nothing is sent until [`LogShipper::start`] is called.
    pub fn new(buffer: Arc<LogBuffer>, config: ShipperConfig) -> Self {
        Self {
            buffer,
            config,
            running: Mutex::new(None),
        }
    }

    /// Spawn the shipping task.
    ///
    /// Returns without spawning anything when shipping is off, or when it is on
    /// but has no endpoint to send to. The second case is a misconfiguration
    /// that `RuntimeConfig::validate` already refuses at startup; the check here
    /// exists so that a shipper built directly in code cannot end up silently
    /// pretending to ship.
    pub async fn start(&self) {
        if !self.config.enabled {
            info!(
                "Log shipping is disabled; this node's app logs stay in its own output and are \
                 not queryable through deis-telemetry"
            );
            return;
        }

        let Some(endpoint) = self.config.endpoint.clone() else {
            error!(
                "Log shipping is enabled but no endpoint is configured; no log will reach \
                 deis-telemetry. Set logging.endpoint to the LogService address."
            );
            return;
        };

        let mut running = self.running.lock().await;
        if running.is_some() {
            warn!("Log shipper is already running; ignoring the second start");
            return;
        }

        let (stop_tx, stop_rx) = oneshot::channel();
        let (finished_tx, finished_rx) = oneshot::channel();

        info!(
            %endpoint,
            every_secs = self.config.flush_interval_secs,
            batch_size = self.config.batch_size,
            "Log shipper started; app logs from this node will be queryable through deis-telemetry"
        );
        report_posture(&self.config.credential, &endpoint);

        let buffer = self.buffer.clone();
        let config = self.config.clone();
        tokio::spawn(async move {
            let mut sink = TelemetrySink::new(endpoint, config.credential.clone());
            run_shipper_loop(buffer, config, &mut sink, stop_rx).await;
            let _ = finished_tx.send(());
        });

        *running = Some(RunningShipper {
            stop: stop_tx,
            finished: finished_rx,
        });
    }

    /// Stop the shipping task, giving it one last chance to hand over what is
    /// still buffered.
    ///
    /// Returns once the task has finished. Anything it could not deliver is
    /// reported by the task itself before it exits.
    pub async fn shutdown(&self) {
        let Some(handle) = self.running.lock().await.take() else {
            return;
        };

        let _ = handle.stop.send(());
        if handle.finished.await.is_err() {
            warn!("Log shipper task ended without reporting; buffered logs may not have been sent");
        }
    }
}

/// Everything carried from one flush to the next.
struct ShipperState {
    /// Entries taken out of the buffer but not yet accepted by deis-telemetry.
    pending: VecDeque<LogEntry>,

    /// The buffer's drop counter as of the previous flush, so each warning can
    /// state how many entries were lost since the last one rather than a
    /// running total.
    dropped_at_last_flush: u64,
}

impl ShipperState {
    fn new(buffer: &LogBuffer) -> Self {
        let (_, dropped, _) = buffer.stats();
        Self {
            pending: VecDeque::new(),
            dropped_at_last_flush: dropped,
        }
    }
}

/// What one flush did, for tests and for the shutdown summary.
#[derive(Debug, Default, PartialEq)]
struct FlushReport {
    /// Entries moved out of the buffer into the retry queue.
    drained: usize,

    /// Entries deis-telemetry answered for and that were dropped from the
    /// queue as a result.
    shipped: usize,

    /// Entries still queued when the flush returned.
    retained: usize,

    /// Entries the buffer discarded since the previous flush because it was
    /// full.
    dropped_by_buffer: u64,
}

async fn run_shipper_loop<S: LogSink>(
    buffer: Arc<LogBuffer>,
    config: ShipperConfig,
    sink: &mut S,
    mut stop: oneshot::Receiver<()>,
) {
    let mut ticker = interval(Duration::from_secs(config.flush_interval_secs));
    let mut state = ShipperState::new(&buffer);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                flush_once(&buffer, &config, &mut state, sink).await;
            }
            _ = &mut stop => {
                break;
            }
        }
    }

    // Final drain: keep handing over batches while they are being accepted.
    // A failed batch ends the drain rather than blocking shutdown on an
    // unreachable telemetry node.
    while flush_once(&buffer, &config, &mut state, sink).await.shipped > 0 {}

    let left = state.pending.len() + buffer.len();
    if left > 0 {
        warn!(
            entries = left,
            "Log shipper stopped with entries it could not deliver; they existed only in memory \
             and are lost. They are still in this node's own output."
        );
    }

    info!("Log shipper stopped");
}

/// Move one batch from the buffer towards deis-telemetry.
///
/// The batch stays in `state.pending` until deis-telemetry has answered for it.
/// Draining first and sending afterwards -- the shape this used to have --
/// destroys the batch on any failure, which is precisely when logs matter most.
async fn flush_once<S: LogSink>(
    buffer: &LogBuffer,
    config: &ShipperConfig,
    state: &mut ShipperState,
    sink: &mut S,
) -> FlushReport {
    let mut report = FlushReport::default();

    // Top up the retry queue, never past its ceiling.
    let room = config
        .max_pending_entries
        .saturating_sub(state.pending.len());
    if room == 0 {
        warn!(
            queued = state.pending.len(),
            ceiling = config.max_pending_entries,
            "Log shipper retry queue is full; the buffer is no longer being drained and its \
             oldest entries are being discarded to make room for new ones"
        );
    } else {
        let drained = buffer.drain(config.batch_size.min(room));
        report.drained = drained.len();
        state.pending.extend(drained);
    }

    // The buffer discards its oldest entries once full. Report the loss with a
    // number, since nothing downstream can tell that those lines ever existed.
    let (_, dropped_total, _) = buffer.stats();
    report.dropped_by_buffer = dropped_total.saturating_sub(state.dropped_at_last_flush);
    state.dropped_at_last_flush = dropped_total;
    if report.dropped_by_buffer > 0 {
        warn!(
            lost = report.dropped_by_buffer,
            queued = state.pending.len(),
            "Log buffer overflowed since the last flush; that many of the oldest app log entries \
             were discarded and will never reach deis-telemetry"
        );
    }

    if state.pending.is_empty() {
        return report;
    }

    let batch_len = config.batch_size.min(state.pending.len());
    let result = {
        let batch = &state.pending.make_contiguous()[..batch_len];
        ship_with_retry(batch, config, sink).await
    };

    match result {
        Ok(outcome) => {
            state.pending.drain(..batch_len);
            report.shipped = batch_len;

            if outcome.rejected > 0 {
                warn!(
                    rejected = outcome.rejected,
                    accepted = outcome.accepted,
                    reason = %outcome.message,
                    "deis-telemetry rejected part of a log batch; those entries are discarded \
                     rather than retried, since a rejection would repeat"
                );
            } else {
                debug!(shipped = batch_len, "Shipped a log batch to deis-telemetry");
            }
        }
        Err(e) => {
            warn!(
                queued = state.pending.len(),
                ceiling = config.max_pending_entries,
                error = %e,
                "Could not ship a log batch to deis-telemetry; it stays queued and goes out on \
                 the next flush. Nothing is lost until the queue reaches its ceiling."
            );
        }
    }

    report.retained = state.pending.len();
    report
}

/// Push one batch, retrying with exponential backoff.
async fn ship_with_retry<S: LogSink>(
    entries: &[LogEntry],
    config: &ShipperConfig,
    sink: &mut S,
) -> Result<PushOutcome> {
    let attempts = config.max_retries.max(1);
    let mut last_error = None;

    for attempt in 1..=attempts {
        match sink.push(entries).await {
            Ok(outcome) => return Ok(outcome),
            Err(e) => {
                debug!(
                    attempt,
                    attempts,
                    error = %e,
                    "Log batch push failed; the batch is kept for another attempt"
                );
                last_error = Some(e);

                if attempt < attempts {
                    let backoff =
                        Duration::from_secs(2u64.saturating_pow(attempt)).min(MAX_BACKOFF);
                    sleep(backoff).await;
                }
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| anyhow::anyhow!("log batch push failed without reporting an error"))
        .context(format!("after {attempts} attempts")))
}

/// What deis-telemetry answered for one batch.
#[derive(Debug, Clone, Default, PartialEq)]
struct PushOutcome {
    accepted: i32,
    rejected: i32,
    message: String,
}

/// Somewhere a batch of log entries can be handed to.
///
/// Only [`TelemetrySink`] implements this in production; the indirection exists
/// so the buffering and retry rules can be tested against a sink that fails on
/// demand, without a live deis-telemetry.
trait LogSink {
    async fn push(&mut self, entries: &[LogEntry]) -> Result<PushOutcome>;
}

/// A `LogService` client that redials whenever the connection lets it down.
struct TelemetrySink {
    endpoint: String,
    /// Stamped on every push. Held here rather than derived per call because
    /// the derivation is an HMAC and the value never changes for the life of
    /// the process.
    credential: TelemetryCredential,
    client: Option<LogServiceClient<Channel>>,
}

impl TelemetrySink {
    fn new(endpoint: String, credential: TelemetryCredential) -> Self {
        Self {
            endpoint,
            credential,
            client: None,
        }
    }

    /// Return a connected client, dialling first if there is none.
    ///
    /// The client is dropped on every failure (see [`LogSink::push`]), so this
    /// is also the reconnect path: deis-telemetry restarting costs one failed
    /// batch, not a shipper that never recovers.
    async fn client(&mut self) -> Result<&mut LogServiceClient<Channel>> {
        if self.client.is_none() {
            let channel = Endpoint::from_shared(self.endpoint.clone())
                .with_context(|| format!("{} is not a valid gRPC endpoint", self.endpoint))?
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .connect()
                .await
                .with_context(|| format!("connecting to deis-telemetry at {}", self.endpoint))?;

            info!(endpoint = %self.endpoint, "Connected to deis-telemetry log service");
            self.client = Some(
                LogServiceClient::new(channel)
                    .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
                    .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES),
            );
        }

        Ok(self
            .client
            .as_mut()
            .expect("client was just connected above"))
    }
}

impl LogSink for TelemetrySink {
    async fn push(&mut self, entries: &[LogEntry]) -> Result<PushOutcome> {
        let request = PushLogsRequest {
            entries: entries.iter().map(to_proto).collect(),
            metadata: HashMap::new(),
        };

        let result = {
            let credential = self.credential.clone();
            let client = self.client().await?;
            // The token travels in the request's gRPC metadata, not in the
            // message's own `metadata` map: deis-telemetry checks it in an
            // interceptor, before the handler that would read the message.
            client.push_logs(credential.stamp(request)).await
        };

        match result {
            Ok(response) => {
                let response = response.into_inner();
                Ok(PushOutcome {
                    accepted: response.accepted_count,
                    rejected: response.rejected_count,
                    message: response.error_message,
                })
            }
            Err(status) => {
                // Drop the channel rather than reuse one the server may have
                // closed; the next attempt dials again.
                self.client = None;
                if status.code() == tonic::Code::Unauthenticated {
                    // Its own line, at error, because it is the one failure
                    // here that no amount of retrying fixes and that looks
                    // exactly like a transient outage from the outside: the
                    // collector is up and answering, and every batch this node
                    // ever produces will be refused until an operator changes
                    // a setting.
                    error!(
                        endpoint = %self.endpoint,
                        carrying_a_token = self.credential.is_present(),
                        "deis-telemetry refused this node's logs: {}. This is a credential \
                         mismatch and not an outage -- retrying cannot fix it, and until it is \
                         fixed no line from any application on this node is queryable. Set \
                         logging.auth_secret to the same value as deis-telemetry's auth_secret",
                        status.message()
                    );
                }
                Err(anyhow::anyhow!(
                    "PushLogs to {} failed: {} ({})",
                    self.endpoint,
                    status.message(),
                    status.code()
                ))
            }
        }
    }
}

/// Map one buffered entry onto the wire type.
///
/// `LogEntry.source` holds this node's id, but the proto's `component` field is
/// deliberately not a node -- deis-telemetry has no node dimension, only
/// component and application. So the component is the constant below and the
/// node travels as a structured field, where a query can still filter on it.
fn to_proto(entry: &LogEntry) -> ProtoLogEntry {
    let mut fields = entry.fields.clone();
    fields
        .entry(NODE_FIELD.to_string())
        .or_insert_with(|| entry.source.clone());

    let app_id =
        entry.app_id.as_deref().map(
            |app_id| match deis_wire::conventions::split_tenant(app_id) {
                Some((application, environment)) => {
                    fields
                        .entry(ENVIRONMENT_FIELD.to_string())
                        .or_insert_with(|| environment.to_string());
                    application.to_string()
                }
                None => app_id.to_string(),
            },
        );

    ProtoLogEntry {
        timestamp_us: micros_since_epoch(entry.timestamp),
        level: proto_level(entry.level) as i32,
        component: COMPONENT.to_string(),
        app_id: app_id.unwrap_or_default(),
        message: entry.message.clone(),
        fields,
        trace_id: entry.trace_id.clone().unwrap_or_default(),
        span_id: entry.span_id.clone().unwrap_or_default(),
    }
}

/// Microseconds since the Unix epoch.
///
/// A timestamp before the epoch (a clock stepped backwards during boot, say)
/// becomes 0 rather than failing the whole batch: one entry with an odd
/// timestamp is a much smaller problem than a batch that can never be shipped.
fn micros_since_epoch(timestamp: SystemTime) -> i64 {
    timestamp
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_micros()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Translate to the wire enum rather than reusing a hand-written number, so
/// the mapping cannot drift away from the proto.
fn proto_level(level: LogLevel) -> ProtoLogLevel {
    match level {
        LogLevel::Trace => ProtoLogLevel::Trace,
        LogLevel::Debug => ProtoLogLevel::Debug,
        LogLevel::Info => ProtoLogLevel::Info,
        LogLevel::Warn => ProtoLogLevel::Warn,
        LogLevel::Error => ProtoLogLevel::Error,
        LogLevel::Fatal => ProtoLogLevel::Fatal,
    }
}

#[cfg(test)]
#[path = "shipper_test.rs"]
mod shipper_test;
