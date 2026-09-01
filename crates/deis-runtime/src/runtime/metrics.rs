//! Runtime metrics and per-invocation traces, and their delivery to
//! deis-telemetry.
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
//! # Counters, and the histogram
//!
//! The counters are cumulative for the life of the process, the way a
//! Prometheus counter is: each push carries the current total rather than a
//! delta, so a push that fails costs nothing -- the next one carries the same
//! total, and the consumer takes differences.
//!
//! Latency is a histogram: [`EXECUTION_MS_BUCKET`] carries one cumulative
//! counter per bucket, next to the [`EXECUTION_MS_SUM`] and
//! [`EXECUTION_MS_COUNT`] the histogram is completed by. It replaced a
//! high-water gauge that only ever rose and never decayed, which meant one
//! 30-second timeout pinned the number at 30 seconds for the rest of the
//! process's life and no percentile could be computed from it at all. Buckets
//! give `histogram_quantile` something to work with, and a slow minute stops
//! mattering once it is out of the range being asked about.
//!
//! # Traces
//!
//! A counter says how many invocations failed. It cannot say *which*. That is
//! [`TraceRecorder`]: one [`InvocationTrace`] per invocation, carrying what it
//! was aimed at, how long it ran, what the client got and - the part only the
//! runtime knows - whether the application failed or the platform stopped it.
//! Traces go to deis-telemetry's `TraceService` on the same push task as the
//! samples, on a faster cadence, because a trace is an event and a counter is
//! a total.
//!
//! Traces are higher volume than logs by construction, so the recorder is
//! bounded and samples. Both of those lose invocations, and neither loss is
//! silent: what sampling skipped and what the buffer dropped are counted, sent
//! on every push, and exported by deis-telemetry.
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

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::time::interval;
use tonic::transport::{Channel, Endpoint};
use tracing::{debug, error, info, warn};

use crate::config::LoggingConfig;
use crate::telemetry::{report_posture, TelemetryCredential};

/// Generated clients for deis-telemetry's `MetricService` and `TraceService`
/// (package `deis.telemetry`).
///
/// The log shipper compiles the same file into its own module. Sharing one
/// copy would mean making that private module public, which is another
/// track's file; two copies of generated code cost a little binary size and
/// nothing else.
pub mod proto {
    tonic::include_proto!("deis.telemetry");
}

use proto::metric_service_client::MetricServiceClient;
use proto::trace_service_client::TraceServiceClient;
use proto::{MetricSample, PushMetricsRequest, PushTracesRequest, TraceRecord};

// The invocation vocabulary is spelled once, in the proto, and used from
// there by both ends. A hand-written copy on this side is exactly the drift
// `deis_wire::conventions` exists to prevent.
pub use proto::{InvocationKind, InvocationOutcome};

/// Requests handed to an isolate, whatever the outcome.
pub const REQUESTS_TOTAL: &str = "deis_runtime_requests_total";
/// Requests that did not produce a successful response: the isolate failed,
/// was terminated, or the app answered 5xx.
pub const REQUEST_ERRORS_TOTAL: &str = "deis_runtime_request_errors_total";
/// Total milliseconds spent inside isolates.
pub const EXECUTION_MS_SUM: &str = "deis_runtime_isolate_execution_ms_sum";
/// Number of isolate executions the sum is over.
pub const EXECUTION_MS_COUNT: &str = "deis_runtime_isolate_execution_ms_count";
/// Cumulative bucket counts of isolate execution time.
///
/// Carries an `le` label naming the bucket's upper bound in milliseconds, and
/// together with [`EXECUTION_MS_SUM`] and [`EXECUTION_MS_COUNT`] forms the
/// `deis_runtime_isolate_execution_ms` histogram. Each bucket counts every
/// execution that finished at or under its bound, which is what
/// `histogram_quantile` needs to read a p50, a p95 or a p99 out of it.
pub const EXECUTION_MS_BUCKET: &str = "deis_runtime_isolate_execution_ms_bucket";

/// The `le` label naming a bucket's upper bound.
const BUCKET_BOUND_LABEL: &str = "le";

/// The bound of the bucket that catches everything, spelled the way the
/// exposition format spells it.
const BUCKET_INFINITY: &str = "+Inf";

