//! Deislet storage services: KV, SQL database and blob objects.
//!
//! The service is deliberately "dumb". It knows exactly one thing about the
//! world outside itself: a tenant id, carried in the `x-deis-tenant` gRPC
//! metadata header. It has no idea what a version, an environment or a
//! deployment is, and it never calls anything back. It holds no opinion about
//! which application owns which tenant either - only about whether the caller
//! in front of it can prove it holds the capability for the tenant it names.
//! Those decisions live in deis-control and deis-runtime; keeping them out of
//! here is what lets storage be restarted, backed up and reasoned about on
//! its own.
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
//! kv.sqlite                             every namespace's keys, by namespace
//! blob.sqlite                           object metadata, by namespace
//! db/<namespace>.sqlite                 one SQL database per namespace
//! blob/<namespace>/<aa>/<bb>/<sha256>   object content, addressed by digest
//! ```
//!
//! `<namespace>` is the tenant - `demo.production` - for a call that names no
//! binding, and `demo.production@MEDIA` for one that names `MEDIA`. Both are
//! ordinary names on disk here; see [`tenant`] for why a binding cannot widen
//! which tenant is addressed, and [`legacy`] for the startup check that has
//! to know both forms.
//!
//! This directory is the only copy of every tenant's data, and it cannot be
//! backed up by copying it: SQLite runs in WAL mode here, so a plain `cp -r`
//! of a live store can restore as a database missing its last commits, or as
//! a corrupt one, without a word of warning. [`snapshot`] is how it is copied
//! and put back - `deis-store snapshot --out DIR` and `deis-store restore
//! --from DIR` - and that module documents what a snapshot does and does not
//! promise.
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
//! # Trust model: a tenant is claimed in a header and proved with a token
//!
//! Every request names its tenant in `x-deis-tenant`. On its own that is a
//! claim: the header says which namespace the caller wants, and nothing in
//! it says the caller is entitled to it. What goes with the claim is a
//! capability token in `Authorization: Bearer <token>`, an Ed25519-signed
//! JCS-JWT minted by deis-control and verified against the control plane's
//! public key. The token's resources must include a (tenant, binding) pair
//! that matches the request, so a token minted for one application opens
//! nothing for another. See [`auth`].
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
pub mod legacy;
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
