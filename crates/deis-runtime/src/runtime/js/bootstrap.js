// The isolate's standard web globals.
//
// This runtime boots the bare `deno_core` engine rather than `deno_runtime`,
// so nothing here happens by itself. The extensions are initialised in
// runtime_init.rs and their JavaScript is imported below, but importing a
// module only defines its classes inside that module: Deno puts them on the
// global object in `99_main.js`/`98_global_scope_*.js`, which this runtime
// does not use. Without the block at the bottom of this file, `fetch`,
// `crypto`, `Request` and the rest exist in memory and are unreachable from an
// application, which is exactly what happened until now.
//
// SPECIFICATION_BASIC.md section 8.1 fixes the list an application may assume.
// `crates/deis-runtime/src/runtime/isolate_test.rs` asserts that list in both
// directions -- what is here and what is deliberately not -- so a name added
// or lost here fails a test rather than quietly changing what the platform
// promises.
//
// # Outbound fetch is not the raw one
//
// `fetch` below is a wrapper. The real one is bound into this module's scope
// and never reaches an application, so every outbound call passes through
// `op_deis_fetch_guard` first. See `runtime/fetch_policy.rs` for what that
// refuses and why an edge platform cannot hand an application an unrestricted
// HTTP client: deis-store trusts the tenant header it is given, so a `fetch`
// with no policy is a read of every other application's data.
//
// The wrapper does two things beyond calling the guard, and both are there
// because of what a `fetch` can otherwise reach or hold:
//
//   * it follows redirects itself, so that a hop the application never named
//     is judged by the same policy the destination it did name was judged by,
//     rather than by a layer that cannot express the same rules;
//   * it arms an `AbortSignal` on what is left of the invocation's execution
//     budget, so a call that never answers cannot outlive the request it was
//     made for and hold a runtime thread shared with other applications.
//
// Each has its own section below.

import { core, primordials } from "ext:core/mod.js";
import {
  op_deis_fetch_budget_ms,
  op_deis_fetch_guard,
} from "ext:core/ops";

// deno_web, deno_fetch, deno_crypto and deno_net stopped being ES modules.
//
// They used to be `esm` files this module imported by name; they are now
// `lazy_loaded_js` scripts, each an IIFE that returns its exports, fetched with
// `core.loadExtScript`. The mechanical difference is small - `import * as x
// from "ext:y"` becomes `const x = core.loadExtScript("ext:y")` - but the
// timing difference is not: nothing is evaluated until it is asked for.
//
// Everything this platform binds as a global is asked for right here, at
// bootstrap, on purpose. These scripts read `globalThis.__bootstrap` for their
// own plumbing, and this runtime deletes that object before any application
// code runs (see `js/runtime.js`); a script first reached during a request
// would find it gone. Loading them all now also keeps the cost where it
// belongs, in building the isolate rather than in an arbitrary request.
//
// deno_webidl is loaded by the scripts that need it rather than from here, and
// deno_websocket's two scripts are not loaded at all: the socket an
// application gets is `ws_client.js` below, which goes through the outbound
// fetch policy, and deno_websocket's own class would be a second way out of
// the isolate that never reaches it.
core.loadExtScript("ext:deno_web/00_infra.js");
const url = core.loadExtScript("ext:deno_web/00_url.js");
const domException = core.loadExtScript("ext:deno_web/01_dom_exception.js");
core.loadExtScript("ext:deno_web/01_mimesniff.js");
core.loadExtScript("ext:deno_web/01_console.js");
const urlPattern = core.loadExtScript("ext:deno_web/01_urlpattern.js");
const event = core.loadExtScript("ext:deno_web/02_event.js");
core.loadExtScript("ext:deno_web/02_structured_clone.js");
const timers = core.loadExtScript("ext:deno_web/02_timers.js");
const abortSignal = core.loadExtScript("ext:deno_web/03_abort_signal.js");
core.loadExtScript("ext:deno_web/04_global_interfaces.js");
core.loadExtScript("ext:deno_web/05_base64.js");
const streams = core.loadExtScript("ext:deno_web/06_streams.js");
core.loadExtScript("ext:deno_web/08_text_encoding.js");
const file = core.loadExtScript("ext:deno_web/09_file.js");
core.loadExtScript("ext:deno_web/10_filereader.js");
core.loadExtScript("ext:deno_web/12_location.js");
const messagePort = core.loadExtScript("ext:deno_web/13_message_port.js");
core.loadExtScript("ext:deno_web/14_compression.js");
core.loadExtScript("ext:deno_web/15_performance.js");
core.loadExtScript("ext:deno_web/16_image_data.js");
const headers = core.loadExtScript("ext:deno_fetch/20_headers.js");
const formData = core.loadExtScript("ext:deno_fetch/21_formdata.js");
core.loadExtScript("ext:deno_fetch/22_body.js");
core.loadExtScript("ext:deno_fetch/22_http_client.js");
const request = core.loadExtScript("ext:deno_fetch/23_request.js");
const response = core.loadExtScript("ext:deno_fetch/23_response.js");
const fetchModule = core.loadExtScript("ext:deno_fetch/26_fetch.js");
core.loadExtScript("ext:deno_fetch/27_eventsource.js");
const crypto = core.loadExtScript("ext:deno_crypto/00_crypto.js");
core.loadExtScript("ext:deno_net/01_net.js");
core.loadExtScript("ext:deno_net/02_tls.js");

