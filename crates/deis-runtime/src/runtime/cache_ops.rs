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
//! is length-prefixed at every join so no two `(name, caller, key)` triples
//! can ever produce the same composed key. Sharing the store is what keeps the
//! isolate's memory ceiling one number rather than one per name an application
//! invents.
//!
//! These ops take whatever name they are given and know nothing about which
//! one is the default. `caches.default` and `ctx.services.cache` are one
//! object in `js/runtime.js` holding one name, so the two spellings cannot
//! come apart -- and that name is refused to `caches.open` and
//! `caches.delete`, because an application opening a cache under a name it
//! took from a request would otherwise hand a caller who sent `"default"` the
//! namespace the rest of that application caches into.
//!
//! # Freshness, and what is not stored at all
//!
//! An entry carries the lifetime the response asked for. The JavaScript side
//! reads `s-maxage`, then `max-age`, then `Expires`, and passes the result
//! here as whole seconds; zero means the response named no lifetime and the
//! store's own default applies. Expiry is from the write, not from the last
//! read: reading a cached response does not make it fresher.
//!
//! The same three fields decide whether the response reaches this module at
//! all. This is a *shared* cache -- one store per isolate, read by every later
//! request to the same application, from every client -- so `js/runtime.js`
//! refuses `no-store`, `private`, `no-cache`, a shared lifetime of zero, an
//! `Expires` that has already passed, and `Pragma: no-cache` where there is no
//! `Cache-Control` to override it. The first two are what RFC 9111 says for a
//! shared cache. The rest all mean "check with the origin before reusing
//! this", and there is nothing here that can check: no revalidation, no
//! conditional request, no upstream to ask. An entry that may only be served
//! after a check that cannot be made is one that may never be served, so it is
//! refused rather than kept for the default lifetime -- which is what used to
//! happen to every `no-cache`, every `max-age=0`, and every response that said
//! in `Expires` that it had gone stale an hour ago.
//!
//! # The rule
//!
//! **An entry is stored only when this platform can see that it is safe to
//! hand to a caller it cannot identify: every header on the request being
//! answered and on the key is one of a closed list that cannot single a caller
//! out, or one the response files the entry under with `Vary`; and every
//! header on the response is one of a closed list that describes the
//! representation. Anything else is refused -- including every name neither
//! list has heard of -- and an application that wants a per-caller answer
//! cached says whose it is on the call.**
//!
//! ## Why it is written that way round
//!
//! Because the other way round does not converge. Four rounds of adversarial
//! review found a cross-user disclosure here, each repair was correct for what
//! it covered, and each left the next name to be found: repeated `Set-Cookie`
//! collapsed onto one, then `Vary` unread, then the request's method unread,
//! then a credential rule that named `Authorization` and nothing else -- so
//! `Cookie`, which is how most of the web authenticates, walked past it and a
//! private dashboard went out to the next anonymous caller together with its
//! `HttpOnly` session cookie. The pattern is the finding: the set of headers
//! that can carry an identity, and the set of response headers that carry
//! per-caller state, are both open-ended, and a cache that decides by
//! enumerating what is unsafe is always one name short of the next round.
//!
//! So the lists in `js/runtime.js` are lists of what is *safe*
//! (`HEADERS_THAT_CANNOT_NAME_A_CALLER`, `HEADERS_A_STRANGER_MAY_RECEIVE`),
//! and a name on neither of them is refused. They are arguable name by name,
//! and every argument about them is settled in the direction of refusing to
//! store something rather than the direction of handing one caller's page to
//! the next.
//!
//! ## The four questions, and the whole of the answer to each
//!
//! They are enforced in one place -- `Cache.put`, `Cache.match` and
//! `Cache.delete` in `js/runtime.js`, which read the predicates written
//! directly above them -- rather than one rule per axis somebody happened to
//! look at. Every refusal names itself and names the fix, because a `put` that
//! quietly does nothing leaves the application believing it has a cache.
//!
//! ### How a refusal reaches the application
//!
//! `put` **throws** when the call is not one this API accepts, and **answers**
//! `{ stored: false, reason }` when the call is fine but this response may not
//! be kept. The line between them is not a matter of severity; it is whether
//! the application can fix it by editing a line:
//!
//! * an options bag that is not an object, an option this cache will not
//!   honour, a `forCaller` that is not a non-empty string, a key that is not a
//!   `Request`/`URL`/string, a key whose method is not GET -- all the same on
//!   every request, all a mistake in the source, all thrown;
//! * everything read off the response or off who is asking -- `Vary: *`, a
//!   206, `private`, a `Set-Cookie`, a caller carrying a header nothing files
//!   the entry under, an entry larger than the store -- answered.
//!
//! The second list is the *ordinary* outcome, which is why it may not be an
//! exception. After question 3 below, any handler serving a browser that sends
//! a session cookie is refused unless it says whose answer it is; while those
//! threw, an uncaught one killed the invocation, so a `put` made after the
//! handler had already computed its answer threw that answer away and the
//! client got a 500. This platform's own example did exactly that, and the
//! thing that exercised it sent no cookies, so nothing saw it. A failed cache
//! write is not a reason to fail the request the cache was there to speed up.
//!
//! Nothing is silent either way: a refusal is written to the application's log
//! at warn as well as answered, so a `put` whose report is ignored is still
//! visible in `deis tail`.
//!
//! ### 1. What an entry is an answer to
//!
//! One GET, and nothing else. The Cache API's key is a `Request`, and a
//! `Request` carries a method: an entry is the answer to *that request*, not
//! to its URL. The answer to `POST /orders` is the result of an action rather
//! than a representation of anything, so a `put` keyed by any other method is
//! refused, and a `match` or a `delete` for one finds nothing unless the
//! caller passes `{ ignoreMethod: true }` and asks for the GET entry by name.
//! A key given as a string or a `URL` is the spelling for `new Request(url)`,
//! which is a GET.
//!
//! This was missing outright: the store was keyed by the URL alone, so a
//! handler that cached the answer to a POST handed that answer to every later
//! GET of the same address, whoever sent it.
//!
//! ### 2. What may be stored
//!
//! * **A whole answer.** A status of 200-599 -- 101 and the rest of the 1xx
//!   range are not answers, and a stored 101 would come back out of `match` as
//!   an upgrade nothing can perform. Not `206 Partial Content` and nothing
//!   carrying `Content-Range`: those are one range of a body, and there is
//!   nothing here that can join ranges or match a `Range` header against a
//!   stored one. Not `304 Not Modified`: it answers a conditional request by
//!   saying the caller already has the body, and this cache has no validator
//!   to check.
//! * **Not one the response itself excludes**: `no-store`, `private`,
//!   `no-cache`, a shared lifetime of zero, a past `Expires`, `Pragma:
//!   no-cache`. See the section on freshness above.
//! * **Nothing whose reader this store cannot tell from another**: question 3.
//! * **Nothing that would give its reader something that is somebody's**:
//!   question 4.
//! * **Nothing so large that admitting it would empty the store**; see
//!   [`CacheScope::max_entry_bytes`].
//!
//! ### 3. Whether the answer could be one caller's
//!
//! Two requests are read, not one. The key says what the entry will be filed
//! under; the request this invocation is answering says what the response
//! could have been *about*, because a handler serving a caller who sent a
//! session cookie may produce anything from that cookie whatever key the
//! application then writes the answer under. Every header on either of them
//! that is not on the closed safe list has to be accounted for: the response
//! must name it in `Vary`, and the key must carry the same value for it as the
//! request being answered.
//!
//! A response that carries `Vary` says its body depends on named headers of
//! the request. The values those names had are stored *with* the entry, so the
//! store holds one variant per key -- a second `put` replaces the first --
//! which costs a hit and never costs a wrong answer. The property that makes a
//! hit safe is that **the variant an entry is filed under was stated by
//! somebody, never inferred by this platform**, so `Vary: *` is refused (no
//! later request may reuse it), a varying response under a bare URL is refused
//! (nothing to read the values off), and a varying response under a `Request`
//! carrying no value for one of those names is refused unless that `Request`
//! is the one being answered -- where "carries none of them" is not a claim but
//! the truth.
//!
//! `public` and `s-maxage` say nothing here. RFC 9111 lists them as ways a
//! response may permit a shared cache to reuse an authenticated answer, and
//! that is exactly what made them dangerous: they are what an upstream, a
//! framework or a copied header produces by accident, and the refusal message
//! used to recommend them, so a developer following this platform's own
//! written instruction created the disclosure it had just refused. A directive
//! is a lifetime now, and nothing more.
//!
//! ### 4. What the answer would give its reader
//!
//! A response does not only *depend* on a caller; it can also *establish*
//! something for one, and no `Vary` can make that safe because `Vary`
//! describes what the request decided. A `Set-Cookie` replayed out of this
//! store hands every later caller the session the first one was given -- which
//! is session fixation, not disclosure -- and the family it belongs to
//! (`Set-Cookie2`, `Clear-Site-Data`, `Authentication-Info`, whatever is
//! defined next, plus every per-request token an application puts in a header
//! of its own naming) has no more of an end than the credential family does.
//! So the same sentence in the other direction: a stored response may carry
//! only what describes the representation, how long it is good for, or a
//! policy that is the same for every caller of the site.
//!
//! ## The two statements, and why they are arguments
//!
//! An application that genuinely wants a per-caller answer cached says so in a
//! third argument to `put`, and reads it back by naming the same caller:
//!
//! ```text
//! await cache.put(request, response, { forCaller: userId });
//! const hit = await cache.match(request, { forCaller: userId });
//! ```
//!
//! The entry is filed under that string -- it is part of the composed key, see
//! [`namespaced`] -- so it is never handed to a `match` that names another
//! caller, and the checks that keep one caller's answer away from another do
//! not arise. The other statement, `{ forAnyCaller: true }`, is how an
//! application says an answer does not depend on the caller at all, which is
//! what a handler needs when it is serving an identified caller and caching
//! something shared. It does not waive question 4: a response that hands its
//! reader a session and an application saying the answer suits everybody
//! contradict each other, and a contradiction is refused.
//!
//! Both are arguments rather than headers, and that is the whole point of
//! them. `Cache-Control: public` can be produced by an upstream, a framework,
//! a proxy or a copied line; a third argument in this application's own source
//! cannot be produced by accident.
//!
//! ## What it costs
//!
//! An application relying on the old behaviour loses these, and is told so at
//! the call rather than left to find out:
//!
//! * caching the answer to any request carrying a header this platform does
//!   not recognise -- a `Cookie`, an `Origin`, an `X-Forwarded-For`, a vendor
//!   header a CDN in front adds -- unless the response varies on it or the
//!   application makes one of the two statements;
//! * caching an upstream response verbatim when the upstream decorates it
//!   (`x-amz-request-id`, `cf-ray`, `server-timing`): store a copy carrying
//!   the headers it means to serve;
//! * `public` and `s-maxage` as permission to store anything;
//! * `caches.open("default")` and `caches.delete("default")`, which now throw;
//! * `match`/`delete` quietly ignoring `ignoreVary`, `ignoreSearch` and
//!   `cacheName`, which are now refused by name.
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

