//! `ctx.services.cache`, `globalThis.caches.default` and `caches.open(name)`:
//! the Cache API.
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
//! **This is the product decision, not a temporary state.** A cache shared
//! across the nodes of an installation would be a storage service of its own -
//! something to write to, replicate, invalidate and operate - and it would
//! have the failure modes of one, on the path of every request that reads it.
//! What is here instead is memory the isolate already has, and the price of
//! that is written into the API rather than left to be discovered: see
//! [`CacheScope`], which an application reads as `caches.scope`. An
//! application that must not lose an entry is holding data, not a cache, and
//! belongs in KV or blob storage.
//!
//! # Named caches
//!
//! `caches.open(name)` answers a cache of its own. There is one store, and a
//! name is a namespace inside it: keys are composed by [`namespaced`], which
//! is length-prefixed so no two `(name, key)` pairs can ever produce the same
//! composed key. Sharing the store is what keeps the isolate's memory ceiling
//! one number rather than one per name an application invents.
//!
//! Nothing here knows which name is the default one. `caches.default` and
//! `ctx.services.cache` are one object in `js/runtime.js` holding one name, so
//! the two spellings cannot come apart; these ops take whatever name they are
//! given and would treat `"default"` as an ordinary namespace if an
//! application opened it by hand -- which is the right answer, because it is
//! the same cache.
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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// What an application is told about the cache it is using, published as
/// `caches.scope`.
///
/// It exists so that the decision in this module's header is *checkable* from
/// inside an application rather than only readable in a document. A cache
/// that dies with the isolate has two kinds of miss, and they mean opposite
/// things:
///
/// * the entry was written into **this** store and has since been evicted or
///   gone stale. The application's own reasoning still holds; it refetches
///   and carries on;
/// * the store itself is new - a version was deployed, the node restarted,
///   or the request landed on a different node. Everything ever written is
///   gone at once, and an application that was using the cache to remember
///   whether it had already done something is about to do it again.
///
/// Without a way to tell those apart an application cannot notice the second
/// case at all, and the bug it builds on top surfaces only on the day of a
/// deployment. [`Self::id`] is what tells them apart: record it beside
/// anything cached that a decision depends on, and a value that no longer
/// matches means the whole cache went with the old isolate.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheScope {
    /// How far this cache reaches. Always `"isolate"` today, and named rather
    /// than implied so that an application testing for it keeps working if a
    /// wider cache is ever offered beside this one.
    pub kind: &'static str,
    /// This store's identity, fresh for every isolate. Two reads that see the
    /// same value were served by the same store; two that do not have nothing
    /// in common, however similar their misses look.
    pub id: String,
    /// Unix millisecond this store began, which is when its isolate was
    /// built. Nothing written before this instant survives.
    pub since: u64,
    /// Entries this store holds before the least useful are evicted, and the
    /// freshness applied to a response that named none. Both are the node's
    /// numbers, published because an application sizing what it caches is
    /// otherwise guessing at them.
    pub max_entries: u64,
    pub default_ttl_seconds: u64,
}

/// The Cache API's store for one isolate: the entries plus the identity of
/// the store holding them.
pub struct IsolateCache {
    entries: Cache<String, CacheEntry>,
    scope: CacheScope,
}

impl IsolateCache {
    /// What `caches.scope` reports.
    pub fn scope(&self) -> &CacheScope {
        &self.scope
    }

    /// Read one entry by the pair it was stored under.
    ///
    /// The tests are the only caller: everything else reaches the store
    /// through the ops, and this is how a test asks what an op actually wrote
    /// without having to compose the namespaced key itself.
    #[cfg(test)]
    pub(crate) async fn entry(&self, name: &str, key: &str) -> Option<CacheEntry> {
        self.entries.get(&namespaced(name, key)).await
    }

    /// How many entries this store holds, after pending maintenance.
    #[cfg(test)]
    pub(crate) async fn len(&self) -> u64 {
        self.entries.run_pending_tasks().await;
        self.entries.entry_count()
    }
}

/// Cache storage type.
///
/// Wrapped in `Arc` because the ops reach it through `OpState` from an async
/// context and must not hold the state borrow across an await.
pub type CacheStore = Arc<IsolateCache>;