// This repository's stand-in for deno_telemetry, which deno_fetch loads on
// first use. Loaded here rather than left to that first use for the
// `__bootstrap` reason above.
core.loadExtScript("ext:deno_telemetry/telemetry.ts");
core.loadExtScript("ext:deno_telemetry/util.ts");

// Still an ES module: this extension is this repository's own and declares its
// file with `esm`.
import * as wsClient from "ext:deis_runtime_ws_client_ops/src/runtime/js/ws_client.js";

const {
  ArrayPrototypeIncludes,
  ArrayPrototypePush,
  DateNow,
  FunctionPrototypeCall,
  MathFloor,
  ObjectDefineProperties,
  ObjectFreeze,
  ObjectGetOwnPropertyDescriptor,
  SafeSet,
  SafeSetIterator,
  SetPrototypeAdd,
  SetPrototypeClear,
  SetPrototypeDelete,
  SetPrototypeHas,
  StringPrototypeStartsWith,
  StringPrototypeToLowerCase,
  TypeError,
} = primordials;

// ---------------------------------------------------------------------------
// Outbound fetch
// ---------------------------------------------------------------------------

const nativeFetch = fetchModule.fetch;

// Bound here, at module load, rather than read off the global object when a
// call is made. `fetch` runs inside an isolate the application has already
// had its way with: `js/runtime.js` itself replaces `globalThis.Response`,
// and an application may replace `Request`, `Headers` or `URL`, or patch
// their prototypes. A wrapper that reached for the global names would be
// deciding where a redirect goes using objects the application supplied.
const NativeRequest = request.Request;
const NativeURL = url.URL;
const toInnerRequest = request.toInnerRequest;
const toInnerResponse = response.toInnerResponse;

// `URL.prototype.href` and `URL.prototype.origin` as they are before any
// application code runs. A patched getter that answered differently on a
// second read is how a check and the call it guards are made to disagree.
const urlHref = ObjectGetOwnPropertyDescriptor(NativeURL.prototype, "href").get;
const urlOrigin =
  ObjectGetOwnPropertyDescriptor(NativeURL.prototype, "origin").get;

// How many hops one call may take. The number the fetch specification fixes,
// so a redirect chain that works on another platform works here; past it the
// call fails with a `TypeError` naming the limit, rather than looping.
const MAX_REDIRECTS = 20;

// The statuses that mean "what you asked for is somewhere else".
const REDIRECT_STATUSES = new SafeSet([301, 302, 303, 307, 308]);

// Headers that carry the caller's identity, plus `host`, which names the
// authority the request was addressed to. None of them may travel to a
// different origin: a `Location:` header is chosen by whoever answered the
// previous hop, so carrying an `Authorization:` across it hands that party's
// pick of destination a credential the application only ever meant for the
// destination it named itself.
//
// Deliberately stricter than `deno_fetch`, which keeps these when the new
// host is a subdomain of the old one. A subdomain of a host an application
// trusts is not automatically a party it trusts, and the point of following
// redirects by hand here is that each hop is decided rather than inherited.
const ORIGIN_BOUND_HEADERS = [
  "authorization",
  "proxy-authorization",
  "cookie",
  "host",
];

// Headers that describe a body. When a redirect turns the request into a GET
// there is no body left for them to describe.
const BODY_HEADERS = [
  "content-encoding",
  "content-language",
  "content-location",
  "content-type",
];

// The `Location:` of one hop, read off the response's own header list rather
// than through `Headers.prototype.get`, which an application can replace.
function locationOf(inner) {
  const list = inner.headerList;
  for (let i = 0; i < list.length; i++) {
    if (StringPrototypeToLowerCase(list[i][0]) === "location") {
      return list[i][1];
    }
  }
  return null;
}

// Let go of a hop's body without reading it.
//
// Every hop but the last arrives with an open response body, and the socket
// stays charged to this isolate until something closes it. Nothing reads
// these, so they are cancelled as soon as the `Location:` header has been
// taken off them. A cancel that throws is swallowed: the call is already on
// its way elsewhere, and failing to tidy up must not become the error the
// application sees.
function discard(inner) {
  try {
    if (inner.body !== null) {
      inner.body.cancel();
    }
  } catch (_) {
    // Nothing to do; the body is going away either way.
  }
}

// The header list the next hop carries.
//
// Built from the inner request's own list -- a plain array of name/value
// pairs -- so that neither the copy nor the removals go through anything an
// application can replace.
function nextHeaderList(current, dropsBody, crossesOrigin) {
  const source = toInnerRequest(current).headerList;
  const kept = [];
  for (let i = 0; i < source.length; i++) {
    const name = StringPrototypeToLowerCase(source[i][0]);
    if (dropsBody && ArrayPrototypeIncludes(BODY_HEADERS, name)) {
      continue;
    }
    if (crossesOrigin && ArrayPrototypeIncludes(ORIGIN_BOUND_HEADERS, name)) {
      continue;
    }
    ArrayPrototypePush(kept, [source[i][0], source[i][1]]);
  }
  return kept;
}