use super::ops::AppId;
use deno_core::{extension, op2, JsBuffer, OpState, ToJsBuffer};
use deno_error::JsErrorBox;
use moka::future::Cache;
use moka::Expiry;
use serde::Serialize;
use std::cell::Cell;
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
    /// Bytes this store holds before the least useful are evicted.
    ///
    /// The ceiling that actually bounds what this cache costs the node: an
    /// entry count says nothing about size, and a response body is whatever
    /// the application put in it. Published for the same reason the entry
    /// count is -- an application deciding what is worth caching is otherwise
    /// guessing -- and it is derived from the isolate's own memory limit, so
    /// an application that asked for more heap gets a larger cache with it.
    pub max_bytes: u64,
    /// The largest single response this store will accept. A `put` of
    /// anything above it is refused rather than admitted, because admitting
    /// it would evict most of the store to make room for one entry.
    pub max_entry_bytes: u64,
}

/// The Cache API's store for one isolate: the entries plus the identity of
/// the store holding them.
pub struct IsolateCache {
    entries: Cache<String, CacheEntry>,
    scope: CacheScope,
}

/// What one entry costs the store, in the units its ceiling is counted in.
///
/// The bytes it holds, plus a floor of one entry's share of the whole store.
/// The floor is what keeps the entry ceiling meaningful once the ceiling is
/// counted in bytes: without it, an application could hold millions of empty
/// responses, and the cost of an entry is not only its body -- it is a key
/// string, three allocations and moka's own bookkeeping, none of which the
/// body's length describes. With it, a store of `max_bytes` can hold at most
/// `max_entries` of anything, and at most `max_bytes` worth of bodies.
///
/// `u32` because that is what moka weighs in. Nothing this large can be
/// stored anyway: a single entry is refused far below it (see
/// [`CacheScope::max_entry_bytes`]), and a body arrives from an isolate whose
/// whole heap is smaller than 4 GiB.
fn entry_weight(key: &str, entry: &CacheEntry, min_weight: u32) -> u32 {
    let held = key
        .len()
        .saturating_add(entry.meta.len())
        .saturating_add(entry.body.len());
    u32::try_from(held).unwrap_or(u32::MAX).max(min_weight)
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
    pub(crate) async fn entry(&self, name: &str, caller: &str, key: &str) -> Option<CacheEntry> {
        self.entries.get(&namespaced(name, caller, key)).await
    }

    /// How many entries this store holds, after pending maintenance.
    #[cfg(test)]
    pub(crate) async fn len(&self) -> u64 {
        self.entries.run_pending_tasks().await;
        self.entries.entry_count()
    }

    /// What this store is holding, in the units its ceiling is counted in.
    ///
    /// Tests only, and it is the number the ceiling is about: entries say
    /// nothing about what a store costs the node.
    #[cfg(test)]
    pub(crate) async fn held_bytes(&self) -> u64 {
        self.entries.run_pending_tasks().await;
        self.entries.weighted_size()
    }
}

