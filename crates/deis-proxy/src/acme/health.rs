//! What the renewal task has to say about itself, other than in the log.
//!
//! A renewal that keeps failing used to have exactly one surface: the log
//! line, at a level that rose as the slack ran out. That is the right first
//! surface and not enough on its own - a log nobody ships is a log nobody
//! reads at three in the morning. These counters are the second: they are
//! updated by every pass and read by whatever reports this process's state
//! (the control plane's `get_status` answer, a metrics endpoint when one
//! exists), so "is certificate renewal healthy" can be asked of the process
//! rather than searched for in its output.
//!
//! Plain atomics, no lock: the pass writes a handful of numbers every few
//! hours and a status request reads them, and neither should ever wait for
//! the other.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Counters and gauges the renewal task keeps.
///
/// Counters (`orders_*`, `passes_*`) only grow for the life of the process.
/// Gauges (`names_*`) are what the most recent pass found. `days_left_min`
/// is the least remaining validity among the certificates that are being
/// served, or `-1` when a served certificate has already expired, or
/// nothing when none is served - the one number a dashboard wants.
#[derive(Debug, Default)]
pub struct AcmeHealth {
    passes_finished: AtomicU64,
    passes_skipped_locked: AtomicU64,
    orders_issued: AtomicU64,
    orders_failed: AtomicU64,
    saves_failed: AtomicU64,
    connect_failures: AtomicU64,
    names_wanted: AtomicU64,
    names_covered: AtomicU64,
    names_failing: AtomicU64,
    names_uncovered: AtomicU64,
    names_unsaved: AtomicU64,
    /// `i64::MAX` while unknown, so the gauge can tell "nothing served"
    /// from "a certificate with no days left".
    days_left_min: AtomicI64,
    last_pass_unix: AtomicU64,
}

/// One pass's tally, written into [`AcmeHealth`] as a unit when it ends.
#[derive(Debug, Default, Clone, Copy)]
pub struct PassTally {
    pub wanted: usize,
    pub covered: usize,
    pub failing: usize,
    pub uncovered: usize,
    pub unsaved: usize,
    pub issued: usize,
    pub failed: usize,
    pub saves_failed: usize,
    pub days_left_min: Option<i64>,
}

/// A point-in-time copy of the counters, in the shape a status answer or a
/// metrics exporter can carry as it is.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HealthSnapshot {
    pub passes_finished: u64,
    pub passes_skipped_locked: u64,
    pub orders_issued: u64,
    pub orders_failed: u64,
    pub saves_failed: u64,
    pub connect_failures: u64,
    pub names_wanted: u64,
    pub names_covered: u64,
    pub names_failing: u64,
    pub names_uncovered: u64,
    pub names_unsaved: u64,
    pub days_left_min: Option<i64>,
    pub last_pass_unix: Option<u64>,
    /// `false` when the most recent pass left a name failing, uncovered or
    /// unsaved, or when the authority could not be reached since the last
    /// finished pass. The one boolean a health check wants.
    pub healthy: bool,
}

impl AcmeHealth {
    pub fn new() -> Self {
        Self {
            days_left_min: AtomicI64::new(i64::MAX),
            ..Default::default()
        }
    }