/// The key one `(cache name, request key)` pair is stored under.
///
/// Length-prefixed, so the composition is injective: `("ab", "c")` becomes
/// `2:ab:c` and `("a", "b:c")` becomes `1:a:b:c`, and no pair of names and
/// keys can be made to collide. A plain `name + ":" + key` could, and an
/// application that opened a cache named after user input would then be able
/// to read and overwrite another of its own caches -- a correctness failure
/// that would look like the cache occasionally returning the wrong page.
fn namespaced(name: &str, key: &str) -> String {
    format!("{}:{}:{}", name.len(), name, key)
}

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
/// - `None`: never written, evicted, past its lifetime, or written into a
///   store this isolate replaced. `caches.scope.id` is what tells the last of
///   those from the others; see [`CacheScope`].
#[op2]
#[serde]
async fn op_cache_get(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] key: String,
) -> Option<CachedResponse> {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };
    cache
        .entries
        .get(&namespaced(&name, &key))
        .await
        .map(|entry| CachedResponse {
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
#[op2]
async fn op_cache_set(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
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

    cache.entries.insert(namespaced(&name, &key), entry).await;
}

/// Drop a cached response.
///
/// Returns whether there was one to drop, which is what the Cache API's
/// `delete()` resolves to; answering `true` unconditionally would tell an
/// application it had invalidated something it never had.
#[op2]
async fn op_cache_delete(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] key: String,
) -> bool {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };
    cache
        .entries
        .remove(&namespaced(&name, &key))
        .await
        .is_some()
}

/// Drop every entry of one named cache, answering how many there were.
///
/// Behind `caches.delete(name)`. A named cache here is a namespace rather
/// than an object that was created, so "did this cache exist?" can only be
/// answered as "did it hold anything?", and the count is what lets the
/// JavaScript side say so honestly.
///
/// It walks the store rather than asking moka to invalidate by predicate,
/// because `invalidate_entries_if` is applied lazily -- an entry it has
/// marked is still returned by a `get` until maintenance runs -- and a
/// `delete` that leaves the responses readable is worse than none. Walking is
/// linear in the whole store, which is affordable for an operation an
/// application performs deliberately and rarely.
#[op2]
async fn op_cache_clear(state: Rc<RefCell<OpState>>, #[string] name: String) -> u32 {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };

    let prefix = namespaced(&name, "");
    let doomed: Vec<Arc<String>> = cache
        .entries
        .iter()
        .map(|(key, _)| key)
        .filter(|key| key.starts_with(&prefix))
        .collect();

    let dropped = doomed.len() as u32;
    for key in doomed {
        cache.entries.remove(key.as_str()).await;
    }
    // So that the entry the caller deletes is gone by the time the promise
    // resolves, rather than at whatever later moment moka gets round to it.
    cache.entries.run_pending_tasks().await;
    dropped
}

/// What kind of cache this is, how long it has existed, and what it holds.
///
/// Read once per isolate by `js/runtime.js` and published as `caches.scope`.
/// See [`CacheScope`] for why an application needs it.
#[op2]
#[serde]
fn op_cache_scope(state: &mut OpState) -> CacheScope {
    state.borrow::<CacheStore>().scope().clone()
}

// Define cache extension
extension!(
    deis_runtime_cache_ops,
    ops = [
        op_cache_get,
        op_cache_set,
        op_cache_delete,
        op_cache_clear,
        op_cache_scope,
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
///
/// The identity in [`CacheScope`] is minted here, which is what makes it mean
/// "this store": a new one is created exactly when an isolate is, so a
/// deployment, a restart or a request served by another node all produce a
/// value the application has not seen before.
pub fn create_cache_store(max_capacity: u64, default_ttl_secs: u64) -> CacheStore {
    let default_ttl = if default_ttl_secs > 0 {
        Some(Duration::from_secs(default_ttl_secs))
    } else {
        None
    };

    Arc::new(IsolateCache {
        entries: Cache::builder()
            .max_capacity(max_capacity)
            .expire_after(ResponseFreshness { default_ttl })
            .build(),
        scope: CacheScope {
            kind: "isolate",
            id: uuid::Uuid::new_v4().to_string(),
            since: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_millis() as u64)
                .unwrap_or(0),
            max_entries: max_capacity,
            default_ttl_seconds: default_ttl_secs,
        },
    })
}

#[cfg(test)]
#[path = "cache_ops_test.rs"]
mod cache_ops_test;