/// Cache storage type.
///
/// Wrapped in `Arc` because the ops reach it through `OpState` from an async
/// context and must not hold the state borrow across an await.
pub type CacheStore = Arc<IsolateCache>;

/// Everything one named cache holds, whichever caller it was filed under.
///
/// The prefix `namespaced` builds on, and what `op_cache_clear` walks: a
/// `caches.delete(name)` empties that namespace for every caller at once,
/// which is what an application asking for the namespace to be empty means.
fn namespace_prefix(name: &str) -> String {
    format!("{}:{}:", name.len(), name)
}

/// The key one `(cache name, caller, request key)` triple is stored under.
///
/// Length-prefixed at both joins, so the composition is injective:
/// `("ab", "", "c")` becomes `2:ab:0::c` and `("a", "", "b:c")` becomes
/// `1:a:0::b:c`, and no triple can be made to collide with another. A plain
/// `name + ":" + key` could, and an application that opened a cache named
/// after user input would then be able to read and overwrite another of its
/// own caches -- a correctness failure that would look like the cache
/// occasionally returning the wrong page.
///
/// `caller` is empty for the ordinary entry, which is one any caller may be
/// handed, and is the string the application named in `{ forCaller }` for an
/// entry that is one caller's. Composed here rather than in `js/runtime.js`
/// so that there is one composition and one test of it: an application that
/// names its callers from its own data would otherwise be able to pick a
/// string that reads as another caller's key, which is the same collision
/// one join up.
fn namespaced(name: &str, caller: &str, key: &str) -> String {
    format!(
        "{}{}:{}:{}",
        namespace_prefix(name),
        caller.len(),
        caller,
        key
    )
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
    #[string] caller: String,
    #[string] key: String,
) -> Option<CachedResponse> {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };
    cache
        .entries
        .get(&namespaced(&name, &caller, &key))
        .await
        .map(|entry| CachedResponse {
            meta: entry.meta,
            body: ToJsBuffer::from(entry.body),
        })
}

