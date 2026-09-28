//! Deislet storage services: KV, SQL database and blob objects.
//!
//! The service is deliberately "dumb". It knows exactly one thing about the
//! world outside itself: a tenant, which the capability on each call names.
//! It has no idea what a version, an environment or a deployment is, and it
//! never calls anything back. It holds no opinion about which application
//! owns which tenant either - only about whether the caller in front of it
//! holds a capability for one. Those decisions live in deis-control and
//! deis-runtime; keeping them out of here is what lets storage be restarted,
//! backed up and reasoned about on its own.
//!
//! # What a tenant is here, and what it is not
//!
//! A tenant is one application environment, keyed by canonical ids:
//! `v1/<application id>/<environment id>`. A preview and production of the
//! same application are two tenants, and an application deleted and created
//! again under the same name is a new one, so neither can read or overwrite
//! what the other stored. This service validates the key and never takes it
//! apart for any other purpose. No request header names a tenant; a call
//! whose capability names none for this binding is refused.
//!
//! Data layout under the configured data directory:
//!
//! ```text
//! kv.sqlite                             every namespace's keys, by namespace
//! blob.sqlite                           object metadata, by namespace
//! db/<namespace>.sqlite                 one SQL database per namespace
//! blob/<namespace>/<aa>/<bb>/<sha256>   object content, addressed by digest
//! ```
//!
//! `<namespace>` is the canonical tenant key -
//! `ct1_<application id>_<environment id>` - for a call that names no
//! binding, and `ct1_<application id>_<environment id>@MEDIA` for one that
//! names `MEDIA`. See [`tenant`] for why a binding cannot widen which tenant
//! is addressed.
//!
//! This directory is the only copy of every tenant's data, and it cannot be
//! backed up by copying it: SQLite runs in WAL mode here, so a plain `cp -r`
//! of a live store can restore as a database missing its last commits, or as
//! a corrupt one, without a word of warning. [`snapshot`] is how it is copied
//! and put back - `deis-store snapshot --out DIR` and `deis-store restore
//! --from DIR` - and that module documents what a snapshot does and does not
//! promise.
//!
//! The namespace key is a directory name and a database file name. It holds
//! no `.`, `/` or `\`, so no accepted value can name a parent directory. See
//! [`tenant`].
//!
//! # Trust model: the capability names the tenant
//!
//! Every call carries a capability token in `Authorization: Bearer <token>`,
//! an Ed25519-signed JCS-JWT minted by deis-control and verified against the
//! control plane's published keys. The tenant a call acts for is the one
//! tenant that token names for the service's binding; nothing in the request
//! names it, so a token minted for one application environment opens nothing
//! for another. See [`auth`].
//!
//! Every call must carry a valid capability token. There is no anonymous or
//! loopback bypass. The verifying key is obtained from the control plane's
//! well-known endpoint on startup.
//!
//! The health service is answered without a token: it names no tenant and
//! reads no data, and a liveness probe that needed a tenant credential is a
//! probe the control plane could not make.
//!
//! # Backups run themselves
//!
//! [`snapshot`] is how this directory is copied and put back, and
//! [`backup`] is what runs it: set `snapshot_dir` and the service takes a
//! snapshot on an interval, prunes past a retention count, and logs a failed
//! snapshot at `error` because a failed snapshot means this interval has no
//! backup at all. Leaving `snapshot_dir` unset is also supported - and also
//! said out loud at startup, at `warn`, because then nothing is copying the
//! only copy of every tenant's data.

// Re-exported so every component in the platform spells these the same
// way; see deis_wire::conventions for why they live in one place.
pub use deis_wire::conventions::{MAX_GRPC_MESSAGE_BYTES, STREAM_CHUNK_BYTES};

pub mod auth;
pub mod backup;
pub mod blob;
pub mod config;
pub mod database;
pub mod error;
pub mod kv;
pub mod proto;
pub mod server;
pub mod snapshot;
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

/// The smallest string strictly greater than every string starting with
/// `prefix`, or `None` when no such bound exists (empty prefix, or a prefix
/// made only of the highest code point).
///
/// UTF-8 orders bytewise exactly as its code points order, which is what lets
/// a plain `key >= prefix AND key < upper` range use the primary key index
/// instead of scanning every row and testing `LIKE`.
///
/// At the crate root because two stores need the identical bound for the
/// identical reason: [`kv`] pages a tenant's keys under a prefix, and [`blob`]
/// pages a tenant's objects under one and additionally seeks past a whole
/// rolled-up folder. Two copies of this would be two chances to disagree about
/// where a prefix ends, and a listing that disagreed would either skip keys or
/// return keys the caller did not ask for.
pub fn prefix_upper_bound(prefix: &str) -> Option<String> {
    let mut bound = prefix.to_string();
    while let Some(last) = bound.pop() {
        let mut next = last as u32 + 1;
        // The surrogate range is not valid in a Rust `char`; step over it.
        if (0xD800..=0xDFFF).contains(&next) {
            next = 0xE000;
        }
        if let Some(next) = char::from_u32(next) {
            bound.push(next);
            return Some(bound);
        }
        // `last` was char::MAX: it cannot be incremented, so carry into the
        // character before it.
    }
    None
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
