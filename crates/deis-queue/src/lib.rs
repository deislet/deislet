//! Deislet message queue: durable enqueue plus lease-based consumption.
//!
//! The service is deliberately "dumb". It knows exactly one thing about the
//! world outside itself: a tenant id, carried in the `x-deis-tenant` gRPC
//! metadata header. It has no idea what a version, an environment, a
//! deployment or a node is, and it never calls anything back. Consumers come
//! to it; it does not push to them, because pushing would mean the queue had
//! to know which node runs which application.
//!
//! # What a tenant is here, and what it is not
//!
//! The platform above builds the tenant id out of an application and an
//! environment (`deis_wire::conventions::tenant_id`), so `demo.production`
//! and `demo.preview-7` are two tenants of one application with two separate
//! sets of queues. To this service that is invisible: it sees one opaque key,
//! one tenant dimension, and it never takes the value apart for any purpose
//! other than validating it. That is the point - a preview deployment must
//! not be able to consume the work production enqueued, and the cheapest way
//! to guarantee it is that the two never name the same rows here.
//!
//! Note which header carries it. `x-deis-app-id` names the *application* on
//! the data path and is not the storage tenant; a call that stamps only that
//! one is refused rather than served from a default namespace.
//!
//! # Delivery semantics
//!
//! At-least-once. `Pull` leases a message rather than removing it: the row
//! stays in the table with its visibility pushed past the lease, and only
//! `Ack` deletes it. A consumer that crashes between the two gets the same
//! message again once the lease lapses, so application handlers have to be
//! idempotent. A message that keeps failing is moved to the dead letter table
//! once its attempts reach the configured ceiling, where it stays for an
//! operator instead of being retried forever.
//!
//! Nothing here streams and nothing here waits for work: every RPC is unary
//! and answers from what is already in the table. That is on purpose. Wave A
//! found that a client which stops reading a server stream pins the sending
//! task and its file descriptor for as long as it likes; a queue that
//! long-polled would have the same shape. Here a consumer that goes quiet
//! leaves nothing behind but leases, and those lapse on their own.
//!
//! Data layout under the configured data directory:
//!
//! ```text
//! queue.sqlite    messages and dead letters, both partitioned by tenant
//! ```
//!
//! # A data directory from before this change is not readable
//!
//! Rows used to be keyed by a bare application id in an `app_id` column. That
//! column no longer exists and nothing looks for it, so an old `queue.sqlite`
//! holds messages that would never be delivered again. There is no migration,
//! and there will not be one: this is a development platform with no deployed
//! data, and inventing an environment for an old message would be inventing
//! where to deliver it.
//!
//! What there is instead is a refusal: the service checks at startup and will
//! not serve over a database in the old shape, because the alternative is a
//! queue that looks healthy while every pull answers "nothing ready". See
//! [`legacy`].
//!
//! # Trust model: the tenant header is believed, and that is why we listen on
//! loopback
//!
//! Every request names its tenant in the `x-deis-tenant` header and the
//! service acts on it as given. There is no authorization step, because there
//! is nothing to authorize against: the queue holds no credentials and no
//! table of who may be whom. A client that can open a connection to this port
//! can read and delete any tenant's messages simply by putting that tenant's
//! id in the header - including another environment of its own application,
//! which the tenant split otherwise keeps apart. There is no privilege
//! escalation to find here; it is the design as it stands, and it is the same
//! trade deis-store makes.
//!
//! What contains it, for this round, is reachability: the default listen
//! address is `127.0.0.1`, so the only callers are processes on the same
//! host - which is where deis-runtime, the sole intended caller, runs. The
//! service warns at startup when it is bound anywhere else, because that
//! turns a local trust assumption into an open one. Exposing this service
//! beyond the local host is not a configuration change, it is a feature that
//! does not exist yet: it needs an identity per caller, a check that the
//! identity is entitled to the tenant the call claims, and refusals logged
//! with enough context to investigate.

// Re-exported so every component in the platform spells these the same
// way; see deis_wire::conventions for why they live in one place.
pub use deis_wire::conventions::MAX_GRPC_MESSAGE_BYTES;

pub mod config;
pub mod error;
pub mod legacy;
pub mod proto;
pub mod queue;
pub mod server;
pub mod sqlite;
pub mod tenant;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// Seconds since the unix epoch, the one time representation used in every
/// table here.
///
/// A clock set before 1970 yields 0 rather than a negative instant: that
/// makes every lease look already lapsed, which is visible and recoverable
/// (messages are delivered again), whereas a wrapped timestamp would hide
/// messages until long after anyone cared.
pub fn unix_seconds() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => {
            tracing::error!(
                error = %e,
                "system clock is before the unix epoch; timestamps are recorded as 0, so every \
                 lease reads as already lapsed and messages will be delivered repeatedly"
            );
            0
        }
    }
}

/// The clock the queue reads.
///
/// Leases, delays and dead letter retention are all "now plus a number of
/// seconds", so every one of them is only testable by either waiting out real
/// time or telling the service what time it is. This exists for the second
/// option: [`Clock::fixed`] hands the tests a clock they advance by hand, so
/// a lease can lapse without the test standing still for it.
#[derive(Clone, Debug, Default)]
pub enum Clock {
    /// The system clock, via [`unix_seconds`]. What the binary runs on.
    #[default]
    System,
    /// A clock that only moves when [`Clock::advance`] is called.
    Manual(Arc<AtomicI64>),
}

impl Clock {
    /// A hand-advanced clock starting at `at` (unix seconds).
    pub fn fixed(at: i64) -> Self {
        Self::Manual(Arc::new(AtomicI64::new(at)))
    }

    /// The current unix second.
    pub fn now(&self) -> i64 {
        match self {
            Self::System => unix_seconds(),
            Self::Manual(at) => at.load(Ordering::SeqCst),
        }
    }

    /// Move a manual clock forward. A no-op on the system clock, which is why
    /// this cannot silently make a production timer fire early.
    pub fn advance(&self, seconds: i64) {
        match self {
            Self::System => tracing::warn!(
                seconds,
                "asked to advance the system clock; ignored, because nothing may move real time"
            ),
            Self::Manual(at) => {
                at.fetch_add(seconds, Ordering::SeqCst);
            }
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

    #[test]
    fn a_manual_clock_only_moves_when_told() {
        let clock = Clock::fixed(1_000);
        assert_eq!(clock.now(), 1_000);
        clock.advance(30);
        assert_eq!(clock.now(), 1_030);
        // Cloning shares the same instant, so a service holding a clone sees
        // what the test advances.
        let shared = clock.clone();
        clock.advance(5);
        assert_eq!(shared.now(), 1_035);
    }

    #[test]
    fn the_system_clock_cannot_be_advanced() {
        let clock = Clock::System;
        let before = clock.now();
        clock.advance(3_600);
        assert!(clock.now() - before < 5, "real time must not have moved");
    }
}