/// Bucket bounds in milliseconds, and the exact text each one is labelled
/// with. The text is fixed rather than formatted so a bound can never be
/// spelled two ways across pushes, which would split one bucket into two
/// series and make every quantile wrong.
///
/// Chosen for this workload rather than taken from a library default. A
/// handler that only reads its request and answers lands in single-digit
/// milliseconds, which is why the bottom is dense: 1, 2, 5, 10 is where the
/// difference between a healthy node and a struggling one actually shows.
/// Anything doing real work (a KV read, a database query, one outbound fetch)
/// lands between 25 and 250ms, so that range is where the useful percentiles
/// live and it gets four bounds. Above a second an application is in trouble
/// whatever the exact number, so the top is coarse. The last finite bound is
/// 30000ms because that is `default_execution_limits.timeout_ms`: an
/// execution cannot legitimately exceed it, so a count above that bound means
/// an application running with a raised budget, which is worth being able to
/// see.
const EXECUTION_MS_BUCKETS: [(f64, &str); 14] = [
    (1.0, "1"),
    (2.0, "2"),
    (5.0, "5"),
    (10.0, "10"),
    (25.0, "25"),
    (50.0, "50"),
    (100.0, "100"),
    (250.0, "250"),
    (500.0, "500"),
    (1000.0, "1000"),
    (2500.0, "2500"),
    (5000.0, "5000"),
    (10000.0, "10000"),
    (30000.0, "30000"),
];
/// Executions cut off because the app exceeded its op dispatch rate limit.
pub const RATE_LIMIT_CUTOFFS_TOTAL: &str = "deis_runtime_rate_limit_cutoffs_total";
/// Executions cut off because the app exceeded its wall-clock timeout.
pub const TIMEOUT_CUTOFFS_TOTAL: &str = "deis_runtime_timeout_cutoffs_total";
/// Total CPU milliseconds this app's invocations spent on a runtime thread.
///
/// The answer to "which application is burning this node", which
/// [`EXECUTION_MS_SUM`] cannot give: that one counts wall clock, so an app
/// awaiting a slow upstream and an app spinning on a core look the same in it
/// while costing amounts that differ by orders of magnitude. Charged on every
/// path out of an invocation, cut off or not, so the ranking is right for
/// applications nowhere near their ceiling.
pub const CPU_MS_SUM: &str = "deis_runtime_isolate_cpu_ms_sum";
/// Executions cut off because the app exceeded its CPU-time ceiling.
///
/// Deliberately not the same counter as [`TIMEOUT_CUTOFFS_TOTAL`]: they are
/// different faults with different remedies. A duration cut-off usually points
/// downstream of the app, a CPU cut-off always points at the app itself.
pub const CPU_CUTOFFS_TOTAL: &str = "deis_runtime_cpu_cutoffs_total";
/// Queue messages handed to an app's queue handler.
pub const QUEUE_MESSAGES_TOTAL: &str = "deis_runtime_queue_messages_total";
/// Queue messages the handler did not complete, and which were handed back.
pub const QUEUE_MESSAGE_FAILURES_TOTAL: &str = "deis_runtime_queue_message_failures_total";
/// Failed polls of deis-queue: the queue was unreachable or refused the call.
pub const QUEUE_POLL_ERRORS_TOTAL: &str = "deis_runtime_queue_poll_errors_total";
/// Executions cut off because the isolate reached its V8 heap ceiling.
pub const MEMORY_CUTOFFS_TOTAL: &str = "deis_runtime_memory_cutoffs_total";
/// Invocations this node built a trace record for.
pub const TRACES_RECORDED_TOTAL: &str = "deis_runtime_traces_recorded_total";
/// Invocations sampling deliberately did not record. Policy, not loss.
pub const TRACES_SAMPLED_OUT_TOTAL: &str = "deis_runtime_traces_sampled_out_total";
/// Trace records built and then discarded because the node's own buffer was
/// full. Unlike [`TRACES_SAMPLED_OUT_TOTAL`] this is a fault: either
/// deis-telemetry is unreachable or the buffer is too small for the traffic.
pub const TRACES_DROPPED_TOTAL: &str = "deis_runtime_traces_dropped_total";

/// Label carrying the environment half of a tenant on a shipped sample.
const ENVIRONMENT_LABEL: &str = "environment";

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

/// Process-wide samples, aggregated per application, and the trace recorder
/// that sits beside them.
///
/// The samples are counters and one histogram; the field is not called
/// `counters` because [`EXECUTION_MS_BUCKET`] is neither one number nor a
/// counter of events but a bucket of a distribution.
#[derive(Debug, Default)]
pub struct MetricsRegistry {
    samples: Mutex<HashMap<SampleKey, f64>>,
    /// Why the platform stopped an application's current invocation, kept
    /// between the cut-off and the request path that reports the failure.
    ///
    /// At most one entry per tenant, which is the same order as `samples`:
    /// a new cut-off for an application replaces its previous one, and an
    /// entry nobody claims is overwritten rather than accumulating.
    cut_offs: Mutex<HashMap<String, PendingCutOff>>,
    traces: Arc<TraceRecorder>,
}

/// A cut-off that has fired and not yet been attributed to an invocation.
///
/// The two ends are on different threads: a duration or CPU cut-off fires on
/// the watchdog thread and a rate-limit cut-off inside V8's op-metrics
/// callback, while the invocation itself only finds out that it was killed
/// when its `execute_script` returns an error a moment later. Without this
/// the trace for that invocation would say "the application threw", which is
/// the one thing about it that is not true.
#[derive(Debug, Clone, Copy)]
struct PendingCutOff {
    cause: InvocationOutcome,
    at: Instant,
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The trace recorder these metrics are kept beside.
    ///
    /// Same lifetime and same reach as the registry: whoever can record a
    /// counter can record the invocation it belongs to.
    pub fn traces(&self) -> &Arc<TraceRecorder> {
        &self.traces
    }

    /// Add to a counter.
    fn add(&self, key: SampleKey, delta: f64) {
        let mut samples = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        *samples.entry(key).or_insert(0.0) += delta;
    }

    /// Record one observation into the execution-time histogram.
    ///
    /// Every bucket is touched, not only the ones the observation falls in:
    /// a bucket that exists but has never been hit must still be exported as
    /// zero, because a bucket that appears out of nowhere partway through a
    /// day makes `rate()` over it read as a burst that never happened.
    fn observe_execution(&self, app_id: &str, millis: f64) {
        for (bound, label) in EXECUTION_MS_BUCKETS {
            let delta = if millis <= bound { 1.0 } else { 0.0 };
            self.add(
                SampleKey::new(EXECUTION_MS_BUCKET, app_id).with_label(BUCKET_BOUND_LABEL, label),
                delta,
            );
        }
        self.add(
            SampleKey::new(EXECUTION_MS_BUCKET, app_id)
                .with_label(BUCKET_BOUND_LABEL, BUCKET_INFINITY),
            1.0,
        );
        self.add(SampleKey::new(EXECUTION_MS_SUM, app_id), millis);
        self.add(SampleKey::new(EXECUTION_MS_COUNT, app_id), 1.0);
    }

