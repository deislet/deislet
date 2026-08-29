//! One isolate host, shared by every test that needs a loaded application.
//!
//! V8 is process-wide. Two tests standing up their own runtime pool at the
//! same time do not fail independently - they abort the whole test binary
//! from inside V8 - so tests that need a real application take turns on one
//! host instead of building their own.

use std::sync::{Arc, Mutex, MutexGuard};

use once_cell::sync::Lazy;

use super::ApplicationManager;

/// One thread started up front. The pool starts another whenever a test loads
/// more applications than it has threads, because an application gets a
/// runtime thread to itself - two isolates cannot share one.
static HOST: Lazy<Arc<ApplicationManager>> =
    Lazy::new(|| Arc::new(ApplicationManager::with_threads(Some(1))));

static TURN: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

/// The shared host. Load applications under ids only your test uses, and
/// unload them when it ends.
pub(crate) fn host() -> Arc<ApplicationManager> {
    HOST.clone()
}

/// Take the host for the duration of one test.
///
/// A poisoned lock is taken anyway: it only means an earlier test panicked,
/// and refusing every later test would hide that test's own result behind
/// somebody else's failure.
pub(crate) fn take() -> MutexGuard<'static, ()> {
    TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
