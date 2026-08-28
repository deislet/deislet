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

pub mod blob;
pub mod config;
pub mod database;
pub mod error;
pub mod kv;
pub mod proto;
pub mod server;
pub mod sqlite;
pub mod tenant;

/// gRPC payload ceiling. The 4 MB tonic default is too tight for batched
/// messages and manifests, so every Deislet service agrees on 16 MB.
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

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
