//! What makes an isolate unfit to be reused, and the ArrayBuffer budget that
//! keeps one tenant's buffers inside its own ceiling.
//!
//! ADR-010 ("CPU 与 Wall Time 归属", revised by D-7): V8 can only end an
//! execution by terminating the whole isolate, so an invocation that crosses
//! its CPU, wall-clock or memory ceiling costs the isolate it ran in. The
//! isolate is marked here, the runtime thread that hosts it reads the mark
//! once the message it was handling is finished, and a fresh isolate is built
//! in its place from the same inputs before the next message is taken - so
//! nothing queued behind the overrun sees an error, and nothing that ran
//! inside the terminated isolate is ever run again.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

/// Why an isolate is being discarded instead of reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecycleReason {
    /// The V8 heap ceiling was tripped. The near-heap-limit callback had to
    /// raise the limit to let the termination unwind, and V8 cannot lower it
    /// again, so the isolate is more generous than its configuration.
    Memory,
    /// The watchdog cut an execution off at its CPU-time ceiling.
    CpuTime,
    /// The watchdog cut an execution off at its wall-clock ceiling.
    Timeout,
    /// The op rate limiter terminated an execution.
    OpRate,
}

impl RecycleReason {
    /// The `reason` label on `deis_runtime_isolate_recycled_total`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Memory => super::metrics::RECYCLE_REASON_MEMORY,
            Self::CpuTime => super::metrics::RECYCLE_REASON_CPU,
            Self::Timeout => super::metrics::RECYCLE_REASON_TIMEOUT,
            Self::OpRate => super::metrics::RECYCLE_REASON_OP_RATE,
        }
    }

    /// The clause a log line reads.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Memory => "its V8 heap limit was tripped",
            Self::CpuTime => "an execution was cut off at its CPU-time ceiling",
            Self::Timeout => "an execution was cut off at its wall-clock ceiling",
            Self::OpRate => "an execution was cut off by the op rate limiter",
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::Memory => 1,
            Self::CpuTime => 2,
            Self::Timeout => 3,
            Self::OpRate => 4,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Memory),
            2 => Some(Self::CpuTime),
            3 => Some(Self::Timeout),
            4 => Some(Self::OpRate),
            _ => None,
        }
    }
}

/// Set once by whichever guard terminates an execution, and never cleared
/// for the life of the isolate.
///
/// Written from other threads - the watchdog and V8's heap callback - and
/// read by the runtime thread, hence the atomic. The first reason wins: a
/// heap cut-off that the watchdog then also times out is a memory recycle.
#[derive(Debug, Default)]
pub struct RecycleLatch(AtomicU8);

impl RecycleLatch {
    pub fn latch(&self, reason: RecycleReason) {
        let _ = self
            .0
            .compare_exchange(0, reason.code(), Ordering::AcqRel, Ordering::Acquire);
    }

    pub fn reason(&self) -> Option<RecycleReason> {
        RecycleReason::from_code(self.0.load(Ordering::Acquire))
    }
}

/// How many bytes of `ArrayBuffer` backing store one isolate may hold.
///
/// Backing stores live outside the V8 heap, so `heap_limits` never sees them:
/// before this existed, `new ArrayBuffer(2 **31)` in any tenant was an
/// allocation the whole node paid for. V8 asks the isolate's
/// `ArrayBuffer::Allocator` for every backing store it creates itself, and
/// this budget is that allocator's ledger. An allocation that would cross the
/// limit is refused - V8 then throws a `RangeError` in the application, and
/// nothing was allocated, so the isolate stays usable.
///
/// What it does not see: backing stores an op builds from Rust memory it
/// already owns (`with_backing_store`, used for bytes a `fetch`, blob or cache
/// read hands to JS), and the page-reserved memory behind resizable
/// `ArrayBuffer`s and WebAssembly memories, which V8 takes from its page
/// allocator rather than from this one.
#[derive(Debug)]
pub struct ArrayBufferBudget {
    used: AtomicUsize,
    limit: usize,
    /// Raised when an allocation is refused, lowered by the next one that
    /// succeeds or by [`Self::take_refusal`]. V8 answers a refused backing
    /// store with last-resort garbage collections, which reach the isolate's
    /// near-heap-limit callback; this is how that callback tells them apart
    /// from the heap itself running out.
    refusal_pending: AtomicBool,
}

