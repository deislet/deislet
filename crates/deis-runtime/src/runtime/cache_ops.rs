//! `ctx.services.cache` and `globalThis.caches.default`: the Cache API.
//!
//! Edge Canon makes an HTTP response cache a *basic* feature
//! (`SPECIFICATION_BASIC.md` section 7), so every application may assume it
//! exists without declaring anything. There is nothing to declare here either:
//! the store is built with the isolate and lives entirely inside it.
//!
//! # What the scope of this cache actually is
//!
//! One store per isolate, and deis-runtime gives each `(application,
//! environment)` an isolate of its own on each node it is loaded on. So an
//! entry written by one request is visible to the next request for the same
//! application on the *same node*, and to nothing else: not to another node,
//! and not to the replacement isolate a new version is loaded into. That is
//! narrower than a shared cache tier and roughly what a Cloudflare colo gives
//! an application; a hit is an optimisation, never a place to keep state.
//!
//! # Freshness
//!
//! An entry carries the lifetime the response asked for. The JavaScript side
//! reads `s-maxage`, then `max-age`, off the response's `Cache-Control` and
//! passes it here as whole seconds; zero means the response said nothing and
//! the store's own default applies. Expiry is from the write, not from the
//! last read: reading a cached response does not make it fresher.
//!
//! # Why a body is bytes and the rest is a string
//!
//! An entry is split in two on purpose. The status and the headers are text
//! and travel as one JSON string; the body travels as a buffer, because a
//! response body is whatever the application put in it -- a PNG, a protobuf,
//! a gzip frame -- and a byte outside the ASCII range is not a character.
//! Carrying the body through a string meant `Cache.put` had to decode it as
//! UTF-8 first, which replaces every ill-formed sequence with U+FFFD: bytes
//! `0x80..=0xff` all came back as `ef bf bd`, and `caches.default` exists in
//! every isolate whether the application asked for it or not, so an image put
//! in the cache was served back as mojibake with nothing reporting an error.
//! Buffers in and out, the way `blob_ops.rs` carries bytes.

use deno_core::{extension, op2, JsBuffer, OpState, ToJsBuffer};
use moka::future::Cache;
use moka::Expiry;
use serde::Serialize;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One cached response, plus how long it asked to stay fresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheEntry {
    /// Everything about the response that is text: the status and the headers,
    /// serialised by the JavaScript side as one JSON object.
    pub meta: String,
    /// The response body, byte for byte as the application wrote it.
    pub body: Vec<u8>,
    /// Whole seconds of freshness taken from the response's `Cache-Control`.
    /// `None` means the response named no lifetime, so the store's default
    /// applies.
    pub ttl: Option<Duration>,
}

/// A hit, as `Cache.match` receives it.
///
/// Two fields rather than one because they are two different kinds of thing:
/// `meta` is JSON the JavaScript side parses, `body` arrives as a
/// `Uint8Array` and is handed straight to the `Response` constructor.
#[derive(Serialize)]
struct CachedResponse {
    meta: String,
    body: ToJsBuffer,
}

/// Cache storage type.
///
/// Wrapped in `Arc` because the ops reach it through `OpState` from an async
/// context and must not hold the state borrow across an await.
pub type CacheStore = Arc<Cache<String, CacheEntry>>;

/// Applies each entry's own lifetime, falling back to the store's default.
///
/// Only the create/update hooks are implemented. Leaving `expire_after_read`
/// at its default is the point: an HTTP cache entry goes stale a fixed time
/// after it was written, and a stream of reads must not keep a stale response
/// alive forever.
pub(crate) struct ResponseFreshness {
    /// Applied to an entry whose response named no `max-age`. `None` means
    /// such an entry never expires on its own.
    pub(crate) default_ttl: Option<Duration>,
}

impl Expiry<String, CacheEntry> for ResponseFreshness {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &CacheEntry,
        _created_at: Instant,
    ) -> Option<Duration> {
        value.ttl.or(self.default_ttl)
    }

    fn expire_after_update(
        &self,
        _key: &String,
        value: &CacheEntry,
        _updated_at: Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        // A second `put` for the same key is a fresh response, so its clock
        // starts again rather than inheriting what was left of the old one.
        value.ttl.or(self.default_ttl)
    }
}

/// Read a cached response.
///
/// # Returns
/// - `Some({ meta, body })`: hit, and still fresh.
/// - `None`: never written, evicted, or past its lifetime.
#[op2(async)]
#[serde]
async fn op_cache_get(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Option<CachedResponse> {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };
    cache.get(&key).await.map(|entry| CachedResponse {
        meta: entry.meta,
        body: ToJsBuffer::from(entry.body),
    })
}

/// Write a cached response.
///
/// `meta` is the JSON the JavaScript side built out of the status and the
/// headers; `body` is the response body as a `Uint8Array`, taken as a buffer
/// so that a body which is not text survives. `ttl_seconds` is the freshness
/// the response asked for; zero means it asked for none and the store's
/// default applies.
#[op2(async)]
async fn op_cache_set(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[string] meta: String,
    #[buffer] body: JsBuffer,
    ttl_seconds: u32,
) {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };

    let entry = CacheEntry {
        meta,
        body: body.to_vec(),
        ttl: if ttl_seconds == 0 {
            None
        } else {
            Some(Duration::from_secs(u64::from(ttl_seconds)))
        },
    };

    cache.insert(key, entry).await;
}

/// Drop a cached response.
///
/// Returns whether there was one to drop, which is what the Cache API's
/// `delete()` resolves to; answering `true` unconditionally would tell an
/// application it had invalidated something it never had.
#[op2(async)]
async fn op_cache_delete(state: Rc<RefCell<OpState>>, #[string] key: String) -> bool {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };
    cache.remove(&key).await.is_some()
}

/// Whether a key is present and still fresh.
#[op2(async)]
async fn op_cache_has(state: Rc<RefCell<OpState>>, #[string] key: String) -> bool {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };
    cache.contains_key(&key)
}

/// Drop every cached response of this isolate.
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

/// Cache statistics.
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

/// Create a `CacheStore`.
///
/// # Parameters
/// - `max_capacity`: entries kept before the least useful are evicted.
/// - `default_ttl_secs`: how long an entry whose response named no `max-age`
///   stays fresh. Zero means such entries only ever leave by eviction or by
///   `delete()`.
pub fn create_cache_store(max_capacity: u64, default_ttl_secs: u64) -> CacheStore {
    let default_ttl = if default_ttl_secs > 0 {
        Some(Duration::from_secs(default_ttl_secs))
    } else {
        None
    };

    Arc::new(
        Cache::builder()
            .max_capacity(max_capacity)
            .expire_after(ResponseFreshness { default_ttl })
            .build(),
    )
}

#[cfg(test)]
#[path = "cache_ops_test.rs"]
mod cache_ops_test;
