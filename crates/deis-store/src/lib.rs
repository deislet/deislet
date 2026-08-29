//! Deislet storage services: KV, SQL database and blob objects.
//!
//! The service is deliberately "dumb". It knows exactly one thing about the
//! world outside itself: an `app_id`, carried in the `x-deis-app-id` gRPC
//! metadata header. It has no idea what a version, an environment or a
//! deployment is, it never calls anything back, and it holds no opinion about
//! who is allowed to be which app. Those decisions live in deis-control and
//! deis-runtime; keeping them out of here is what lets storage be restarted,
//! backed up and reasoned about on its own.
//!
//! Data layout under the configured data directory:
//!
//! ```text
//! kv.sqlite                          every app's keys, partitioned by app_id
//! blob.sqlite                        object metadata, partitioned by app_id
//! db/<app_id>.sqlite                 one SQL database per app
//! blob/<app_id>/<aa>/<bb>/<sha256>   object content, addressed by digest
//! ```
//!
//! # Trust model: the tenant header is believed, and that is why we listen on
//! loopback
//!
//! Every request names its tenant in the `x-deis-app-id` header and the
//! service acts on it as given. There is no authorization step, because
//! there is nothing to authorize against: the store holds no credentials, no
//! per-app keys and no table of who may be whom. So a client that can open a
//! connection to this port can read, overwrite and delete any app's data
//! simply by putting that app's id in the header. There is no privilege
//! escalation to find here; it is the design as it stands.
//!
//! What contains it, for this round, is reachability. The default listen
//! address is `127.0.0.1`, so the only callers are processes on the same
//! host - which is where deis-runtime, the sole intended caller, runs. The
//! service warns at startup when it is bound anywhere else, because that
//! turns a local trust assumption into an open one.
//!
//! Exposing this service beyond the local host is therefore not a
//! configuration change, it is a feature that does not exist yet. It needs,
//! at a minimum: an identity per caller (mutual TLS certificate or a signed
//! token issued by deis-control), a check that the identity is entitled to
//! the `app_id` the call claims, refusals logged with enough context to
//! investigate, and the same treatment for the health endpoint. Until that
//! exists, an operator who widens `listen` is publishing every app's data to
//! whoever can route to the port.

// Re-exported so every component in the platform spells these the same
// way; see deis_wire::conventions for why they live in one place.
pub use deis_wire::conventions::{MAX_GRPC_MESSAGE_BYTES, STREAM_CHUNK_BYTES};

pub mod blob;
pub mod config;
pub mod database;
pub mod error;
pub mod kv;
pub mod proto;
pub mod server;
pub mod sqlite;
pub mod tenant;

/// Seconds since the unix epoch, the one time representation used in every
/// table here.
///
/// A clock set before 1970 yields 0 rather than a negative instant: that
/// makes every stored expiry look already lapsed, which is visible and
/// recoverable, whereas a wrapped timestamp would silently make entries
/// immortal.
pub fn unix_seconds() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => {
            tracing::error!(
                error = %e,
                "system clock is before the unix epoch; timestamps are recorded as 0 and every \
                 expiry will read as already lapsed"
            );
            0
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn unix_seconds_is_plausible() {
        // Well after 2020 and well before 2100: catches a unit mix-up
        // (milliseconds, nanoseconds) rather than testing the clock.
        let now = unix_seconds();
        assert!(now > 1_577_836_800, "{now} looks too small");
        assert!(now < 4_102_444_800, "{now} looks too large");
    }
}
