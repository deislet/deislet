//! How many ops an isolate dispatched, and how many of the asynchronous ones
//! came back.
//!
//! deno_core carried these two types until 0.410 and dropped them in 0.411,
//! keeping only the plumbing they were built on: the event enum, the callback
//! type and the factory signature. What it dropped is the counting, which this
//! node needs for two things that are not optional here -- the per-app op
//! counters `deis status` reports, and the check for work still in flight when
//! an invocation ends. So the counting lives here now, against the plumbing
//! that stayed.
//!
//! One difference from the version deno_core carried: that one indexed its
//! table with `OpCtx::id` at dispatch time, which is no longer reachable from
//! outside the crate. Each op's slot index is captured when its callback is
//! built instead, which is both public API and one fewer lookup per dispatch.

use std::cell::{Ref, RefCell};
use std::rc::Rc;

use deno_core::{OpDecl, OpId, OpMetricsEvent, OpMetricsFn, OpMetricsSource};

/// What one op, or a whole isolate, did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpMetricsSummary {
    /// Dispatches of ops that answer without yielding.
    pub ops_dispatched_sync: u64,
    /// Dispatches of ops that return a promise.
    pub ops_dispatched_async: u64,
    /// Dispatches that took V8's fast path. A subset of the two above, not a
    /// third category: a fast dispatch is counted here *and* as sync or async.
    pub ops_dispatched_fast: u64,
    /// Asynchronous dispatches that have since finished, whether they
    /// resolved or threw.
    pub ops_completed_async: u64,
}

impl OpMetricsSummary {
    /// Whether an asynchronous op dispatched here has not come back.
    ///
    /// Read at the end of an invocation: work still outstanding means the
    /// handler returned while something it started is still running.
    pub fn has_outstanding_ops(&self) -> bool {
        self.ops_dispatched_async > self.ops_completed_async
    }
}

/// One counter per op in an isolate, plus the aggregate over all of them.
#[derive(Debug, Default)]
pub struct OpMetricsSummaryTracker {
    ops: RefCell<Vec<OpMetricsSummary>>,
}

impl OpMetricsSummaryTracker {
    /// The per-op table, in the order the ops were registered.
    pub fn per_op(&self) -> Ref<'_, Vec<OpMetricsSummary>> {
        self.ops.borrow()
    }

    /// Everything this isolate's ops did, added together.
    pub fn aggregate(&self) -> OpMetricsSummary {
        let mut sum = OpMetricsSummary::default();
        for metrics in self.ops.borrow().iter() {
            sum.ops_dispatched_sync += metrics.ops_dispatched_sync;
            sum.ops_dispatched_fast += metrics.ops_dispatched_fast;
            sum.ops_dispatched_async += metrics.ops_dispatched_async;
            sum.ops_completed_async += metrics.ops_completed_async;
        }
        sum
    }

    /// Claim a slot for one op and return the callback that fills it, or
    /// `None` for an op this tracker does not count.
    ///
    /// Called once per op while the runtime is being built, from inside a
    /// [`deno_core::OpMetricsFactoryFn`]; `total` is how many ops are coming,
    /// so the table is allocated once rather than grown op by op. A slot is
    /// reserved even for an op that is not counted, which keeps a slot's
    /// position equal to the op's registration order and makes [`Self::per_op`]
    /// readable beside an op list.
    pub fn watch(
        self: &Rc<Self>,
        _id: OpId,
        total: usize,
        decl: &OpDecl,
        counted: impl Fn(&OpDecl) -> bool,
    ) -> Option<OpMetricsFn> {
        let index = {
            let mut ops = self.ops.borrow_mut();
            if ops.capacity() == 0 {
                ops.reserve_exact(total);
            }
            ops.push(OpMetricsSummary::default());
            ops.len() - 1
        };

        if !counted(decl) {
            return None;
        }

        // Captured rather than read from the op context on every dispatch:
        // whether an op is asynchronous is fixed when it is declared.
        let is_async = decl.is_async;
        let tracker = Rc::clone(self);
        Some(Rc::new(move |_ctx: &_, event, source| {
            let mut ops = tracker.ops.borrow_mut();
            let Some(slot) = ops.get_mut(index) else {
                return;
            };
            match event {
                OpMetricsEvent::Dispatched => {
                    if source == OpMetricsSource::Fast {
                        slot.ops_dispatched_fast += 1;
                    }
                    if is_async {
                        slot.ops_dispatched_async += 1;
                    } else {
                        slot.ops_dispatched_sync += 1;
                    }
                }
                OpMetricsEvent::Completed
                | OpMetricsEvent::Error
                | OpMetricsEvent::CompletedAsync
                | OpMetricsEvent::ErrorAsync => {
                    if is_async {
                        slot.ops_completed_async += 1;
                    }
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_tracker_aggregates_to_nothing_and_reports_no_outstanding_work() {
        let tracker = Rc::new(OpMetricsSummaryTracker::default());
        let summary = tracker.aggregate();
        assert_eq!(summary, OpMetricsSummary::default());
        assert!(!summary.has_outstanding_ops());
    }

    #[test]
    fn an_async_dispatch_that_has_not_come_back_is_outstanding() {
        let summary = OpMetricsSummary {
            ops_dispatched_async: 2,
            ops_completed_async: 1,
            ..Default::default()
        };
        assert!(summary.has_outstanding_ops());

        let settled = OpMetricsSummary {
            ops_completed_async: 2,
            ..summary
        };
        assert!(!settled.has_outstanding_ops());
    }

    #[test]
    fn the_aggregate_adds_every_slot_up() {
        let tracker = OpMetricsSummaryTracker {
            ops: RefCell::new(vec![
                OpMetricsSummary {
                    ops_dispatched_sync: 3,
                    ops_dispatched_fast: 2,
                    ..Default::default()
                },
                OpMetricsSummary {
                    ops_dispatched_async: 5,
                    ops_completed_async: 4,
                    ..Default::default()
                },
            ]),
        };
        assert_eq!(
            tracker.aggregate(),
            OpMetricsSummary {
                ops_dispatched_sync: 3,
                ops_dispatched_async: 5,
                ops_dispatched_fast: 2,
                ops_completed_async: 4,
            }
        );
        assert_eq!(tracker.per_op().len(), 2);
    }
}