// The request for the next hop.
//
// The method and body rules are the ones `fetch` is defined by, and
// applications rely on them: a 303 means "now go and GET this instead", and a
// 301 or 302 answering a POST has meant the same thing since long before the
// specification wrote it down. 307 and 308 exist to say "do the same thing
// again over there", so those keep both the method and the body.
function nextRequest(current, status, from, to) {
  const inner = toInnerRequest(current);
  let method = inner.method;
  // `source` is the value the body was built from -- a string, a Uint8Array,
  // a Blob, a FormData. It is null when the body only ever existed as a
  // stream, and a stream cannot be replayed onto a second connection.
  let body = inner.body === null ? null : inner.body.source;

  const becomesAGet =
    ((status === 301 || status === 302) && method === "POST") ||
    (status === 303 && method !== "GET" && method !== "HEAD");

  if (becomesAGet) {
    method = "GET";
    body = null;
  } else if (inner.body !== null && body === null) {
    throw new TypeError(
      `fetch could not follow the redirect from ${from} to ${to}: the ` +
        "request body is a stream, which cannot be sent a second time. " +
        "Buffer the body -- a string, a Uint8Array or a Blob -- if the " +
        "destination may redirect",
    );
  }

  const crossesOrigin =
    FunctionPrototypeCall(urlOrigin, new NativeURL(to)) !==
      FunctionPrototypeCall(urlOrigin, new NativeURL(from));

  // `__proto__: null` because this init is a dictionary, and a dictionary is
  // read member by member with an ordinary property get: every member this
  // literal does not spell would otherwise be answered by whatever the
  // application had put on `Object.prototype`. This hop is one the
  // application never named, so what it carries is decided here alone.
  return new NativeRequest(to, {
    __proto__: null,
    method,
    headers: nextHeaderList(current, becomesAGet, crossesOrigin),
    body,
  });
}

// The response the application is handed, carrying the history of the call.
//
// `response.url` and `response.redirected` are part of what `fetch` promises,
// and following redirects here rather than inside `deno_fetch` means nothing
// else fills them in: each hop is a separate call that knows only its own
// URL. Both properties are computed from the same list of URLs -- the last
// entry is `url`, more than one entry is `redirected` -- so writing the whole
// chain onto the final response restores both at once.
function withHistory(hop, urlList) {
  if (urlList.length > 1) {
    toInnerResponse(hop).urlList = urlList;
  }
  return hop;
}

// Follow redirects here, in front of the policy, instead of inside
// `deno_fetch`.
//
// `fetch` follows a redirect by default, and a redirect is a destination the
// application never named: whoever answered the previous hop chose it. Left
// to `deno_fetch`, those hops were judged only by deno's own net permission
// -- a deny list of address ranges -- because the pre-flight guard runs once,
// in front of the call. That list cannot say what this platform's policy
// says. It has no spelling for an IPv6 prefix, so `Location:
// http://[fd00::1]/` walked straight through it; and it has no per-port
// exception, so an operator allowlist entry that is a literal address took
// that entire address out of it, every port included -- the node's own
// telemetry metrics port among them.
//
// So every hop is issued with `redirect: "manual"` and put through the same
// `op_deis_fetch_guard` the first request went through: one decision point
// with one set of rules, instead of three of differing expressiveness. deno's
// deny list is still installed (see `runtime/isolate.rs`), now purely as a
// backstop behind this loop rather than as the thing that enforces the policy
// on a redirect.
async function fetch(input, init = undefined) {
  // Normalised first and judged afterwards, so that what the policy is shown
  // is exactly what the connector will be given. `op_fetch` is handed the
  // inner request's current URL, so that is the string that goes to the
  // guard; judging the argument instead would leave a gap for anything that
  // answers differently the second time it is read -- an application's own
  // `toString`, or a patched `URL.prototype.href`.
  let current = new NativeRequest(input, init);
  const firstInner = toInnerRequest(current);
  let currentUrl = firstInner.currentUrl();
  const mode = firstInner.redirectMode;
  const urlList = [currentUrl];

  // Throws a TypeError -- which is what a failed `fetch` throws per spec, so
  // a handler's existing error path catches it -- and logs the refusal on the
  // node with the application, the destination and the reason. Nothing is
  // dialled when this rejects.
  await op_deis_fetch_guard(currentUrl);

  const signal = boundedByExecutionBudget(current.signal);

  for (let hops = 0; ; hops++) {
    // Same reason as the init in `nextRequest`: read as a dictionary, so
    // nothing an application left on `Object.prototype` gets to fill in the
    // members that keep this loop -- and therefore the policy check on every
    // hop -- in charge of the redirect.
    const hop = await nativeFetch(current, {
      __proto__: null,
      redirect: "manual",
      signal,
    });
    const inner = toInnerResponse(hop);

    // `manual` is the application asking to be handed the redirect itself,
    // which it then acts on with another `fetch` -- and that call comes
    // through this same guard. A status that is not a redirect, and a
    // redirect with no `Location:` to follow, are both simply the answer.
    if (mode === "manual" || !SetPrototypeHas(REDIRECT_STATUSES, inner.status)) {
      return withHistory(hop, urlList);
    }
    const location = locationOf(inner);
    if (location === null) {
      return withHistory(hop, urlList);
    }

    if (mode === "error") {
      discard(inner);
      throw new TypeError(
        `fetch to ${currentUrl} was answered with ${inner.status} ` +
          `${location} and this request set redirect: "error"`,
      );
    }

    // From here this response is only a signpost: nothing will read its body,
    // and every way out of the rest of the loop leads somewhere else.
    const status = inner.status;
    discard(inner);

    if (hops >= MAX_REDIRECTS) {
      throw new TypeError(
        `fetch gave up on ${urlList[0]}: it was redirected more than ` +
          `${MAX_REDIRECTS} times, most recently from ${currentUrl} to ` +
          `${location}`,
      );
    }

    // Resolved against the URL that *returned* the header, which is what
    // makes `Location: /elsewhere` and `Location: ../elsewhere` mean what the
    // server meant by them.
    let to;
    try {
      to = FunctionPrototypeCall(
        urlHref,
        new NativeURL(location, currentUrl),
      );
    } catch (_) {
      throw new TypeError(
        `fetch could not follow the redirect from ${currentUrl}: the ` +
          `Location header ${location} is not a URL`,
      );
    }

    // The guard below refuses `file:` and every other scheme an application
    // may not open, but `data:` and `blob:` are not network destinations at
    // all and the policy has nothing to say about them -- so the scheme is
    // settled here as well. A redirect must not be able to turn an outbound
    // request into a read of something that never leaves the isolate.
    if (
      !StringPrototypeStartsWith(to, "http://") &&
      !StringPrototypeStartsWith(to, "https://")
    ) {
      throw new TypeError(
        `fetch will not follow the redirect from ${currentUrl} to ${to}: ` +
          "only http and https destinations may be followed",
      );
    }

    const following = nextRequest(current, status, currentUrl, to);
    const followingUrl = toInnerRequest(following).currentUrl();

    // The whole point of the loop. This destination is one the application
    // never wrote down, and it is judged by exactly the rules the one it did
    // write down was judged by: the same schemes, the same address ranges,
    // the same ports, the same allowlist, the same refusal of this platform's
    // own endpoints.
    await op_deis_fetch_guard(followingUrl);

    current = following;
    currentUrl = followingUrl;
    ArrayPrototypePush(urlList, currentUrl);
  }
}

