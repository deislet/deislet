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

import { core, primordials } from "ext:core/mod.js";
import { op_deis_fetch_guard } from "ext:core/ops";

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
  ObjectDefineProperties,
  ObjectPrototypeIsPrototypeOf,
  SafeSet,
  SetPrototypeAdd,
  SetPrototypeClear,
  SetPrototypeDelete,
  SafeSetIterator,
  String,
} = primordials;

// ---------------------------------------------------------------------------
// Outbound fetch
// ---------------------------------------------------------------------------

const nativeFetch = fetchModule.fetch;

// The URL this call will actually be made to.
//
// Read once and reused, which matters: `String(input)` on an object the
// application controls runs the application's own `toString`, and a second
// call could answer differently. The value that is judged must be the value
// that is dialled, so a non-Request input is turned into a plain string here
// and that string -- not the original object -- is what is handed on.
//
// A `Request` is read through its inner request, the same field `fetch`
// itself uses, rather than through the `url` getter a subclass could shadow.
function destinationOf(input) {
  if (ObjectPrototypeIsPrototypeOf(request.RequestPrototype, input)) {
    return { url: request.toInnerRequest(input).url(), target: input };
  }
  const url = String(input);
  return { url, target: url };
}

async function fetch(input, init = undefined) {
  const { url, target } = destinationOf(input);
  // Throws a TypeError -- which is what a failed `fetch` throws per spec, so a
  // handler's existing error path catches it -- and logs the refusal on the
  // node with the application, the destination and the reason. Nothing is
  // dialled when this rejects.
  await op_deis_fetch_guard(url);
  return await nativeFetch(target, init);
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
  // timers section above.
  __deis_runtime_sweep_intervals: core.propNonEnumerable(sweepIntervals),
});
