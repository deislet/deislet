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
//! This service is deliberately "dumb": its ingest and query model uses
//! `app_id` as the application dimension and makes no health, version or
//! deployment decision. Environment stays a record attribute.
//!
//! deis-runtime does run production and preview environments of one
//! application side by side, and it names them apart everywhere it stores
//! data - the storage tenant is `demo.production`. It does not do that here.
//! Unlike deis-store, this service partitions nothing on disk: a composite
//! key would cost the question people actually ask - "show me this
//! application's logs", meaning all of its environments. So the environment
//! arrives as an ordinary structured field on a log entry, and as an ordinary
//! label on a metric sample, and `app_id` stays the application. A record
//! with no `app_id` at all is a node's own, and belongs to the platform.
//!
//! Who may write and read what is decided per record from the caller's
//! capability, minted by deis-control and verified against its published
//! keys on every call and every scrape. A node may push only the application
//! environments it runs (plus its own node-level records); a reader sees only
//! the environments its capability names, or everything when it holds the
//! platform-wide read. There is no configuration in which the check is off.
//! [`auth`] has the whole reasoning.
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
//! Its own health is answered by the standard `grpc.health.v1` service and
//! nothing else, the same probe deis-store and deis-queue answer. A second,
//! home-grown health RPC would only add an answer that could disagree.
//!
//! That answer follows the database rather than the process. All three
//! services report `NOT_SERVING` from the moment a storage call fails and
//! `SERVING` again from the moment one works, because a collector that
//! reports healthy while refusing every push is worse than one that reports
//! broken - nothing downstream has another way to find out. Writes and reads
//! are tracked apart, since the commonest failure of all, a disk with no
//! space left, refuses every write while answering every read. Both show up
//! on `/metrics` as `deis_telemetry_store_writable` and
//! `deis_telemetry_store_readable`, beside `deis_telemetry_storage_failures_total`
//! and the `*_lost_total` counters saying how much went missing. See
//! [`stats::StoreHealth`].
//!
//! Layout:
//! - [`auth`]    the capability check, and what each binding may touch
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
//! - [`snapshot`] consistent, verified offline backup and restore

// Re-exported so every component in the platform spells these the same
// way; see deis_wire::conventions for why they live in one place.
pub use deis_wire::conventions::MAX_GRPC_MESSAGE_BYTES;

pub mod auth;
pub mod config;
pub mod http;
mod identity;
pub mod prom;
pub mod server;
pub mod service;
pub mod snapshot;
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