    /// Remember that the platform stopped this application's invocation, and
    /// why. Taken back out by the next [`Self::record_request`] on the same
    /// application; see [`PendingCutOff`].
    fn note_cut_off(&self, app_id: &str, cause: InvocationOutcome) {
        let mut cut_offs = self.cut_offs.lock().unwrap_or_else(|e| e.into_inner());
        cut_offs.insert(
            app_id.to_string(),
            PendingCutOff {
                cause,
                at: Instant::now(),
            },
        );
    }

    /// Take the cut-off that stopped an invocation of `duration` ending now,
    /// if there was one.
    ///
    /// The window matters: a cut-off can also fire during a `waitUntil` tail,
    /// after the request it belonged to has already been recorded. Without
    /// the check that the cut-off happened inside this invocation's own span,
    /// that stale cause would be pinned on whichever request failed next --
    /// naming the wrong invocation is worse than naming none.
    ///
    /// Public because a queue batch and a cron tick need the same answer as a
    /// request and do not go through [`Self::record_request_with`]: a handler
    /// that was cut off and one that threw both come back as an error, and
    /// only the cut-off the platform noted says which. Taking it is the point
    /// -- one cut-off explains one invocation, and leaving it behind would let
    /// it explain the next one too.
    pub fn take_cut_off(&self, app_id: &str, duration: Duration) -> Option<InvocationOutcome> {
        let mut cut_offs = self.cut_offs.lock().unwrap_or_else(|e| e.into_inner());
        let pending = *cut_offs.get(app_id)?;
        if pending.at.elapsed() > duration {
            return None;
        }
        cut_offs.remove(app_id);
        Some(pending.cause)
    }

    /// One isolate execution of a request: its outcome and how long the
    /// isolate ran.
    ///
    /// Records a trace for the invocation as well, with the URL and status
    /// left empty because this signature cannot see them. A caller that has
    /// them uses [`Self::record_request_with`], which is the same call plus
    /// the facts an operator actually wants to read.
    pub fn record_request(&self, app_id: &str, duration: Duration, outcome: RequestOutcome) {
        self.record_request_with(app_id, duration, outcome, RequestFacts::default());
    }

    /// One isolate execution of a request, with what only the invocation path
    /// can see: the method and URL, the status the client was given, the
    /// uncaught exception, and the id tying the invocation to its log lines.
    pub fn record_request_with(
        &self,
        app_id: &str,
        duration: Duration,
        outcome: RequestOutcome,
        facts: RequestFacts,
    ) {
        let millis = duration.as_secs_f64() * 1000.0;
        self.add(SampleKey::new(REQUESTS_TOTAL, app_id), 1.0);
        if outcome == RequestOutcome::Error {
            self.add(SampleKey::new(REQUEST_ERRORS_TOTAL, app_id), 1.0);
        }
        self.observe_execution(app_id, millis);

        // The verdict, which is the whole point of the record: a cut-off the
        // platform decided on beats any inference from the status, because an
        // isolate killed mid-flight answers 500 exactly like a handler that
        // threw and only this node can tell the two apart.
        let verdict = match outcome {
            RequestOutcome::Ok => InvocationOutcome::Ok,
            RequestOutcome::Error => {
                self.take_cut_off(app_id, duration)
                    .unwrap_or(if facts.platform_fault {
                        InvocationOutcome::InternalError
                    } else {
                        InvocationOutcome::ApplicationError
                    })
            }
        };

        let mut trace = InvocationTrace::new(app_id, InvocationKind::HttpRequest);
        trace.outcome = verdict;
        // Back-dated from the end, because that is where the invocation path
        // records. Floored at the epoch so a clock that has not been set
        // cannot produce a negative start, which deis-telemetry refuses.
        trace.started_us = (now_us() - (duration.as_micros().min(i64::MAX as u128) as i64)).max(0);
        trace.duration = duration;
        trace.trace_id = facts.trace_id;
        trace.method = facts.method;
        trace.target = facts.url;
        trace.status = facts.status;
        trace.error = facts.error;
        self.record_invocation(trace);
    }

    /// Record one invocation that is not an HTTP request: a queue batch, a
    /// cron tick, a WebSocket message.
    ///
    /// The trace only. The counters for those kinds live on the calls that
    /// already keep them ([`Self::record_queue_batch`] and its neighbours),
    /// so that nothing is counted twice by a caller that does both.
    pub fn record_invocation(&self, trace: InvocationTrace) {
        let app_id = trace.tenant.clone();
        match self.traces.record(trace) {
            TraceDisposition::Recorded => {
                self.add(SampleKey::new(TRACES_RECORDED_TOTAL, &app_id), 1.0)
            }
            TraceDisposition::SampledOut => {
                self.add(SampleKey::new(TRACES_SAMPLED_OUT_TOTAL, &app_id), 1.0)
            }
            TraceDisposition::RecordedEvicting { evicted } => {
                self.add(SampleKey::new(TRACES_RECORDED_TOTAL, &app_id), 1.0);
                // Charged to whoever lost the record, which on a shared node
                // is often not the application that filled the buffer.
                self.add(SampleKey::new(TRACES_DROPPED_TOTAL, &evicted), 1.0);
            }
        }
    }

    /// An execution terminated for exceeding the app's op dispatch rate
    /// limit. The request it belonged to fails.
    pub fn record_rate_limit_cutoff(&self, app_id: &str) {
        self.add(SampleKey::new(RATE_LIMIT_CUTOFFS_TOTAL, app_id), 1.0);
        self.note_cut_off(app_id, InvocationOutcome::RateLimited);
    }