/// Read one entry's `meta` without its body.
///
/// `Cache.delete` has to know what variant an entry was filed under before it
/// may remove it -- a caller who does not have that variant is deleting
/// somebody else's entry -- and the variant is in `meta`. Reading it through
/// `op_cache_get` would copy the whole body out of the store to answer a
/// question about its headers, which for a cached image is megabytes moved to
/// decide one comparison.
#[op2]
#[string]
async fn op_cache_meta(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] caller: String,
    #[string] key: String,
) -> Option<String> {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };
    cache
        .entries
        .get(&namespaced(&name, &caller, &key))
        .await
        .map(|entry| entry.meta)
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
    #[string] caller: String,
    #[string] key: String,
    #[string] meta: String,
    #[buffer] body: JsBuffer,
    ttl_seconds: u32,
) -> Result<(), JsErrorBox> {
    let (cache, app_id) = {
        let state_borrow = state.borrow();
        (
            state_borrow.borrow::<CacheStore>().clone(),
            state_borrow
                .try_borrow::<AppId>()
                .map(|id| id.0.clone())
                .unwrap_or_default(),
        )
    };

    let composed = namespaced(&name, &caller, &key);
    let entry = CacheEntry {
        meta,
        body: body.to_vec(),
        ttl: if ttl_seconds == 0 {
            None
        } else {
            Some(Duration::from_secs(u64::from(ttl_seconds)))
        },
    };

    // Refused, not admitted and then paid for by everything else in the
    // store. Checked on the bytes this entry actually holds rather than on the
    // weight, because the weight has a floor and would make a small entry
    // read as large in the message the application is given.
    let held = (composed.len() + entry.meta.len() + entry.body.len()) as u64;
    let ceiling = cache.scope.max_entry_bytes;
    if held > ceiling {
        tracing::debug!(
            target: "app",
            app_id = %app_id,
            cache = %name,
            entry_bytes = held,
            max_entry_bytes = ceiling,
            "Refusing to cache a {} byte response for {}: one entry may hold at most {} bytes \
             of this store's {} byte ceiling",
            held,
            app_id,
            ceiling,
            cache.scope.max_bytes
        );
        // The reason alone: `js/runtime.js` names the call and says that
        // nothing was stored, for this refusal and for the dozen it states
        // itself. Two halves of one sentence written in two places is how they
        // come to disagree.
        return Err(JsErrorBox::type_error(format!(
            "this entry is {held} bytes of body, headers and key, and one entry of this cache \
             may hold at most {ceiling} bytes (the whole cache holds {})",
            cache.scope.max_bytes
        )));
    }

    cache.entries.insert(composed, entry).await;
    Ok(())
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
    #[string] caller: String,
    #[string] key: String,
) -> bool {
    let cache = {
        let state_borrow = state.borrow();
        state_borrow.borrow::<CacheStore>().clone()
    };
    cache
        .entries
        .remove(&namespaced(&name, &caller, &key))
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

    let prefix = namespace_prefix(&name);
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

/// How many stretches of application code this isolate has begun or finished.
///
/// `runtime/isolate.rs` steps it at both ends of every invocation --
/// `begin_invocation` and `end_invocation`, which every runner on this
/// platform passes through -- so the number identifies one stretch and the
/// number between two stretches belongs to neither of them.
///
/// # Why the cache is the thing that needs a counter
///
/// `Cache.put` is the one call on this platform whose answer depends on *what
/// the code making it is answering*: a handler serving a caller who sent a
/// session cookie may produce anything from that cookie, whatever key it then
/// writes the answer under, so the request being answered is read alongside
/// the key. `js/runtime.js` used to keep that request in a module-level
/// variable, set when a request invocation built its context and cleared at
/// the top of the three invocation entry points. Nothing cleared it when an
/// invocation *ended*, and four runners never set it at all, so every stretch
/// of application code that runs between invocations -- a WebSocket
/// `onmessage`, a response-stream producer, a timer -- read the last request
/// this isolate happened to serve. Which request that was decided whether a
/// `put` was refused or stored, so the same handler was refused when nothing
/// had run in between and stored after one unrelated anonymous `GET`, and the
/// entry it then stored was handed to every later anonymous caller.
///
/// A clear at the end of each runner would have been a fifth entry in a list
/// that was already missing four. This is the other shape: what the runtime
/// states about the request being answered is stamped with this number, a
/// statement whose number is not the current one is not readable at all, and
/// a runner that states nothing therefore cannot inherit a request -- it is
/// told it does not know, which `Cache.put` refuses rather than guesses at.
/// Nothing has to remember to clear anything.
///
/// Held as an `Rc<Cell<u64>>` so `runtime/isolate.rs` can step it without
/// borrowing the whole op state; the isolate keeps one handle and the op state
/// keeps the other.
#[derive(Clone, Default)]
pub(crate) struct InvocationCounter(Rc<Cell<u64>>);

impl InvocationCounter {
    /// One more stretch has begun or ended. Called from both ends of every
    /// invocation, so the count is of boundaries rather than of invocations.
    pub(crate) fn step(&self) {
        self.0.set(self.0.get().wrapping_add(1));
    }

    /// Which stretch is running now.
    fn now(&self) -> u64 {
        self.0.get()
    }
}

/// The number of the stretch of application code that is running.
///
/// Read by `js/runtime.js` whenever it states, or reads back, which request
/// the code now running is answering. See [`InvocationCounter`] for why that
/// question needs a number at all.
///
/// Answered as a `f64` because that is what a JavaScript number is. It is
/// exact to 2^53, which is more invocations than an isolate that lived for the
/// age of the universe could serve, and the value is only ever compared for
/// equality with one read moments earlier.
#[op2(fast)]
fn op_cache_invocation(state: &mut OpState) -> f64 {
    state
        .try_borrow::<InvocationCounter>()
        .map(|counter| counter.now() as f64)
        // An isolate built without the counter would otherwise report the same
        // number for every stretch, which is the inheritance this exists to
        // prevent. `f64::NAN` is not equal to itself, so a statement made
        // under it can never be read back: the cache falls to "nothing here
        // says what this is answering", which is refused rather than guessed.
        .unwrap_or(f64::NAN)
}

// Define cache extension
extension!(
    deis_runtime_cache_ops,
    ops = [
        op_cache_get,
        op_cache_meta,
        op_cache_set,
        op_cache_delete,
        op_cache_clear,
        op_cache_scope,
        op_cache_invocation,
    ],
);

/// Initialize cache ops extension
pub fn init_cache_extension() -> deno_core::Extension {
    deis_runtime_cache_ops::init()
}

/// How much of a store one entry may take before a `put` is refused rather
/// than admitted.
///
/// A quarter. An entry above it could only be admitted by evicting at least
/// three quarters of everything else, which is not caching -- it is one
/// response taking the store away from all the others, once per `put`. The
/// application is told instead, with both numbers in the message, because
/// "your response is too big for this cache" is something it can act on and a
/// silent mass eviction is not.
const ENTRY_SHARE_OF_STORE: u64 = 4;

/// Create a `CacheStore`.
///
/// # Parameters
/// - `max_entries`: entries kept before the least useful are evicted.
/// - `max_bytes`: bytes kept before the least useful are evicted. Both
///   ceilings hold at once; see [`entry_weight`] for how one counter enforces
///   the pair.
/// - `default_ttl_secs`: how long an entry whose response named no `max-age`
///   stays fresh. Zero means such entries only ever leave by eviction or by
///   `delete()`.
///
/// # Why there is a byte ceiling at all
///
/// This store is Rust memory beside the isolate, not inside the V8 heap that
/// `memoryMb` governs, and an entry count says nothing about size. Bounded by
/// entries alone, one tenant could hold `max_entries` responses of any size
/// each -- ten thousand one-megabyte responses is ten gigabytes -- and grow
/// the whole node's process without ever coming near its own memory limit or
/// anything reporting a fault. The caller derives `max_bytes` from that same
/// memory limit, so what an application costs this node stays one number an
/// operator can multiply by the number of tenants.
///
/// The identity in [`CacheScope`] is minted here, which is what makes it mean
/// "this store": a new one is created exactly when an isolate is, so a
/// deployment, a restart or a request served by another node all produce a
/// value the application has not seen before.
pub fn create_cache_store(max_entries: u64, max_bytes: u64, default_ttl_secs: u64) -> CacheStore {
    let default_ttl = if default_ttl_secs > 0 {
        Some(Duration::from_secs(default_ttl_secs))
    } else {
        None
    };

    // One entry's share of the store, and the least any entry may cost. See
    // [`entry_weight`]: it is what keeps `max_entries` true once the ceiling
    // is counted in bytes.
    let min_weight = u32::try_from(max_bytes / max_entries.max(1))
        .unwrap_or(u32::MAX)
        .max(1);
    let max_entry_bytes = max_bytes / ENTRY_SHARE_OF_STORE;

    Arc::new(IsolateCache {
        entries: Cache::builder()
            // Counted in the units `entry_weight` answers in, which are
            // bytes, not entries.
            .max_capacity(max_bytes)
            .weigher(move |key: &String, entry: &CacheEntry| entry_weight(key, entry, min_weight))
            .expire_after(ResponseFreshness { default_ttl })
            .build(),
        scope: CacheScope {
            kind: "isolate",
            id: uuid::Uuid::new_v4().to_string(),
            since: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_millis() as u64)
                .unwrap_or(0),
            max_entries,
            default_ttl_seconds: default_ttl_secs,
            max_bytes,
            max_entry_bytes,
        },
    })
}

#[cfg(test)]
#[path = "cache_ops_test.rs"]
mod cache_ops_test;