// ---------------------------------------------------------------------------
// The execution budget an outbound call has to fit inside
// ---------------------------------------------------------------------------
//
// An isolate's execution timeout works by terminating V8, and terminating V8
// does nothing to a Rust operation that is already in flight. A `fetch` to an
// address that accepts a connection and then says nothing kept running long
// after the timeout had fired and been logged -- measured at 81 seconds
// against a 30-second budget -- and for all of that time it held the runtime
// thread its application shares with every other application on that thread.
// That is a denial of service against those applications, not just a slow
// request for this one.
//
// So the call carries a deadline of its own. `op_deis_fetch_budget_ms` says
// how much of this invocation's budget is left; the timer below aborts the
// call when that runs out, which closes deno's cancel handle and unwinds the
// Rust op. The budget the op reports stops a little short of the isolate's
// own deadline on purpose, so the application is handed a `TimeoutError` it
// can catch and answer, rather than being terminated mid-statement -- which
// tells it nothing and costs the request anyway.
//
// The guards are tracked and swept at the end of the invocation for two
// reasons. One is the reason intervals are: an isolate is reused, and a timer
// left armed would fire inside somebody else's request. The other is the more
// important half of the same guarantee -- a `fetch` the handler started but
// never awaited, and never handed to `waitUntil`, is still open with nobody
// watching it when the invocation ends. Its budget timer would not trip for
// another thirty seconds, and until it did the socket stayed charged to the
// node process, which is a resource an application shares with every other
// one on the node: a handler that fires such a call on every request piles up
// held connections without bound. So the sweep does not merely stop the
// countdown, it aborts the call. By the time it runs, `waitUntil` work has
// already been awaited (see `js/runtime.js`), so anything still open here is
// a detached call that was meant to end with the request.

// Lifted out of the module namespace before they are called, for the reason
// given in the timers section below: deno's timers check their receiver.
const nativeSetTimeout = timers.setTimeout;
const nativeClearTimeout = timers.clearTimeout;

// Each entry is `{ controller, id, set }`: the `AbortController` bounding one
// outbound call, the timeout that trips it when the budget runs out, and the
// set the entry is filed in so the timeout can take itself out of the right
// one. See `entryOwner` below for why there is more than one set.
const budgetGuards = new SafeSet();

// What an application is told when the budget ran out under it. A
// `TimeoutError` is what an `AbortSignal` deadline produces everywhere else,
// so `e.name === "TimeoutError"` is the check a handler already knows.
function budgetExpired() {
  return new domException.DOMException(
    "The outbound fetch was aborted: this invocation's execution budget ran " +
      "out while the request was still in flight. Shorten the call, or ask " +
      "the operator for a longer execution timeout",
    "TimeoutError",
  );
}

// The signal a call is made under: the application's own, plus the deadline.
//
// A negative answer from the op means this isolate is running without a
// budget -- the snapshot build, and any path with no invocation in flight --
// and the application's own signal is then the only one.
function boundedByExecutionBudget(callerSignal) {
  const remaining = op_deis_fetch_budget_ms();
  if (remaining < 0) {
    return callerSignal;
  }

  const controller = new abortSignal.AbortController();
  if (remaining === 0) {
    // Nothing left to spend. Aborting before the call is made means no socket
    // is opened at all, and the application sees the same error it would have
    // seen a moment later.
    controller.abort(budgetExpired());
  } else {
    armGuard(ownedGuards(), controller, remaining);
  }

  return abortSignal.createDependentAbortSignal(
    [callerSignal, controller.signal],
    "Failed to bound fetch() by the execution budget",
  );
}