    /// An execution terminated for exceeding the app's wall-clock timeout.
    /// The request it belonged to fails.
    pub fn record_timeout_cutoff(&self, app_id: &str) {
        self.add(SampleKey::new(TIMEOUT_CUTOFFS_TOTAL, app_id), 1.0);
        self.note_cut_off(app_id, InvocationOutcome::ExecutionTimeout);
    }

    /// An execution terminated because the isolate reached its heap ceiling.
    ///
    /// The V8 near-heap-limit callback is the only place that knows this
    /// happened; from the request path an out-of-memory kill is
    /// indistinguishable from a handler that threw.
    pub fn record_memory_cutoff(&self, app_id: &str) {
        self.add(SampleKey::new(MEMORY_CUTOFFS_TOTAL, app_id), 1.0);
        self.note_cut_off(app_id, InvocationOutcome::MemoryLimit);
    }

    /// CPU time one stretch of an app's code spent on a runtime thread.
    ///
    /// Recorded when the stretch ends, whatever ended it. See [`CPU_MS_SUM`].
    pub fn record_cpu_time(&self, app_id: &str, used: Duration) {
        self.add(
            SampleKey::new(CPU_MS_SUM, app_id),
            used.as_secs_f64() * 1000.0,
        );
    }

    /// An execution terminated for exceeding the app's CPU-time ceiling. The
    /// milliseconds it spent are counted by [`Self::record_cpu_time`] on the
    /// same path; this counts only the crossings.
    pub fn record_cpu_cutoff(&self, app_id: &str) {
        self.add(SampleKey::new(CPU_CUTOFFS_TOTAL, app_id), 1.0);
        self.note_cut_off(app_id, InvocationOutcome::CpuLimit);
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

    /// Current value of one sample for one application, summed across label
    /// sets (so a queue counter is the total over every queue).
    ///
    /// Summing is right for every counter here. It is not right for
    /// [`EXECUTION_MS_BUCKET`], whose label sets are the buckets of one
    /// distribution: adding them produces a number that means nothing. Read a
    /// bucket out of [`Self::snapshot`] instead.
    pub fn value(&self, name: &str, app_id: &str) -> f64 {
        let samples = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        samples
            .iter()
            .filter(|(key, _)| key.name == name && key.app_id == app_id)
            .map(|(_, value)| *value)
            .sum()
    }

    /// Everything recorded so far, as samples stamped with `timestamp_us`.
    ///
    /// Nothing is reset: the counters and the histogram buckets are all
    /// cumulative, so a push that fails loses nothing -- the next one carries
    /// the same totals and the consumer takes differences.
    pub fn snapshot(&self, timestamp_us: i64) -> Vec<MetricSample> {
        let samples = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        let mut keys: Vec<&SampleKey> = samples.keys().collect();
        keys.sort();
        keys.into_iter()
            .map(|key| {
                // The key here is the tenant, because that is what a request
                // is charged to. On the wire the two halves separate: a
                // Prometheus series is `{app_id="demo",environment="preview"}`,
                // so `sum by (app_id)` covers an application's environments
                // and a query can still single one out. Sending the joined
                // string would make every environment its own application and
                // there would be no way to add them up.
                let (app_id, environment) = match deis_wire::conventions::split_tenant(&key.app_id)
                {
                    Some((app, environment)) => (app.to_string(), Some(environment)),
                    None => (key.app_id.clone(), None),
                };
                let mut labels: BTreeMap<String, String> = key.labels.clone();
                if let Some(environment) = environment {
                    labels
                        .entry(ENVIRONMENT_LABEL.to_string())
                        .or_insert_with(|| environment.to_string());
                }
                MetricSample {
                    name: key.name.to_string(),
                    app_id,
                    labels: labels.into_iter().collect(),
                    value: samples[key],
                    timestamp_us,
                }
            })
            .collect()
    }

    /// How many distinct samples are being kept.
    pub fn len(&self) -> usize {
        self.samples.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Facts about a request that the counter call cannot see for itself.
///
/// Everything is optional. `RequestFacts::default()` is "the caller did not
/// supply them", which is what [`MetricsRegistry::record_request`] passes: the
/// invocation is still traced, with a URL nobody filled in.
#[derive(Debug, Clone, Default)]
pub struct RequestFacts {
    /// The id the invocation's log lines carry, so one query joins the trace
    /// to the lines the handler wrote. Empty when the caller has none.
    pub trace_id: String,
    pub method: String,
    /// The request URL, as the client sent it.
    pub url: String,
    /// The status the client was given; 0 when the invocation never produced
    /// one.
    pub status: u16,
    /// The uncaught exception, or whatever the platform is about to answer
    /// with. Empty when the invocation ended cleanly.
    pub error: String,
    /// True when the platform failed around the application rather than the
    /// application failing: the bundle would not load, the handler answered
    /// with something unparseable. An operator reading a trace has to be able
    /// to tell "your code is broken" from "our node is".
    pub platform_fault: bool,
}

/// One invocation of one application, as an operator needs to see it.
///
/// Built at the end of an invocation, whatever kind it was and however it
/// ended, and handed to [`MetricsRegistry::record_invocation`].
#[derive(Debug, Clone)]
pub struct InvocationTrace {
    /// The id the invocation's log lines carry. Empty when there is none.
    pub trace_id: String,
    /// The tenant the invocation is charged to, e.g. `demo.production`. The
    /// two halves are separated on the wire, the same way a metric sample's
    /// are, so a query for an application answers with every environment of
    /// it.
    pub tenant: String,
    pub kind: InvocationKind,
    pub outcome: InvocationOutcome,
    /// When it started, in microseconds since the Unix epoch.
    pub started_us: i64,
    pub duration: Duration,
    /// What it was aimed at: the request URL, the queue name, the cron
    /// expression, the WebSocket path.
    pub target: String,
    /// HTTP method; empty for every other kind.
    pub method: String,
    /// HTTP status; 0 when the invocation has none.
    pub status: u16,
    /// The uncaught exception, or why the platform stopped it.
    pub error: String,
    /// Extra dimensions: a queue batch's message counts, a retry number.
    pub attributes: BTreeMap<String, String>,
}

impl InvocationTrace {
    /// An invocation of `tenant` that started now and has not ended yet.
    ///
    /// The caller fills in the outcome, the duration and whatever else its
    /// kind of invocation has, then hands it over. Kept as public fields
    /// rather than a builder because every kind fills in a different subset
    /// and a builder would only be a longer way to write the same struct.
    pub fn new(tenant: &str, kind: InvocationKind) -> Self {
        Self {
            trace_id: String::new(),
            tenant: tenant.to_string(),
            kind,
            outcome: InvocationOutcome::Unspecified,
            started_us: now_us(),
            duration: Duration::ZERO,
            target: String::new(),
            method: String::new(),
            status: 0,
            error: String::new(),
            attributes: BTreeMap::new(),
        }
    }

    /// True when the platform, not the application, ended the invocation.
    ///
    /// Sampling never discards one of these: an operator asking "what is
    /// failing" is asking about exactly these records, and a node under a
    /// sample rate that threw them away would answer the question with
    /// silence.
    pub fn is_failure(&self) -> bool {
        !matches!(
            self.outcome,
            InvocationOutcome::Ok | InvocationOutcome::Unspecified
        )
    }

    /// Convert to the wire record, splitting the tenant and stamping the node.
    fn to_proto(&self, node_id: &str) -> TraceRecord {
        let (app_id, environment) = match deis_wire::conventions::split_tenant(&self.tenant) {
            Some((app, environment)) => (app.to_string(), environment.to_string()),
            None => (self.tenant.clone(), String::new()),
        };
        TraceRecord {
            trace_id: self.trace_id.clone(),
            started_us: self.started_us,
            duration_us: self.duration.as_micros().min(i64::MAX as u128) as i64,
            app_id,
            environment,
            node: node_id.to_string(),
            kind: self.kind as i32,
            outcome: self.outcome as i32,
            target: self.target.clone(),
            method: self.method.clone(),
            status: i32::from(self.status),
            error: self.error.clone(),
            attributes: self
                .attributes
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }
}

/// What became of one invocation offered to the recorder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceDisposition {
    /// Buffered, and it will go out on the next push.
    Recorded,
    /// Sampling did not select it. Policy, not loss.
    SampledOut,
    /// Buffered, but the buffer was already full, so the oldest record in it
    /// was discarded to make room. `evicted` is the tenant that lost one,
    /// which is not necessarily the tenant of the record just accepted.
    RecordedEvicting { evicted: String },
}

/// How many invocations the recorder let go of since the last push, and why.
///
/// Sent with every push so the gap in a node's trace history is a number
/// deis-telemetry exports rather than an absence nobody can see.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TraceGaps {
    pub sampled_out: u64,
    pub dropped: u64,
}

impl TraceGaps {
    pub fn is_empty(&self) -> bool {
        self.sampled_out == 0 && self.dropped == 0
    }
}

/// Default ceiling on buffered traces, and the default number of records one
/// push carries.
///
/// Ten thousand records is a few megabytes at the size caps deis-telemetry
/// enforces, which is the same order as the log buffer's ceiling and small
/// enough that a telemetry outage costs old traces rather than the node.
const DEFAULT_TRACE_CAPACITY: usize = 10_000;

/// Records per `PushTraces` call. Below the gRPC message ceiling with room to
/// spare even when every record carries a long URL and a stack trace.
const DEFAULT_TRACE_BATCH: usize = 500;

/// Collects one record per invocation, bounded and sampled.
///
/// The two ways an invocation is lost here are deliberately different things
/// and are counted apart. *Sampled out* is policy: the node was told to record
/// one invocation in N and this was not the one. *Dropped* is a fault: the
/// record was built and then thrown away because nothing had drained the
/// buffer, which means either deis-telemetry is unreachable or the buffer is
/// too small for this node's traffic. An operator needs to know which.
#[derive(Debug)]
pub struct TraceRecorder {
    pending: Mutex<VecDeque<InvocationTrace>>,
    /// Ceiling on `pending`.
    capacity: AtomicUsize,
    /// Of every `keep_one_in` successful invocations, one is recorded. 1
    /// records all of them; failures ignore this entirely.
    keep_one_in: AtomicU64,
    /// Successful invocations seen, which is what the sampler counts off.
    seen: AtomicU64,
    sampled_out: AtomicU64,
    dropped: AtomicU64,
}

impl Default for TraceRecorder {
    fn default() -> Self {
        Self {
            pending: Mutex::new(VecDeque::new()),
            capacity: AtomicUsize::new(DEFAULT_TRACE_CAPACITY),
            // Everything, until an operator says otherwise: a node that
            // silently sampled by default would answer "which requests
            // failed" with a fraction of the truth and never say so.
            keep_one_in: AtomicU64::new(1),
            seen: AtomicU64::new(0),
            sampled_out: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }
}

impl TraceRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply the operator's settings.
    ///
    /// Called once at startup, before the node serves anything, because the
    /// process-wide recorder exists before the configuration is read.
    /// `sample_rate` is the fraction of *successful* invocations to record;
    /// failures are always recorded whatever it says.
    pub fn configure(&self, capacity: usize, sample_rate: f64) {
        let capacity = capacity.max(1);
        self.capacity.store(capacity, Ordering::Relaxed);
        let keep_one_in = keep_one_in(sample_rate);
        self.keep_one_in.store(keep_one_in, Ordering::Relaxed);
        if keep_one_in > 1 {
            info!(
                keep_one_in,
                capacity,
                "Trace sampling is on: one successful invocation in {keep_one_in} is recorded, \
                 and every failed one. What sampling skips is counted and reported to \
                 deis-telemetry, so the gap is visible rather than silent",
            );
        } else {
            info!(
                capacity,
                "Every invocation on this node is traced; set DEIS_TRACE_SAMPLE_RATE below 1.0 \
                 to record only a fraction of the successful ones"
            );
        }
    }

    /// Offer one invocation to the recorder.
    pub fn record(&self, trace: InvocationTrace) -> TraceDisposition {
        if !self.selects(&trace) {
            self.sampled_out.fetch_add(1, Ordering::Relaxed);
            return TraceDisposition::SampledOut;
        }
        let capacity = self.capacity.load(Ordering::Relaxed);
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        // The oldest goes rather than the newest. When traces back up it is
        // because nothing is draining them, and what an operator will want to
        // look at afterwards is what happened during the incident, not what
        // happened in the quiet hour before it started.
        let evicted = if pending.len() >= capacity {
            let lost = pending.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
            lost.map(|lost| lost.tenant)
        } else {
            None
        };
        pending.push_back(trace);
        match evicted {
            Some(evicted) => TraceDisposition::RecordedEvicting { evicted },
            None => TraceDisposition::Recorded,
        }
    }

    /// Whether this invocation is one of the ones being recorded.
    fn selects(&self, trace: &InvocationTrace) -> bool {
        if trace.is_failure() {
            return true;
        }
        let keep_one_in = self.keep_one_in.load(Ordering::Relaxed);
        if keep_one_in <= 1 {
            return true;
        }
        // Counted off rather than drawn at random, so the number of records a
        // node produces follows from its traffic. An operator sizing
        // retention can then do the arithmetic instead of guessing.
        self.seen
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(keep_one_in)
    }

    /// Take up to `max` buffered records, oldest first.
    pub fn drain(&self, max: usize) -> Vec<InvocationTrace> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let take = max.min(pending.len());
        pending.drain(..take).collect()
    }

    /// Put a batch back at the front after a push that did not get through.
    ///
    /// Returns how many made it back. Records that no longer fit are counted
    /// as dropped, so a long outage costs the oldest traces and says so
    /// rather than growing this buffer without bound.
    pub fn requeue(&self, batch: Vec<InvocationTrace>) -> usize {
        let capacity = self.capacity.load(Ordering::Relaxed);
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let mut returned = 0usize;
        for trace in batch.into_iter().rev() {
            if pending.len() >= capacity {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            pending.push_front(trace);
            returned += 1;
        }
        returned
    }

    /// How many records are waiting to be pushed.
    pub fn pending(&self) -> usize {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Take the gap counts, resetting them.
    ///
    /// They are deltas, because that is what `PushTracesRequest` carries:
    /// "since the previous push". A push that fails hands them back with
    /// [`Self::restore_gaps`] rather than losing them, so a telemetry outage
    /// does not also erase the record that traces were lost during it.
    pub fn take_gaps(&self) -> TraceGaps {
        TraceGaps {
            sampled_out: self.sampled_out.swap(0, Ordering::Relaxed),
            dropped: self.dropped.swap(0, Ordering::Relaxed),
        }
    }

    /// Put gap counts back after a push that did not get through.
    pub fn restore_gaps(&self, gaps: TraceGaps) {
        if gaps.sampled_out > 0 {
            self.sampled_out
                .fetch_add(gaps.sampled_out, Ordering::Relaxed);
        }
        if gaps.dropped > 0 {
            self.dropped.fetch_add(gaps.dropped, Ordering::Relaxed);
        }
    }
}

/// Turn a sample rate into "record one in N".
///
/// Counting off is exact where multiplying a float by a counter drifts, and
/// N is the number an operator can reason about: a rate of 0.1 is one in ten,
/// and a rate at or below zero would be "record nothing", which is not on
/// offer -- a node that recorded no traces at all would answer "which
/// invocations failed" with silence and never say why. The floor is one in a
/// million, which for any real node is "effectively off" while still leaving
/// every failure recorded.
fn keep_one_in(sample_rate: f64) -> u64 {
    if !sample_rate.is_finite() || sample_rate >= 1.0 {
        return 1;
    }
    if sample_rate <= 0.0 {
        return 1_000_000;
    }
    ((1.0 / sample_rate).round() as u64).clamp(1, 1_000_000)
}

/// The registry every isolate and every consumer in this process records into.
pub fn recorder() -> &'static Arc<MetricsRegistry> {
    static REGISTRY: OnceLock<Arc<MetricsRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Arc::new(MetricsRegistry::new()))
}

/// Environment variable setting the fraction of successful invocations that
/// are traced. `1.0` traces all of them; failures are always traced.
///
/// An environment variable rather than a TOML key because the node's
/// configuration has no `[traces]` section and `config.rs` is another track's
/// file. `config.rs` already reads env overrides for other limits, so this is
/// the shape that file uses; moving it into a section there is a change
/// somebody else has to make.
pub const TRACE_SAMPLE_RATE_ENV: &str = "DEIS_TRACE_SAMPLE_RATE";

/// Environment variable setting the ceiling on buffered trace records.
pub const TRACE_BUFFER_ENV: &str = "DEIS_TRACE_BUFFER";

/// Where and how often metrics and traces are pushed.
///
/// There is no `[metrics]` section in the node's TOML yet: the destination is
/// the same deis-telemetry instance the logs go to, so the address and the
/// on/off switch are taken from `[logging]`. Splitting them into their own
/// section is a change to `config.rs`, which this track does not own.
#[derive(Debug, Clone)]
pub struct MetricShipperConfig {
    /// deis-telemetry address, e.g. `http://127.0.0.1:8092`.
    pub endpoint: Option<String>,
    /// The capability token this node presents on every metric and trace push.
    /// The same credential the log shipper carries, from the same setting:
    /// one service, one connection, one secret. See [`crate::telemetry`].
    pub credential: TelemetryCredential,
    /// Whether to push at all.
    pub enabled: bool,
    /// Seconds between metric pushes.
    pub interval_secs: u64,
    /// Seconds between trace pushes. Faster than `interval_secs`: a trace is
    /// an event somebody is waiting to see, a counter is a running total.
    pub trace_interval_secs: u64,
    /// Records carried by one `PushTraces` call.
    pub trace_batch_size: usize,
    /// Ceiling on buffered trace records.
    pub max_pending_traces: usize,
    /// Fraction of *successful* invocations to trace, in `0.0..=1.0`.
    /// Failures are traced whatever this says.
    pub trace_sample_rate: f64,
}

impl Default for MetricShipperConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            credential: TelemetryCredential::default(),
            enabled: false,
            interval_secs: 15,
            trace_interval_secs: 5,
            trace_batch_size: DEFAULT_TRACE_BATCH,
            max_pending_traces: DEFAULT_TRACE_CAPACITY,
            trace_sample_rate: 1.0,
        }
    }
}