impl ArrayBufferBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            used: AtomicUsize::new(0),
            limit,
            refusal_pending: AtomicBool::new(false),
        })
    }

    /// Whether an allocation was refused since the last successful one,
    /// consuming the answer.
    pub fn take_refusal(&self) -> bool {
        self.refusal_pending.swap(false, Ordering::AcqRel)
    }

    /// Bytes currently held.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    fn reserve(&self, len: usize) -> bool {
        let reserved = self
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(len).filter(|next| *next <= self.limit)
            })
            .is_ok();
        self.refusal_pending.store(!reserved, Ordering::Release);
        reserved
    }

    fn release(&self, len: usize) {
        self.used.fetch_sub(len, Ordering::AcqRel);
    }

    /// The allocator to hand V8 in the isolate's `CreateParams`.
    pub fn allocator(self: &Arc<Self>) -> deno_core::v8::UniqueRef<deno_core::v8::Allocator> {
        let handle = Arc::into_raw(self.clone());
        // SAFETY: `handle` is a live `Arc` whose strong count this call
        // transferred to V8; `drop_budget` below takes it back exactly once,
        // when V8 destroys the allocator. The vtable functions only touch the
        // budget and libc, never V8, as the allocator contract requires.
        unsafe { deno_core::v8::new_rust_allocator(handle, &VTABLE) }
    }
}

static VTABLE: deno_core::v8::RustAllocatorVtable<ArrayBufferBudget> =
    deno_core::v8::RustAllocatorVtable {
        allocate,
        allocate_uninitialized,
        free,
        drop: drop_budget,
    };

unsafe extern "C" fn allocate(budget: &ArrayBufferBudget, len: usize) -> *mut c_void {
    if !budget.reserve(len) {
        return std::ptr::null_mut();
    }
    // SAFETY: plain libc allocation; a zero-length request is rounded up so
    // a successful allocation is never mistaken for a refusal.
    let data = unsafe { libc::calloc(len.max(1), 1) };
    if data.is_null() {
        budget.release(len);
    }
    data
}

unsafe extern "C" fn allocate_uninitialized(budget: &ArrayBufferBudget, len: usize) -> *mut c_void {
    if !budget.reserve(len) {
        return std::ptr::null_mut();
    }
    // SAFETY: as in `allocate`.
    let data = unsafe { libc::malloc(len.max(1)) };
    if data.is_null() {
        budget.release(len);
    }
    data
}

unsafe extern "C" fn free(budget: &ArrayBufferBudget, data: *mut c_void, len: usize) {
    // SAFETY: V8 frees only what `allocate`/`allocate_uninitialized`
    // returned, with the length it asked for.
    unsafe { libc::free(data) };
    budget.release(len);
}

unsafe extern "C" fn drop_budget(handle: *const ArrayBufferBudget) {
    // SAFETY: the strong count `ArrayBufferBudget::allocator` leaked.
    drop(unsafe { Arc::from_raw(handle) });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_reason_latched_is_the_one_kept() {
        let latch = RecycleLatch::default();
        assert_eq!(latch.reason(), None);
        latch.latch(RecycleReason::Memory);
        latch.latch(RecycleReason::Timeout);
        assert_eq!(latch.reason(), Some(RecycleReason::Memory));
    }

    #[test]
    fn a_reservation_past_the_limit_is_refused_and_leaves_nothing_behind() {
        let budget = ArrayBufferBudget::new(100);
        assert!(budget.reserve(60));
        assert!(!budget.reserve(41));
        assert!(budget.take_refusal());
        assert!(!budget.take_refusal());
        assert_eq!(budget.used(), 60);
        assert!(budget.reserve(40));
        budget.release(100);
        assert_eq!(budget.used(), 0);
        assert!(!budget.reserve(usize::MAX));
    }
}