// File one guard in `set` and start its countdown.
//
// The set is carried on the guard because the timeout has to remove the entry
// from the set it is actually in, and a guard changes sets exactly once: when
// a response stream adopts the call that is feeding it.
function armGuard(set, controller, remaining) {
  const guard = { controller, id: 0, set };
  guard.id = nativeSetTimeout(() => {
    SetPrototypeDelete(guard.set, guard);
    controller.abort(budgetExpired());
  }, remaining);
  SetPrototypeAdd(set, guard);
  return guard;
}

// End every outbound call still bounded by the budget when the invocation
// ends, and say how many there were. `js/runtime.js` calls this after
// `waitUntil` work has been awaited, next to the interval sweep.
//
// Each guard's timer is stopped so it cannot fire inside a later request, and
// its controller is aborted. Aborting the controller of a call that already
// finished is a no-op; aborting one that is still open -- a detached `fetch`
// the handler never awaited -- closes its socket, so no outbound call outlives
// the request that made it.
function sweepBudgetTimers() {
  return sweepGuardSet(budgetGuards);
}

// End every outbound call filed in one set. Written apart from the sweep above
// because a response stream owns a set of its own and is released the same way.
function sweepGuardSet(set) {
  let swept = 0;
  for (const guard of new SafeSetIterator(set)) {
    nativeClearTimeout(guard.id);
    try {
      guard.controller.abort(budgetExpired());
    } catch (_) {
      // The call is ending either way; a failure to abort must not become the
      // error a later request sees.
    }
    swept++;
  }
  SetPrototypeClear(set);
  return swept;
}

// ---------------------------------------------------------------------------
// Timers
// ---------------------------------------------------------------------------
//
// An isolate outlives the request that ran in it: deis-runtime keeps one per
// (application, environment) on each node and hands it every later request.
// That makes a timer left running a leak, and the two kinds of timer are not
// equally bad:
//
//   * a `setTimeout` fires once and is gone. A pending one costs one callback
//     on some later request's event loop and then nothing. `waitUntil` work
//     depends on being able to schedule one, so they are left alone.
//   * a `setInterval` never stops on its own. One created per request
//     accumulates without bound: every one of them keeps firing on every
//     later request in that isolate, burning CPU that the request being served
//     is charged for, and each op the callback dispatches counts against the
//     isolate's rate limit -- so a handler that sets an interval and returns
//     eventually gets its own isolate terminated for a reason nothing in its
//     code explains.
//
// So intervals are tracked, and `__deis_runtime_sweep_intervals` clears
// whatever is still running. `js/runtime.js` calls it at the end of every
// invocation, after `waitUntil` work has been awaited, so an interval lives
// exactly as long as the request that created it.

const liveIntervals = new SafeSet();

// Lifted out of the module namespace before they are called. deno's timers
// check their receiver -- `this` has to be undefined or globalThis -- and
// calling `timers.setInterval(...)` makes the namespace object the receiver,
// which throws "Illegal invocation".
const nativeSetInterval = timers.setInterval;
const nativeClearInterval = timers.clearInterval;

function setInterval(callback, delay, ...args) {
  const id = nativeSetInterval(callback, delay, ...args);
  SetPrototypeAdd(ownedIntervals(), id);
  return id;
}

function clearInterval(id) {
  nativeClearInterval(id);
  SetPrototypeDelete(liveIntervals, id);
  if (entryOwner !== null) SetPrototypeDelete(entryOwner.intervals, id);
}

// Stop every interval still running and say how many there were, so the
// caller can tell the application it left something behind.
function sweepIntervals() {
  return sweepIntervalSet(liveIntervals);
}

function sweepIntervalSet(set) {
  let swept = 0;
  for (const id of new SafeSetIterator(set)) {
    nativeClearInterval(id);
    swept++;
  }
  SetPrototypeClear(set);
  return swept;
}

// ---------------------------------------------------------------------------
// Who owns an outbound call or an interval
// ---------------------------------------------------------------------------
//
// Everywhere else in this runtime, "as long as the request" is the answer, and
// the two sweeps above are how it is kept: whatever a stretch of application
// code left running is ended when that stretch ends.
//
// A response stream is the one thing that outlives the stretch that created it.
// Its head has already gone to the client and its body is still being produced,
// by code that legitimately holds the `fetch` it is copying from and the
// interval that paces it. Sweeping those when the handler returned would cut
// the stream's own source out from under it; leaving them in the shared sets
// would mean the *next* request to this application sweeps them instead, which
// is worse -- the failure would then depend on somebody else's traffic.
//
// So a stream takes ownership. `adopt` moves everything the handler still has
// open into sets of the stream's own; `enter` points new entries at those sets;
// `release` ends all of it when the stream does. `entryOwner` is `null` --
// "the stretch running now owns it" -- on every path that existed before
// streams did.
//
// `enter` is sticky, and that is the part worth reading twice. `js/runtime.js`
// calls it immediately before each step of a stream's producer, and *nothing*
// clears it except the start of the next armed stretch (`leave`, called at the
// top of an invocation and of its `waitUntil` tail). It has to be sticky: a
// producer's own code runs inside the `await` that follows `enter`, and its
// continuations run in microtasks after that await has returned, so an owner
// cleared on the way out of one step would leave the `fetch` a producer starts
// in the next one filed in the shared set -- where the next request to this
// application would sweep it.
//
// The cost of sticky is an attribution that can be off by one *stream*: while
// two streams of the same application are producing at once, work one of them
// starts outside its own step is filed against whichever stream stepped last.
// Both are streams of this application and both are bounded by their own
// budgets, so the entry is still swept, just possibly a little early or a
// little late. What cannot happen -- and is the thing this whole section is
// for -- is a producer's outbound call being swept by somebody else's request.
let entryOwner = null;