impl From<&LoggingConfig> for MetricShipperConfig {
    fn from(config: &LoggingConfig) -> Self {
        Self {
            endpoint: config.endpoint.clone(),
            credential: TelemetryCredential::from_secret(config.auth_secret.as_deref()),
            enabled: config.enabled,
            // Metrics are cumulative totals, not events, so they are pushed on
            // their own slower cadence than logs: a missed push costs nothing
            // and the next one carries the same numbers.
            interval_secs: (config.flush_interval_secs * 3).max(10),
            // Traces go out on the log cadence instead. Somebody watching a
            // deployment wants to see the invocation that just failed, and a
            // record held for the metric interval is a record they are
            // waiting on.
            trace_interval_secs: config.flush_interval_secs.max(1),
            trace_batch_size: DEFAULT_TRACE_BATCH,
            max_pending_traces: env_override(TRACE_BUFFER_ENV, DEFAULT_TRACE_CAPACITY),
            trace_sample_rate: env_override(TRACE_SAMPLE_RATE_ENV, 1.0),
        }
    }
}

/// Read one setting out of the environment, keeping `fallback` and saying so
/// when the value cannot be used.
///
/// A misconfigured sample rate must not take the node down, and must not be
/// applied silently either: an operator who typed `0,1` and got full-rate
/// tracing needs to be told, not left to work it out from the disk usage.
fn env_override<T: std::str::FromStr + std::fmt::Display>(name: &str, fallback: T) -> T {
    match std::env::var(name) {
        Err(_) => fallback,
        Ok(raw) => match raw.trim().parse::<T>() {
            Ok(value) => value,
            Err(_) => {
                warn!(
                    variable = name,
                    value = %raw,
                    default = %fallback,
                    "{name} is not a value this setting accepts; the default is used instead"
                );
                fallback
            }
        },
    }
}

