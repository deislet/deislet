//! Deislet storage services: KV, SQL database and blob objects.
//!
//! The service is deliberately "dumb". It knows exactly one thing about the
//! world outside itself: a tenant id, carried in the `x-deis-tenant` gRPC
//! metadata header. It has no idea what a version, an environment or a
//! deployment is, it never calls anything back, and it holds no opinion about
//! who is allowed to be which tenant. Those decisions live in deis-control and
//! deis-runtime; keeping them out of here is what lets storage be restarted,
//! backed up and reasoned about on its own.
//!
//! # What a tenant is here, and what it is not
//!
//! The platform above builds the tenant id out of an application and an
//! environment (`deis_wire::conventions::tenant_id`), so `demo.production`
//! and `demo.preview-7` are two tenants of one application. To this service
//! that is invisible: it sees one opaque key, one tenant dimension, and it
//! never takes the value apart for any purpose other than validating it.
//! That is the point - a preview deployment must not be able to read or
//! overwrite what production stored, and the cheapest way to guarantee it is
//! that the two never name the same namespace here.
//!
//! Note which header carries it. `x-deis-app-id` names the *application* on
//! the data path and is not the storage tenant; a call that stamps only that
//! one is refused rather than served from a default namespace.
//!
//! Data layout under the configured data directory:
//!
//! ```text
//! kv.sqlite                          every tenant's keys, by tenant
//! blob.sqlite                        object metadata, by tenant
//! db/<tenant>.sqlite                 one SQL database per tenant
//! blob/<tenant>/<aa>/<bb>/<sha256>   object content, addressed by digest
//! ```
//!
//! The tenant id is a directory name and a database file name, which is why
//! it is validated hard rather than sanitised: the character set has no `.`,
//! `/` or `\` in either half, so no accepted value can name a parent
//! directory. See [`tenant`].
//!
//! # A data directory from before this change is not readable
//!
//! Storage used to be keyed by a bare application id: an `app_id` column,
//! `db/<app>.sqlite`, `blob/<app>/...`. Those names do not exist under the
//! tenant scheme and nothing looks for them, so an old data directory holds
//! data no application can reach. There is no migration - this is a
//! development platform with no deployed data, and inventing an environment
//! for old rows would be inventing where production data belongs.
//!
//! What there is instead is a refusal: the service checks at startup and
//! will not serve over a data directory in the old shape, because the
//! alternative is a store that looks healthy while every read answers "not
//! found". See [`legacy`].
//!
//! # Trust model: the tenant header is believed, and that is why we listen on
//! loopback
//!
//! Every request names its tenant in the `x-deis-tenant` header and the
//! service acts on it as given. There is no authorization step, because
//! there is nothing to authorize against: the store holds no credentials, no
//! per-tenant keys and no table of who may be whom. So a client that can open
//! a connection to this port can read, overwrite and delete any tenant's data
//! simply by putting that tenant's id in the header - including another
//! environment of its own application, which the tenant split otherwise keeps
//! apart. There is no privilege escalation to find here; it is the design as
//! it stands.
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
//! the tenant the call claims, refusals logged with enough context to
//! investigate, and the same treatment for the health endpoint. Until that
//! exists, an operator who widens `listen` is publishing every tenant's data
//! to whoever can route to the port.

// Re-exported so every component in the platform spells these the same
// way; see deis_wire::conventions for why they live in one place.
pub use deis_wire::conventions::{MAX_GRPC_MESSAGE_BYTES, STREAM_CHUNK_BYTES};

pub mod blob;
pub mod config;
pub mod database;
pub mod error;
pub mod kv;
pub mod legacy;
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