function ownedGuards() {
  return entryOwner === null ? budgetGuards : entryOwner.guards;
}

function ownedIntervals() {
  return entryOwner === null ? liveIntervals : entryOwner.intervals;
}

// Re-arm one owner's outbound calls against the budget now in force.
//
// A call the handler started was bounded by the *request's* deadline, which is
// spent by the time the head has gone out. Left alone it would abort the
// upstream feeding the stream some tens of seconds in, mid-body, for no fault:
// `return new Response(upstream.body)` is exactly the shape this is for.
// `runtime/isolate.rs` publishes the stream's own deadline before calling
// `adopt`, so what `op_deis_fetch_budget_ms` answers here is the stream's.
function rearmGuards(owner) {
  const remaining = op_deis_fetch_budget_ms();
  for (const guard of new SafeSetIterator(owner.guards)) {
    nativeClearTimeout(guard.id);
    if (remaining <= 0) {
      SetPrototypeDelete(owner.guards, guard);
      try {
        guard.controller.abort(budgetExpired());
      } catch (_) {
        // Nothing here may become the error the stream reports.
      }
      continue;
    }
    guard.id = nativeSetTimeout(() => {
      SetPrototypeDelete(guard.set, guard);
      guard.controller.abort(budgetExpired());
    }, remaining);
  }
}

const streamScope = ObjectFreeze({
  // Hand everything the stretch that just ran still has open to a new owner,
  // and put its outbound calls on the stream's clock.
  adopt() {
    const owner = { guards: new SafeSet(), intervals: new SafeSet() };
    for (const guard of new SafeSetIterator(budgetGuards)) {
      guard.set = owner.guards;
      SetPrototypeAdd(owner.guards, guard);
    }
    SetPrototypeClear(budgetGuards);
    for (const id of new SafeSetIterator(liveIntervals)) {
      SetPrototypeAdd(owner.intervals, id);
    }
    SetPrototypeClear(liveIntervals);
    rearmGuards(owner);
    return owner;
  },
  enter(owner) {
    entryOwner = owner;
  },
  leave() {
    entryOwner = null;
  },
  // End everything this stream held, and say how much there was.
  //
  // The owner is cleared if it is still the one in force: filing anything into
  // a released set would leave it with nothing to sweep it, which is the leak
  // this whole section exists to prevent.
  release(owner) {
    if (entryOwner === owner) entryOwner = null;
    return sweepGuardSet(owner.guards) + sweepIntervalSet(owner.intervals);
  },
});

// ---------------------------------------------------------------------------
// The clock, and what this platform calls itself
// ---------------------------------------------------------------------------
//
// # performance
//
// `performance` below is this runtime's own object. deno_web builds a real one
// in `15_performance.js` -- still imported above, because other deno_web
// modules measure themselves with it -- and that one is deliberately not what
// an application gets, for one reason: its `now()` reads `op_now`, a
// nanosecond clock returned as a seconds/nanoseconds pair, and a
// sub-millisecond clock an application can read is the first ingredient of a
// timing side channel. With one, a handler can tell a cache hit from a cache
// miss and, over enough samples, read what the hardware is doing for somebody
// else. On this node "somebody else" is another tenant: the pool runs many
// applications' isolates on one machine and several isolates on one OS thread
// (`runtime/pool.rs`). Browsers coarsened this clock after Spectre and the
// edge platform this one is modelled on ships it clamped to whole
// milliseconds; so does this.
//
// deno_web's object is not usable here for a second, duller reason as well:
// its `timeOrigin` is filled in by `setTimeOrigin()`, which Deno calls from
// the `99_main.js` this runtime does not run, so it would answer `undefined`.
//
// `timeOrigin` is fixed at 0, which is also what that platform reports, and it
// buys two things. `timeOrigin + now()` is then an ordinary Unix timestamp, so
// the two halves agree with each other; and the origin says nothing about when
// this isolate was created, which would otherwise let a handler tell a cold
// start from a reused isolate and date the node it is sharing.
//
// Two things this deliberately does not do, so that nobody reads more into it
// than is there:
//
//   * the clock is coarse, not frozen. That platform additionally holds time
//     still between I/O operations, so a stretch of pure computation cannot be
//     measured at all. Here `now()` advances with the wall clock, one
//     millisecond at a time. A millisecond is a wide enough step to spoil the
//     measurements above; it is not the absence of a timer.
//   * `mark()`, `measure()` and the `PerformanceObserver` family are absent.
//     They exist to feed a profiler, their entry list lives as long as the
//     isolate -- which outlives the request that filled it -- and nothing on
//     this node collects it.
//
// What is left is a millisecond stopwatch: enough to say how long an upstream
// took, not enough to microbenchmark, which is the trade being made.

function performanceNow() {
  return MathFloor(DateNow());
}

