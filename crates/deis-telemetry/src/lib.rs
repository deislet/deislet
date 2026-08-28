//! Deislet log aggregation and metrics.
//!
//! This service is deliberately "dumb": it knows only `app_id` as a tenant
//! dimension, and knows nothing about versions, environments or deployments.
//! Those concepts live in deis-control and deis-runtime.
//!
//! It is equally dumb about health. It stores logs and metric samples and
//! hands them back on request; it never decides that a component is unwell,
//! never raises an alert and never calls another component. Whether a node is
//! alive is answered by a direct health check against that node - routing
//! that judgement through here would mean an outage of the observability
//! component looked like an outage of everything it observes.
//!
//! Layout:
//! - [`store`]   SQLite: day-partitioned logs, a metric time series, retention
//! - [`tail`]    live fan-out to `Tail` subscribers, dropping for slow ones
//! - [`service`] the gRPC surface
//! - [`http`]    the Prometheus scrape endpoint on its own port
//! - [`prom`]    exposition-format rendering
//! - [`stats`]   this process's own counters
//! - [`server`]  wiring: listeners, retention sweeper, shutdown

// Re-exported so every component in the platform spells these the same
// way; see deis_wire::conventions for why they live in one place.
pub use deis_wire::conventions::MAX_GRPC_MESSAGE_BYTES;

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
