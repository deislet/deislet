// Cache Ops definitions
// Provides high-performance local caching based on moka

use deno_core::{extension, op2, OpState};
use moka::future::Cache;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// Cache storage type
/// Wrapped in Arc to support cross-thread sharing and async access
pub type CacheStore = Arc<Cache<String, String>>;

/// Cache Get operation
///
/// Get value from cache
///
/// # Parameters
/// - key: Cache key
///
/// # Returns
/// - Some(value): Cache hit
/// - None: Cache miss or expired
#[op2(async)]
#[string]
async fn op_cache_get(state: Rc<RefCell<OpState>>, #[string] key: String) -> Option<String> {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };
    cache.get(&key).await
}

/// Cache Set operation
///
/// Set cache value
///
/// # Parameters
/// - key: Cache key
/// - value: Cache value
/// - ttl: TTL in seconds, currently uses global TTL config, this parameter is reserved
///
/// # Note
/// Current version uses global TTL setting from Cache creation
/// Future versions will support per-entry TTL
#[op2(async)]
async fn op_cache_set(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[string] value: String,
    _ttl: u32,
) {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };

    cache.insert(key, value).await;
}

/// Cache Delete operation
///
/// Delete cache item
///
/// # Parameters
/// - key: Cache key
#[op2(async)]
async fn op_cache_delete(state: Rc<RefCell<OpState>>, #[string] key: String) {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };
    cache.invalidate(&key).await;
}

/// Cache Has operation
///
/// Check if cache key exists
///
/// # Parameters
/// - key: Cache key
///
/// # Returns
/// - true: Exists and not expired
/// - false: Does not exist or expired
#[op2(async)]
async fn op_cache_has(state: Rc<RefCell<OpState>>, #[string] key: String) -> bool {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };
    cache.contains_key(&key)
}

/// Cache Clear operation
///
/// Clear all caches
#[op2(async)]
async fn op_cache_clear(state: Rc<RefCell<OpState>>) {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };
    cache.invalidate_all();
    // Wait for cleanup completion
    cache.run_pending_tasks().await;
}

/// Cache statistics
///
/// Returns cache statistics
///
/// # Returns
/// JSON object containing:
/// - entry_count: Current number of entries
/// - weighted_size: Weighted size
#[op2(async)]
#[serde]
async fn op_cache_stats(state: Rc<RefCell<OpState>>) -> serde_json::Value {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };

    // Ensure pending tasks complete to get accurate statistics
    cache.run_pending_tasks().await;

    serde_json::json!({
        "entry_count": cache.entry_count(),
        "weighted_size": cache.weighted_size(),
    })
}

// Define cache extension
extension!(
    deis_runtime_cache_ops,
    ops = [
        op_cache_get,
        op_cache_set,
        op_cache_delete,
        op_cache_has,
        op_cache_clear,
        op_cache_stats,
    ],
);

/// Initialize cache ops extension
pub fn init_cache_extension() -> deno_core::Extension {
    deis_runtime_cache_ops::init()
}

/// Create CacheStore instance
///
/// # Parameters
/// - max_capacity: Maximum capacity (number of entries)
/// - tti: Time-to-idle (seconds), 0 for none
///
/// # Returns
/// CacheStore instance
pub fn create_cache_store(max_capacity: u64, tti: u64) -> CacheStore {
    let mut builder = Cache::builder().max_capacity(max_capacity);

    if tti > 0 {
        builder = builder.time_to_idle(Duration::from_secs(tti));
    }

    Arc::new(builder.build())
}

#[cfg(test)]
#[path = "cache_ops_test.rs"]
mod cache_ops_test;