/// How long one dial to deis-telemetry may take before the push is treated as
/// failed and retried on the next tick.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one push may take, so a wedged telemetry node cannot stall the
/// shipping task indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

struct RunningShipper {
    stop: oneshot::Sender<()>,
    finished: oneshot::Receiver<()>,
}

/// Background task that pushes this node's telemetry to deis-telemetry: the
/// [`MetricsRegistry`] counters and the [`TraceRecorder`] records.
///
/// One task, two cadences. Both destinations are the same service on the same
/// channel, and giving traces a task of their own would mean a second
/// lifecycle for `main` to start and stop for no gain.
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
                "Telemetry shipping is disabled; this node's counters, its latency histogram \
                 and every invocation trace stay in memory. No dashboard can see them, and \
                 `deis trace` will find nothing for anything this node served"
            );
            return;
        }

        let Some(endpoint) = self.config.endpoint.clone() else {
            error!(
                "Telemetry shipping is enabled but no endpoint is configured; no metric and no \
                 trace will reach deis-telemetry. Set logging.endpoint to the telemetry address."
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

        // The recorder is process-wide and exists before the configuration is
        // read, so the operator's settings are applied here, before the node
        // starts serving.
        self.registry.traces().configure(
            self.config.max_pending_traces,
            self.config.trace_sample_rate,
        );

        info!(
            %endpoint,
            metrics_every_secs = self.config.interval_secs,
            traces_every_secs = self.config.trace_interval_secs,
            "Telemetry shipper started; this node's counters and its invocation traces will be \
             queryable through deis-telemetry"
        );
        report_posture(&self.config.credential, &endpoint);

        let registry = self.registry.clone();
        let credential = self.config.credential.clone();
        let interval_secs = self.config.interval_secs;
        let trace_interval_secs = self.config.trace_interval_secs;
        let trace_batch_size = self.config.trace_batch_size;
        let node_id = self.node_id.clone();
        tokio::spawn(async move {
            let mut client = MetricServiceClient::new(channel.clone())
                .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
                .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES);
            let mut traces = TraceServiceClient::new(channel)
                .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
                .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES);
            let mut ticker = interval(Duration::from_secs(interval_secs));
            let mut trace_ticker = interval(Duration::from_secs(trace_interval_secs));
            let mut stop = stop_rx;
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        push_once(&registry, &node_id, &credential, &mut client).await;
                    }
                    _ = trace_ticker.tick() => {
                        push_traces_once(
                            registry.traces(),
                            &node_id,
                            &credential,
                            trace_batch_size,
                            &mut traces,
                        )
                        .await;
                    }
                    _ = &mut stop => break,
                }
            }
            // One last push of each so the final minute of a node's life is
            // not missing from the dashboard, and the invocations it served on
            // its way out are still queryable. Traces are drained batch by
            // batch rather than once: a busy node can have more of them
            // buffered than one push carries, and they exist nowhere else.
            push_once(&registry, &node_id, &credential, &mut client).await;
            loop {
                let before = registry.traces().pending();
                push_traces_once(
                    registry.traces(),
                    &node_id,
                    &credential,
                    trace_batch_size,
                    &mut traces,
                )
                .await;
                // No progress means deis-telemetry is not taking them, and
                // retrying would only hold the shutdown open.
                if registry.traces().pending() >= before {
                    break;
                }
            }
            let left = registry.traces().pending();
            if left > 0 {
                warn!(
                    traces = left,
                    "Telemetry shipper stopped with invocation traces it could not deliver; \
                     they existed only in memory, so those invocations will never appear in \
                     `deis trace`. The node's own log still has whatever they logged"
                );
            }
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
                "Telemetry shipper task ended without reporting; the last counters and traces \
                 may not have been pushed"
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