    /// Record a finished pass.
    pub fn record_pass(&self, tally: PassTally) {
        self.passes_finished.fetch_add(1, Ordering::Relaxed);
        self.orders_issued
            .fetch_add(tally.issued as u64, Ordering::Relaxed);
        self.orders_failed
            .fetch_add(tally.failed as u64, Ordering::Relaxed);
        self.saves_failed
            .fetch_add(tally.saves_failed as u64, Ordering::Relaxed);
        self.names_wanted
            .store(tally.wanted as u64, Ordering::Relaxed);
        self.names_covered
            .store(tally.covered as u64, Ordering::Relaxed);
        self.names_failing
            .store(tally.failing as u64, Ordering::Relaxed);
        self.names_uncovered
            .store(tally.uncovered as u64, Ordering::Relaxed);
        self.names_unsaved
            .store(tally.unsaved as u64, Ordering::Relaxed);
        self.days_left_min
            .store(tally.days_left_min.unwrap_or(i64::MAX), Ordering::Relaxed);
        // A finished pass means the authority answered; the connect count
        // is "failures since the last good pass".
        self.connect_failures.store(0, Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.last_pass_unix.store(now, Ordering::Relaxed);
    }

    /// Record a pass that did not run because another process holds the
    /// store's renewal lock.
    pub fn record_skipped_locked(&self) {
        self.passes_skipped_locked.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one failure to reach the authority or register the account.
    pub fn record_connect_failure(&self) {
        self.connect_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> HealthSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let days_left_min = match self.days_left_min.load(Ordering::Relaxed) {
            i64::MAX => None,
            days => Some(days),
        };
        let last_pass_unix = match load(&self.last_pass_unix) {
            0 => None,
            at => Some(at),
        };
        let names_failing = load(&self.names_failing);
        let names_uncovered = load(&self.names_uncovered);
        let names_unsaved = load(&self.names_unsaved);
        let connect_failures = load(&self.connect_failures);
        HealthSnapshot {
            passes_finished: load(&self.passes_finished),
            passes_skipped_locked: load(&self.passes_skipped_locked),
            orders_issued: load(&self.orders_issued),
            orders_failed: load(&self.orders_failed),
            saves_failed: load(&self.saves_failed),
            connect_failures,
            names_wanted: load(&self.names_wanted),
            names_covered: load(&self.names_covered),
            names_failing,
            names_uncovered,
            names_unsaved,
            days_left_min,
            last_pass_unix,
            healthy: names_failing == 0
                && names_uncovered == 0
                && names_unsaved == 0
                && connect_failures == 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pass_that_left_a_name_failing_reads_as_unhealthy_until_one_does_not() {
        // The property a health check depends on: the answer follows the
        // most recent pass, and a counter of past failures is not a reason
        // to stay unhealthy once a pass has covered every name.
        let health = AcmeHealth::new();
        assert!(health.snapshot().healthy, "nothing has failed yet");
        assert_eq!(health.snapshot().last_pass_unix, None);

        health.record_pass(PassTally {
            wanted: 3,
            covered: 2,
            failing: 1,
            failed: 1,
            days_left_min: Some(12),
            ..Default::default()
        });
        let after_failure = health.snapshot();
        assert!(!after_failure.healthy);
        assert_eq!(after_failure.names_failing, 1);
        assert_eq!(after_failure.orders_failed, 1);
        assert_eq!(after_failure.days_left_min, Some(12));
        assert!(after_failure.last_pass_unix.is_some());

        health.record_pass(PassTally {
            wanted: 3,
            covered: 3,
            issued: 1,
            days_left_min: Some(12),
            ..Default::default()
        });
        let after_recovery = health.snapshot();
        assert!(after_recovery.healthy);
        assert_eq!(
            after_recovery.orders_failed, 1,
            "counters keep their history"
        );
        assert_eq!(after_recovery.orders_issued, 1);
        assert_eq!(after_recovery.passes_finished, 2);
    }

    #[test]
    fn a_name_the_ceiling_left_out_and_an_unreachable_authority_are_both_unhealthy() {
        let health = AcmeHealth::new();
        health.record_pass(PassTally {
            wanted: 1,
            covered: 1,
            uncovered: 1,
            ..Default::default()
        });
        assert!(
            !health.snapshot().healthy,
            "an uncovered name is a name that will expire"
        );

        let health = AcmeHealth::new();
        health.record_pass(PassTally {
            wanted: 1,
            covered: 1,
            ..Default::default()
        });
        health.record_connect_failure();
        let snapshot = health.snapshot();
        assert!(!snapshot.healthy);
        assert_eq!(snapshot.connect_failures, 1);
        health.record_pass(PassTally {
            wanted: 1,
            covered: 1,
            ..Default::default()
        });
        assert!(
            health.snapshot().healthy,
            "a finished pass means the authority answered"
        );
        assert_eq!(health.snapshot().connect_failures, 0);
    }

    #[test]
    fn the_snapshot_serialises_as_flat_json() {
        let health = AcmeHealth::new();
        health.record_skipped_locked();
        let json = serde_json::to_value(health.snapshot()).unwrap();
        assert_eq!(json["passes_skipped_locked"], 1);
        assert_eq!(json["healthy"], true);
        assert!(json["days_left_min"].is_null());
    }
}