const performance = ObjectFreeze({
  timeOrigin: 0,
  now: performanceNow,
});

// # navigator
//
// One property, and the object is frozen.
//
// The name is worth having because libraries branch on it: a package that
// supports several runtimes reads `navigator.userAgent` to pick its edge
// implementation, and one that finds no `navigator` at all falls through to
// the browser branch and reaches for `document`. So the object exists and
// answers with the name of this platform.
//
// Nothing else is on it. `hardwareConcurrency` describes the node's cores,
// which is the operator's business and not the tenant's; `language`,
// `onLine`, `storage` and `sendBeacon` either mean nothing on a server or are
// promises this platform cannot keep, and a member that answers a plausible
// lie is worse than one that is missing. It is frozen so that a handler cannot
// bolt one on either -- an isolate is reused across requests, so a property
// written onto a platform global during one request would still be there,
// looking like platform surface, during the next. Freezing the object is only
// half of that; the *binding* is sealed where the globals are defined, because
// a frozen object reached by a name that can be redefined is no protection at
// all.

const navigator = ObjectFreeze({
  userAgent: "Deislet",
});

// ---------------------------------------------------------------------------
// The global surface
// ---------------------------------------------------------------------------
//
// Classes are non-enumerable and functions are writable, which is how the same
// names are defined in a browser and in Deno; an application that assigns over
// `globalThis.fetch` to install its own instrumentation is doing something
// ordinary and should not get a silent no-op.
//
// # Read-only is not the same as unreplaceable
//
// `core.propReadOnly` is `{ writable: false, configurable: true }`, which
// protects the *value* and leaves the *binding* open: plain assignment throws
// in a module's strict mode, and then `Object.defineProperty(globalThis,
// "navigator", { value: mine })` and `delete globalThis.navigator` both
// succeed. That is not an application lying to itself for one request. An
// isolate is reused across every request for the same application on this
// node, so whatever a handler installed was still standing for the next
// request: a `performance.now()` with a fractional part and an invented
// `timeOrigin` -- which is the sub-millisecond clock the coarse one exists to
// withhold -- and a `navigator.userAgent` of the handler's choosing, both
// looking like platform surface to code that had never touched them.
//
// So the names that describe the platform rather than serve the application
// are sealed: no write, no redefinition, no delete. The two internal hooks are
// sealed for a harder reason -- `__deis_runtime_sweep_intervals` is what ends
// an application's `setInterval` when its request does, and an application
// that could replace it with a no-op would leave callbacks running on a
// runtime thread its request no longer owns.

function propSealed(value) {
  return { value, writable: false, enumerable: true, configurable: false };
}

function propSealedNonEnumerable(value) {
  return { value, writable: false, enumerable: false, configurable: false };
}
//
// What is deliberately *not* here is as much a decision as what is, and each
// name left out has its own reason rather than a shared one: `Deno` and
// `__bootstrap` are the raw op table, `addEventListener` would accept a
// "fetch" listener this platform never fires, and `BroadcastChannel` has no
// bus to carry it. The reasons are written out one per name in
// `isolate_test.rs::EXTRA_GLOBALS_ABSENT`, which is also what asserts they
// stay absent.

