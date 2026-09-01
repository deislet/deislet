//! Deislet log aggregation, per-invocation traces and metrics.
//!
//! Three surfaces, one database and one process:
//!
//! - **logs** - lines an application or a component wrote;
//! - **traces** - one record per *invocation*: which application, which kind
//!   of invocation, what it was aimed at, how long it ran, and how the
//!   platform decided it ended. A log line says something happened; a trace
//!   says an invocation happened, which is what makes "show me the requests
//!   that failed in the last hour" answerable at all, including for an
//!   application that fails without logging a word. Traces carry the same
//!   `trace_id` an application's log lines carry, so one query joins them;
//! - **metrics** - numeric samples, exported in the Prometheus text format.
//!
//! Traces are higher volume than logs by construction, so they are kept for
//! fewer days (`trace_retention_days`) and producers are expected to sample.
//! Neither loss is silent: a producer reports what sampling skipped and what
//! its own buffer lost on every push, and both come back out on `/metrics` as
//! `deis_telemetry_traces_sampled_out_total` and
//! `deis_telemetry_traces_dropped_by_producer_total`.
//!
//! This service is deliberately "dumb": it knows only `app_id` as an
//! application dimension, and knows nothing about versions, environments or
//! deployments. Those concepts live in deis-control and deis-runtime.
//!
//! deis-runtime does run production and preview environments of one
//! application side by side, and it names them apart everywhere it stores
//! data - the storage tenant is `demo.production`. It does not do that here.
//! Unlike deis-store, this service partitions nothing: a composite key would
//! cost the question people actually ask - "show me this application's logs",
//! meaning all of its environments - and would buy no isolation, because
//! anything holding this service's credential may name any `app_id` anyway.
//! So the environment arrives as an ordinary structured field on a log entry,
//! and as an ordinary label on a metric sample, and `app_id` stays the
//! application.
//!
//! What it does protect is the channel. With an `auth_secret` configured,
//! every call and every scrape must present a capability token, and with none
//! configured the service refuses to bind anywhere but loopback rather than
//! quietly publishing every application's logs, traces and metrics to the
//! network. [`auth`] has the whole reasoning, including why the token names
//! this service rather than a tenant.
//!
//! The live side is shared out per application rather than first-come: one
//! application logging in a loop cannot push a co-tenant's lines out of a
//! follow stream before the operator watching it gets to read them, and
//! anything that is thrown away is counted against the application that
//! produced it - see [`tail`].
//!
//! It is equally dumb about health. It stores logs and metric samples and
//! hands them back on request; it never decides that a component is unwell,
//! never raises an alert and never calls another component. Whether a node is
//! alive is answered by a direct health check against that node - routing
//! that judgement through here would mean an outage of the observability
//! component looked like an outage of everything it observes.
//!
//! Its own liveness is answered by the standard `grpc.health.v1` service and
//! nothing else, the same probe deis-store and deis-queue answer. A second,
//! home-grown health RPC would only add an answer that could disagree.
//!
//! Layout:
//! - [`auth`]    the capability token, and the refusal to serve unguarded
//! - [`store`]   SQLite: day-partitioned logs and traces, a metric time
//!   series, retention
//! - [`tail`]    live fan-out to `Tail` and `TailTraces` subscribers, each
//!   application holding its own share of a slow one's buffer
//! - [`service`] the three gRPC services: `LogService`, `TraceService` and
//!   `MetricService`
//! - [`http`]    the Prometheus scrape endpoint on its own port
//! - [`prom`]    exposition-format rendering
//! - [`stats`]   this process's own counters
//! - [`server`]  wiring: listeners, retention sweeper, shutdown

// Re-exported so every component in the platform spells these the same
// way; see deis_wire::conventions for why they live in one place.
pub use deis_wire::conventions::MAX_GRPC_MESSAGE_BYTES;

pub mod auth;
pub mod config;
pub mod http;
pub mod prom;
pub mod server;
pub mod service;
pub mod stats;
pub mod store;
pub mod tail;

/// Generated types and stubs for `deis.telemetry`.
pub mod proto {

    tonic::include_proto!("deis.telemetry");
}

/// Current wall-clock time in microseconds since the Unix epoch.
///
/// A clock before the epoch would make every timestamp negative and every
/// entry look malformed, so it saturates at zero rather than going negative.
pub fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}