pub(super) fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

async fn push_once(
    registry: &MetricsRegistry,
    node_id: &str,
    credential: &TelemetryCredential,
    client: &mut MetricServiceClient<Channel>,
) {
    let samples = stamp_node(registry.snapshot(now_us()), node_id);
    if samples.is_empty() {
        return;
    }
    let count = samples.len();

    // The token goes in the request's gRPC metadata, where deis-telemetry's
    // interceptor reads it, and not in the message's own `metadata` map.
    match client
        .push_metrics(credential.stamp(PushMetricsRequest {
            samples,
            metadata: Default::default(),
        }))
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
        Err(status) if status.code() == tonic::Code::Unauthenticated => error!(
            count,
            carrying_a_token = credential.is_present(),
            error = %status.message(),
            "deis-telemetry refused this node's metrics: a credential mismatch, not an outage. \
             Retrying cannot fix it, and until it is fixed no counter and no latency bucket from \
             this node reaches any dashboard. Set logging.auth_secret to the same value as \
             deis-telemetry's auth_secret"
        ),
        Err(status) => warn!(
            count,
            error = %status,
            "Pushing metrics to deis-telemetry failed; the counters are cumulative so nothing is \
             lost, but dashboards will show a gap until the next push succeeds"
        ),
    }
}

/// Hand one batch of traces to deis-telemetry.
///
/// The batch is taken out of the recorder and, on failure, put back at the
/// front of the queue: an invocation happened whether or not the observability
/// service was reachable, and losing the record of it because of a transient
/// outage is exactly the failure this whole surface exists to prevent. Only a
/// full buffer discards anything, and that discard is counted.
async fn push_traces_once(
    recorder: &Arc<TraceRecorder>,
    node_id: &str,
    credential: &TelemetryCredential,
    batch_size: usize,
    client: &mut TraceServiceClient<Channel>,
) {
    let gaps = recorder.take_gaps();
    let batch = recorder.drain(batch_size);
    if batch.is_empty() && gaps.is_empty() {
        return;
    }
    let count = batch.len();
    let records: Vec<TraceRecord> = batch.iter().map(|trace| trace.to_proto(node_id)).collect();

    match client
        .push_traces(credential.stamp(PushTracesRequest {
            records,
            sampled_out: gaps.sampled_out,
            dropped: gaps.dropped,
            metadata: Default::default(),
        }))
        .await
    {
        Ok(response) => {
            let response = response.into_inner();
            if response.rejected_count > 0 {
                warn!(
                    accepted = response.accepted_count,
                    rejected = response.rejected_count,
                    error = %response.error_message,
                    "deis-telemetry rejected some trace records; those invocations are \
                     discarded rather than retried, since a rejection would repeat"
                );
            } else if count > 0 {
                debug!(count, "Pushed invocation traces to deis-telemetry");
            }
        }
        Err(status) => {
            // The counts go back with the records, so a telemetry outage does
            // not also erase the evidence that traces were lost during it.
            recorder.restore_gaps(gaps);
            let returned = recorder.requeue(batch);
            if status.code() == tonic::Code::Unauthenticated {
                error!(
                    count,
                    returned,
                    carrying_a_token = credential.is_present(),
                    error = %status.message(),
                    "deis-telemetry refused this node's invocation traces: a credential \
                     mismatch, not an outage. Retrying cannot fix it; the records queue until \
                     the buffer fills and are then discarded, so `deis trace` will never show \
                     an invocation this node served. Set logging.auth_secret to the same value \
                     as deis-telemetry's auth_secret"
                );
            } else {
                warn!(
                    count,
                    returned,
                    error = %status,
                    "Pushing invocation traces to deis-telemetry failed; they stay queued and go \
                     out on the next tick. Nothing is lost until the buffer reaches its ceiling."
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "metrics_test.rs"]
mod metrics_test;