ObjectDefineProperties(globalThis, {
  // Section 8.1: the outbound HTTP client, policed. See the header.
  fetch: core.propWritable(fetch),
  Request: core.propNonEnumerable(request.Request),
  Response: core.propNonEnumerable(response.Response),
  Headers: core.propNonEnumerable(headers.Headers),

  // Section 8.1: URL parsing.
  URL: core.propNonEnumerable(url.URL),
  URLSearchParams: core.propNonEnumerable(url.URLSearchParams),

  // Section 8.1: Web Crypto, which is `crypto.getRandomValues`,
  // `crypto.randomUUID` and the whole of `crypto.subtle`. Sealed: a
  // `getRandomValues` an application had swapped for a counter would still be
  // there on the next request through this isolate, and everything that then
  // asked for a random number -- a session id, a nonce, a token -- would get
  // the application's number instead.
  crypto: propSealed(crypto.crypto),

  // Section 8.1: the stream types. The readers, writers, controllers and
  // queuing strategies are not bound as globals: an application reaches them
  // through the three constructors below, which is how they are normally
  // used.
  ReadableStream: core.propNonEnumerable(streams.ReadableStream),
  WritableStream: core.propNonEnumerable(streams.WritableStream),
  TransformStream: core.propNonEnumerable(streams.TransformStream),

  // Section 8.1: timers. `setInterval` and `clearInterval` are the wrappers
  // above, so an interval cannot outlive the request that created it.
  setTimeout: core.propWritable(timers.setTimeout),
  clearTimeout: core.propWritable(timers.clearTimeout),
  setInterval: core.propWritable(setInterval),
  clearInterval: core.propWritable(clearInterval),

  // Beyond section 8.1, but not optional in practice: without these an
  // outbound call cannot be cancelled, so a slow upstream burns the whole of
  // the isolate's execution budget, and the errors that `crypto.subtle`, the
  // streams and an aborted fetch throw cannot be caught by type.
  AbortController: core.propNonEnumerable(abortSignal.AbortController),
  AbortSignal: core.propNonEnumerable(abortSignal.AbortSignal),
  DOMException: core.propNonEnumerable(domException.DOMException),

  // Multipart bodies. `request.formData()` already works -- deno_fetch parses
  // one -- but without these three names an application can receive an upload
  // and not read it: a file entry is a `File`, `entry instanceof File` is the
  // check every upload handler makes, and there is no way to build a
  // multipart body to send one on. `Blob` is the type the other two are
  // defined in terms of, and the one `response.blob()` has been returning
  // unnameably all along.
  Blob: core.propNonEnumerable(file.Blob),
  File: core.propNonEnumerable(file.File),
  FormData: core.propNonEnumerable(formData.FormData),

  // Routing. A request handler's first job is to decide which path it was
  // given, and `URLPattern` is what the routers written for edge runtimes
  // reach for to do it -- so its absence is felt on line one of a ported
  // application, not in some corner of it. Backed by deno_web's
  // implementation, so `:named` groups and `*` mean what they mean elsewhere.
  URLPattern: core.propNonEnumerable(urlPattern.URLPattern),

  // Deep copy. The alternative an application falls back on is
  // `JSON.parse(JSON.stringify(x))`, which silently drops `undefined`,
  // functions and `Map`/`Set`, turns a `Date` into a string and throws on a
  // cycle.
  //
  // One caveat, measured rather than assumed: this is the variant from
  // `13_message_port.js`, which takes a `{ transfer: [...] }` list, and that
  // list does nothing here. deno_core only moves a buffer when the runtime
  // was built with a shared ArrayBuffer store (`ops_builtin_v8.rs`), and this
  // one is not, so a buffer named for transfer is copied like everything else
  // and is *not* left detached. The clone is correct either way; what an
  // application does not get is the move. Pinned by
  // `structuredclone_deep_copies_what_a_json_round_trip_loses`, so enabling
  // that store later is a test failure rather than a silent change.
  structuredClone: core.propWritable(messagePort.structuredClone),

  // Events, and one implementation of them for the whole isolate.
  //
  // There used to be two. These classes are deno_web's, while `EventTarget`
  // was a small hand-written one in `js/runtime.js` that dispatched plain
  // `{ type, data }` literals -- so an outbound `WebSocket`, which extends
  // deno_web's `EventTarget`, was not an instance of the `EventTarget` an
  // application could name. Listeners worked; the `instanceof` guard a
  // library runs *before* attaching one saw false and took its other branch.
  // Now deno_web's is the only one, `js/runtime.js` builds real events for
  // the inbound socket out of `__deis_runtime_events` below, and every
  // dispatch fills in `event.target` the way the specification says.
  EventTarget: core.propNonEnumerable(event.EventTarget),
  Event: core.propNonEnumerable(event.Event),
  CustomEvent: core.propNonEnumerable(event.CustomEvent),

  // The outbound socket, policed. Not deno_websocket's class: that one dials
  // with a connector of its own that never reaches
  // `runtime/fetch_policy.rs`, which would give an application a second way
  // out of the isolate past the single decision point the outbound policy is
  // built around. This one is backed by `runtime/ws_client_ops.rs`, which
  // puts every destination through that policy and then connects to the
  // address the policy judged - so a name that resolves to loopback, to a
  // private range or to one of this platform's own services is refused here
  // exactly as it is for `fetch`. The inbound half (`WebSocketPair`,
  // `WebSocketServer`) is bound in `js/runtime.js`.
  WebSocket: core.propNonEnumerable(wsClient.WebSocket),

  // Not deno_web's `performance`, and one property of `navigator`. Both are
  // built above; the section there says why the clock is coarse and why
  // `navigator` answers with a name and nothing else. Both sealed: freezing
  // the object protects what is on it and not the name it is reached by, and
  // the name is what the next request through this isolate resolves.
  performance: propSealed(performance),
  navigator: propSealed(navigator),

  // Internal, not part of the platform's promise to applications: see the
  // timers section above and the execution-budget section further up. Both
  // are called by `js/runtime.js` at the end of every invocation, so both are
  // sealed -- a no-op in place of either is an application keeping work
  // running past the request that started it.
  __deis_runtime_sweep_intervals: propSealedNonEnumerable(sweepIntervals),
  __deis_runtime_sweep_fetch_budgets: propSealedNonEnumerable(sweepBudgetTimers),

  // How a response stream keeps hold of the outbound call and the interval
  // that feed it for as long as its body is being written, instead of losing
  // them to the sweep that ends the request. Sealed for the same reason the
  // sweeps are: an application that could replace it would keep work running
  // on a runtime thread no request of its own still owns.
  __deis_runtime_stream_scope: propSealedNonEnumerable(streamScope),

  // The event classes `js/runtime.js` builds its WebSocket events out of.
  // That file runs through `execute_script`, so it cannot import from
  // `ext:deno_web/02_event.js` the way this module can, and it must not build
  // its events out of whatever `globalThis.Event` happens to be by the time a
  // frame arrives. `MessageEvent` and `CloseEvent` are not bound under their
  // own names because the platform's global list is a promise the documents
  // and `isolate_test.rs` hold this file to; this is plumbing between two
  // files of the runtime, and it is sealed for the same reason the sweeps
  // are.
  __deis_runtime_events: propSealedNonEnumerable(ObjectFreeze({
    Event: event.Event,
    MessageEvent: event.MessageEvent,
    CloseEvent: event.CloseEvent,
    ErrorEvent: event.ErrorEvent,
    defineEventHandler: event.defineEventHandler,
  })),
});
