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

import "ext:deno_webidl/00_webidl.js";
import "ext:deno_web/00_infra.js";
import * as url from "ext:deno_web/00_url.js";
import * as domException from "ext:deno_web/01_dom_exception.js";
import "ext:deno_web/01_mimesniff.js";
import "ext:deno_web/01_broadcast_channel.js";
import "ext:deno_web/01_console.js";
import "ext:deno_web/01_urlpattern.js";
import "ext:deno_web/02_event.js";
import "ext:deno_web/02_structured_clone.js";
import * as timers from "ext:deno_web/02_timers.js";
import * as abortSignal from "ext:deno_web/03_abort_signal.js";
import "ext:deno_web/04_global_interfaces.js";
import "ext:deno_web/05_base64.js";
import * as streams from "ext:deno_web/06_streams.js";
import "ext:deno_web/08_text_encoding.js";
import "ext:deno_web/09_file.js";
import "ext:deno_web/10_filereader.js";
import "ext:deno_web/12_location.js";
import "ext:deno_web/13_message_port.js";
import "ext:deno_web/14_compression.js";
import "ext:deno_web/15_performance.js";
import "ext:deno_web/16_image_data.js";
import * as headers from "ext:deno_fetch/20_headers.js";
import "ext:deno_fetch/21_formdata.js";
import "ext:deno_fetch/22_body.js";
import "ext:deno_fetch/22_http_client.js";
import * as request from "ext:deno_fetch/23_request.js";
import * as response from "ext:deno_fetch/23_response.js";
import * as fetchModule from "ext:deno_fetch/26_fetch.js";
import "ext:deno_fetch/27_eventsource.js";
import "ext:deno_websocket/01_websocket.js";
import "ext:deno_websocket/02_websocketstream.js";
import * as crypto from "ext:deno_crypto/00_crypto.js";
import "ext:deno_net/01_net.js";
import "ext:deno_net/02_tls.js";
import "ext:deno_telemetry/telemetry.ts";
import "ext:deno_telemetry/util.ts";

const {
  ArrayPrototypeIncludes,
  ArrayPrototypePush,
  FunctionPrototypeCall,
  ObjectDefineProperties,
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

  return new NativeRequest(to, {
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
    const hop = await nativeFetch(current, { redirect: "manual", signal });
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

// Each entry is `{ controller, id }`: the `AbortController` bounding one
// outbound call, and the timeout that trips it when the budget runs out.
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
    const guard = { controller, id: 0 };
    guard.id = nativeSetTimeout(() => {
      SetPrototypeDelete(budgetGuards, guard);
      controller.abort(budgetExpired());
    }, remaining);
    SetPrototypeAdd(budgetGuards, guard);
  }

  return abortSignal.createDependentAbortSignal(
    [callerSignal, controller.signal],
    "Failed to bound fetch() by the execution budget",
  );
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
  let swept = 0;
  for (const guard of new SafeSetIterator(budgetGuards)) {
    nativeClearTimeout(guard.id);
    try {
      guard.controller.abort(budgetExpired());
    } catch (_) {
      // The call is ending either way; a failure to abort must not become the
      // error a later request sees.
    }
    swept++;
  }
  SetPrototypeClear(budgetGuards);
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
  SetPrototypeAdd(liveIntervals, id);
  return id;
}

function clearInterval(id) {
  nativeClearInterval(id);
  SetPrototypeDelete(liveIntervals, id);
}

// Stop every interval still running and say how many there were, so the
// caller can tell the application it left something behind.
function sweepIntervals() {
  let swept = 0;
  for (const id of new SafeSetIterator(liveIntervals)) {
    nativeClearInterval(id);
    swept++;
  }
  SetPrototypeClear(liveIntervals);
  return swept;
}

// ---------------------------------------------------------------------------
// The global surface
// ---------------------------------------------------------------------------
//
// Classes are non-enumerable and functions are writable, which is how the same
// names are defined in a browser and in Deno; an application that assigns over
// `globalThis.fetch` to install its own instrumentation is doing something
// ordinary and should not get a silent no-op.
//
// What is deliberately *not* here is as much a decision as what is: `Deno`,
// `WebSocket`, `BroadcastChannel`, `URLPattern`, `Blob`, `FormData`,
// `structuredClone`, `performance` and `navigator` are all loaded above,
// because deno_fetch and deno_web need them internally, and are all left
// unbound. See `isolate_test.rs` for the list and `README.md` for the reasons.

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
  // `crypto.randomUUID` and the whole of `crypto.subtle`.
  crypto: core.propReadOnly(crypto.crypto),

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

  // Internal, not part of the platform's promise to applications: see the
  // timers section above and the execution-budget section further up. Both
  // are called by `js/runtime.js` at the end of every invocation.
  __deis_runtime_sweep_intervals: core.propNonEnumerable(sweepIntervals),
  __deis_runtime_sweep_fetch_budgets: core.propNonEnumerable(sweepBudgetTimers),
});
