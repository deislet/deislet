// Edge Canon Runtime (v0.2.0) for Runtime.
//
// The standard web globals -- fetch, crypto, the streams, Request/Response,
// URL, the timers -- are bound in `js/bootstrap.js`, which runs before this
// file. What is here is the part that is this platform rather than the web:
// the service bindings behind `ctx.services`, the WebSocket bridge, the Cache
// API, the console bridge, and the entry points the Rust side calls into.
//
// # Why the whole file is a closure
//
// This runs through `execute_script`, so it is a classic script, and a
// top-level `const` in one of those goes into the *global* lexical scope --
// visible by name to every script and module evaluated afterwards, which
// includes the application's own. `DenoCore` was declared that way, so until
// this closure existed an application could write `DenoCore.ops.op_fetch(...)`
// and reach every op in the isolate directly: past the outbound fetch policy,
// past `ctx.services`, past everything. Inside a closure none of these names
// exist outside this file, and the only way in is the handful of properties
// explicitly written onto `globalThis` below.
//
// # Why `Deno` is not the only door that has to be shut
//
// `deno_core` publishes its op table on the global object twice: as
// `globalThis.Deno.core` and as `globalThis.__bootstrap.core`. The second is
// the namespace `99_main.js` cleans up at the end of a full Deno bootstrap --
// which this runtime does not run, so it is left behind. Either one hands an
// application `core.ops.op_fetch`, `core.ops.op_net_connect_tcp` and every
// other op directly, which is every bit as much a bypass of the outbound
// fetch policy as reaching `Deno.core` would be: a raw `op_net_connect_tcp`
// opens a socket to any address deno's coarse net backstop does not deny, and
// on a node that means a loopback service the policy would have refused.
// `ext:core/mod.js` captured `core` into this module's imports before either
// global existed for an application to read, so deleting both here costs the
// runtime nothing and closes the door for good. `isolate_test.rs` asserts an
// application can reach neither.
(() => {

const DenoCore = Deno.core;
delete globalThis.Deno;
delete globalThis.__bootstrap;

// The console this file logs through, which is deliberately not the one an
// application sees.
//
// `globalThis.console` is writable, as it is in a browser and in Deno, and an
// application that assigns over it to install its own formatting is doing
// something ordinary. But every `console` call in *this* file is the platform
// speaking, and most of them are addressed to an operator rather than to the
// application: what an invocation left armed and had cleared, a `waitUntil`
// that failed, a queue batch dropped for want of a handler. Read off the global
// name, one line of `globalThis.console = { warn() {} }` in a handler's
// top-level code would silence all of it for the life of the isolate -- and the
// timer sweep's warning is the only account an operator gets of an application
// that leaves work running past its requests. Filled in by the console-bridge
// block at the end of this file, which is also what puts the application's copy
// on `globalThis`; the value here is the raw console until then, and nothing
// below runs before the bridge is built.
let platformConsole = globalThis.console;

// ---------------------------------------------------------
// Reading an options object the application handed in
// ---------------------------------------------------------
//
// An ordinary property read walks the prototype chain, and every options bag
// on this platform used to be read that way -- `init.status`,
// `opts.expirationTtl`, `m.body.text`. An application may write to its own
// `Object.prototype`; nothing can stop it, and nothing should. But the
// isolate is reused across requests, so one line of
// `Object.prototype.status = 418` in a handler decided the status of every
// later response built with no init at all, on every later request, including
// requests belonging to somebody else's traffic through the same isolate.
// With `Object.prototype.status = 101` and a forged `webSocket` beside it,
// the same line made a plain GET answer 101, which is the dispatcher's signal
// to start a WebSocket bridge.
//
// So the platform reads only what the application actually passed. `optionOf`
// is that read: own properties, and `undefined` for a bag that was never
// given. What it deliberately does not do is narrow the bags an application
// passes on purpose -- `new Response(body, upstream)` puts the members on a
// prototype and means them -- so the dictionaries themselves are still handed
// on whole, and only the members the *platform* acts on are read this way.
//
// Captured here rather than reached for at each call: `Object.hasOwn` and the
// rest are on the global object, and an application can replace them.
const ObjectAssign = Object.assign;
const ObjectCreate = Object.create;
const ObjectDefineProperty = Object.defineProperty;
const ObjectFreeze = Object.freeze;
const NativeWeakSet = WeakSet;
const NativeMap = Map;
const NativeSet = Set;
const NumberIsInteger = Number.isInteger;
const NumberIsFinite = Number.isFinite;
const hasOwn = typeof Object.hasOwn === "function"
  ? Object.hasOwn
  : (target, key) => Object.prototype.hasOwnProperty.call(target, key);

// One member of an options bag, or undefined when the caller did not pass it.
function optionOf(source, name) {
  if (source === null || source === undefined) return undefined;
  const kind = typeof source;
  if (kind !== "object" && kind !== "function") return undefined;
  return hasOwn(source, name) ? source[name] : undefined;
}

// ---------------------------------------------------------
// 0. Encoding
// ---------------------------------------------------------
//
// TextEncoder, TextDecoder, btoa and atob, which SPECIFICATION_BASIC.md
// section 8.1 lists among the Web APIs every platform must provide.
//
// These four are written out here rather than bound from deno_web the way the
// rest of section 8.1 is, and that is a deliberate narrowing: UTF-8 is the
// only encoding the canon asks for, so it is the only one implemented, and any
// other label is refused rather than silently decoded as UTF-8. deno_web's
// decoder accepts the whole WHATWG encoding registry; an application that
// happened to work here on `iso-8859-1` would be relying on something the
// canon does not promise.
//
// They are not decoration. Several things below already reached for them and
// therefore threw ReferenceError instead of doing their job: reading a binary
// request body, `request.arrayBuffer()`, `blob.put(key, "a string")`,
// `(await blob.get(key)).text()` and a binary WebSocket frame.

if (typeof globalThis.TextEncoder === "undefined") {
  class TextEncoder {
    get encoding() { return "utf-8"; }

    encode(input = "") {
      const str = String(input);
      const bytes = [];
      for (let i = 0; i < str.length; i++) {
        let cp = str.codePointAt(i);
        if (cp > 0xffff) {
          // A surrogate pair is one code point spelled with two units.
          i++;
        } else if (cp >= 0xd800 && cp <= 0xdfff) {
          // A lone surrogate is not a character; UTF-8 has no encoding for
          // it, and the standard replaces it rather than emitting bytes no
          // decoder would accept.
          cp = 0xfffd;
        }

        if (cp < 0x80) {
          bytes.push(cp);
        } else if (cp < 0x800) {
          bytes.push(0xc0 | (cp >> 6), 0x80 | (cp & 0x3f));
        } else if (cp < 0x10000) {
          bytes.push(0xe0 | (cp >> 12), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f));
        } else {
          bytes.push(
            0xf0 | (cp >> 18),
            0x80 | ((cp >> 12) & 0x3f),
            0x80 | ((cp >> 6) & 0x3f),
            0x80 | (cp & 0x3f),
          );
        }
      }
      return new Uint8Array(bytes);
    }

    encodeInto(source, destination) {
      const encoded = this.encode(source);
      // Only whole code points are written, so a destination that cannot hold
      // the last one is left short rather than filled with half a character.
      let written = 0;
      while (written < encoded.length && written < destination.length) {
        let width = 1;
        const lead = encoded[written];
        if (lead >= 0xf0) width = 4;
        else if (lead >= 0xe0) width = 3;
        else if (lead >= 0xc0) width = 2;
        if (written + width > destination.length) break;
        for (let i = 0; i < width; i++) destination[written + i] = encoded[written + i];
        written += width;
      }
      // `read` counts UTF-16 units consumed, which is what the standard asks
      // for; recovering it exactly means re-walking the source.
      let read = 0;
      let produced = 0;
      const str = String(source ?? "");
      for (let i = 0; i < str.length && produced < written; i++) {
        const cp = str.codePointAt(i);
        const units = cp > 0xffff ? 2 : 1;
        const width = cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
        if (produced + width > written) break;
        produced += width;
        read += units;
        if (units === 2) i++;
      }
      return { read, written };
    }
  }
  globalThis.TextEncoder = TextEncoder;
}

if (typeof globalThis.TextDecoder === "undefined") {
  const UTF8_LABELS = new Set(["utf-8", "utf8", "unicode-1-1-utf-8"]);

  class TextDecoder {
    constructor(label = "utf-8", options = undefined) {
      const encoding = String(label).trim().toLowerCase();
      if (!UTF8_LABELS.has(encoding)) {
        throw new RangeError(
          "TextDecoder on this platform decodes UTF-8 only; '" + label + "' is not supported",
        );
      }
      this._fatal = !!optionOf(options, "fatal");
      this._ignoreBOM = !!optionOf(options, "ignoreBOM");
    }

    get encoding() { return "utf-8"; }
    get fatal() { return this._fatal; }
    get ignoreBOM() { return this._ignoreBOM; }

    decode(input) {
      if (input === undefined || input === null) return "";

      let bytes;
      if (input instanceof Uint8Array) {
        bytes = input;
      } else if (input instanceof ArrayBuffer) {
        bytes = new Uint8Array(input);
      } else if (ArrayBuffer.isView(input)) {
        bytes = new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
      } else {
        throw new TypeError("TextDecoder.decode expects a BufferSource");
      }

      let i = 0;
      // A byte order mark is metadata, not text, and is dropped unless the
      // caller asked to see it.
      if (
        !this._ignoreBOM && bytes.length >= 3 &&
        bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf
      ) {
        i = 3;
      }

      let out = "";
      while (i < bytes.length) {
        const lead = bytes[i];
        let cp;
        let width;

        if (lead < 0x80) { cp = lead; width = 1; }
        else if (lead >= 0xc2 && lead <= 0xdf) { cp = lead & 0x1f; width = 2; }
        else if (lead >= 0xe0 && lead <= 0xef) { cp = lead & 0x0f; width = 3; }
        else if (lead >= 0xf0 && lead <= 0xf4) { cp = lead & 0x07; width = 4; }
        else { cp = -1; width = 1; }

        if (cp >= 0 && i + width <= bytes.length) {
          for (let k = 1; k < width; k++) {
            const cont = bytes[i + k];
            if ((cont & 0xc0) !== 0x80) { cp = -1; break; }
            cp = (cp << 6) | (cont & 0x3f);
          }
        } else if (cp >= 0) {
          // Truncated at the end of the input.
          cp = -1;
        }

        // Overlong forms, surrogates and anything past the last code point
        // are all invalid however they were spelled.
        if (
          cp < 0 ||
          (width === 3 && cp < 0x800) ||
          (width === 4 && cp < 0x10000) ||
          (cp >= 0xd800 && cp <= 0xdfff) ||
          cp > 0x10ffff
        ) {
          if (this._fatal) {
            throw new TypeError("TextDecoder: the input is not well-formed UTF-8");
          }
          out += "�";
          i += 1;
          continue;
        }

        out += String.fromCodePoint(cp);
        i += width;
      }
      return out;
    }
  }
  globalThis.TextDecoder = TextDecoder;
}

if (typeof globalThis.btoa === "undefined") {
  const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

  // btoa/atob work on binary strings - one character per byte - not on text.
  // A character above U+00FF has no byte to stand for, and the standard
  // throws rather than truncating it.
  globalThis.btoa = function btoa(input) {
    const str = String(input);
    let out = "";
    for (let i = 0; i < str.length; i += 3) {
      const c0 = str.charCodeAt(i);
      const c1 = i + 1 < str.length ? str.charCodeAt(i + 1) : 0;
      const c2 = i + 2 < str.length ? str.charCodeAt(i + 2) : 0;
      if (c0 > 0xff || c1 > 0xff || c2 > 0xff) {
        throw new TypeError("btoa: the string contains characters outside the Latin-1 range");
      }
      const triple = (c0 << 16) | (c1 << 8) | c2;
      out += B64[(triple >> 18) & 0x3f];
      out += B64[(triple >> 12) & 0x3f];
      out += i + 1 < str.length ? B64[(triple >> 6) & 0x3f] : "=";
      out += i + 2 < str.length ? B64[triple & 0x3f] : "=";
    }
    return out;
  };

  globalThis.atob = function atob(input) {
    // Whitespace is allowed anywhere and ignored; everything else must be
    // base64 or the input is not what it claims to be.
    const str = String(input).replace(/[ \t\n\f\r]/g, "");
    if (str.length % 4 !== 0) {
      throw new TypeError("atob: the input is not correctly encoded base64");
    }
    const body = str.replace(/=+$/, "");
    let out = "";
    let buffer = 0;
    let bits = 0;
    for (const ch of body) {
      const value = B64.indexOf(ch);
      if (value < 0) {
        throw new TypeError("atob: the input is not correctly encoded base64");
      }
      buffer = (buffer << 6) | value;
      bits += 6;
      if (bits >= 8) {
        bits -= 8;
        out += String.fromCharCode((buffer >> bits) & 0xff);
      }
    }
    return out;
  };
}

// ---------------------------------------------------------
// 1. WebSocket Support (Custom Bridge)
// ---------------------------------------------------------

// One `EventTarget` for the isolate, and it is deno_web's, bound as a global
// by `js/bootstrap.js`.
//
// This file used to declare a small one of its own and install it under that
// name, which left the isolate with two: the outbound `WebSocket` in
// `js/ws_client.js` extends deno_web's, so `ws instanceof EventTarget` --
// the guard a library runs before it attaches a listener -- answered false
// against the class an application could actually name. Listeners still
// fired, so nothing failed loudly; the library simply took its other branch.
//
// Captured here rather than read off the global object at dispatch time,
// along with the event classes, so that a frame arriving mid-request is
// turned into an event by the platform's constructors and not by whatever an
// application has since put on `globalThis`.
const PlatformEventTarget = globalThis.EventTarget;
const platformEvents = globalThis.__deis_runtime_events;
if (!PlatformEventTarget || !platformEvents) {
  // Said out loud rather than left to fail as "class extends undefined" three
  // lines later. The one way to reach here is an isolate restored from a
  // startup snapshot taken before `js/bootstrap.js` bound these, since a
  // snapshot skips bootstrap entirely; the snapshot file carries no version,
  // so the fix is to delete it and let it be taken again.
  throw new Error(
    "js/runtime.js: the platform's EventTarget and event classes are missing from this " +
      "isolate. It was restored from a startup snapshot taken before js/bootstrap.js bound " +
      "them; delete the file named by DEIS_RUNTIME_SNAPSHOT_PATH so the snapshot is retaken."
  );
}
const {
  Event: PlatformEvent,
  MessageEvent: PlatformMessageEvent,
  CloseEvent: PlatformCloseEvent,
  ErrorEvent: PlatformErrorEvent,
  defineEventHandler,
} = platformEvents;

// Base64 as it is before any application code runs; a frame carrying bytes is
// decoded with this and not with a replacement.
const decodeBase64 = globalThis.atob;

// How the kind of an inbound frame reaches this file.
//
// `dispatcher/handlers.rs` stamps one of these characters in front of every
// payload it forwards, and `runtime/ws_server_ops.rs` is where the two are
// spelled on the Rust side. The kind travels in front of the payload rather
// than inside it because the payload is the client's: the bridge used to wrap
// a binary frame as `{"type":"binary","data":"<base64>"}` and this file
// `JSON.parse`d *every* text frame and believed any object shaped like that
// one, so a client could choose which kind the application saw, and an
// application whose own protocol happened to use those field names had its
// messages silently turned into bytes. A leading character is a position the
// client's own bytes can never occupy.
const WS_FRAME_TEXT = "\u0001";
const WS_FRAME_BINARY = "\u0002";

// The Response an application returns, and the one it gets back from
// `fetch()`, are both the standard `Response` that `js/bootstrap.js` binds
// from deno_fetch. This subclass adds the one thing the platform needs on top
// of it, and nothing else.
//
// # Why there is a subclass at all
//
// A WebSocket upgrade is spelled `new Response(null, { status: 101, webSocket:
// client })` -- that is the shape on Cloudflare, it is the shape the canon's
// examples use, and `dispatcher/handlers.rs` looks for exactly it. The
// standard `Response` cannot express either half of it: its constructor
// refuses any status outside 200-599, and it drops init members it does not
// know, so `webSocket` would never arrive. So:
//
//   * status 101 is built as 200 and then written over the instance, where it
//     shadows the prototype's getter. `__deis_runtime_invoke` reads
//     `resp.status`, so what it sees is the 101 the handler asked for.
//   * `webSocket` is carried across as an own property, which is where
//     `__deis_runtime_invoke` looks for it.
//
// # Why `instanceof` is widened
//
// `__deis_runtime_invoke` rejects a handler that does not return a Response,
// and `return fetch(upstream)` -- a plain reverse proxy, and the first thing
// anyone writes once `fetch` exists -- hands back a *native* Response, which
// is not an instance of a subclass of it. Without the `hasInstance` below that
// handler would get "Handler did not return a Response object" and a 500. Both
// directions are accepted here: anything the native constructor made counts.
const NativeResponse = globalThis.Response;

// Captured before any application code can replace the global: a body's road
// out of the isolate must not be decided by a class the application swapped in.
const ReadableStreamCtor = globalThis.ReadableStream;

// The responses whose bodies are already bytes. See the constructor below.
const bufferedBodies = new NativeWeakSet();

class Response extends NativeResponse {
  constructor(body = null, init = undefined) {
    // No init means no init. This read `init ?? {}`, and an empty object
    // literal inherits from `Object.prototype` -- so both decisions below, the
    // upgrade status and the `webSocket` that goes with it, were answered by
    // whatever the application had left there, for every response built with
    // no init for the rest of the isolate's life. `Object.prototype.status =
    // 101` with a forged `webSocket` beside it made a plain GET come back as
    // the upgrade `dispatcher/handlers.rs` acts on.
    const options = init === null || init === undefined ? undefined : init;

    // Own members, both of them. An init the application really did pass is
    // handed to `super` as it stands -- `new Response(body, upstream)`, where
    // the members are getters on `Response.prototype`, is how a handler
    // rewrites a response it fetched, and copying own members only would
    // quietly drop the status and the headers it meant to keep. The two read
    // here are read the narrow way regardless: an upgrade is spelled with a
    // literal, and `webSocket` is not a member of any dictionary the web
    // defines, so neither has a reason to arrive down a prototype chain.
    const upgrading = Number(optionOf(options, "status")) === 101;
    super(
      body,
      // 101 is outside the range the native constructor accepts, so it is
      // built as 200 and written over below.
      upgrading
        ? ObjectAssign(ObjectCreate(null), options, { status: 200 })
        : options,
    );
    if (upgrading) {
      Object.defineProperty(this, "status", {
        value: 101,
        writable: false,
        enumerable: false,
        configurable: true,
      });
    }
    const socket = optionOf(options, "webSocket");
    if (socket) {
      this.webSocket = socket;
    }

    // Which road this response's body takes out of the isolate.
    //
    // A body the application handed over as bytes -- a string, a buffer, a
    // Blob, nothing at all -- is already whole, and it leaves as one buffer
    // exactly as it always has. A body handed over as a `ReadableStream` is
    // one the application is still producing, and it leaves chunk by chunk;
    // see `__deis_runtime_stream_begin`.
    //
    // Recorded in a WeakSet rather than as a property so that neither an
    // application nor a `structuredClone` can move a response from one road to
    // the other, and so that nothing is added to the shape of a Response.
    //
    // A Response this constructor did not build is one that came back from
    // `fetch()` (or from `Response.prototype.clone`, which builds a native
    // one), and it is not in the set: it streams. That is the point of the
    // rule rather than a side effect of it -- `return fetch(url)` is the
    // shortest reverse proxy anyone writes, and reading a large upstream into
    // this isolate's heap before answering is what used to get such an
    // application killed for a file it never needed to hold.
    if (!(body instanceof ReadableStreamCtor)) {
      bufferedBodies.add(this);
    }
  }

  static [Symbol.hasInstance](value) {
    return value instanceof NativeResponse;
  }
}

globalThis.Response = Response;

// BridgeWebSocket: A placeholder WebSocket for the client side of a WebSocketPair.
// It mainly serves to carry the ws_id back to the platform via Response.
class BridgeWebSocket extends PlatformEventTarget {
  constructor() {
    super();
    this.ws_id = `ws-bridge-${Date.now()}-${Math.random()}`;
    this.readyState = 1; // OPEN (Simulated)
  }
  
  send(data) { platformConsole.warn("BridgeWebSocket.send ignored"); }
  close() { this.readyState = 3; }
  accept() {} 
}

if (!globalThis.__deis_runtime_ws_servers) {
  globalThis.__deis_runtime_ws_servers = {};
}

class WebSocketServer extends PlatformEventTarget {
  constructor(ws_id) {
    super();
    this.ws_id = ws_id;
    this.readyState = 0; // CONNECTING
    this._accepted = false;
    this._closeDispatched = false;
    // What this socket is a continuation of.
    //
    // A `WebSocketPair` is built inside the handler answering the upgrade, so
    // that request is what everything this socket ever does is for: a frame
    // arriving on it two minutes later is still that caller's, and whatever
    // the application answers it with is that caller's answer. The node runs
    // `_handleMessage` and `_handleClose` from outside any invocation of their
    // own, and before this they read whichever request this isolate had served
    // most recently -- which made a `Cache.put` from a message handler refused
    // when nothing had run in between, and stored when an unrelated anonymous
    // `GET` had. Read once, here, where the answer is still true.
    this._answering = whatIsAnswered();
    // Register global reference for Rust ops to find
    globalThis.__deis_runtime_ws_servers[ws_id] = this;
  }

  // Called by user to accept the connection
  accept() {
    if (this._accepted) throw new Error("WebSocket already accepted");
    this._accepted = true;
    this.readyState = 1; // OPEN
    const result = DenoCore.ops.op_ws_server_accept(this.ws_id);
    if (result.success) {
      this.dispatchEvent(new PlatformEvent("open"));
    } else {
      this.dispatchEvent(
        new PlatformErrorEvent("error", {
          message: result.error,
          error: result.error,
        })
      );
    }
  }

  // Called by Rust when a frame arrives from the client.
  //
  // The leading character says which kind of frame it was; see the constants
  // above for why that cannot be read out of the payload. A payload with no
  // stamp is text, which is the only reading that is safe: nothing a client
  // sends reaches this branch, because the bridge stamps every frame it
  // forwards.
  _handleMessage(message) {
    // The node calls this from a stretch of its own, so the socket says what
    // that stretch is answering; see the constructor.
    nowAnswering(this._answering);
    const payload = String(message);
    const kind = payload.charAt(0);
    let data;
    if (kind === WS_FRAME_BINARY) {
      const binaryString = decodeBase64(payload.slice(1));
      const bytes = new Uint8Array(binaryString.length);
      for (let i = 0; i < binaryString.length; i++) {
        bytes[i] = binaryString.charCodeAt(i);
      }
      data = bytes.buffer;
    } else if (kind === WS_FRAME_TEXT) {
      data = payload.slice(1);
    } else {
      data = payload;
    }

    this.dispatchEvent(new PlatformMessageEvent("message", { data }));
  }

  // Called by Rust when connection closed
  _handleClose(code, reason) {
    // As in `_handleMessage`: this runs from the node, in a stretch of its own.
    // `_dispatchClose` deliberately does not do this -- it is also reached from
    // `close()`, which an application may call from a handler answering a
    // different request, and that handler's own statement must not be
    // overwritten by a socket it happens to be closing.
    nowAnswering(this._answering);
    this.readyState = 3; // CLOSED
    // 1006 is the code a connection that ended without a close frame is
    // reported under, and it is the one case that is not a clean shutdown.
    this._dispatchClose(code, reason, code !== 1006);
  }

  // A socket closes once, however many paths reach the closing.
  _dispatchClose(code, reason, wasClean) {
    if (this._closeDispatched) return;
    this._closeDispatched = true;
    this.dispatchEvent(
      new PlatformCloseEvent("close", { code, reason, wasClean })
    );
  }

  // A frame back to the client, of the kind the argument is.
  //
  // Bytes go out as a BINARY frame. They used to go through
  // `JSON.stringify`, so `send(new Uint8Array([1, 2, 3]))` reached the client
  // as the TEXT frame `{"0":1,"1":2,"2":3}` -- an application on this
  // platform could not send a binary frame at all, and the `Binary` arm of
  // `WsServerMessage` was written, compiled and never constructed.
  async send(data) {
    if (this.readyState !== 1) throw new Error("WebSocket is not open");

    let result;
    if (typeof data === "string") {
      result = await DenoCore.ops.op_ws_server_send(this.ws_id, data);
    } else if (data instanceof ArrayBuffer) {
      result = await DenoCore.ops.op_ws_server_send_binary(
        this.ws_id,
        new Uint8Array(data)
      );
    } else if (ArrayBuffer.isView(data)) {
      // The view's own window, not the whole buffer behind it: a subarray
      // must send its own bytes and not the neighbours it shares storage
      // with.
      result = await DenoCore.ops.op_ws_server_send_binary(
        this.ws_id,
        new Uint8Array(data.buffer, data.byteOffset, data.byteLength)
      );
    } else {
      // Anything else keeps the shape this platform has always sent: JSON in
      // a TEXT frame, which is what applications written against it expect
      // from `send({ ... })`.
      result = await DenoCore.ops.op_ws_server_send(
        this.ws_id,
        JSON.stringify(data)
      );
    }

    if (!result.success) throw new Error(`Failed to send: ${result.error}`);
  }

  async close(code = 1000, reason = "") {
    this.readyState = 2; // CLOSING
    const request = { ws_id: this.ws_id, code, reason };
    await DenoCore.ops.op_ws_server_close(JSON.stringify(request));
    this.readyState = 3; // CLOSED
    this._dispatchClose(code, reason, true);
  }
}

// `onopen`, `onmessage`, `onclose` and `onerror` as event handler attributes
// rather than plain properties, so that assigning one and calling
// `addEventListener` are the same mechanism -- which is what they are on the
// outbound `WebSocket` and everywhere else. They used to be plain properties
// called by hand next to `dispatchEvent`, so a handler that did both was
// invoked twice for one frame.
defineEventHandler(WebSocketServer.prototype, "open");
defineEventHandler(WebSocketServer.prototype, "message");
defineEventHandler(WebSocketServer.prototype, "close");
defineEventHandler(WebSocketServer.prototype, "error");

globalThis.WebSocketServer = WebSocketServer;

class WebSocketPair {
  constructor() {
    // 0: Client (returned to response), 1: Server (used by app)
    const client = new BridgeWebSocket();
    const server = new WebSocketServer(client.ws_id);
    this[0] = client;
    this[1] = server;
  }
}
globalThis.WebSocketPair = WebSocketPair;

// ---------------------------------------------------------
// 2. Service Adapters (The Bridge)
// ---------------------------------------------------------

// The Cache API, which Edge Canon makes a basic feature every application may
// assume (SPECIFICATION_BASIC.md section 7): put a Response away under a
// request's URL, read it back, drop it. The default cache is reached two ways,
// both of them the same namespace: `context.services.cache`, which is what the
// canon specifies, and `globalThis.caches.default`, which is what a handler
// ported from Cloudflare reaches for. `caches.open(name)` gives an application
// caches of its own beside it.
//
// Scope: one store per isolate, so an entry is visible to later requests for
// the same application on the same node and to nothing else - not another
// node, not the replacement isolate a new version is loaded into. A hit is an
// optimisation; nothing may depend on one. `caches.scope` below is how an
// application checks that rather than having to know it.

// The key one of these calls is about. The canon's own examples pass all
// three of these: a string, the `Request`, and a `URL` built from the
// request's url. A URL has `href` and no `url`, so reading `.url` off one -
// which is what this did - keyed every such entry under the string
// "undefined", quietly collapsing every URL-keyed entry onto one.
function cacheKeyOf(request) {
  if (typeof request === "string") return request;
  if (request) {
    if (typeof request.url === "string") return request.url;   // Request
    if (typeof request.href === "string") return request.href; // URL
  }
  throw new TypeError(
    "cache: the key must be a Request, a URL or a string, got " + typeof request
  );
}

// One named header of a request or a response: its value, or null when the
// header is absent and equally when there is nothing to read a header off at
// all.
//
// Written once because four things below ask this question -- the directives,
// the `Vary` list, the range rule and the credential rule -- and a fourth copy
// of `headers && typeof headers.get === "function"` is a fourth chance for one
// of them to answer differently about an object that is not what it looks
// like.
function headerValue(source, name) {
  const headers = readableHeaders(source);
  return headers === null ? null : headers.get(name);
}

// The method a cache key names.
//
// The Cache API's key is a `Request`, and a `Request` carries a method: an
// entry is the answer to *that request*, not to its URL. A key given as a
// string or a `URL` is the spelling for `new Request(url)`, which is a GET, so
// those answer GET -- as does anything with no readable method, which makes no
// claim to the contrary.
//
// This is the question the whole store used to be missing. It was keyed by the
// URL alone, so the answer to `POST /orders` -- the result of an action, not a
// representation of anything -- was stored and then handed to every later
// `GET /orders`, whoever sent it.
function cacheKeyMethod(request) {
  if (typeof request === "string") return "GET";
  const method = request && request.method;
  return typeof method === "string" ? method.toUpperCase() : "GET";
}

// Headers a request may carry that cannot single one caller out from another.
//
// This is the list the whole cache now turns on, and it is a list of what is
// *safe* rather than of what is not. Four rounds of repair to this file each
// added the name that round had found -- repeated `Set-Cookie` collapsed, then
// `Vary` unread, then the method unread, then `Authorization` -- and each
// repair was correct and left the next name to be found, because the set of
// headers that can carry an identity has no end. `Cookie` is how most of the
// web authenticates and walked past a rule that named `Authorization`;
// `Proxy-Authorization` sits beside it in the same RFC; the one after that is
// whatever an application decides to call its own session token. A list of
// what is unsafe is always one name short of the next round. A list of what is
// safe is short, arguable name by name, and wrong in the direction that
// refuses to store something rather than the direction that hands one caller's
// page to the next.
//
// Every name is here for one reason: its value describes the format the caller
// can read, the software it is running, the network the request came over, or
// the copy the caller already holds. None of them says which principal is
// asking. A header this platform has never heard of is not here, which is the
// entire point -- an unknown name is refused, and the application is told
// which name and what to do about it.
const HEADERS_THAT_CANNOT_NAME_A_CALLER = new NativeSet([
  // What the caller can read, and what it already has. A language or an
  // encoding is a preference millions of callers share; a validator describes
  // a copy of the representation rather than its holder.
  "accept",
  "accept-charset",
  "accept-encoding",
  "accept-language",
  "accept-datetime",
  "if-match",
  "if-none-match",
  "if-modified-since",
  "if-unmodified-since",
  "if-range",
  "range",
  // How the request travelled and where it was addressed.
  "host",
  "connection",
  "keep-alive",
  "te",
  "trailer",
  "transfer-encoding",
  "upgrade-insecure-requests",
  "expect",
  "content-length",
  "content-type",
  "referer",
  // The caller's own software, and what it would like given its device.
  "user-agent",
  "dnt",
  "sec-gpc",
  "priority",
  "save-data",
  "device-memory",
  "downlink",
  "ect",
  "rtt",
  "viewport-width",
  "width",
  // The caller's own cache directives: what it wants done, not who it is.
  "cache-control",
  "pragma",
  // This node's own routing decision, put back on the request by the
  // dispatcher after the client's copy of the platform namespace was swept.
  // Both carry the same value on every request this isolate will ever see,
  // because the isolate *is* one application and one environment. The rest of
  // `x-deis-` is deliberately absent: `x-deis-caller` names which application
  // made a service call, which is precisely an identity.
  "x-deis-app-id",
  "x-deis-environment"
]);

// Two families rather than two hundred names, for the same reason and with the
// same care: a client hint describes the device and the browser, and a fetch
// metadata header describes how the fetch was made. Neither family has a
// member that says who is asking, and both keep growing names this platform
// will not have heard of.
const PREFIXES_THAT_CANNOT_NAME_A_CALLER = ["sec-ch-", "sec-fetch-"];

// Whether this header could make a response one caller's.
//
// The default is yes. That is the inversion: a name nobody listed is treated
// as one that might carry an identity, so the next credential header anybody
// invents arrives already refused.
function couldNameACaller(name) {
  if (HEADERS_THAT_CANNOT_NAME_A_CALLER.has(name)) return false;
  for (const prefix of PREFIXES_THAT_CANNOT_NAME_A_CALLER) {
    if (name.slice(0, prefix.length) === prefix) return false;
  }
  return true;
}

// The names on one request that could make its answer one caller's, lower
// cased, sorted and without repeats.
function headersThatCouldNameTheCaller(request) {
  const headers = readableHeaders(request);
  if (headers === null) return [];
  const names = [];
  for (const pair of headers) {
    const name = String(pair[0]).toLowerCase();
    if (!couldNameACaller(name)) continue;
    if (names.indexOf(name) === -1) names.push(name);
  }
  names.sort();
  return names;
}

// Response headers a stranger may be handed.
//
// The other half of the same inversion, and it is needed even when the request
// carried nothing: a response does not only *depend* on a caller, it can also
// *establish* something for one. `Set-Cookie` replayed out of a cache hands
// every later caller the session the first one was given, which is fixation
// rather than disclosure -- and no `Vary` can make it safe, because `Vary`
// describes what the request decided and this is something the response does
// to whoever receives it. Enumerating that family fails the same way the
// credential family did: `Set-Cookie2`, `Clear-Site-Data`,
// `Authentication-Info` and whatever is defined next.
//
// So the same sentence, in the other direction: a stored response may carry
// only what describes the representation, its freshness, or a policy that is
// the same for every caller of the site. Everything else -- including every
// header an application invented a name for, which is where a per-request
// token actually lives -- is refused, and the application stores a copy
// carrying the headers it means to serve.
const HEADERS_A_STRANGER_MAY_RECEIVE = new NativeSet([
  // What the body is.
  "content-type",
  "content-length",
  "content-encoding",
  "content-language",
  "content-location",
  "content-disposition",
  "content-digest",
  "repr-digest",
  "digest",
  "etag",
  "last-modified",
  "accept-ranges",
  "allow",
  "link",
  "location",
  "retry-after",
  "vary",
  // How long it is good for.
  "cache-control",
  "expires",
  "age",
  "date",
  "pragma",
  "warning",
  // Policy the site states, which is the same sentence for every caller of it.
  "content-security-policy",
  "content-security-policy-report-only",
  "cross-origin-opener-policy",
  "cross-origin-embedder-policy",
  "cross-origin-resource-policy",
  "permissions-policy",
  "referrer-policy",
  "strict-transport-security",
  "timing-allow-origin",
  "x-content-type-options",
  "x-frame-options",
  "x-permitted-cross-domain-policies",
  "x-xss-protection",
  "nel",
  "report-to",
  "reporting-endpoints",
  "alt-svc",
  "server",
  // Which origins may read it. `access-control-allow-credentials` is
  // deliberately absent: a credentialed cross-origin answer is one caller's by
  // definition. `Origin` is not on the request list either, so a response that
  // reflects one is filed under it or refused.
  "access-control-allow-origin",
  "access-control-allow-methods",
  "access-control-allow-headers",
  "access-control-expose-headers",
  "access-control-max-age"
]);

// The names on this response that a later, unidentified caller may not be
// handed. Empty when the whole response may be replayed.
function headersAStrangerMayNotReceive(response) {
  const headers = readableHeaders(response);
  if (headers === null) return [];
  const names = [];
  for (const pair of headers) {
    const name = String(pair[0]).toLowerCase();
    if (HEADERS_A_STRANGER_MAY_RECEIVE.has(name)) continue;
    if (names.indexOf(name) === -1) names.push(name);
  }
  names.sort();
  return names;
}

// One `Cache-Control` field, taken apart into the directives it carries.
//
// Written once and read by everything below, because the field decides two
// separate things -- whether the response may be stored at all, and how long
// it stays fresh -- and a regular expression per question, scanning the same
// string, is one chance per pair of them to disagree about what a directive
// is. A quoted value is understood, so `private="set-cookie"` is the `private`
// directive rather than a header value that happens to contain the word, and
// `no-cache="set-cookie"` is `no-cache`. Both used to be stored.
//
// Answers a Map of directive name (lower case) to its value, or `true` for a
// directive that carries none. An absent or unreadable field answers an empty
// Map, which every reader below treats as "the response said nothing".
function cacheControlOf(response) {
  const directives = new NativeMap();
  const raw = headerValue(response, "cache-control");
  if (typeof raw !== "string") return directives;

  let name = "";
  let value = "";
  let inValue = false;
  let quoted = false;
  const finish = () => {
    const directive = name.trim().toLowerCase();
    if (directive !== "") directives.set(directive, inValue ? value.trim() : true);
    name = "";
    value = "";
    inValue = false;
  };
  for (let i = 0; i < raw.length; i++) {
    const c = raw[i];
    if (quoted) {
      if (c === "\\" && i + 1 < raw.length) {
        value += raw[++i];
        continue;
      }
      if (c === '"') {
        quoted = false;
        continue;
      }
      value += c;
      continue;
    }
    if (c === '"' && inValue) {
      quoted = true;
      continue;
    }
    if (c === ",") {
      finish();
      continue;
    }
    if (c === "=" && !inValue) {
      inValue = true;
      continue;
    }
    if (inValue) value += c;
    else name += c;
  }
  finish();
  return directives;
}

// A directive's value as whole seconds, or null when it named none or named
// something that is not a count.
function wholeSeconds(value) {
  if (typeof value !== "string") return null;
  const digits = value.trim();
  if (digits === "" || /[^0-9]/.test(digits)) return null;
  const seconds = Number(digits);
  return Number.isFinite(seconds) ? seconds : null;
}

// How long the response asked to stay fresh, in whole seconds, or 0 when it
// named no lifetime and the store's default should apply.
//
// `s-maxage` wins over `max-age` where both are present: this is a shared
// cache, and that is what the directive is for.
//
// A lifetime of zero never reaches here: it means the response is stale the
// moment it is stored, which `refusesToBeStored` turns into a refusal. Without
// that, "the response asked for zero" and "the response asked for nothing"
// were the same answer here, and a `max-age=0` response -- the ordinary way of
// saying *check with me every time* -- was kept for the store's ten-minute
// default.
function freshnessSeconds(directives) {
  const shared = wholeSeconds(directives.get("s-maxage"));
  const chosen = shared !== null ? shared : wholeSeconds(directives.get("max-age"));
  if (chosen === null || chosen <= 0) return 0;
  // The op takes a u32; a longer lifetime than that is indistinguishable from
  // "as long as this isolate lives" anyway.
  return Math.min(Math.floor(chosen), 4294967295);
}

// Why this response may not be put in a cache that later requests read, or
// null when it may.
//
// This store is a *shared* cache: one per isolate, read by every later request
// to the same application, from every client. RFC 9111 gives each of these
// directives a meaning for such a cache, and this is that meaning and no
// interpretation of it:
//
//   * `no-store` -- must not be stored at all;
//   * `private` -- must not be stored by a shared cache. It is the directive a
//     response carries when its body is one user's;
//   * `no-cache` -- may be stored, but must not be reused without first
//     revalidating with the origin. There is no revalidation here: nothing in
//     this cache can ask an upstream whether an entry is still good. An entry
//     that may only be served after a check that cannot be made is one that may
//     never be served, so storing it would spend the store's memory on
//     something no `match` is allowed to hand back;
//   * a shared lifetime of zero (`s-maxage=0`, or `max-age=0` with no
//     `s-maxage`) -- stale on arrival, which is the same position as `no-cache`
//     for a cache that cannot revalidate.
//
// `must-revalidate` and `proxy-revalidate` are deliberately not here: they
// constrain what happens once an entry is *stale*, and an entry served inside
// its own lifetime is not stale. This store drops an entry when its lifetime
// ends rather than serving it on, so it never reaches the case they govern.
function refusesToBeStored(directives) {
  if (directives.has("no-store")) {
    return "Cache-Control: no-store, which means it may not be stored at all";
  }
  if (directives.has("private")) {
    return "Cache-Control: private, which means it is one user's and a cache " +
      "every later request reads may not hold it";
  }
  if (directives.has("no-cache")) {
    return "Cache-Control: no-cache, which means it may only be served after " +
      "revalidating with the origin, and this cache cannot revalidate";
  }
  const shared = wholeSeconds(directives.get("s-maxage"));
  const lifetime = shared !== null ? shared : wholeSeconds(directives.get("max-age"));
  if (lifetime === 0) {
    return "Cache-Control: a lifetime of zero seconds, which means it is stale " +
      "as soon as it is stored, and this cache cannot revalidate a stale entry";
  }
  return null;
}

// How long this response may be kept, and why it may not be kept at all.
//
// Answers `{ refusal, seconds }`: `refusal` is the sentence the application is
// given when the response says a cache like this one may not hold it, and
// `seconds` is the lifetime to store it with -- zero meaning the response named
// none and the store's default applies.
//
// Three fields say something about freshness and all three are read here. A
// response is stale when *any* of them says so, and which one its author wrote
// is not this platform's to choose: reading only `Cache-Control` meant a
// response saying in `Expires` that it went stale an hour ago was kept for the
// store's ten-minute default and served to everything that asked. The order is
// RFC 9111's: `Cache-Control` decides whenever it names a lifetime; `Expires`
// is read when it does not; `Pragma` -- the HTTP/1.0 spelling, still written by
// plenty of frameworks -- is read only when there is no `Cache-Control` at all,
// because a response carrying both has already said what it means in the field
// that has a meaning for a shared cache.
function lifetimeOf(response, directives) {
  const refusal = refusesToBeStored(directives);
  if (refusal !== null) return { refusal, seconds: 0 };

  const named = freshnessSeconds(directives);
  if (named > 0) return { refusal: null, seconds: named };

  const expires = headerValue(response, "expires");
  if (typeof expires === "string" && expires.trim() !== "") {
    // RFC 9111 section 5.3: a value that is not an HTTP-date means the
    // response is already stale, and `0` and `-1` are what frameworks write
    // when they mean exactly that.
    const at = Date.parse(expires);
    const now = Date.now();
    if (!NumberIsFinite(at) || at <= now) {
      return {
        refusal: "Expires: " + expires.trim() + ", which is a moment that has " +
          "already passed, so the response is stale before it is stored and " +
          "this cache cannot revalidate a stale entry",
        seconds: 0
      };
    }
    // At least a second: a date less than a second away is still a lifetime,
    // and rounding it to zero would mean "the response named none", which is
    // the store's ten-minute default and the opposite of what it said.
    return { refusal: null, seconds: Math.max(1, Math.ceil((at - now) / 1000)) };
  }

  if (directives.size === 0) {
    const pragma = headerValue(response, "pragma");
    if (typeof pragma === "string" && pragma.toLowerCase().indexOf("no-cache") !== -1) {
      return {
        refusal: "Pragma: no-cache, the HTTP/1.0 spelling of no-cache, which " +
          "means it may only be served after revalidating with the origin, and " +
          "this cache cannot revalidate",
        seconds: 0
      };
    }
  }
  return { refusal: null, seconds: 0 };
}

// The header names a response says its body depends on, lower-cased, sorted
// and without repeats.
//
// Sorted so that `Vary: Accept-Language, Authorization` and
// `Vary: Authorization, Accept-Language` describe the same entry: the order the
// names were written in means nothing, and two entries that differ only by it
// would otherwise never match each other.
function varyNames(response) {
  const raw = headerValue(response, "vary");
  if (typeof raw !== "string" || raw === "") return [];
  const names = [];
  for (const part of raw.split(",")) {
    const name = part.trim().toLowerCase();
    if (name !== "" && names.indexOf(name) === -1) names.push(name);
  }
  names.sort();
  return names;
}

// What those names were worth on one request: `[[name, value or null], ...]`.
//
// A key that is not a `Request` -- a URL or a string, both of which the Cache
// API accepts -- carries no headers, so every name is worth null. On the way
// in that is refused (see `put` below); on the way out it is exactly right: a
// `match` given a bare URL is asking for the variant that carries none of the
// varying headers, and an entry stored for a caller who carried them is not
// that variant.
function varySelector(names, request) {
  const headers = readableHeaders(request);
  return names.map((name) => [name, headers ? headers.get(name) : null]);
}

// The headers of a cache key that is a `Request`, or null for one that is a
// URL or a string -- both of which the Cache API accepts and neither of which
// carries any. Also the way every rule below reads a header off a *response*,
// through `headerValue`: the question and its one wrong answer are the same
// on both sides.
function readableHeaders(request) {
  const headers = request && request.headers;
  return headers && typeof headers.get === "function" ? headers : null;
}

// ---------------------------------------------------------
// What the code running right now is answering
// ---------------------------------------------------------
//
// `Cache.put` is the one call on this platform whose answer depends on what
// the code making it is answering, and not only on its arguments: a handler
// serving a caller who sent a session cookie may produce anything from that
// cookie, whatever key it then writes the answer under. So the request being
// answered is read alongside the key, and something has to say what it is.
//
// The shape this replaces was one module-level variable, `servedRequest`, set
// when a request invocation built its context and cleared at the top of the
// three invocation entry points. Nothing cleared it when an invocation
// *ended*, and four runners that execute application code never set it at all,
// so every stretch that runs between invocations -- a WebSocket `onmessage`, a
// response-stream producer, a timer, a `waitUntil` left over from a cut-off --
// read the last request this isolate happened to serve. Which request that was
// decided whether a `put` was refused or stored: the same handler was refused
// when nothing had run in between, and stored after one unrelated anonymous
// `GET` -- and the entry it stored was then handed to every later anonymous
// caller.
//
// A clear at the end of each runner would have been a fifth entry in a list
// that was already missing four, which is the shape that failed. So what a
// runner states is stamped with the number `runtime/isolate.rs` gives the
// stretch it is running in (`InvocationCounter`, stepped at both ends of every
// invocation). A statement stamped with any other number is not readable at
// all, so:
//
//   * a runner that states nothing cannot inherit the last request. It reads
//     UNATTRIBUTED, which `Cache.put` refuses rather than guesses at;
//   * a runner that answers no request -- a queue batch, a cron tick, the
//     module's own top level -- says so, and gets the ordinary treatment;
//   * a runner that continues an earlier invocation's work -- a WebSocket
//     message on a socket opened by a request, the producer of a streaming
//     body -- carries that request with it explicitly, rather than finding it
//     left behind. The producer's is *derived* rather than stated; see
//     `streamBeingProduced` for why a statement does not survive the await its
//     own code runs inside.
//
// Nothing has to remember to clear anything, which is the whole point.
//
// # What this does not answer
//
// A stretch is a stretch of the *node's* work, not of one piece of application
// code. A promise an application leaves pending in one invocation and resolves
// from another -- `new Promise(r => stash = r)` in one handler, `stash()` in
// the next -- continues inside the second invocation's stretch and is read as
// answering the second invocation's request. That is not the previous defect
// coming back: nothing stale is inherited, and the request it is credited to is
// one that is genuinely being served right now. But it is not the request the
// continuation was written for either, and telling those apart needs the
// continuation itself to carry the context, which is what a host promise hook
// would give and this runtime does not have yet. The application is on both
// sides of it -- it is its own code resuming its own code -- so what is at
// stake is which of its own callers an entry is judged against, and the two
// statements on the call (`forCaller`, `forAnyCaller`) are how an application
// that does this says which.

// Nothing has said what this stretch of code is answering. The default, and
// what every statement made in another stretch collapses to.
const UNATTRIBUTED = null;

// The runner said it answers no request at all.
const NO_REQUEST = ObjectFreeze({ request: null });

// The statement in force, and the stretch it was made in. `-1` is a number
// `op_cache_invocation` never answers, so nothing is in force before the first
// statement is made.
let statedAnswer = UNATTRIBUTED;
let statedIn = -1;

// Which stretch of application code is running, as the node counts them.
//
// Read rather than remembered: the whole mechanism is that this side cannot
// tell one stretch from another on its own, which is exactly how the variable
// this replaces came to be read by four runners that never wrote it.
function stretchNow() {
  return DenoCore.ops.op_cache_invocation();
}

// State what the stretch now running is answering: `NO_REQUEST`, a record
// holding the `Request`, or a statement carried over from the stretch that
// created a socket or a response stream.
function nowAnswering(answer) {
  statedAnswer = answer;
  statedIn = stretchNow();
}

// A statement that this code answers the given `Request`.
function answerTo(request) {
  return ObjectFreeze({ request });
}

// What the code now running is answering: `NO_REQUEST`, a record holding the
// `Request`, or `UNATTRIBUTED` when nothing in this stretch has said.
//
// A statement made in another stretch is dropped here rather than merely
// ignored, so that it cannot be read by anything later either.
function whatIsAnswered() {
  if (statedIn !== stretchNow()) {
    statedAnswer = UNATTRIBUTED;
    statedIn = -1;
  }
  return statedAnswer;
}

// The one name the Rust side calls to say that a stretch answers no request.
// `runtime/isolate.rs` calls it at the top of the module-evaluation glue,
// because the top level of a bundle is application code like any other and has
// to be able to use the cache. Sealed with the rest of the runtime's names at
// the foot of this file.
//
// One-shot, because sealing is a floor and not a ceiling (`js/bootstrap.js`
// on `__deis_runtime_stream_scope` is this same lesson, already learned once):
// a sealed function is still a callable one, and a fetch handler that could
// call this mid-request would be stating "no request is being answered" while
// holding the request in its hands -- one line, and every check that reads
// `whatIsAnswered()` is waived for the rest of the stretch. The glue runs
// before a single line of application code has, so the only call that can be
// first is the glue's own; every later call, which can only be an
// application's, states nothing.
let noRequestClaimed = false;
globalThis.__deis_runtime_answers_no_request = function() {
  if (noRequestClaimed) return;
  noRequestClaimed = true;
  nowAnswering(NO_REQUEST);
};

// The headers a response varies on whose value the key does not carry.
//
// A variant is the list of values the response's `Vary` names had *on the
// request that produced it*. When the key is that request, every value in the
// selector is a fact. When it is anything else, a value the key carries is at
// least the caller's own claim about what this response is for -- but a value
// the key does *not* carry is nobody's claim at all: it is this platform
// inferring "the caller sent none of this header" from a key that simply has
// no headers, and filing the entry under the variant every anonymous caller
// matches.
//
// So the question is not "does the key carry any of them", which is what this
// used to ask. One extra name in `Vary` walked straight past that: a key
// carrying `Accept-Language` and no `Authorization` counted as carrying some,
// was filed as `accept-language=en, authorization=null`, and the next
// anonymous caller sending that language was handed the authenticated page.
// The question is which values are missing, and the answer has to be none.
function varyNamesTheKeyDoesNotCarry(selector) {
  const missing = [];
  for (const [name, value] of selector) {
    if (value === null) missing.push(name);
  }
  return missing;
}

// Why this response is not a whole answer that a later request could be given,
// or null when it is.
//
// An entry here is served whole, on its own, to a request that asked for
// nothing in particular. Three kinds of response cannot survive that, and each
// of them is an answer to a question the request that gets the entry never
// asked:
//
//   * `206 Partial Content` is one range of a body. This cache cannot join
//     ranges and cannot match a `Range` header against a stored one, so the
//     entry could only ever be handed whole to a caller who asked for the
//     whole body -- and told it is partial.
//   * a `Content-Range` says the same thing about the body regardless of the
//     status, and a status is written by hand often enough that the header is
//     the reliable half.
//   * `304 Not Modified` answers a conditional request, and means "the copy
//     you have is still good". Handed to a caller who has no copy it is a
//     dead end, and this cache has no validator to check and no upstream to
//     ask.
//
// A status outside 200-599 is not a response at all. `Response` refuses those,
// but `status` is read off the object, an own property shadows the getter, and
// a 101 built for a WebSocket upgrade is exactly such an object: stored, it
// would come back out of `match` as a bare 101 and hang whichever client was
// given it.
function notACompleteAnswer(response) {
  const status = response && response.status;
  if (!NumberIsInteger(status) || status < 200 || status > 599) {
    return "a status of " + String(status) + ", which is not one a stored " +
      "answer may carry: a cache entry has to be a complete response, and " +
      "200 through 599 is what a response may be";
  }
  if (status === 206) {
    return "206 Partial Content, which is one range of a body. This cache " +
      "cannot complete a range or match a Range header against one, so the " +
      "entry could only be handed whole to a caller who asked for the whole " +
      "body";
  }
  if (headerValue(response, "content-range") !== null) {
    return "a Content-Range, which says its body is one range of a larger " +
      "one. This cache stores whole answers, and a later request would be " +
      "given those bytes as though they were all of them";
  }
  if (status === 304) {
    return "304 Not Modified, which answers a conditional request by saying " +
      "the caller already has the body. This cache has no validator to check " +
      "and nothing to revalidate against, so no request it could be handed to " +
      "would be able to use it";
  }
  return null;
}

// Why this entry would not be safe to hand to a caller this platform cannot
// identify, or null when it would be.
//
// This is the whole of the rule, and it is asked of two requests rather than
// one. `served` is the request the code making this call is answering, or null
// when it answers none -- it is passed in rather than read from a variable
// here, because the one thing this rule may not do is read a request that
// belongs to some other stretch of code; see "What the code running right now
// is answering". The key says what the entry will be filed under. The request
// this invocation is answering says what the response could have been *about*: a
// handler serving a caller who sent a session cookie may produce anything from
// that cookie, whatever key the application then writes the answer under. So
// both are read, and every name on either of them that this platform does not
// recognise as one that cannot single a caller out has to be accounted for:
//
//   * the response has to file the entry under it, by naming it in `Vary`; and
//   * the key has to carry the same value for it as the request being
//     answered, or the entry would be filed under one caller and hold
//     another's answer.
//
// What this replaces named `Authorization` and nothing else, and offered
// `public` and `s-maxage` as ways round it. `Cookie` walked straight past the
// name, and the two ways round turned a correctly refused `put` into the
// disclosure it had just refused -- with the refusal message recommending
// them. Neither directive says anything here now: a directive is a lifetime,
// not a permission, and the only statements that mean "this answer is not one
// caller's" are made on the call, where no upstream and no copied header can
// produce one by accident.
function notSafeForAStranger(request, vary, served) {
  const alsoServed =
    served !== null && served !== request
      ? headersThatCouldNameTheCaller(served)
      : [];
  const names = headersThatCouldNameTheCaller(request);
  for (const name of alsoServed) {
    if (names.indexOf(name) === -1) names.push(name);
  }
  if (names.length === 0) return null;
  names.sort();

  const unfiled = names.filter((name) => vary.indexOf(name) === -1);
  if (unfiled.length > 0) {
    return "the request this entry answers, or the key it would be filed " +
      "under, carries " + unfiled.join(", ") + ", and nothing files the entry " +
      "under " +
      (unfiled.length === 1 ? "it" : "them") + ". This platform does not know " +
      "what every header means, so a name it cannot recognise as one that " +
      "describes the format, the software, the network or the caller's own " +
      "copy is treated as one that could name the caller -- a session cookie, " +
      "an API key, whatever this application calls its own token. A cache " +
      "every later request reads would hand this answer to whoever asked " +
      "next. Either file the entry under " + (unfiled.length === 1 ? "it" : "them") +
      " with Vary: " + unfiled.join(", ") + ", so only a caller sending the " +
      "same value matches, or say it on the call: { forCaller: <the caller " +
      "this answer is for> } stores it for that caller alone, and " +
      "{ forAnyCaller: true } states that the answer does not depend on the " +
      "caller at all";
  }

  if (served === null || served === request) return null;
  const servedHeaders = readableHeaders(served);
  if (servedHeaders === null) return null;
  const keyHeaders = readableHeaders(request);
  const mismatched = names.filter(
    (name) => (keyHeaders === null ? null : keyHeaders.get(name)) !== servedHeaders.get(name)
  );
  if (mismatched.length > 0) {
    return "the key carries a different " + mismatched.join(", ") + " than the " +
      "request this handler is answering, so the entry would be filed under " +
      "one caller and hold another's answer, and the caller it is filed under " +
      "is the one who would be given it. Key the entry with the request this " +
      "handler was given, or say whose answer it is with { forCaller: <the " +
      "caller this answer is for> }";
  }
  return null;
}

// Whether a stored entry is the variant this request is asking for.
//
// An entry stored with no selector is the ordinary case -- the response named
// no `Vary` -- and matches anything. An entry that named one matches only a
// request whose values for those headers are the ones it was stored for; every
// other request is a miss, and a miss is a refetch, which is the whole point.
// Without this, an application following the pattern the Cache API is written
// for -- check the cache, fall through to a handler that answers differently
// for different users, cache the answer -- served the first user's page to
// everybody who came after.
function selectedBy(stored, request) {
  if (!Array.isArray(stored) || stored.length === 0) return true;
  const names = [];
  for (const entry of stored) {
    if (!Array.isArray(entry) || entry.length !== 2) return false;
    names.push(entry[0]);
  }
  const asked = varySelector(names, request);
  for (let i = 0; i < stored.length; i++) {
    if (stored[i][1] !== asked[i][1]) return false;
  }
  return true;
}

// The options one of these calls understands, per verb.
//
// `put` takes the two statements an application makes about who an answer is
// for; `match` and `delete` take the one that says which caller's entry to
// look for, plus the one option of the specification's bag this store can
// honour.
const PUT_OPTIONS = ["forCaller", "forAnyCaller"];
const READ_OPTIONS = ["forCaller", "ignoreMethod"];

// The options the specification defines that this cache will not honour, and
// why each one.
//
// They are refused by name rather than ignored. `match(request, { ignoreVary:
// true })` asks in so many words for the entry stored for a different set of
// request headers than this request carries -- which is the answer belonging
// to somebody else -- and the bag used to be taken and dropped on the floor,
// so the call read as an ordinary match and was answered as though nothing had
// been asked. An option that is silently ignored is worse than one that does
// not exist, because the application is written believing it took effect.
const OPTIONS_THIS_CACHE_REFUSES = new NativeMap([
  ["ignoreVary",
    "it asks for an entry stored for a different set of request headers than " +
    "this request carries, which is one caller's answer handed to another, and " +
    "refusing that is what this cache is for"],
  ["ignoreSearch",
    "this store is keyed by the exact URL and has no way to find the entries a " +
    "key without its query string would stand for; build the key you mean to " +
    "read"],
  ["cacheName",
    "each cache here is reached through the object caches.open answered, so " +
    "there is no second name for this call to consult"]
]);

// What one call said about who its entry is for.
//
// `forCaller` is the string the entry is filed under, empty for the ordinary
// entry that any caller may be handed. `forAnyCaller` is the statement that
// this answer does not depend on the caller at all.
//
// Both are arguments rather than headers, and that is the point of them: an
// upstream, a framework and a copied header can all produce `Cache-Control:
// public`, which is why it no longer means anything here, and none of them can
// produce a third argument to a call in this application's own source.
function cacheCallOptions(verb, options, understood) {
  const said = { forCaller: "", forAnyCaller: false, ignoreMethod: false };
  if (options === null || options === undefined) return said;
  const kind = typeof options;
  if (kind !== "object" && kind !== "function") {
    throw new TypeError(
      "cache." + verb + ": the options have to be an object, got " + kind +
        ". Nothing was done"
    );
  }
  for (const name in options) {
    if (!hasOwn(options, name)) continue;
    if (understood.indexOf(name) !== -1) continue;
    const refused = OPTIONS_THIS_CACHE_REFUSES.get(name);
    throw new TypeError(
      "cache." + verb + ": the option " + name + " is not honoured here, and " +
        "this call did nothing rather than pretending it had been read. " +
        (refused !== undefined
          ? refused
          : "cache." + verb + " understands " + understood.join(" and ") +
            ", and nothing else")
    );
  }

  const forCaller = optionOf(options, "forCaller");
  if (forCaller !== undefined) {
    if (typeof forCaller !== "string" || forCaller === "") {
      throw new TypeError(
        "cache." + verb + ": forCaller names the caller this entry belongs to, " +
          "so it has to be a non-empty string, got " +
          (forCaller === "" ? "an empty string" : typeof forCaller) +
          ". Nothing was done"
      );
    }
    said.forCaller = forCaller;
  }

  const forAnyCaller = optionOf(options, "forAnyCaller");
  if (forAnyCaller !== undefined) {
    if (forAnyCaller !== true) {
      throw new TypeError(
        "cache." + verb + ": forAnyCaller is the statement that this answer " +
          "does not depend on who asked, so the only value it takes is true, " +
          "got " + String(forAnyCaller) + ". Nothing was done"
      );
    }
    if (said.forCaller !== "") {
      throw new TypeError(
        "cache." + verb + ": forCaller and forAnyCaller say opposite things " +
          "about the same entry -- that it is one caller's, and that it is " +
          "nobody's in particular. Pass one of them. Nothing was done"
      );
    }
    said.forAnyCaller = true;
  }

  const ignoreMethod = optionOf(options, "ignoreMethod");
  if (ignoreMethod !== undefined) {
    if (typeof ignoreMethod !== "boolean") {
      throw new TypeError(
        "cache." + verb + ": ignoreMethod is a boolean, got " +
          typeof ignoreMethod + ". Nothing was done"
      );
    }
    said.ignoreMethod = ignoreMethod;
  }
  return said;
}

// What a `put` did, and why it did not.
//
// # Why a refusal is an answer here rather than an exception
//
// A `put` is refused for two different kinds of reason, and they want two
// different kinds of report.
//
// One is that the call itself is not one this API accepts: the options bag is
// not an object, it names an option this cache will not honour, `forCaller` is
// not a non-empty string, the key is not a `Request`, a `URL` or a string, or
// the key's method is not GET. Every one of those is a mistake in the source
// that is the same on every request, and it throws -- the application has to
// change a line.
//
// The other is that *this* response may not be kept in a store every later
// caller reads: it varies on something the key cannot vouch for, it carries a
// `Set-Cookie`, it says `private`, it is one range of a body, it is larger
// than the store, or the caller being served carries a header that could name
// them and nothing files the entry under it. None of those is a mistake in the
// source. They depend on what an upstream said and on who is asking, so the
// same line of code is fine for one caller and refused for the next -- and
// they are the *ordinary* outcome: after the rule that closed the last round,
// any handler serving a browser that sends a session cookie is refused unless
// it says whose answer it is.
//
// Those threw too, and the platform's own example did not catch it, because
// the thing that exercised it sent no cookies. What a browser got was a 500:
// an uncaught rejection kills the invocation, so a `put` the handler made
// after it had already computed its answer threw that answer away. A cache
// write failing is not a reason to fail the request it was meant to speed up,
// so those resolve, and the report says what happened:
//
//   const kept = await cache.put(request, response);
//   if (!kept.stored) context.log.warn(kept.reason);
//
// Nothing is silent either way: every refusal is written to this application's
// log at warn as well, because a `put` that quietly did nothing leaves an
// application believing it has a cache.
const STORED = ObjectFreeze({ stored: true, reason: null });

function notStored(why) {
  const reason = "cache.put: " + why + ". It was not stored";
  // The application's own log, so this reaches `deis tail` and the log panel
  // beside everything else this invocation said.
  platformConsole.warn(reason);
  return ObjectFreeze({ stored: false, reason });
}

class Cache {
  // `name` is the namespace this instance reads and writes; see
  // `runtime/cache_ops.rs`. `caches.default` and `ctx.services.cache` are the
  // one named "default", so an application that writes through one and reads
  // through the other sees its own entry, and a cache opened by any other
  // name shares nothing with them.
  constructor(name) {
    this.name = name;
  }
  // What one `put` did.
  //
  // `stored: true`, or `stored: false` with the sentence saying why not. See
  // the note above `Cache.put` for why the second one is an answer rather than
  // an exception.
  async put(request, response, options) {
    const said = cacheCallOptions("put", options, PUT_OPTIONS);
    const key = cacheKeyOf(request);

    // Question one: what is this an answer *to*. A cache entry is the answer
    // to the request its key names, and a `Request` names a method. The answer
    // to a POST is the result of an action rather than a representation of
    // anything, and it was stored under the URL alone and then handed to every
    // later GET of it.
    //
    // This one throws, because the key is an argument of the call and its
    // method is part of what it *is*: a `put` keyed by a POST is a call this
    // API does not accept, whatever the response turns out to be.
    const method = cacheKeyMethod(request);
    if (method !== "GET") {
      throw new TypeError(
        "cache.put: the key given for " + key + " is a request whose method " +
          "is " + method + ", and this cache holds answers to GET requests " +
          "only -- a later GET of the same URL would otherwise be given this " +
          "response. Key the entry with the GET it answers, or do not cache " +
          "it. It was not stored"
      );
    }

    // What this call is answering, read once and passed to every rule below
    // that needs it. See "What the code running right now is answering": this
    // is `NO_REQUEST` for a queue batch, a cron tick or the module's own top
    // level, a record holding the `Request` for a handler and for the code
    // that continues one, and `UNATTRIBUTED` when nothing has said -- which is
    // its own refusal further down.
    const producing = streamBeingProduced();
    const answering =
      producing === undefined ? whatIsAnswered() : producing.answering;
    const served = answering === UNATTRIBUTED ? null : answering.request;

    // Question two, first half: is this a whole answer that a GET could be
    // given. A range, a "you already have this", or a status no response may
    // carry is not one; see `notACompleteAnswer`.
    const incomplete = notACompleteAnswer(response);
    if (incomplete) {
      return notStored("this response carries " + incomplete);
    }

    // A response that declares itself unusable by a cache like this one is not
    // stored, and the caller is told rather than left to believe it was.
    // Silently keeping a `private` or `no-cache` response in a store every
    // later request reads is how one user's data reaches another. All three
    // freshness fields are read here, not just `Cache-Control`; see
    // `lifetimeOf`.
    const directives = cacheControlOf(response);
    const lifetime = lifetimeOf(response, directives);
    if (lifetime.refusal) {
      return notStored("this response carries " + lifetime.refusal);
    }

    // What the response says its body depends on. `Vary: *` is the response
    // saying that nothing may reuse it -- the specification's own words are
    // that such an entry always fails to match -- so it is refused here rather
    // than stored as something no `match` could ever hand back. It is refused
    // even for an entry filed under one named caller, because "no later
    // request may reuse this" includes that caller's later requests.
    const vary = varyNames(response);
    if (vary.indexOf("*") !== -1) {
      return notStored(
        "this response carries Vary: *, which means no later request may " +
          "reuse it"
      );
    }
    // What variant this entry would be filed under, which is the question the
    // refusals below are about: not what the key is, but what it is able to
    // say about who this response is for.
    const selector = vary.length > 0 ? varySelector(vary, request) : [];

    // Everything from here to the write is one question -- could this entry be
    // handed to a caller nobody can identify -- and an application that has
    // said whose answer it is has already answered it. An entry stored with
    // `forCaller` lives under that string and is never handed to a `match`
    // that names another, so the ways one caller's answer reaches a different
    // one do not arise; the statement is on the call, which no header an
    // upstream sets can produce.
    if (said.forCaller === "") {
      if (!said.forAnyCaller) {
        // Nothing in this stretch of code said what it is answering, so this
        // platform does not know what the response could have been *about*.
        // That is not the same as knowing it answers nothing: a WebSocket
        // message handler, a response-stream producer and a timer callback all
        // run outside any invocation of their own, and the variable this
        // replaces handed each of them whichever request this isolate served
        // last. Refused rather than read, because the reading was wrong in
        // both directions -- too strict when the last request carried a
        // credential, and a disclosure when it did not.
        if (answering === UNATTRIBUTED) {
          return notStored(
            "nothing here says what request this code is answering. A cache " +
              "entry is served whole to a later caller, and what decides " +
              "whether that is safe is what the handler making it was " +
              "answering -- so a call from a timer, from the producer of a " +
              "streaming body whose request has finished, or from anywhere " +
              "else that runs between invocations cannot be judged at all. " +
              "Say whose answer it is on the call: { forCaller: <the caller " +
              "this answer is for> } stores it for that caller alone, and " +
              "{ forAnyCaller: true } states that the answer does not depend " +
              "on the caller at all"
          );
        }

        // And the same question about the *other* request, which is the one
        // this platform reads and the key does not name: the entry would be
        // filed under a GET of this URL, so if the handler is answering
        // something else, a later GET is handed the result of an action it
        // never asked for. `cache_ops.rs` states this rule for the key; this
        // is the half of it that reads the request being answered, and without
        // it a handler answering `POST /answer` from its body and caching
        // under the URL collapsed two callers' answers into one entry.
        const servedMethod = served === null ? null : cacheKeyMethod(served);
        if (servedMethod !== null && servedMethod !== "GET") {
          return notStored(
            "the request this handler is answering is a " + servedMethod +
              ", and the entry would be filed under a GET of " + key + ". The " +
              "answer to a " + servedMethod + " is the result of an action " +
              "rather than a representation of anything, so a later GET of " +
              "that URL would be handed one caller's result. Say whose answer " +
              "it is with { forCaller: <the caller this answer is for> }, or " +
              "state that it does not depend on the caller at all with " +
              "{ forAnyCaller: true }"
          );
        }
      }

      if (vary.length > 0 && !readableHeaders(request)) {
        // The key was a URL or a string, so there is nothing to read the
        // varying headers off, and an entry whose selector is unknown is
        // exactly the one that must not be stored: a later `match` under that
        // same string would be answered with it whoever asked. Refused rather
        // than stored, with the fix named in the message, because this is the
        // shape that serves one user's page to another.
        return notStored(
          "this response varies on " + vary.join(", ") +
            ", and the key given for " + key + " was not a Request, so there is " +
            "nothing to read those headers from and no way to tell one caller's " +
            "copy from another's. Pass the Request itself as the key"
        );
      }
      const unvouched =
        vary.length > 0 && request !== served
          ? varyNamesTheKeyDoesNotCarry(selector)
          : [];
      if (unvouched.length > 0) {
        // The variant this entry would be filed under has to be one somebody
        // actually stated. A value the key carries is at least the caller's own
        // statement about what this response is for; a value it does not carry
        // is this platform inventing "the caller sent none of that header",
        // which is the variant every anonymous caller matches. `new
        // Request(url)` -- built from the request being served, to normalise a
        // key, to drop a query string -- is a `Request` and carries no headers
        // at all, so a page fetched with somebody's `Authorization` was filed as
        // the "carried no Authorization" variant and handed to every anonymous
        // caller after.
        //
        // The rule this replaces asked whether the key carried *any* of them,
        // which one extra name in `Vary` walked straight past: a key carrying
        // `Accept-Language` and nothing else counted as carrying some, and the
        // authenticated page went out to the next anonymous caller sending that
        // language. The property is per name, not per key.
        //
        // The exception is why this is not one rule with the refusal above: when
        // the key is the very request this invocation is answering, "carries
        // none of them" is not a claim, it is the truth. An anonymous caller
        // storing an anonymous page under `context.request` is the ordinary case
        // and stays ordinary. Any other key has to bring the values with it --
        // which `cache.put(context.request, ...)` does for free, and which
        // caching an upstream answer under the request that fetched it does too.
        return notStored(
          "this response varies on " + vary.join(", ") +
            ", and the key given for " + key + " carries no value for " +
            unvouched.join(", ") + " while not being the request this handler " +
            "is answering. Stored, it would be filed as the variant for a " +
            "caller who sends none of those, and handed to every such caller " +
            "afterwards. Pass the Request this handler was given, or a key that " +
            "carries the same values for those headers"
        );
      }

      // Question two, last half, and the one the previous four rounds kept
      // finding a way round: is this answer one caller's. Not "does the key
      // carry Authorization" -- the set of headers that carry an identity has
      // no end -- but "is every header on either request one this platform
      // knows cannot name a caller, or one the entry is filed under". See
      // `notSafeForAStranger`.
      if (!said.forAnyCaller) {
        const unsafe = notSafeForAStranger(request, vary, served);
        if (unsafe) {
          return notStored(unsafe);
        }
      }

      // And what the response would *give* whoever receives it, which no
      // `Vary` and no statement about the request can make safe: see
      // `headersAStrangerMayNotReceive`. `forAnyCaller` does not waive this
      // one, because a response that hands the recipient a session and an
      // application saying the answer suits every caller contradict each
      // other, and the safe reading of a contradiction is to refuse it.
      const replayed = headersAStrangerMayNotReceive(response);
      if (replayed.length > 0) {
        return notStored(
          "this response carries " + replayed.join(", ") + ", which " +
            "this platform will not replay to a later caller. A stored response " +
            "may carry only what describes the representation, how long it is " +
            "good for, or a policy that is the same for every caller of the " +
            "site; anything else -- a Set-Cookie, a token, a header this " +
            "application invented -- is given to whoever is handed the entry " +
            "next, which for a Set-Cookie means every later caller shares one " +
            "caller's session. Store a copy carrying the headers you mean to " +
            "serve, or say whose answer it is with { forCaller: <the caller " +
            "this answer is for> }"
        );
      }
    }

    // Clone first: reading the body of the response the handler is about to
    // return would leave the caller with an empty one.
    //
    // Bytes, not text. `caches.default` exists in every isolate whether the
    // application asked for it or not, so this is the path an image, a
    // protobuf or a gzip frame takes; decoding it as UTF-8 on the way in
    // turned every byte in 0x80..0xff into U+FFFD and the application got
    // mojibake back with nothing reporting an error. The status and the
    // headers are text and still travel as JSON beside the body.
    const body = new Uint8Array(await response.clone().arrayBuffer());
    // Pairs, not an object, for the reason `headerPairs` below is: an object
    // keeps one value per name, and `Set-Cookie` is the one name that legally
    // appears more than once. `Object.fromEntries` here stored the last cookie
    // only, so a login that also issued a CSRF token was answered correctly on
    // the miss and with one cookie on the hit -- the same defect
    // `runtime/header_list.rs` exists to have caught once, one layer in.
    //
    // Nothing is folded here, and that is deliberate. Which of the two answers
    // an application gets is decided once, on the way out of this isolate, from
    // its compatibility date; a cached entry is not on its way anywhere. A
    // response rebuilt by `match` below is returned by the handler and leaves
    // through that same gate, so folding at this point would be a second copy
    // of the rule -- one that agrees with the first only while the flag is off,
    // and disagrees with it in exactly the case the flag exists for. Storing
    // the whole list keeps the choice where it is made: a list can always be
    // folded later, and a folded list can never be unfolded.
    const meta = {
      status: response.status,
      headers: headerPairs(response.headers)
    };
    // What this entry is the variant *for*, kept beside it because that is the
    // only place `match` can read it: the store is keyed by URL, and nothing
    // knows a response varied until the entry it varied on has been read. One
    // variant per key, therefore -- a second `put` for the same URL replaces
    // it -- which costs a hit and never costs a wrong answer.
    if (vary.length > 0) meta.vary = selector;
    try {
      await DenoCore.ops.op_cache_set(
        this.name,
        said.forCaller,
        key,
        JSON.stringify(meta),
        body,
        lifetime.seconds
      );
    } catch (e) {
      // The store's own refusal -- today, an entry so large that admitting it
      // would empty the cache; see `runtime/cache_ops.rs`. It is a statement
      // about this response rather than about the call, so it is reported the
      // way every other one is instead of arriving as an exception from one
      // rule out of a dozen.
      return notStored(e && e.message ? String(e.message) : String(e));
    }
    return STORED;
  }
  async match(request, options) {
    const said = cacheCallOptions("match", options, READ_OPTIONS);
    const key = cacheKeyOf(request);
    // Nothing but the answer to a GET is ever stored, so a key naming another
    // method has no entry here and a miss is the honest answer: the handler
    // falls through and answers the request properly, which is what a miss is
    // for. Without this the store was consulted by URL alone, so a POST was
    // answered with whatever a GET of that URL had left behind. `ignoreMethod`
    // is the specification's way for an application to ask for the GET entry
    // from inside a handler answering something else, which is a question it
    // is entitled to ask about its own cache.
    if (!said.ignoreMethod && cacheKeyMethod(request) !== "GET") return undefined;
    const hit = await DenoCore.ops.op_cache_get(this.name, said.forCaller, key);
    if (!hit) return undefined;
    try {
      const meta = JSON.parse(hit.meta);
      // The entry is under the right URL; whether it is the right *variant* is
      // a second question, and answering it wrongly is how one user's page is
      // served to another.
      if (!selectedBy(meta.vary, request)) return undefined;
      // A 204 or a 304 may not carry a body at all, and the constructor
      // refuses an empty buffer as loudly as a full one -- so an entry with
      // nothing in it is rebuilt with a null body rather than a zero-length
      // one, which is also what a bodiless response was to begin with.
      const body = hit.body.byteLength === 0 ? null : hit.body;
      return new Response(body, {
        status: meta.status,
        // The list `put` stored, handed back as it was: the `Headers`
        // constructor takes a sequence of pairs, so the repeats it holds are
        // rebuilt as repeats rather than collapsed on the way in.
        headers: meta.headers
      });
    } catch {
      return undefined;
    }
  }
  async delete(request, options) {
    const said = cacheCallOptions("delete", options, READ_OPTIONS);
    const key = cacheKeyOf(request);
    const method = cacheKeyMethod(request);
    if (!said.ignoreMethod && method !== "GET") {
      // Nothing was ever stored under a key that is not a GET, so `false` --
      // the Cache API's word for "there was nothing to delete" -- is the
      // truth. It is said out loud because it is almost never what the caller
      // meant: invalidating a page after a write is written as
      // `cache.delete(context.request)` inside a POST handler, which deletes
      // nothing, answers as though the cache were already empty, and leaves
      // the stale page being served with nothing reporting it.
      platformConsole.warn(
        "cache.delete: the key given for " + key + " is a request whose " +
          "method is " + method + ", and this cache holds answers to GET " +
          "requests only, so there was nothing under that key to delete and " +
          "this call did nothing. To invalidate the cached GET, pass its URL " +
          "or a Request for it"
      );
      return false;
    }
    // Which variant is under that key, before anything is removed. The store
    // holds one variant per key and this used to remove it by key alone, so a
    // caller who does not have the variant -- an anonymous `delete` against an
    // entry filed under somebody's credential -- was told `true`, which says
    // "there was an entry here for you", and threw away an entry that was not
    // its own. `true` for a variant the caller cannot see is also an oracle:
    // it answers "is there a cached page for that credential" to anyone.
    //
    // Read then remove, both from this isolate's single thread. Another
    // invocation of this same application could write between the two, and
    // then this drops an entry a moment younger than the one it checked --
    // which is a miss and a refetch, never a wrong answer, and is the same
    // outcome as the two calls having arrived in the other order.
    const meta = await DenoCore.ops.op_cache_meta(this.name, said.forCaller, key);
    if (meta === null || meta === undefined) return false;
    let stored = null;
    try {
      stored = JSON.parse(meta).vary;
    } catch {
      // An entry this side cannot read is one no `match` can hand back
      // either, so removing it is the useful answer.
      stored = null;
    }
    if (!selectedBy(stored, request)) return false;
    return await DenoCore.ops.op_cache_delete(this.name, said.forCaller, key);
  }
}

// The namespace `caches.default` and `context.services.cache` share.
//
// Named once here rather than on both sides of the boundary: the ops take
// whatever name they are given and know nothing about which one is the
// default, so this is the only place the two spellings could ever come apart
// and there is only one of it.
const DEFAULT_CACHE_NAME = "default";

// One instance, reached under both names, so an application that writes
// through `caches.default` and reads through `context.services.cache` sees
// its own entry.
const defaultCache = new Cache(DEFAULT_CACHE_NAME);

// The one name `caches.open` and `caches.delete` will not take.
//
// The default cache is an ordinary namespace on the Rust side and its name is
// an ordinary string, so an application opening a cache under a name it took
// from a request -- a tenant, a locale, a path segment, anything a caller can
// choose -- handed a caller who sent "default" the namespace every other part
// of that application reads and writes through `context.services.cache`. It
// could read what was cached there and, through `caches.delete`, empty it.
//
// Refused rather than renamed to something unguessable, because an application
// that wrote the name meaning the default cache should be told where the
// default cache is instead of quietly being given a second one that shares
// nothing with it. The cost is that the default namespace has no bulk empty:
// its entries go one key at a time through `cache.delete(key)`, which is the
// only way `caches.default` could ever be emptied anyway on a platform where
// the name is not addressable.
function reserveTheDefaultName(verb, name) {
  if (name !== DEFAULT_CACHE_NAME) return;
  throw new TypeError(
    "caches." + verb + ": \"" + DEFAULT_CACHE_NAME + "\" is the name of the " +
      "cache reached as caches.default and context.services.cache, so it is " +
      "not a name an application may " + verb + " -- a name taken from a " +
      "request would otherwise reach the namespace the rest of this " +
      "application caches into. Use caches.default, or choose another name"
  );
}

// What this cache is and how long it lasts, read once per isolate.
//
// The store lives in this isolate's memory: it dies when a new version is
// deployed, when the node restarts, and it is not shared with the other nodes
// of the installation. That is the platform's decision rather than a gap, and
// this object is what makes it something an application can *check* instead of
// something it has to have read. A miss has two very different causes -- the
// entry was evicted or went stale in a store that is still the one that was
// written to, or the store itself is new and everything went at once -- and
// only the second one invalidates an application's reasoning about what it has
// already done. `scope.id` tells them apart: record it beside anything cached
// that a decision depends on, and a value that no longer matches means the
// cache went with the old isolate.
//
// Read here, at isolate build, rather than on every access: it cannot change
// while this isolate lives, and a property that dispatched an op per read
// would spend an application's op-rate budget on a constant.
const cacheScope = Object.freeze(DenoCore.ops.op_cache_scope());

globalThis.caches = {
  default: defaultCache,
  scope: cacheScope,
  // A cache of this application's own, under a name it chooses. Namespaced
  // inside the one isolate store, so opening a name costs nothing and two
  // names never see each other's entries.
  //
  // Async because `CacheStorage.open` is, and because code ported from
  // Cloudflare awaits it. Nothing here needs to wait for anything.
  async open(name) {
    if (typeof name !== "string" || name === "") {
      throw new TypeError(
        "caches.open: a non-empty cache name is required, got " +
          (name === "" ? "an empty string" : typeof name)
      );
    }
    reserveTheDefaultName("open", name);
    return new Cache(name);
  },
  // Drop everything in one named cache, and say whether it held anything.
  //
  // A named cache here is a namespace rather than an object that was created,
  // so there is no such thing as a cache that exists and is empty: `false`
  // means nothing was stored under that name, which is the same answer an
  // application gets for a name it never used. That is the honest reading of
  // `CacheStorage.delete` on a store built this way, and it is written down
  // here because the specification's answer means something slightly
  // different.
  async delete(name) {
    if (typeof name !== "string" || name === "") {
      throw new TypeError(
        "caches.delete: a non-empty cache name is required, got " +
          (name === "" ? "an empty string" : typeof name)
      );
    }
    reserveTheDefaultName("delete", name);
    return (await DenoCore.ops.op_cache_clear(name)) > 0;
  }
};

// What a KV value is stored as, and what a read is asked to hand back.
//
// deis-store holds a value as bytes, because that is what an application
// writes: a string, a rendered image, a compressed frame. The `type` option
// says how to read those bytes back, and it is the same set of names
// applications are already written against elsewhere: "text" (the default),
// "json" and "arrayBuffer".
//
// "stream" is deliberately absent. It is a name in that same set, and this
// platform has no streaming read behind it -- op_kv_get answers with the
// whole value. Accepting it and quietly handing back text would give an
// application something that is not a ReadableStream and fail on the first
// method it called; refusing it says so on the spot.
const KV_VALUE_TYPES = new Set(["text", "json", "arrayBuffer"]);

function kvValueType(options) {
  const requested =
    typeof options === "string" ? options : optionOf(options, "type");
  if (requested === undefined || requested === null) return "text";
  if (KV_VALUE_TYPES.has(requested)) return requested;
  if (requested === "stream") {
    throw new TypeError(
      'kv: type "stream" is not available on this platform: a KV read returns ' +
        "the whole value, there is no streaming read behind it. Use " +
        '"arrayBuffer" and wrap the bytes yourself if you need a stream.'
    );
  }
  throw new TypeError(
    "kv: unknown type " +
      JSON.stringify(requested) +
      '; expected "text", "json" or "arrayBuffer"'
  );
}

// Turn the bytes op_kv_get answered with into what the caller asked for.
function kvDecode(bytes, type) {
  if (type === "arrayBuffer") {
    // A fresh copy, so a caller cannot reach past the value into whatever
    // else shares the buffer V8 handed over.
    return bytes.slice().buffer;
  }
  const text = new TextDecoder().decode(bytes);
  return type === "json" ? JSON.parse(text) : text;
}

// Encode what put() was handed into the bytes deis-store stores.
function kvEncode(value) {
  if (typeof value === "string") return new TextEncoder().encode(value);
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  if (ArrayBuffer.isView(value)) {
    return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  }
  throw new TypeError(
    "kv.put: value must be a string, an ArrayBuffer or an ArrayBufferView, got " +
      (value === null ? "null" : typeof value)
  );
}

// ---------------------------------------------------------
// Named bindings
// ---------------------------------------------------------
//
// Every storage object below carries the name of the namespace it speaks for,
// and hands that name to the op as its first argument. The empty string is the
// tenant's *unnamed* namespace: the one `context.services.kv` reaches, the one
// deis-store keys by the tenant alone, and the one every application deployed
// before named bindings existed has been reading and writing all along. A
// named one is a second namespace beside it, inside the same tenant, and it is
// empty on the day it is declared.
//
// The name lives on the object rather than being passed at each call site so
// that a handle taken out of one - a prepared statement, an open transaction -
// cannot be used against another by accident: the handle's own object carries
// the binding, so the pair travels together. deis-store refuses the mismatch
// too, which is the backstop; this is what keeps an ordinary program from
// reaching it.
//
// `bindingName` is validated where a binding is declared, not here: the
// objects below are built by `createServiceContext` out of the manifest's own
// table, so a name reaching this file has already been through
// `deis_bundle::DeclaredBindings::validate` at build, at upload and at load.
const UNNAMED_NAMESPACE = "";

class NativeKV {
  // Which namespace this object speaks for. See UNNAMED_NAMESPACE.
  #binding;

  constructor(binding) {
    this.#binding = binding || UNNAMED_NAMESPACE;
  }

  // The binding name, or "" for the tenant's unnamed namespace.
  // Readable so a handler can log which namespace it is writing to.
  get binding() {
    return this.#binding;
  }

  // get(key) -> the value as text, or null when the key is not there.
  // get(key, { type }) -> the value as "text", "json" or "arrayBuffer".
  //
  // The type is validated before the read, so an unsupported one is refused
  // whether or not the key exists: a call that is wrong is wrong the same way
  // every time.
  async get(key, options) {
    const type = kvValueType(options);
    const bytes = await DenoCore.ops.op_kv_get(this.#binding, key);
    if (bytes === null || bytes === undefined) return null;
    return kvDecode(bytes, type);
  }
  // getWithMetadata(key, options) -> { value, metadata, expiration } | null
  //
  // null means the key does not exist or has already lapsed. A key that was
  // written without metadata comes back as a record whose metadata is null:
  // "no such key" and "no metadata on this key" are different answers.
  //
  // `value` is read the same way get() reads it, and takes the same `type`
  // option, so the two never disagree about what a stored value is.
  //
  // metadata is JSON-decoded, which is the exact inverse of what put() stores,
  // so an object goes in and the same object comes out. A value that does not
  // parse is handed back as the raw string rather than throwing: metadata
  // written before put() encoded strings as JSON is stored unquoted, and
  // losing an app's own data to a parse error would be worse than handing it
  // back as it was found.
  //
  // expiration is the absolute unix second the entry lapses at, or null when
  // it never does.
  async getWithMetadata(key, options) {
    const type = kvValueType(options);
    const read = await DenoCore.ops.op_kv_get_with_metadata(this.#binding, key);
    if (!read) return null;
    return {
      value: kvDecode(read.value, type),
      metadata: this.#metadata(read.metadata),
      expiration: read.expiration === undefined ? null : read.expiration
    };
  }
  // What put() stored, read back as what the application put in.
  //
  // Shared with list(), so a key's metadata means the same thing whichever
  // call reported it. null both when nothing was stored and when the store
  // sent nothing. A string that does not parse is handed back as it was
  // found rather than thrown over: metadata written before put() encoded
  // strings as JSON is stored unquoted, and losing an app's own data to a
  // parse error would be worse than handing it back raw.
  #metadata(raw) {
    if (raw === undefined || raw === null) return null;
    try {
      return JSON.parse(raw);
    } catch {
      return raw;
    }
  }
  // put(key, value, { expirationTtl, metadata })
  //
  // value is a string, an ArrayBuffer or an ArrayBufferView. A string is
  // encoded as UTF-8; bytes are stored exactly as given, which is what makes
  // this usable for anything that is not text.
  //
  // expirationTtl is a lifetime in whole seconds, counted from the moment
  // deis-store accepts the write; leave it out and the entry never lapses.
  //
  // metadata is stored alongside the value and returned untouched by
  // deis-store; getWithMetadata() reads it back. It is JSON-encoded here,
  // strings included, so that decoding it there is a plain JSON.parse and an
  // object, a number and a string all round-trip as themselves.
  async put(key, value, options) {
    // Own members only: read the ordinary way, an `Object.prototype`
    // an application wrote to would have given a lifetime to every later
    // write on this isolate that asked for none.
    const askedTtl = optionOf(options, "expirationTtl");
    let ttl = null;
    if (askedTtl !== undefined && askedTtl !== null) {
      ttl = Number(askedTtl);
      if (!Number.isFinite(ttl) || Math.floor(ttl) !== ttl || ttl <= 0) {
        throw new TypeError(
          "kv.put: expirationTtl must be a whole number of seconds greater than 0, got " +
            String(askedTtl)
        );
      }
    }

    const askedMetadata = optionOf(options, "metadata");
    let metadata = null;
    if (askedMetadata !== undefined && askedMetadata !== null) {
      metadata = JSON.stringify(askedMetadata);
    }

    await DenoCore.ops.op_kv_set(this.#binding, key, kvEncode(value), {
      ttlSeconds: ttl,
      metadata: metadata
    });
  }
  async delete(key) {
    await DenoCore.ops.op_kv_delete(this.#binding, key);
  }
  // list() -> every key this application has
  // list("session:") -> the keys under a prefix
  // list({ prefix, limit, cursor }) -> one page of them
  //
  // Resolves to { keys, list_complete, cursor }:
  //
  //   keys           an array of { name, expiration, metadata }, in key
  //                  order. The stored value is not in a listing -- a page
  //                  may hold thousands of keys -- so a value is fetched by
  //                  name afterwards. expiration is the absolute unix second
  //                  the key lapses at, null when it never does; metadata is
  //                  what put() attached, decoded, null when it attached
  //                  nothing.
  //   list_complete  true when this page was the end of the scan.
  //   cursor         what to pass back as `cursor` to get the next page, and
  //                  undefined once list_complete is true.
  //
  // Page until list_complete, not until a page comes back short:
  //
  //   const all = [];
  //   let page = { list_complete: false, cursor: undefined };
  //   while (!page.list_complete) {
  //     page = await kv.list({ prefix: "session:", limit: 100, cursor: page.cursor });
  //     all.push(...page.keys);
  //   }
  //
  // deis-store answers with a cursor whenever it filled the page to the
  // limit, which is not the same as knowing there is more: four keys listed
  // two at a time give two full pages and then an empty third one. A loop
  // that stopped at the first short page would be right by luck; a loop that
  // stops when the cursor is gone is right always.
  //
  // limit is a request, not a guarantee. deis-store substitutes its own page
  // size (1000) when none is given and caps anything larger than its ceiling
  // (10000), so asking for a million yields ten thousand keys and a cursor
  // rather than an error.
  async list(options) {
    const bag = typeof options === "string" ? { prefix: options } : options;

    const askedPrefix = optionOf(bag, "prefix");
    let prefix = null;
    if (askedPrefix !== undefined && askedPrefix !== null) {
      if (typeof askedPrefix !== "string") {
        throw new TypeError(
          "kv.list: prefix must be a string, got " + typeof askedPrefix
        );
      }
      prefix = askedPrefix;
    }

    const askedLimit = optionOf(bag, "limit");
    let limit = null;
    if (askedLimit !== undefined && askedLimit !== null) {
      limit = Number(askedLimit);
      if (!Number.isFinite(limit) || Math.floor(limit) !== limit || limit <= 0) {
        throw new TypeError(
          "kv.list: limit must be a whole number of keys greater than 0, got " +
            String(askedLimit)
        );
      }
      // The op takes a 32-bit count, and a number past that would be refused
      // by the argument decoder with a message about integers rather than
      // about keys. Hand over the largest one it can hold instead: deis-store
      // caps every limit at its own ceiling anyway, so an absurd request has
      // always answered with the ceiling and a cursor, and that stays true.
      if (limit > 2147483647) limit = 2147483647;
    }

    const askedCursor = optionOf(bag, "cursor");
    let cursor = null;
    if (askedCursor !== undefined && askedCursor !== null) {
      if (typeof askedCursor !== "string") {
        throw new TypeError(
          "kv.list: cursor must be the string a previous page answered with, got " +
            typeof askedCursor
        );
      }
      cursor = askedCursor;
    }

    const page = await DenoCore.ops.op_kv_list(this.#binding, {
      prefix: prefix,
      limit: limit,
      cursor: cursor
    });

    const next =
      page.cursor === undefined || page.cursor === null ? undefined : page.cursor;
    return {
      keys: page.keys.map((key) => ({
        name: key.name,
        expiration: key.expiration === undefined ? null : key.expiration,
        metadata: this.#metadata(key.metadata)
      })),
      list_complete: next === undefined,
      cursor: next
    };
  }
}

// The database ops report failure as an `error` field rather than by
// throwing, so a caller that did not look for it would see `undefined`
// rows and no reason why. Turn it into an exception, which is what the
// other services do and what a handler's try/catch expects.
//
// Whatever deis-store said is kept whole behind the call's name. A
// transaction that died on its deadline explains itself in a full sentence,
// including what became of the writes, and rewording that here would cost the
// handler the only account it gets of what happened.
function checkDbResult(result, call) {
  if (result && result.error) {
    throw new Error("database." + call + " failed: " + result.error);
  }
  return result;
}

// The deadline a caller asked its transaction to have, in milliseconds.
// Validated here so a nonsense value is refused before deis-store pins a
// connection for it.
function transactionTimeout(options) {
  const asked = optionOf(options, "timeoutMs");
  if (asked === undefined || asked === null) return null;
  const ms = Number(asked);
  if (!Number.isFinite(ms) || Math.floor(ms) !== ms || ms <= 0) {
    throw new TypeError(
      "database.transaction: timeoutMs must be a whole number of milliseconds greater " +
        "than 0, got " + String(asked)
    );
  }
  return ms;
}

// The handle a transaction callback is given. `execute` and `query` are the
// same two calls the database itself has, and take and return the same
// things, so moving a statement into a transaction is a matter of writing
// `tx.` in front of it.
//
// The difference is where they run. Every statement here goes to the one
// connection deis-store pinned to this transaction, so a `query` sees the
// transaction's own uncommitted writes -- the read-then-decide-then-write
// that is the whole reason to open one.
//
// It stops working the moment the callback settles. That is deliberate: a
// statement run on a spent handle could only be sent outside any transaction,
// where it would commit on its own and look like it had succeeded.
class NativeTransaction {
  // Shared with the `transaction()` call that opened it, which is what marks
  // it ended. Deliberately not a method on this object: `tx` is the
  // application's handle, not the switch that closes it.
  #state;

  // The database this transaction belongs to. A binding is a separate file in
  // deis-store, so a handle opened on one is meaningless against another; it
  // travels with the handle rather than being read from whatever `database`
  // the call site happens to name.
  #binding;

  constructor(state, expiresInMs, binding) {
    this.#state = state;
    this.#binding = binding || UNNAMED_NAMESPACE;
    // How long deis-store granted this transaction, counted from when it
    // answered. A handler with slow work to do can look before it starts.
    this.expiresInMs = expiresInMs;
  }

  // The handle, or an account of why there no longer is one.
  #handle() {
    if (this.#state.ended !== null) {
      throw new Error(
        "database.transaction: this transaction was already " + this.#state.ended +
          ", so this statement cannot be part of it. Every statement has to run inside " +
          "the callback, before it returns; a tx kept past that point is spent."
      );
    }
    return this.#state.id;
  }

  async execute(sql, params = []) {
    return checkDbResult(
      await DenoCore.ops.op_db_execute_in_transaction(
        this.#binding,
        this.#handle(),
        sql,
        JSON.stringify(params)
      ),
      "transaction.execute"
    );
  }

  async query(sql, params = []) {
    const res = checkDbResult(
      await DenoCore.ops.op_db_query_in_transaction(
        this.#binding,
        this.#handle(),
        sql,
        JSON.stringify(params)
      ),
      "transaction.query"
    );
    return res.rows;
  }
}

class NativeDB {
  // Which namespace this object speaks for. A binding is a database file of its own in deis-store, not a schema inside one.
  // See UNNAMED_NAMESPACE.
  #binding;

  constructor(binding) {
    this.#binding = binding || UNNAMED_NAMESPACE;
  }

  // The binding name, or "" for the tenant's unnamed namespace.
  get binding() {
    return this.#binding;
  }

  async execute(sql, params = []) {
    return checkDbResult(
      await DenoCore.ops.op_db_execute(this.#binding, sql, JSON.stringify(params)),
      "execute"
    );
  }
  async query(sql, params = []) {
    const res = checkDbResult(
      await DenoCore.ops.op_db_query(this.#binding, sql, JSON.stringify(params)),
      "query"
    );
    return res.rows;
  }
  // Runs several statements as one transaction, in one round trip.
  //
  //   const [stock, order] = await ctx.services.database.batch([
  //     { sql: "UPDATE items SET stock = stock - 1 WHERE id = ? AND stock > 0", params: [id] },
  //     { sql: "INSERT INTO orders (item, buyer) VALUES (?, ?)", params: [id, buyer] }
  //   ]);
  //
  // Each element is the same statement `execute` takes, written as an object:
  // `sql`, and `params` as the array of values its `?` placeholders bind to.
  // Resolves to one result per statement, in order, each the
  // `{ rows_affected, last_insert_id }` `execute` resolves to.
  //
  // What it guarantees. deis-store opens a transaction, runs the statements
  // in order, and commits. If any one of them fails the transaction is rolled
  // back and NOTHING is applied -- not even the statements that had already
  // succeeded -- and this call throws, naming the statement by its index in
  // the array. So the two statements above cannot leave an order for stock
  // that was never taken.
  //
  // How it differs from `transaction`. Both are all-or-nothing; the
  // difference is who decides what to run.
  //
  //   batch        every statement is decided before anything is sent. One
  //                request, one answer, no connection held between them, and
  //                it cannot branch: there is no point at which the
  //                application sees a result and chooses the next statement.
  //                Row-returning statements are refused -- a SELECT belongs
  //                in `query`, or in a transaction -- so a batch is writes.
  //   transaction  the callback reads, decides, and writes. It can branch on
  //                what it read, because deis-store keeps one of the tenant's
  //                connections pinned to the transaction for as long as the
  //                callback runs. That connection is the cost, which is why
  //                the store puts a deadline on it and bounds how many may be
  //                open at once.
  //
  // Reach for `batch` first. Use `transaction` when, and only when, a later
  // statement depends on what an earlier one returned.
  async batch(statements) {
    if (!Array.isArray(statements)) {
      throw new TypeError(
        "database.batch: expects an array of { sql, params } statements, got " +
          (statements === null ? "null" : typeof statements)
      );
    }

    const encoded = statements.map((statement, index) => {
      const sql = optionOf(statement, "sql");
      if (typeof sql !== "string") {
        throw new TypeError(
          "database.batch: statement " + index +
            " has no sql string; each statement is { sql, params }"
        );
      }
      const params = optionOf(statement, "params");
      if (params !== undefined && params !== null && !Array.isArray(params)) {
        throw new TypeError(
          "database.batch: statement " + index +
            " has params that are not an array; they bind to the statement's ? " +
            "placeholders in order"
        );
      }
      // The array itself, not the JSON string execute() sends. There is one
      // JSON encoding around the whole batch already, and nesting a second
      // one inside it buys nothing; op_db_batch encodes each array for the
      // wire, where a parameter list travels as a string.
      return { sql: sql, params: params ?? [] };
    });

    return checkDbResult(
      await DenoCore.ops.op_db_batch(this.#binding, JSON.stringify(encoded)),
      "batch"
    );
  }
  // Runs `callback` inside one interactive transaction and resolves to
  // whatever the callback returned.
  //
  //   const total = await ctx.services.database.transaction(async (tx) => {
  //     const [row] = await tx.query("SELECT balance FROM accounts WHERE id = ?", [id]);
  //     await tx.execute("UPDATE accounts SET balance = ? WHERE id = ?", [row.balance - 10, id]);
  //     return row.balance - 10;
  //   });
  //
  // What it guarantees. deis-store opens the transaction IMMEDIATE on a
  // connection pinned to it, so for as long as the callback runs this
  // transaction holds the tenant's write lock and every statement made
  // through `tx` lands on that one connection. Reads through `tx` see this
  // transaction's own uncommitted writes and nobody else's half-finished
  // ones. The callback returning commits all of it; the callback throwing
  // rolls all of it back and rethrows the error the callback raised, never
  // one about the transaction, so a handler always sees the failure its own
  // code produced. Either way `tx` is spent when the callback settles and
  // using it after that throws rather than writing outside the transaction.
  //
  // What it does not do. `ctx.services.database` is not redirected while the
  // callback runs. A statement run on the outer handle inside the callback
  // goes to a different connection, is not part of this transaction, and
  // commits on its own -- a rollback will not take it back. That is the
  // mistake to look for first when a transaction that failed still left rows
  // behind. Reads on the outer handle answer with the database as it was
  // before this transaction opened; writes on it will sit and wait for the
  // write lock this transaction is holding, and fail. Use `tx` for
  // everything that belongs to the transaction.
  //
  // When not to use it. A set of statements that is decided up front is a
  // `batch`: same all-or-nothing guarantee, one round trip, and no connection
  // held while the application thinks.
  //
  // Deadline. An open transaction is holding one of the tenant's few
  // connections, so deis-store gives it a hard deadline -- five seconds
  // unless `options.timeoutMs` asks for less -- and rolls it back itself when
  // that passes. The next call on `tx`, or the commit, then throws saying the
  // transaction was rolled back for exceeding its deadline and that nothing
  // it wrote was applied. Do not await anything slow between statements.
  async transaction(callback, options) {
    if (typeof callback !== "function") {
      throw new TypeError(
        "database.transaction: the statements go in a callback, as " +
          "database.transaction(async (tx) => { ... })"
      );
    }

    const opened = checkDbResult(
      await DenoCore.ops.op_db_begin_transaction(this.#binding, {
        timeoutMs: transactionTimeout(options)
      }),
      "transaction"
    );

    const state = { id: opened.transaction_id, ended: null };
    const tx = new NativeTransaction(state, opened.expires_in_ms, this.#binding);

    let value;
    try {
      value = await callback(tx);
    } catch (error) {
      state.ended = "rolled back";
      try {
        checkDbResult(
          await DenoCore.ops.op_db_rollback_transaction(this.#binding, state.id),
          "transaction.rollback"
        );
      } catch (rollbackError) {
        // The callback's error is the one the handler can act on. This one
        // usually only says the transaction was already over -- which is
        // exactly the case when its deadline is what made the callback throw
        // -- and putting it in front of the original would hide the bug the
        // application is actually looking for.
        platformConsole.warn(
          "database.transaction: the rollback after the callback threw did not succeed: " +
            rollbackError.message +
            ". The transaction ends either way; the error the callback raised is the one " +
            "being reported."
        );
      }
      throw error;
    }

    // Marked ended before the commit is even sent: the handle is spent from
    // here whether the commit lands or not, and a statement slipped in while
    // this await is outstanding would not be part of the transaction.
    state.ended = "committed";
    try {
      checkDbResult(
        await DenoCore.ops.op_db_commit_transaction(this.#binding, state.id),
        "transaction.commit"
      );
    } catch (error) {
      // A commit that did not happen means deis-store discarded the work, not
      // that the transaction is still open to retry. Say that, so a `tx` the
      // callback let escape cannot go on claiming writes the database never
      // took.
      state.ended = "rolled back";
      throw error;
    }
    return value;
  }
}

// A byte payload on its way into the store: a string is UTF-8 encoded, bytes
// are stored exactly as given. Shared by put() so that one rule decides what
// an application may hand this store.
function blobEncode(call, data) {
  if (typeof data === "string") return new TextEncoder().encode(data);
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (ArrayBuffer.isView(data)) {
    return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  }
  throw new TypeError(
    call + ": the body must be a string, an ArrayBuffer or a typed array, got " +
      (data === null ? "null" : typeof data)
  );
}

// One object as this store hands it over, with the bytes behind three
// accessors and everything the store knows beside them.
//
// etag is the object's version identity: pass it back in `onlyIf` to make the
// next write conditional, or put it in an HTTP ETag header. It is the sha256
// of the whole object even on a ranged read, which is why a range also reports
// where its bytes came from.
function blobObject(read) {
  const bytes = read.body;
  return {
    key: read.key,
    etag: read.etag,
    size: read.size,
    contentType: read.contentType === undefined ? null : read.contentType,
    uploadedAt: read.uploadedAt,
    // { offset, length } for a ranged read, null for a whole one. With size,
    // these are the three numbers a `Content-Range: bytes o-(o+l-1)/size`
    // header needs, resolved by the store rather than recomputed here.
    range: read.range === undefined ? null : read.range,
    bytes: () => bytes,
    arrayBuffer: async () =>
      bytes.byteOffset === 0 && bytes.byteLength === bytes.buffer.byteLength
        ? bytes.buffer
        : bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength),
    text: async () => new TextDecoder().decode(bytes),
    json: async () => JSON.parse(new TextDecoder().decode(bytes))
  };
}

// What a conditional write requires of the object first, in the two spellings
// HTTP uses. Returns undefined for "no condition at all", which is what an
// absent onlyIf and an empty one both mean.
function blobCondition(call, options) {
  const onlyIf = optionOf(options, "onlyIf");
  if (onlyIf === undefined || onlyIf === null) return null;
  if (typeof onlyIf !== "object") {
    throw new TypeError(
      call + ": onlyIf must be an object of { ifMatch } or { ifNoneMatch }, got " + typeof onlyIf
    );
  }
  const ifMatch = optionOf(onlyIf, "ifMatch");
  const ifNoneMatch = optionOf(onlyIf, "ifNoneMatch");
  for (const [name, value] of [["ifMatch", ifMatch], ["ifNoneMatch", ifNoneMatch]]) {
    if (value !== undefined && value !== null && typeof value !== "string") {
      throw new TypeError(
        call + ": onlyIf." + name + " must be the etag string a read answered with, or \"*\", got " +
          typeof value
      );
    }
  }
  return {
    ifMatch: ifMatch === undefined ? null : ifMatch,
    ifNoneMatch: ifNoneMatch === undefined ? null : ifNoneMatch
  };
}

class BlobStore {
  // Which namespace this object speaks for. A binding is an object-storage namespace of its own inside this tenant.
  // See UNNAMED_NAMESPACE.
  #binding;

  constructor(binding) {
    this.#binding = binding || UNNAMED_NAMESPACE;
  }

  // The binding name, or "" for the tenant's unnamed namespace.
  get binding() {
    return this.#binding;
  }

  // get(key) -> the whole object, or null when the key is not there
  // get(key, { range }) -> one slice of it
  //
  // The answer carries the bytes behind bytes() / arrayBuffer() / text() /
  // json(), plus key, etag, size, contentType, uploadedAt and - for a ranged
  // read - range.
  //
  // `range` is either an HTTP Range header value, which is what an application
  // forwards when it was handed one:
  //
  //   const object = await blob.get(key, { range: request.headers.get("range") });
  //
  // or the same range as numbers: { offset, length } from a byte, or
  // { suffix } for the last n bytes. offset with no length runs to the end.
  //
  // A range naming no byte of the object - starting at or past its end - is an
  // exception, not null. That is the difference between "this object has no
  // such bytes" (416) and "there is no such object" (null, then 404), and an
  // application answering HTTP owes its caller different answers for the two.
  async get(key, options) {
    const asked = optionOf(options, "range");
    let range = null;
    if (asked !== undefined && asked !== null) {
      if (typeof asked === "string") {
        // Parsed on the Rust side, where the one implementation of RFC 9110's
        // grammar on this platform lives.
        range = { header: asked };
      } else if (typeof asked === "object") {
        range = {
          offset: blobWholeNumber("blob.get", "range.offset", optionOf(asked, "offset")),
          length: blobWholeNumber("blob.get", "range.length", optionOf(asked, "length")),
          suffix: blobWholeNumber("blob.get", "range.suffix", optionOf(asked, "suffix"))
        };
      } else {
        throw new TypeError(
          "blob.get: range must be a Range header string or { offset, length, suffix }, got " +
            typeof asked
        );
      }
    }
    const read = await DenoCore.ops.op_blob_get(this.#binding, key, { range: range });
    if (!read) return null;
    return blobObject(read);
  }
  // head(key) -> what the store knows about the object, without its body, or
  // null when the key is not there.
  //
  // Same fields as get() minus the bytes: key, etag, size, contentType,
  // uploadedAt. This is how an application learns an object's size or media
  // type without paying for the object.
  async head(key) {
    const meta = await DenoCore.ops.op_blob_head(this.#binding, key);
    if (!meta) return null;
    return {
      key: meta.key,
      etag: meta.etag,
      size: meta.size,
      contentType: meta.contentType === undefined ? null : meta.contentType,
      uploadedAt: meta.uploadedAt
    };
  }
  // list() -> one page of every object this application has
  // list("uploads/") -> one page of the objects under a prefix
  // list({ prefix, delimiter, limit, cursor }) -> the same, spelled out
  //
  // Resolves to { objects, prefixes, list_complete, cursor }:
  //
  //   objects        an array of { key, etag, size, contentType, uploadedAt },
  //                  in key order. No bodies: a page may hold a thousand
  //                  objects, so a body is fetched by key afterwards.
  //   prefixes       with a delimiter, the folder names rolled up at this
  //                  level, each ending with the delimiter. Empty without one.
  //   list_complete  true when this page was the end of the scan.
  //   cursor         what to pass back as `cursor` for the next page, and
  //                  undefined once list_complete is true.
  //
  // Page until list_complete, not until a page comes back short: a scan can
  // fill a page exactly and still have nothing behind it, so a loop that
  // stopped at the first short page would be right only by luck.
  //
  //   const all = [];
  //   let page = { list_complete: false, cursor: undefined };
  //   while (!page.list_complete) {
  //     page = await blob.list({ prefix: "uploads/", limit: 100, cursor: page.cursor });
  //     all.push(...page.objects);
  //   }
  //
  // limit is a request, not a guarantee. deis-store substitutes its own page
  // size (200) when none is given and caps anything larger than its ceiling
  // (1000), so asking for a million yields a thousand objects and a cursor
  // rather than an error. That ceiling is what stops one application with a
  // million objects from asking the store to build a million-entry answer.
  //
  // delimiter is what makes a flat key space walkable one level at a time:
  // list({ delimiter: "/" }) answers the top-level folders in `prefixes` and
  // only the keys with no "/" left in them in `objects`.
  async list(options) {
    const bag = typeof options === "string" ? { prefix: options } : options;

    const prefix = blobString("blob.list", "prefix", optionOf(bag, "prefix"));
    const delimiter = blobString("blob.list", "delimiter", optionOf(bag, "delimiter"));
    const cursor = blobString("blob.list", "cursor", optionOf(bag, "cursor"));

    const askedLimit = optionOf(bag, "limit");
    let limit = null;
    if (askedLimit !== undefined && askedLimit !== null) {
      limit = Number(askedLimit);
      if (!Number.isFinite(limit) || Math.floor(limit) !== limit || limit <= 0) {
        throw new TypeError(
          "blob.list: limit must be a whole number of objects greater than 0, got " +
            String(askedLimit)
        );
      }
      // The op takes a 32-bit count. deis-store caps every limit at its own
      // ceiling anyway, so handing over the largest one the decoder can hold
      // keeps an absurd request answering with the ceiling and a cursor rather
      // than with a message about integers.
      if (limit > 2147483647) limit = 2147483647;
    }

    const page = await DenoCore.ops.op_blob_list(this.#binding, {
      prefix: prefix,
      delimiter: delimiter,
      limit: limit,
      cursor: cursor
    });

    const next = page.cursor === undefined || page.cursor === null ? undefined : page.cursor;
    return {
      objects: page.objects.map((object) => ({
        key: object.key,
        etag: object.etag,
        size: object.size,
        contentType: object.contentType === undefined ? null : object.contentType,
        uploadedAt: object.uploadedAt
      })),
      prefixes: page.prefixes,
      list_complete: next === undefined,
      cursor: next
    };
  }
  // put(key, data) -> { key, etag, size, contentType }
  // put(key, data, { contentType, onlyIf }) -> the same, or null
  //
  // data is a string, an ArrayBuffer or a typed array. A string is UTF-8
  // encoded; bytes are stored exactly as given.
  //
  // contentType is recorded beside the object and read back by get() and
  // head(); the store never interprets it.
  //
  // onlyIf makes the write conditional, in HTTP's two spellings:
  //
  //   { ifNoneMatch: "*" }      write only if the key does not exist -- a
  //                             create-once, and therefore a lock
  //   { ifMatch: etag }         write only if the object still carries this
  //                             etag -- a compare-and-set on the content an
  //                             earlier read returned
  //   { ifMatch: "*" }          write only if the key already exists
  //   { ifNoneMatch: etag }     write unless the object carries this etag
  //
  // A conditional write that lost resolves to **null**, and nothing else here
  // does. That is the one failure to answer by reading the object again and
  // trying: every other failure throws, and an application must not retry
  // those as they stand.
  //
  //   const current = await blob.head(key);
  //   const next = update(await (await blob.get(key)).json());
  //   const written = await blob.put(key, JSON.stringify(next), {
  //     onlyIf: { ifMatch: current.etag }
  //   });
  //   if (written === null) { /* somebody else wrote first; read and retry */ }
  async put(key, data, options) {
    const contentType = blobString("blob.put", "contentType", optionOf(options, "contentType"));
    const written = await DenoCore.ops.op_blob_put(this.#binding, key, blobEncode("blob.put", data), {
      contentType: contentType,
      onlyIf: blobCondition("blob.put", options)
    });
    if (!written) return null;
    return {
      key: written.key,
      etag: written.etag,
      size: written.size,
      contentType: written.contentType === undefined ? null : written.contentType
    };
  }
  // delete(key) -> true
  // delete(key, { onlyIf }) -> true when it was deleted, false when the
  // condition did not hold
  //
  // Deleting a key that is not there succeeds: "this key is gone" holds either
  // way. With an onlyIf, false means the object was not in the state the
  // caller required and is still stored - the same signal put() gives with
  // null, and the same response: read it again.
  async delete(key, options) {
    return await DenoCore.ops.op_blob_delete(this.#binding, key, {
      onlyIf: blobCondition("blob.delete", options)
    });
  }
}

// A string option, or null when it was not given. Rejecting the wrong type
// here rather than at the boundary is what makes the message name the option
// an application actually wrote.
function blobString(call, name, value) {
  if (value === undefined || value === null) return null;
  if (typeof value !== "string") {
    throw new TypeError(call + ": " + name + " must be a string, got " + typeof value);
  }
  return value;
}

// A whole, non-negative number of bytes, or null when it was not given.
// JavaScript has one number type, so "3.5 bytes" and "-1 bytes" both have to
// be refused here or they reach the store as something else entirely.
function blobWholeNumber(call, name, value) {
  if (value === undefined || value === null) return null;
  const number = Number(value);
  if (!Number.isFinite(number) || Math.floor(number) !== number || number < 0) {
    throw new TypeError(
      call + ": " + name + " must be a whole number of bytes, 0 or more, got " + String(value)
    );
  }
  return number;
}


// The queue carries bytes, so this is where a message becomes bytes.
//
// A string is UTF-8 encoded and an object is JSON encoded first, which is
// what an application already expected: `queue.send("text")` and
// `queue.send({ json: true })` reach the consumer as they always did. A
// buffer is sent as it stands, byte for byte -- the case the queue could
// not carry at all while its wire type was a string, even though the dead
// letter table has always been able to show one.
//
// Lifted out of `send` so that `sendBatch` encodes a body by exactly the same
// rules. Two copies of these four branches would eventually disagree, and the
// symptom would be a consumer that reads one shape from `send` and another
// from `sendBatch` for the same value.
function queueMessageBytes(message) {
  if (typeof message === "string") {
    return new TextEncoder().encode(message);
  }
  if (message instanceof ArrayBuffer) {
    return new Uint8Array(message);
  }
  if (ArrayBuffer.isView(message)) {
    return new Uint8Array(message.buffer, message.byteOffset, message.byteLength);
  }
  return new TextEncoder().encode(JSON.stringify(message));
}

class Queue {
  constructor(name = "default") {
    this.name = name;
  }
  async send(message) {
    await DenoCore.ops.op_queue_send(this.name, queueMessageBytes(message));
  }
  // Enqueue a whole array in one call, as one unit.
  //
  // Two properties an application cannot get from `send` in a loop:
  //
  //   * the batch is ATOMIC. deis-queue stores it in one transaction, so a
  //     rejected `sendBatch` has enqueued nothing at all. There is no state
  //     where half an array arrived and the caller has to work out which
  //     half, which is what makes a retry of the same array safe;
  //   * it is ONE op dispatch and one round trip. Fifty separate `send` calls
  //     are fifty dispatches counted against this application's
  //     `rate_limit_ops_per_sec` and fifty chances for the invocation's
  //     budget to run out part way through the array.
  //
  // How many messages one batch may hold is deis-queue's decision, not this
  // side's -- `max_batch_messages`, 100 unless an operator changed it -- and
  // an over-long batch is refused there with nothing stored. Deciding it here
  // would mean an application setting how long one producer may hold the
  // queue's write lock.
  //
  // Each entry must be an object with a `body`, which is Cloudflare's shape
  // and is deliberately NOT the shape `send` takes. `send({ body: x })` means
  // "JSON-encode this object", so an array of bare values and an array of
  // `{ body }` wrappers cannot both be read here without one of them silently
  // meaning the other: code ported from Cloudflare would enqueue the literal
  // text `{"body":"..."}` and nothing would report it. Requiring the wrapper
  // makes the two readings impossible to confuse, and a bare value is refused
  // with a message that says what to write instead.
  async sendBatch(messages) {
    if (!Array.isArray(messages)) {
      throw new TypeError(
        "queue.sendBatch: expected an array of messages, got " + typeof messages
      );
    }
    if (messages.length === 0) {
      throw new TypeError(
        "queue.sendBatch: the array is empty, so there is nothing to enqueue. " +
          "Check the array before calling rather than sending an empty batch"
      );
    }

    // Encoded into one flat buffer plus the length of each body: that is the
    // shape `op_queue_send_batch` splits back apart, and it costs one
    // JavaScript-to-Rust buffer conversion for the whole array instead of one
    // per message.
    const bodies = [];
    const lengths = [];
    let total = 0;
    for (let i = 0; i < messages.length; i++) {
      const entry = messages[i];
      if (entry === null || typeof entry !== "object" || !("body" in entry)) {
        throw new TypeError(
          "queue.sendBatch: entry " + i + " must be an object with a \"body\" " +
            "property, e.g. { body: " + JSON.stringify("your message") + " }; got " +
            (entry === null ? "null" : typeof entry) +
            ". Nothing was enqueued"
        );
      }
      const bytes = queueMessageBytes(entry.body);
      bodies.push(bytes);
      lengths.push(bytes.byteLength);
      total += bytes.byteLength;
    }

    const joined = new Uint8Array(total);
    let offset = 0;
    for (let i = 0; i < bodies.length; i++) {
      joined.set(bodies[i], offset);
      offset += bodies[i].byteLength;
    }

    await DenoCore.ops.op_queue_send_batch(this.name, joined, lengths);
  }
  // A queue other than this app's default one, e.g.
  // ctx.services.queue.to("jobs").send({ ... }).
  //
  // The name has to match a queue some app declares as a consumer, or the
  // messages are stored and never read.
  to(name) {
    if (typeof name !== "string" || name === "") {
      throw new TypeError("queue.to: a queue name is required");
    }
    return new Queue(name);
  }
}

// ---------------------------------------------------------
// 3. The Context & Invocation Logic
// ---------------------------------------------------------

const SENSITIVE_KEYS = /pass|secret|token|key|pwd|authorization/i;

function redact(obj) {
  if (!obj || typeof obj !== 'object') return obj;
  if (Array.isArray(obj)) return obj.map(redact);
  const newObj = {};
  for (const key in obj) {
    if (SENSITIVE_KEYS.test(key)) {
      newObj[key] = "[REDACTED]";
    } else {
      newObj[key] = redact(obj[key]);
    }
  }
  return newObj;
}

// ---------------------------------------------------------
// env.ASSETS
// ---------------------------------------------------------
//
// The way back into asset-first from inside a function. The HTTP entry asks
// the version's bundle about a path before it starts an isolate at all
// (`dispatcher/handlers.rs`), so a request for a file never gets this far.
// This is for the handler that ran anyway and wants the same answer for a path
// it chose: a route that authenticates a visitor and then serves the file
// behind it, a catch-all that answers `/` from `index.html`, an API that falls
// back to the single-page shell.
//
// It is a *binding*, not a service. `ctx.services.*` is what a manifest
// declares and what the control plane hands down an address for; nothing is
// declared or dialled here, because the bundle is this version's own and is
// already in memory. So it lives where the other env-shaped things live, under
// the name Cloudflare uses, and code written against Pages reads the same.
//
// The name cannot be taken. `runtime/bindings.rs` reserves it: a deployment
// carrying `ASSETS=...` has that key removed before the map becomes
// `ctx.env`, and the property installed below is neither writable nor
// configurable, so an application cannot put a string, or a function of its
// own, where the platform's binding goes.
const ASSETS_BINDING_NAME = "ASSETS";

// What a relative path is resolved against. Only the path and the query are
// sent to the asset layer -- a bundle is asked about its own paths and knows
// nothing about hosts -- so this exists to make `new URL` accept a target like
// "/index.html", and its host is never read.
const ASSET_TARGET_BASE = "http://assets.deis.local/";

// Captured for the same reason `ObjectAssign` is: the binding is used on every
// invocation, which is after application code has had its chance to assign
// over a global, and `URL` and `Response` are both writable in `bootstrap.js`
// because an application replacing them is an ordinary thing to do. What the
// platform hands back must still be the platform's own.
const URLCtor = URL;
const ResponseCtor = Response;

// The path and query the asset layer is asked about.
//
// A Request, a URL or a string, which is what any `fetch` takes. Read by the
// member that names the target rather than by `instanceof`, because the
// application may have replaced the constructors these objects came from and
// a request built from its `Request` is still a request. Anything else is
// refused by name rather than coerced: `String({})` is "[object Object]",
// which would be looked up as a path, answered 404, and leave the author
// hunting for a file they never asked for.
function assetTargetOf(input) {
  let raw;
  if (typeof input === "string") {
    raw = input;
  } else if (input !== null && typeof input === "object" && typeof input.url === "string") {
    // A Request names its target `url`.
    raw = input.url;
  } else if (input !== null && typeof input === "object" && typeof input.href === "string") {
    // A URL names it `href`.
    raw = input.href;
  } else {
    throw new TypeError(
      "env.ASSETS.fetch() takes a Request, a URL or a URL string; it was given " +
      (input === null ? "null" : typeof input)
    );
  }

  let url;
  try {
    url = new URLCtor(raw, ASSET_TARGET_BASE);
  } catch (e) {
    throw new TypeError(
      "env.ASSETS.fetch() was given " + JSON.stringify(raw) + ", which is not a URL or a path"
    );
  }
  return url.pathname + url.search;
}

// Frozen so that `env.ASSETS.fetch = ...` cannot replace the one function on
// it; the property itself is locked where it is installed.
const assetsBinding = ObjectFreeze({
  // Answers a Response, the same object shape any other fetch answers, so a
  // handler can `return await env.ASSETS.fetch(request)` unchanged.
  //
  // Always an answer and never a throw for a path that names nothing: a miss
  // comes back as a 404 with a body, because a handler that had to invent its
  // own would invent a different one on every route.
  //
  // Declared async although the op is synchronous -- the bytes have been in
  // memory since the version loaded -- so that the shape an author writes is
  // the one they write everywhere else.
  async fetch(input) {
    const target = assetTargetOf(input);
    const answer = DenoCore.ops.op_deis_assets_fetch(target);
    return new ResponseCtor(answer.body, {
      status: answer.status,
      headers: answer.headers,
    });
  },
});

// ---------------------------------------------------------
// Service bindings: calling another application on this platform
// ---------------------------------------------------------
//
// `context.bindings.AUTH.fetch("/verify", { method: "POST", body })` -- the
// same shape as any other fetch, so a handler that already talks to an HTTP
// service moves to a service binding by changing what it calls, not how.
//
// What is different, and worth knowing before writing one:
//
//   * it never leaves this node. There is no address, no DNS and no socket, so
//     the outbound policy is not involved and cannot be widened by it. A
//     callee running on a different node is refused, in those words;
//   * the callee has to agree. Naming it here is only asking; its own bundle
//     names who may call it (`callableBy`);
//   * it costs one subrequest out of this invocation's allowance, and a chain
//     of them is bounded;
//   * failures are told apart. "the callee is not here", "the callee will not
//     take the call", "it failed inside the callee" and "your own budget ran
//     out waiting" are four different messages, because they have four
//     different fixes.
//
// The response is an ordinary Response with the body already read, which is
// why there is no streaming form: a stream across the boundary would hold the
// callee's runtime thread for a period governed by the callee's limits and
// invisible to the caller's.

// What a relative target is resolved against. Only the path and the query are
// sent -- the callee is addressed by name, and this is only what its own
// router matches on -- so the host here is never read by anything.
const SERVICE_TARGET_BASE = "http://service.deis.local/";

// The path and query of one service call, out of whatever `fetch` was given.
//
// Read by member rather than by `instanceof`, for the reason
// `assetTargetOf` is: an application may have replaced `Request` or `URL`, and
// an object built from its own constructor is still a request.
function serviceTargetOf(binding, input) {
  let raw;
  if (typeof input === "string") {
    raw = input;
  } else if (input !== null && typeof input === "object" && typeof input.url === "string") {
    raw = input.url;
  } else if (input !== null && typeof input === "object" && typeof input.href === "string") {
    raw = input.href;
  } else {
    throw new TypeError(
      "bindings." + binding + ".fetch() takes a Request, a URL or a path; it was given " +
        (input === null ? "null" : typeof input)
    );
  }
  let url;
  try {
    url = new URLCtor(raw, SERVICE_TARGET_BASE);
  } catch (e) {
    throw new TypeError(
      "bindings." + binding + ".fetch() was given " + JSON.stringify(raw) +
        ", which is not a URL or a path"
    );
  }
  return url.pathname + url.search;
}

// The headers of one service call, as a plain object.
//
// A Headers, a plain object or an array of pairs -- the three shapes `fetch`
// takes. Whatever an application puts in `x-deis-caller` is replaced by the
// platform before the call is dispatched, so a callee reading that header is
// reading a fact rather than a claim.
function serviceHeadersOf(init, request) {
  // Pairs, not an object, for the reason `headerPairs` below is: a callee is a
  // handler like any other, and an object keeps one value per name. A call
  // built out of the request it is a hop of -- `bindings.AUTH.fetch(request)`,
  // which is the shape everyone writes -- used to arrive at the callee with
  // one of the caller's two `Cookie` lines and one of its two `Accept` lines.
  const collected = [];
  const absorb = (source) => {
    for (const [name, value] of headerPairs(source)) {
      collected.push([String(name).toLowerCase(), String(value)]);
    }
  };
  // The request's own headers first, then the init's, so an init that names a
  // header overrides the request it was built from -- which is what `fetch`
  // does. Overrides, not adds: the init's value replaces every value the
  // request carried under that name, so `{ headers: { authorization: mine } }`
  // sends one credential rather than two.
  if (request && request.headers) absorb(request.headers);
  if (init && init.headers) {
    const overridden = new NativeSet();
    for (const [name] of headerPairs(init.headers)) {
      overridden.add(String(name).toLowerCase());
    }
    for (let i = collected.length - 1; i >= 0; i--) {
      if (overridden.has(collected[i][0])) collected.splice(i, 1);
    }
    absorb(init.headers);
  }
  return collected;
}

// The body of one service call, as bytes, or null.
//
// Awaited here rather than streamed for the reason the response is collected:
// what crosses between two isolates is a finished body.
async function serviceBodyOf(binding, init, request) {
  const source =
    init && "body" in init ? init.body : request && typeof request.arrayBuffer === "function" ? request : null;
  if (source === null || source === undefined) return null;
  if (typeof source === "string") return new TextEncoder().encode(source);
  if (source instanceof ArrayBuffer) return new Uint8Array(source);
  if (ArrayBuffer.isView(source)) {
    return new Uint8Array(source.buffer, source.byteOffset, source.byteLength);
  }
  if (typeof source.arrayBuffer === "function") {
    const bytes = await source.arrayBuffer();
    return new Uint8Array(bytes);
  }
  throw new TypeError(
    "bindings." + binding + ".fetch(): body must be a string, an ArrayBuffer, an " +
      "ArrayBufferView, or a Request or Response to read one from; got " + typeof source
  );
}

// One service binding, as an application holds it.
//
// `fetch` is the only member: a service binding is a way to call an
// application, and an application answers requests. `app` and `environment`
// are readable so a handler can say in a log line which copy it is talking to
// -- which matters most in a preview, where the answer is "the preview of the
// other application", not production.
class ServiceBinding {
  #name;

  constructor(name, app, environment) {
    this.#name = name;
    this.app = app;
    this.environment = environment;
    ObjectFreeze(this);
  }

  async fetch(input, init) {
    const request = input !== null && typeof input === "object" ? input : null;
    // The body is awaited before the scope is read, not after: reading it can
    // suspend, and the answer has to be which stretch is making the call *now*
    // rather than which one was running when the body started being collected.
    const body = await serviceBodyOf(this.#name, init, request);
    const answer = await DenoCore.ops.op_deis_service_call(
      this.#name,
      {
        method:
          (init && init.method) ||
          (request && typeof request.method === "string" ? request.method : "GET"),
        url: serviceTargetOf(this.#name, input),
        headers: serviceHeadersOf(init, request)
      },
      body,
      // A service call spends the same allowance an outbound fetch does, so it
      // has to say the same thing about whose allowance it is: the invocation's,
      // or the response stream whose producer is making the call. See
      // `runtime/ops.rs`.
      StreamScope.scope()
    );
    return new ResponseCtor(answer.body, {
      status: answer.status,
      headers: answer.headers
    });
  }
}

// Build the table `context.bindings` publishes, out of what the manifest
// declared.
//
// The names come from the platform, never from the application: `op_get_bindings`
// answers with the manifest's own table as the node resolved it, so an
// application cannot invent a binding at the call site and reach something it
// never wrote down. A kind this build of the node does not know is skipped
// with a line rather than made into an object that would fail on its first
// method.
function createBindings() {
  const declared = DenoCore.ops.op_deis_get_bindings() || [];
  const table = {};
  for (const entry of declared) {
    switch (entry.kind) {
      case "kv":
        table[entry.name] = new NativeKV(entry.name);
        break;
      case "database":
        table[entry.name] = new NativeDB(entry.name);
        break;
      case "blob":
        table[entry.name] = new BlobStore(entry.name);
        break;
      case "queue":
        // A queue is already a name inside this tenant, so a queue binding is
        // that name and needs no namespace under it. It is the same name a
        // consumer declares in the manifest's `queues`.
        table[entry.name] = new Queue(entry.name);
        break;
      case "service":
        table[entry.name] = new ServiceBinding(entry.name, entry.app, entry.environment);
        break;
      default:
        platformConsole.warn(
          "binding " + JSON.stringify(entry.name) + " is of kind " +
            JSON.stringify(entry.kind) + ", which this node has no binding for; " +
            "it is not published on context.bindings or on env"
        );
    }
  }
  return table;
}

// The half of the context that does not depend on there being a request:
// the environment, the platform services, `log` and waitUntil. Shared by the
// fetch path, the queue path and the cron path so a queue or scheduled handler
// reaches storage - and writes a log line - exactly the same way a request
// handler does.
//
// `log` lives here rather than only in the entry module deis-build generates,
// because that module wraps the request path alone. Edge Canon lists
// `context.log.info/warn/error()` among the basic features and its own cron
// example calls it, so a scheduled handler that found `context.log` undefined
// would fail on the first line it wrote.
function createServiceContext() {
  // The application's own variables, plus the bindings the platform adds.
  // `ASSETS` is defined rather than assigned, and locked: an application that
  // ships a variable of that name has already had it removed on the Rust side,
  // and one that assigns over it here in strict mode -- which every ES module
  // is -- gets a TypeError rather than a silently disconnected binding.
  const env = DenoCore.ops.op_get_env() || {};
  ObjectDefineProperty(env, ASSETS_BINDING_NAME, {
    value: assetsBinding,
    enumerable: true,
    writable: false,
    configurable: false,
  });

  // Every named binding is published twice, on purpose, and they are the same
  // object either way:
  //
  //   context.bindings.HOT   the platform's table, and the honest place: only
  //                          bindings are in it, so a reader knows what they
  //                          are looking at;
  //   env.HOT                where code written for Cloudflare reaches for it,
  //                          so a project ported from there needs no rewrite.
  //
  // On `env` they are defined and locked, exactly as `ASSETS` is: a variable
  // or a secret of the same name is refused at build, at upload and at load
  // (`Manifest::validate`), and if one somehow arrived it is overwritten here
  // rather than allowed to decide what `env.HOT` means.
  const bindings = createBindings();
  for (const name of Object.keys(bindings)) {
    ObjectDefineProperty(env, name, {
      value: bindings[name],
      enumerable: true,
      writable: false,
      configurable: false,
    });
  }

  return {
    env,
    bindings,
    services: {
      // The tenant's *unnamed* namespaces: what an application has had since
      // before bindings existed, and what its data is still in. A named
      // binding is a second namespace beside these, never a renaming of them.
      kv: new NativeKV(),
      database: new NativeDB(),
      blob: new BlobStore(),
      queue: new Queue("default"),
      // Basic feature: available to every application, declared by none.
      cache: defaultCache,
    },
    log: {
      debug: (...args) => platformConsole.debug(...args),
      info: (...args) => platformConsole.info(...args),
      warn: (...args) => platformConsole.warn(...args),
      error: (...args) => platformConsole.error(...args),
    },
    // Work the invocation wants finished after its answer has gone out.
    //
    // Stamped with the stretch of code that handed it over, because the tail
    // that runs these is not started until that invocation is ending and one
    // that never reached its tail -- a request that was cut off, a WebSocket
    // message, which has no tail at all -- used to leave its promises here for
    // whichever invocation ran a tail next. That one then awaited another
    // invocation's work on its own budget, under its own trace, and with its
    // own statement about the request being answered in force. See
    // `__deis_runtime_wait_until_all`.
    waitUntil: (promise) => {
      if (!globalThis.__pending_wait_until) globalThis.__pending_wait_until = [];
      globalThis.__pending_wait_until.push({ stretch: stretchNow(), promise });
    }
  };
}

// The request context: the service half above plus the request itself and
// whatever the route matcher captured. Built on createServiceContext so a
// service added to one path cannot go missing from the other.
//
// The body is not in `requestData`. That object arrives as JSON written into
// the source text of the invocation script, and a body is arbitrary bytes: an
// upload that was not valid UTF-8 could not survive that trip at all. It is
// staged in the isolate's state instead and taken here, once, as a
// Uint8Array. Null means the request carried no body, which is what a GET
// looks like and what `new Request` wants to be told.
function createContext(requestData) {
  const body = DenoCore.ops.op_deis_take_request_body();
  const request = new Request(requestData.url, {
    method: requestData.method,
    headers: requestData.headers,
    body: body ?? null
  });
  // Stated as well as handed over: `Cache.put` has to be able to tell the
  // request this handler is answering from a stand-in built to look like it,
  // and it has to be able to tell "this code answers that request" from "this
  // code answers nothing" and from "nothing here said". The statement lasts as
  // long as this stretch of code does and no longer; see "What the code
  // running right now is answering".
  nowAnswering(answerTo(request));
  return Object.assign(createServiceContext(), {
    request,
    // Own members only: a `params` an application had planted on
    // `Object.prototype` would otherwise be handed to the next request as if
    // the router had captured it. The object itself is an ordinary one --
    // handlers do reach for `Object.prototype` methods on what they are given.
    params: ObjectAssign({}, optionOf(requestData, "params")),
  });
}

globalThis.__deis_runtime_wait_until_all = async function() {
  // The tail is the invocation's own work, not the work of any stream that
  // invocation left open: what it starts is swept below, with the invocation.
  StreamScope.leave();
  if (globalThis.__pending_wait_until && globalThis.__pending_wait_until.length > 0) {
    const queued = globalThis.__pending_wait_until;
    globalThis.__pending_wait_until = [];
    // This invocation's tail is this invocation's work. Anything handed over
    // by a stretch of code that has since ended is dropped rather than awaited
    // here: awaiting it would spend this invocation's budget on another one's
    // work, stamp its log lines with this trace, and run it with this
    // invocation's statement about the request being answered in force -- so a
    // `Cache.put` from a request that was cut off would have been judged
    // against the queue batch that happened to run next.
    //
    // Reached by an invocation that did not get to its tail (a cut-off), and by
    // one that has none at all: a WebSocket message handler can call
    // `waitUntil` through a context it captured at the upgrade, and nothing
    // runs a tail for it.
    const now = stretchNow();
    const promises = [];
    let orphaned = 0;
    // The array is `globalThis.__pending_wait_until` and an application can
    // write anything into it, so nothing here trusts its shape.
    for (const item of Array.isArray(queued) ? queued : []) {
      if (item && typeof item === "object" && item.stretch === now) {
        promises.push(item.promise);
      } else {
        orphaned++;
      }
    }
    if (orphaned > 0) {
      platformConsole.warn(
        orphaned + " waitUntil promise(s) were handed over by work that has since ended -- an " +
        "invocation that was cut off, or a WebSocket message handler, which has no tail to run " +
        "them in -- and were dropped rather than awaited inside this invocation, whose budget " +
        "and whose trace they are not part of"
      );
    }
    try {
        await Promise.all(promises);
    } catch (e) {
        platformConsole.error("Error in waitUntil promise:", e);
    }
    if (globalThis.__pending_wait_until && globalThis.__pending_wait_until.length > 0) {
      await globalThis.__deis_runtime_wait_until_all();
      // The recursive call already swept; sweeping twice would report the
      // same intervals to the application a second time.
      return;
    }
  }

  // Last thing on every path the Rust side runs -- request, queue batch and
  // cron tick all end here, after waitUntil work has been awaited.
  //
  // Three kinds of thing come back at once, and only two of them are worth
  // telling the application about. The outbound-call deadlines are this
  // runtime's own bookkeeping, one per fetch, and an invocation that ends with
  // one still armed has done nothing wrong -- so they are ended silently.
  //
  // A timer still armed at this point is different: it would keep firing inside
  // every later request this isolate serves, which is why it is named with its
  // count. See the timers section of js/bootstrap.js for why that is unbounded
  // for both kinds of timer, the self-rescheduling `setTimeout` included.
  const swept = globalThis.__deis_runtime_sweep_invocation();
  const left = [];
  if (swept.intervals > 0) left.push(swept.intervals + " interval(s)");
  if (swept.timeouts > 0) left.push(swept.timeouts + " timeout(s)");
  if (left.length > 0) {
    platformConsole.warn(
      left.join(" and ") + " were still armed when this invocation ended, and have been " +
      "cleared. An isolate is reused across requests, so a timer that outlives the request " +
      "that created it keeps firing inside later ones -- a setTimeout whose callback arms " +
      "the next one before it returns is an interval written in two lines. Clear it before " +
      "returning, do the work in the request itself, or hand the promise to " +
      "context.waitUntil, which is awaited before this sweep runs"
    );
  }
};

// ---------------------------------------------------------
// Queue consumption
// ---------------------------------------------------------
//
// The node polls deis-queue for the queues an app declared (see
// runtime/queue_consumer.rs) and hands each batch here. The handler is the
// module's `queue` export, and it takes the three parameters Edge Canon
// defines for it (SPECIFICATION_EXT.md section 3.2):
//
//   export async function queue(batch, env, context) { ... }
//
// `env` is the same table as `context.env`, handed over separately because
// that is the shape the canon fixes and the shape a handler ported from
// another platform is written against. Passing only two arguments, as this
// did, put the context where a handler expected `env` and left it reading
// `context.services` off the environment table.
//
// `batch` is { queue, messages: [...] }; each message carries id, body,
// attempts and timestamp, plus bytes() and json(). `body` is the message
// exactly as it was sent - text, not a parsed object - and `json()` is the
// parse. Delivery is at-least-once, so a handler MUST be idempotent:
// `attempts` above 1 means this message has been delivered before.
//
// A body that is not text: the queue carries bytes now, so a message can
// arrive that has no text form at all. Such a message has `body === null`,
// and `bytes()` is the only way to read it; `json()` says so rather than
// throwing a parse error about a body it never saw. `bytes()` works for every
// message, text or not, so a handler that always wants bytes can just call
// it. Nothing an application used to be able to send reaches the null case:
// until this change a body that was not UTF-8 could not be enqueued.
//
// A message counts as handled unless it says otherwise. `message.retry()`
// marks one message for redelivery and `message.ack()` takes that mark back;
// a handler that throws marks the whole batch. The returned `failed` list is
// what the node hands back to deis-queue; everything else is acknowledged and
// never delivered again.
globalThis.__deis_runtime_invoke_queue = async function(payload) {
  // Ownership goes back to the invocation before a line of the handler runs.
  // A response stream's producer parks inside an `await` with its own owner in
  // force, and the runtime thread comes back here to serve the next thing while
  // it is parked -- so without this, a timer this handler armed would be filed
  // against whichever stream stepped last, and would end when that stream did.
  // `__deis_runtime_invoke` has always done this; the queue and cron paths are
  // invocations in exactly the same sense and had been left out of it.
  StreamScope.leave();
  // A queue batch answers no request, and says so: the alternative is not
  // "answers none" but "nothing said", which `Cache.put` refuses.
  nowAnswering(NO_REQUEST);
  const messages = (payload && payload.messages) || [];
  const allIds = messages.map((m) => m.id);

  let handler = globalThis.user_module && globalThis.user_module.queue;
  if (typeof handler !== "function") {
    const dflt = globalThis.user_module && globalThis.user_module.default;
    if (dflt && typeof dflt.queue === "function") handler = dflt.queue;
  }

  if (typeof handler !== "function") {
    platformConsole.error(
      "Queue batch for '" + payload.queue + "' dropped: this app declares a queue consumer " +
      "but exports no `queue` handler, so its messages cannot be processed"
    );
    return { failed: allIds, error: "no queue handler exported" };
  }

  const failed = new Set();
  const batch = {
    queue: payload.queue,
    messages: messages.map((m) => {
      // The payload is JSON written into the source of this invocation, and
      // JSON cannot spell a byte that is not text: a body that is valid UTF-8
      // arrives as `{ text }` and anything else as `{ base64 }`. Which one it
      // is comes from the Rust side (runtime/queue_client.rs), never from
      // looking at the string - a text body that happened to look like base64
      // would otherwise be decoded into something nobody sent.
      // Which of the two it is comes from the Rust side, so it is read as
      // the Rust side wrote it: an inherited `text` would turn a binary body
      // into a string nobody sent.
      const carried = optionOf(m, "body");
      const carriedText = optionOf(carried, "text");
      const text = typeof carriedText === "string" ? carriedText : null;
      return {
        id: m.id,
        body: text,
        attempts: m.attempts,
        timestamp: m.timestamp,
        bytes() {
          if (text !== null) return new TextEncoder().encode(text);
          const binary = decodeBase64(optionOf(carried, "base64") || "");
          const out = new Uint8Array(binary.length);
          for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
          return out;
        },
        json() {
          if (text === null) {
            throw new TypeError(
              "message " + m.id + " has a body that is not text, so it cannot be parsed as " +
              "JSON; read it with message.bytes()"
            );
          }
          return JSON.parse(text);
        },
        retry() { failed.add(m.id); },
        ack() { failed.delete(m.id); },
      };
    }),
    retryAll() { for (const id of allIds) failed.add(id); },
  };

  const context = createServiceContext();

  try {
    await handler(batch, context.env, context);
  } catch (e) {
    const errorText = e && e.stack ? e.stack : String(e);
    platformConsole.error("Queue handler error on '" + payload.queue + "':", errorText);
    return { failed: allIds, error: errorText };
  }

  return { failed: Array.from(failed) };
};

// ---------------------------------------------------------
// Scheduled execution (cron)
// ---------------------------------------------------------
//
// A cron expression belongs to an application, not to a node: the control
// plane keeps the schedule and, when one is due, picks a single healthy node
// and calls its Trigger/OnScheduled. That lands here.
//
// The handler is the module's `scheduled` export, and it takes the three
// parameters Edge Canon defines for it (SPECIFICATION_EXT.md section 3.1):
//
//   export async function scheduled(event, env, context) { ... }
//
// `env` is the same table as `context.env`; see the queue path above for why
// it is handed over separately.
//
// `event` is { cron, scheduledTime, type }. A tick is fire-and-forget: it is
// not retried and it is not made up later if it fails, so anything that must
// not be missed belongs on a queue that the handler fills.
globalThis.__deis_runtime_invoke_scheduled = async function(payload) {
  // Ownership goes back to the invocation before a line of the handler runs.
  // A response stream's producer parks inside an `await` with its own owner in
  // force, and the runtime thread comes back here to serve the next thing while
  // it is parked -- so without this, a timer this handler armed would be filed
  // against whichever stream stepped last, and would end when that stream did.
  // `__deis_runtime_invoke` has always done this; the queue and cron paths are
  // invocations in exactly the same sense and had been left out of it.
  StreamScope.leave();
  // A cron tick answers no request, and says so; see the queue path above.
  nowAnswering(NO_REQUEST);
  let handler = globalThis.user_module && globalThis.user_module.scheduled;
  if (typeof handler !== "function") {
    const dflt = globalThis.user_module && globalThis.user_module.default;
    if (dflt && typeof dflt.scheduled === "function") handler = dflt.scheduled;
  }

  if (typeof handler !== "function") {
    platformConsole.error(
      "Scheduled trigger for '" + payload.cron + "' did nothing: this app declares a cron " +
      "expression but exports no `scheduled` handler"
    );
    return { ok: false, error: "no scheduled handler exported" };
  }

  const event = {
    cron: payload.cron,
    scheduledTime: payload.scheduledTime,
    type: "scheduled",
  };

  const context = createServiceContext();

  try {
    await handler(event, context.env, context);
  } catch (e) {
    const errorText = e && e.stack ? e.stack : String(e);
    platformConsole.error("Scheduled handler error on '" + payload.cron + "':", errorText);
    return { ok: false, error: errorText };
  }

  return { ok: true };
};

// Response headers that belong to the connection a body arrived on rather
// than to the body itself. See where this is used, in __deis_runtime_invoke.
//
// `content-encoding` is deliberately *not* here. It describes the bytes, not
// the framing: the node writes the handler's bytes through unchanged, so a
// handler that gzipped its own body -- or built any other encoded one -- is
// telling the client the one thing it needs in order to read them, and
// stripping it handed every such client a compressed frame labelled as plain
// content. It used to be in this list for a real reason: `fetch` inflates a
// compressed answer and the Fetch specification has the response keep the
// wire's header anyway, so `return fetch(upstream)` carried a claim about
// bytes that no longer existed. That is now settled where the inflation
// happens (`js/bootstrap.js`, `asTheApplicationSeesIt`), which is the only
// place that can tell the two apart -- so a `Content-Encoding` still here is
// one the application wrote about a body it produced.
const REFRAMED_HEADERS = new Set([
  "connection",
  "content-length",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "te",
  "trailer",
  "transfer-encoding",
  "upgrade",
]);

// Hand the answer back to `AppRuntime::execute_fetch` in two pieces: the
// status and headers as the JSON this returns, and the body -- bytes -- as a
// buffer through its own op.
//
// The body used to be a field of this object, JSON.stringify'd and parsed on
// the Rust side as a String. That corrupted everything that is not text: the
// shortest reverse proxy anyone writes, `return fetch(upstream)`, mangled
// every image it passed through.
//
// A body-less answer -- a 204, a 304, a HEAD, a WebSocket upgrade -- calls the
// op not at all, so the isolate reports "no body" rather than an empty one.
function respond(status, headers, body) {
  if (body !== null && body !== undefined) {
    DenoCore.ops.op_deis_set_response_body(
      typeof body === "string" ? new TextEncoder().encode(body) : body
    );
  }
  return { status: status, headers: headerPairs(headers) };
}

// A header list as [name, value] pairs, never as an object. Every boundary a
// header list crosses on this side goes through here: the answer leaving this
// isolate, the entry `Cache.put` writes into the response cache, and the
// request one application sends another through a service binding. The fourth,
// the inbound request, is built as a list by the node and handed to `Headers`
// unchanged.
//
// One name may legitimately appear twice, and for `Set-Cookie` it has to: the
// Fetch specification exempts that one header from being combined, so
// iterating a `Headers` yields one entry per cookie. On the way in the
// repeatable names are the ordinary ones -- `Cookie`, `Accept`,
// `Accept-Language`, `X-Forwarded-For`. An object keeps one value per key, so
// an answer that set a session cookie and a CSRF cookie used to arrive at the
// client with exactly one of them and nothing said so.
//
// Whether the repeats survive is not decided here. The node decides it, once,
// from the application's compatibility date -- see
// `runtime/header_list.rs` -- so an application written before the fix
// still gets the bytes it was written against. This side's job is only to stop
// throwing the information away before anyone can choose.
function headerPairs(headers) {
  if (Array.isArray(headers)) return headers;
  // A `Headers` first, because it is the one form that carries repeats and the
  // one form `Object.keys` cannot read at all: its entries live behind the
  // prototype, so the plain-object branch below would answer with an empty
  // list and drop every header the caller had. Iterating it yields one entry
  // per `Set-Cookie`, which is the whole reason this function exists.
  if (headers && typeof headers.forEach === "function") {
    const pairs = [];
    headers.forEach((value, name) => pairs.push([name, value]));
    return pairs;
  }
  const pairs = [];
  for (const name of Object.keys(headers || {})) pairs.push([name, headers[name]]);
  return pairs;
}

// ---------------------------------------------------------
// Response bodies that are still being produced
// ---------------------------------------------------------
//
// Almost every answer is a finished thing: a string, a JSON document, a
// rendered page. Those leave this isolate as one buffer, through `respond`
// above, and nothing below is on their path.
//
// A response whose body is a `ReadableStream` is different. Its head can go to
// the client immediately -- status, headers, first byte -- while the body is
// still being produced, and for three kinds of answer that is the only way
// they work at all:
//
//   * server-sent events, whose body by definition never ends;
//   * a large upstream being passed through, which must not be read into this
//     isolate's heap first;
//   * anything whose first byte is ready long before its last.
//
// # What the loop below is for
//
// The chunks cross to the node through `op_deis_stream_write`, which is an
// async op that does not settle until the node's queue for this stream has
// room. So the loop *is* the backpressure: a client that reads slowly leaves
// the producer parked inside that await, holding no CPU and no chunk beyond
// the two the node's queue allows.
//
// # Why the producer's fetch and interval have to be handed over
//
// Everything else this runtime does is swept when the stretch that started it
// ends: an outbound `fetch` left in flight is aborted, a timer left armed is
// cleared. A stream is the one thing that outlives its stretch on purpose, and
// the call feeding it is usually the very `fetch` the handler made. So the
// stream takes ownership of what the handler left open (`StreamScope.adopt`),
// files what its producer opens afterwards in the same place
// (`StreamScope.enter`), and gives all of it back when the stream ends
// (`StreamScope.release`). See the ownership section in `js/bootstrap.js`.
//
// Claimed, not read: the claim answers once and then answers null for good, so
// after this line nothing outside this closure can name the four verbs above.
// That is what stops a handler from choosing its own owner for a timer, which
// is a set no sweep looks in.
const StreamScope = globalThis.__deis_runtime_claim_stream_scope();
if (StreamScope === null) {
  // Only reachable if this file were executed twice in one isolate, which is
  // already impossible -- it reads `Deno.core` and then deletes `globalThis.Deno`
  // at the top. Said out loud rather than left to fail later as "cannot read
  // property adopt of null" the first time an application streams a response.
  throw new TypeError(
    "the response-stream scope was already claimed: js/runtime.js has run twice in this " +
      "isolate, and this isolate cannot serve a streaming response",
  );
}

// Captured for the reason `ObjectAssign` above is: chunks are inspected on
// every write, after the application has had every chance to replace a global.
const NativeUint8Array = Uint8Array;
const NativeArrayBuffer = ArrayBuffer;
const isArrayBufferView = ArrayBuffer.isView;

// The body of the response the handler just returned, waiting for the node to
// open a queue for it.
//
// A slot rather than a return value because the head and the body leave by
// different doors: `__deis_runtime_invoke` answers with the status and the
// headers, `runtime/isolate.rs` sends those to the client, and only then --
// once there is somewhere for the bytes to go -- does it call
// `__deis_runtime_stream_begin`.
//
// Stamped with the stretch of code that filled it, and read back only by that
// same stretch -- the node opens the queue and asks for the producer before
// the invocation ends, so the two are always one stretch apart from nothing.
// A body left here by an invocation that answered `{ stream: true }` and never
// got its queue is therefore not something a later request can be handed; see
// "What the code running right now is answering" for why that is a stamp
// rather than a clear at the top of each entry point.
let pendingStreamBody = null;
let pendingStreamBodyStretch = -1;

// The streams this isolate is producing right now, by the id the node gave
// them. Several at once is ordinary: an application serving server-sent events
// has one per connected browser.
const openStreams = new NativeMap();

// The stream whose body the code now running is producing, or undefined when
// it is not producing one.
//
// Derived rather than stated, and that is the point. A producer's own code
// runs *inside* the `await entry.reader.read()` in `pumpStream`, and that await
// spans whatever the isolate does next -- so a statement made before the read
// is not one the producer can be relied on to still be inside. What survives
// that await is the ownership machinery, because it is what an outbound `fetch`
// and a timer are already filed against: `StreamScope.scope()` answers the id
// of the stream whose owner is in force, and 0 for the invocation's own. So
// this asks the same question the fetch guard asks, and gets the same answer.
//
// `Cache.put` reads it to find what the producer is answering: a stream is
// producing the body of the request whose head has already gone out, and
// everything it caches is that caller's. Before this the producer read a
// module-level variable holding whichever request the isolate had served most
// recently, which between two chunks of one body can be any number of others.
function streamBeingProduced() {
  const id = StreamScope.scope();
  return id === 0 ? undefined : openStreams.get(id);
}

// One chunk as bytes, or null if it is not something a body can carry.
//
// Strings are accepted and encoded as UTF-8. The web's own rule is that a body
// stream carries `BufferSource` only, and refusing a string would be defensible
// -- but every server-sent-events producer ever written enqueues text, and
// making each one wrap a `TextEncoder` around itself buys nothing. Anything
// else is refused by name rather than coerced, because `String({})` is
// "[object Object]" and a body of that is worse than an error.
function chunkBytes(value) {
  if (value instanceof NativeUint8Array) return value;
  if (isArrayBufferView(value)) {
    return new NativeUint8Array(value.buffer, value.byteOffset, value.byteLength);
  }
  if (value instanceof NativeArrayBuffer) return new NativeUint8Array(value);
  if (typeof value === "string") return new TextEncoder().encode(value);
  return null;
}

// Take a stream out of the isolate's books and give back everything it held.
// Every ending goes through here, so none of them can forget the release.
//
// What it held is said out loud when there was anything, at debug: a stream is
// the one thing on this platform allowed to keep an outbound call or a timer
// past the invocation that made it, and this line is where an operator can see
// how much of that there was and that it ended with the stream. Debug rather
// than warn because a producer pacing itself with one interval is doing exactly
// what streams are for -- the number is worth having, the alarm is not.
function retireStream(id, entry) {
  if (entry.closed) return false;
  entry.closed = true;
  openStreams.delete(id);
  const held = StreamScope.release(entry.owner);
  if (held.calls > 0 || held.intervals > 0 || held.timeouts > 0) {
    platformConsole.debug(
      "Response stream " + id + " ended holding " + held.calls + " outbound call(s), " +
      held.intervals + " interval(s) and " + held.timeouts + " timeout(s); all of them ended " +
      "with it, so none outlives the stream that owned them"
    );
  }
  return true;
}

// The producer said it was finished. Closing the node's queue is what writes
// the terminating chunk, so the client sees a complete response.
function finishStream(id, entry) {
  if (!retireStream(id, entry)) return;
  DenoCore.ops.op_deis_stream_close(id);
}

// There is nobody left to write to: the client hung up, or the node closed the
// stream because it ran past `stream_timeout_ms`. Not a failure of the
// application, and there is no client left to tell about it either way.
function abandonStream(id, entry, why) {
  if (!retireStream(id, entry)) return;
  try {
    // `cancel` runs the application's own `cancel` hook. By this point the
    // stream is off the books and its owner has been released, so a
    // `Cache.put` from that hook is code with nothing saying what it answers,
    // and is refused. That is the right answer here rather than a gap: this
    // stream is over, and the request it was producing for is long gone.
    entry.reader.cancel(new Error(why));
  } catch (_) {
    // A reader that refuses to be cancelled must not become a second failure;
    // the stream is off the books either way.
  }
  DenoCore.ops.op_deis_stream_close(id);
}

// The producer threw part-way through. The head went out long ago, so there is
// no status left to change: the node ends the body without its terminating
// chunk, which the client reports as a failed transfer rather than as a short
// success. See `runtime/response_stream.rs`.
function failStream(id, entry, message) {
  if (!retireStream(id, entry)) return;
  try {
    // As in `abandonStream`: the `cancel` hook runs with the stream already
    // off the books, so a `Cache.put` from it is refused.
    entry.reader.cancel(new Error(message));
  } catch (_) {
    // As above.
  }
  DenoCore.ops.op_deis_stream_fail(id, message);
}

// Drain one stream into the node, one chunk at a time, until it ends, fails,
// or the client goes away.
async function pumpStream(id, entry) {
  for (;;) {
    // Set before the read rather than after it, because the application's own
    // producer runs *inside* this await: a `pull` that calls `fetch` is filed
    // against this stream, and so is swept when this stream ends rather than
    // when the next request to this application does.
    StreamScope.enter(entry.owner);

    let step;
    try {
      step = await entry.reader.read();
    } catch (e) {
      failStream(id, entry, e && e.stack ? e.stack : String(e));
      return;
    }
    if (entry.closed) return;
    if (step.done) {
      finishStream(id, entry);
      return;
    }

    const bytes = chunkBytes(step.value);
    if (bytes === null) {
      failStream(
        id,
        entry,
        "a response body stream enqueued a " + typeof step.value +
          "; a body may only carry bytes (a Uint8Array or any other ArrayBuffer view) or a string",
      );
      return;
    }
    // A zero-length chunk is a legal thing to enqueue and means nothing, so it
    // is dropped rather than written: written, it would cost a round trip
    // through the node's queue for no bytes.
    if (bytes.length === 0) continue;

    StreamScope.enter(entry.owner);
    try {
      await DenoCore.ops.op_deis_stream_write(id, bytes);
    } catch (_) {
      abandonStream(id, entry, "the client stopped reading this response");
      return;
    }
    if (entry.closed) return;
  }
}

// Start producing the body of the response whose head has just gone out.
//
// Called by `runtime/isolate.rs` once the client has somewhere to receive
// bytes. Synchronous on purpose: it hands the loop above to the event loop and
// returns, so the invocation that produced the head can finish -- including its
// `waitUntil` tail -- and the runtime thread goes back to serving other
// requests for this application between chunks.
globalThis.__deis_runtime_stream_begin = function(id) {
  // An id this isolate is already producing is not something the node ever
  // asks for twice -- it opens each stream once and calls this once. Refused
  // rather than trusted, because overwriting the entry would take the running
  // stream off the books while leaving it open: its owner would never be
  // released, and the timers and outbound calls that owner holds would go on
  // firing inside every later request. This name is sealed but callable, and a
  // small integer is not hard to guess.
  if (openStreams.has(id)) {
    DenoCore.ops.op_deis_stream_fail(
      id,
      "this response stream is already being produced; it was asked to start a second time",
    );
    return;
  }

  const source = pendingStreamBodyStretch === stretchNow() ? pendingStreamBody : null;
  pendingStreamBody = null;
  pendingStreamBodyStretch = -1;

  if (source === null || source === undefined) {
    DenoCore.ops.op_deis_stream_fail(
      id,
      "the runtime was asked to stream a response body it was not holding; this is a fault in " +
        "the node, not in the application",
    );
    return;
  }

  // Adopted before the reader is taken: whatever the handler still has open
  // belongs to this stream from here on, and its outbound calls are put on the
  // stream's clock rather than on the request's, which is already spent.
  //
  // The handover is logged when there was anything to hand over, so the one
  // exception to "nothing outlives its invocation" is visible at the moment it
  // is made rather than only when the stream ends. The stream's own budget is
  // logged beside this by `runtime/isolate.rs`, which is what bounds all of it.
  const adopted = StreamScope.adopt(id);
  const owner = adopted.owner;
  const held = adopted.held;
  if (held.calls > 0 || held.intervals > 0 || held.timeouts > 0) {
    platformConsole.debug(
      "Response stream " + id + " took over " + held.calls + " outbound call(s), " +
      held.intervals + " interval(s) and " + held.timeouts + " timeout(s) that the handler " +
      "left open; they now end with the stream instead of with the request"
    );
  }

  let reader;
  try {
    reader = source.getReader();
  } catch (e) {
    StreamScope.release(owner);
    DenoCore.ops.op_deis_stream_fail(
      id,
      "the response body could not be read: " + (e && e.message ? e.message : String(e)),
    );
    return;
  }

  // What this stream is producing the body of. Read here, while the invocation
  // that produced the head is still the stretch in force -- the node asks for
  // the producer before that invocation ends -- because from the first chunk
  // onwards there is nothing left to read it from.
  const entry = { reader, owner, closed: false, answering: whatIsAnswered() };
  openStreams.set(id, entry);
  // Nothing in the loop should reach this catch -- every step it takes has one
  // of its own -- but an unhandled rejection inside an isolate is not a local
  // failure: deno_core reports it as an event-loop error, and
  // `AppRuntime::pump_response_streams` would then abort every other stream
  // this application has open along with this one.
  pumpStream(id, entry).catch((e) => {
    failStream(id, entry, e && e.stack ? e.stack : String(e));
  });
  // The pump ran up to its first await and left this stream as the owner of
  // whatever is opened next. The `waitUntil` tail runs after this returns and
  // is not this stream's work, so ownership goes back to the invocation.
  StreamScope.leave();
};

// Stop producing: the client went away, or the stream ran out of time.
//
// Called by `runtime/isolate.rs`, which is where both of those are noticed. It
// is what makes cancellation reach the application: without it a producer
// would go on filling a queue nobody drains until its budget ran out.
globalThis.__deis_runtime_stream_cancel = function(id, reason) {
  const entry = openStreams.get(id);
  if (entry === undefined) return false;
  abandonStream(id, entry, reason);
  return true;
};

// Give back what every open stream is still holding, because this isolate can
// no longer produce any of them.
//
// `runtime/isolate.rs::fail_open_response_streams` ends the client's half of
// each body. This is the isolate's half, and it is not the same thing: the
// paths that get there by terminating the isolate leave every producer stopped
// mid-step, inside an `await` that will never resume, so nothing on this side
// would ever notice its stream is over. The entry would stay in `openStreams`
// holding that producer's owner -- and a stream's owner is the one set the
// end-of-invocation sweep is not allowed to touch, so the interval pacing a
// body nobody is reading any more would go on firing inside every later request
// this application serves. That is the leak the whole ownership arrangement
// exists to prevent, reached without any internal call: return a streaming
// response whose producer will not yield, and the cut-off does the rest.
//
// The producers are not cancelled, and the node's half is not touched: on every
// path that calls this the node has already ended each body, and
// `reader.cancel()` runs the application's own `cancel` hook, whose code has
// either just been terminated or has nothing left that can wake it. The reap in
// `runtime/isolate.rs` is where a stream that ends in the ordinary way gets its
// cancel.
//
// The name is sealed but callable, so an application can release its own
// producers out of turn. What that costs it is its own bodies: the node still
// holds those streams open with nobody writing to them, which
// `pump_response_streams` notices as "no operation or timer outstanding" and
// ends as failures rather than making each client wait out `stream_timeout_ms`.
//
// Answers with how many were released, for the caller's log.
globalThis.__deis_runtime_stream_release_all = function() {
  const ids = [];
  openStreams.forEach((_entry, id) => { ids.push(id); });
  let released = 0;
  for (const id of ids) {
    const entry = openStreams.get(id);
    if (entry !== undefined && retireStream(id, entry)) released++;
  }
  if (released > 0) {
    platformConsole.debug(
      released + " response stream producer(s) were released because this isolate can no longer " +
      "produce any of them; each one's outbound calls and timers ended with it, and the line " +
      "above says how much each was holding"
    );
  }
  return released;
};

// Captured for the reason the captures at the top of this file are: an
// application may replace `Math.random`, and the id below is the node's word
// about one of its own failures rather than something the application gets to
// choose. Captured at load, which is before any application code has run.
const MathRandom = Math.random;

// A handle for one failed invocation: what the client is given in place of the
// error, and what the node's log line about that same error carries beside the
// stack. See the catch in `__deis_runtime_invoke` for why the client gets this
// and not the error.
//
// Sixteen hex characters. It authorises nothing and guards nothing -- it is a
// handle, not a token -- so `Math.random` is enough: what it has to be is
// unique enough that a `deis logs -a <app>` search lands on one line. The loop
// is bounded rather than "until long enough" because `Math.random()` may
// return 0, whose hex form contributes no digits at all.
function newIncidentId() {
  let id = "";
  for (let i = 0; i < 8 && id.length < 16; i++) {
    id += MathRandom().toString(16).slice(2);
  }
  return (id + "0000000000000000").slice(0, 16);
}

globalThis.__deis_runtime_invoke = async function(requestPayload) {
  // The reference to a stream the previous invocation stashed and never began
  // is dropped here. It could not become this request's body -- the slot
  // records which stretch of code filled it and `__deis_runtime_stream_begin`
  // reads it only for that one -- but a `ReadableStream` nobody will ever read
  // should not be held until the next request happens to arrive.
  //
  // Ownership goes back to the invocation because what this handler opens is
  // this handler's, and is swept when it ends.
  //
  // Nothing clears the request being answered, and nothing has to: this is a
  // new stretch of application code, so whatever the last one stated is
  // already unreadable, and `createContext` states this request below. See
  // "What the code running right now is answering".
  pendingStreamBody = null;
  pendingStreamBodyStretch = -1;
  StreamScope.leave();
  try {
    let handler;
    if (globalThis.user_module && typeof globalThis.user_module.default === 'function') {
        handler = globalThis.user_module.default;
    }
    
    if (!handler) {
      return respond(500, {}, "No default export handler found (Edge Canon requires 'export default function(context)')");
    }

    const ctx = createContext(requestPayload);
    
    let resp;
    resp = await handler(ctx);
    
    if (!(resp instanceof Response)) {
       return respond(500, {}, "Handler did not return a Response object");
    }

    // Prepare response
    //
    // Headers that describe how a body was framed on some *other* connection
    // are dropped. What leaves this isolate is the buffer read below, and the
    // node frames it again on its own connection to the client, so a
    // Content-Length or Transfer-Encoding taken from somewhere else is a claim
    // about a body that no longer exists. This is not hypothetical since `fetch` exists:
    // `return fetch(upstream)` -- the shortest reverse proxy anyone writes --
    // hands back a Response still carrying the upstream's framing, and a
    // Content-Length that disagrees with the bytes actually sent is a
    // truncated or hung response at the client.
    const headers = [];
    resp.headers.forEach((v, k) => {
      if (!REFRAMED_HEADERS.has(k.toLowerCase())) headers.push([k, v]);
    });
    
    // Check for WebSocket upgrade
    // Check if user attached webSocket to the response object dynamically
    // An own property, which is where the `Response` constructor above puts
    // it and where a handler that attaches one afterwards puts it too. Read
    // the ordinary way, a `webSocket` on `Object.prototype` would have turned
    // every later plain response on this isolate into an upgrade.
    let webSocketId = undefined;
    const attached = optionOf(resp, "webSocket");
    if (attached && optionOf(attached, "ws_id")) {
        webSocketId = attached.ws_id;
        headers.push(["X-Runtime-WebSocket-ID", webSocketId]);
    }

    // Read body.
    //
    // arrayBuffer(), not text(): the handler's bytes go out as they are. A
    // response with no body at all stays without one -- an upgrade, and the
    // 204/304/HEAD answers where a zero-length body is a different thing from
    // no body.
    // A body the application is still producing goes out chunk by chunk
    // instead. Nothing is read here: `arrayBuffer()` on such a body is exactly
    // the wait this avoids -- for server-sent events it never returns at all,
    // and for a large upstream it is the copy that gets the application killed
    // for its heap.
    if (!webSocketId && resp.body && !bufferedBodies.has(resp)) {
      pendingStreamBody = resp.body;
      pendingStreamBodyStretch = stretchNow();
      return { status: resp.status, headers: headerPairs(headers), stream: true };
    }

    let body = null;
    if (!webSocketId && resp.body) {
        body = new Uint8Array(await resp.arrayBuffer());
    }

    return respond(resp.status, headers, body);
  } catch (e) {
    // What the client is told, and what it is deliberately not told.
    //
    // The stack goes to the node's log and nowhere else. It names the tenant's
    // bundle layout ("deis:///apps/<app>.<env>/worker/index.js:22:24"), this
    // runtime's own line numbers, the vendored deno version's internals, and
    // whatever the exception message happens to carry -- which, for a throw
    // from inside a credential check, is the credential. Anyone who can reach
    // this node can make a handler throw: `curl -X POST` at a handler that
    // parses JSON is enough, and until this wave `curl -X TRACE` needed no
    // handler at all, because deno_fetch's forbidden-method list threw while
    // the request was being built. The body below is also what a *service
    // call* sees -- app A calling app B through a binding gets B's status and
    // body verbatim -- and A is another tenant.
    //
    // The application's author should see the stack, and does: through
    // `deis logs -a <app>`, which is a channel the platform authenticates.
    // The HTTP body is not that channel, because it has no reader the platform
    // can identify.
    //
    // `incident` is what joins the two. `platformConsole.error` goes through
    // `op_log`, which stamps the line with this application's id and this
    // invocation's trace id, so the line below is already attributable and
    // already reaches `deis logs`; the id in the body is what lets an operator
    // find that one line without knowing the second the request arrived, and
    // the same line carries the trace id for `deis trace --trace-id`.
    //
    // It is a fresh id rather than the trace id itself only because the trace
    // id lives in the node's `OpState` and no op hands it to this isolate --
    // see `runtime/ops.rs::current_trace_id`, which every log line goes
    // through and nothing here can call. When an op exposes it, this should
    // carry the trace id and drop the second identifier.
    const errorText = e && e.stack ? e.stack : String(e);
    const incident = newIncidentId();
    platformConsole.error(
      "Runtime invoke error [incident " + incident + "]:",
      errorText
    );
    return respond(500, {}, "Internal Server Error (incident " + incident + ")");
  }
};

// ---------------------------------------------------------
// Console bridge
// ---------------------------------------------------------
//
// deno_core installs a console that only writes to this process's stdout.
// Application output that stops there is invisible to `deis tail` and to the
// log panel: it exists solely in the node's local journal, and disappears with
// the container. This wrapper hands every call to op_log as well, which tags
// the line with the application and puts it in the node's log buffer for the
// shipper to send to deis-telemetry.
//
// The native console is still called, so stdout keeps showing what it showed
// before -- that copy is the fallback the shipper's own warnings point at when
// telemetry is unreachable.
{
  const native = globalThis.console;

  // Render one console argument the way a log line wants it: strings as
  // themselves, errors with their stack, everything else as JSON, and anything
  // JSON refuses (cycles, BigInt, functions) as its plain string form.
  const render = (value) => {
    if (typeof value === "string") return value;
    if (value === undefined) return "undefined";
    if (value === null) return "null";
    if (value instanceof Error) return value.stack || String(value);
    try {
      const json = JSON.stringify(value);
      return json === undefined ? String(value) : json;
    } catch (_) {
      return String(value);
    }
  };

  const forward = (level, args) => {
    let line;
    try {
      line = args.map(render).join(" ");
    } catch (_) {
      // A getter on one of the arguments threw. Losing the line entirely
      // would be worse than losing its contents, so ship a marker.
      line = "<unrenderable console argument>";
    }
    try {
      DenoCore.ops.op_log(level, line);
    } catch (_) {
      // Never let logging break the request that produced it; the native
      // console call below still puts the line in this node's own output.
    }
    return line;
  };

  // The same rule `escape_control` applies on the Rust side (Unicode `Cc`:
  // C0, DEL and the C1 block, spelled the way `char::escape_debug` spells
  // them), for the one copy of an application's text that never passes
  // through Rust: the native console below writes straight to this process's
  // stdout, past `op_log`, past `tracing`, past the writer that keeps every
  // other line whole. The node's log file is that stdout, and fifteen demo
  // checks and every operator read it by line, so a newline here forged a
  // whole line there while the `tracing` copy of the same call was already
  // escaped. Both copies now obey one rule.
  const escapeControl = (text) =>
    text.replace(/[\u0000-\u001f\u007f-\u009f]/g, (c) => {
      switch (c) {
        case "\n": return "\\n";
        case "\r": return "\\r";
        case "\t": return "\\t";
        case "\0": return "\\0";
        default: return "\\u{" + c.charCodeAt(0).toString(16) + "}";
      }
    });

  const bridged = (level, method) =>
    function (...args) {
      const line = forward(level, args);
      if (native && typeof native[method] === "function") {
        // One already-rendered string, not the original arguments: the
        // native console would render an Error's stack and an object's
        // fields itself, and those are exactly the values that carry a
        // tenant's newlines.
        native[method].call(native, escapeControl(line));
      }
    };

  // One object, reached by two names. `globalThis.console` is the
  // application's and stays replaceable; `platformConsole` is this file's and
  // is not reachable from outside this closure, so what the platform says about
  // an application cannot be turned off by that application.
  platformConsole = Object.assign(Object.create(native ?? null), native, {
    log: bridged("info", "log"),
    info: bridged("info", "info"),
    debug: bridged("debug", "debug"),
    warn: bridged("warn", "warn"),
    error: bridged("error", "error"),
    trace: bridged("debug", "trace"),
  });
  globalThis.console = platformConsole;
}

globalThis.user_module = {}; globalThis.__deis_runtime_set_response = (json) => DenoCore.ops.op_set_response(json);

// The names the Rust side calls by, sealed in one place now that they all
// exist.
//
// These are not the application's surface, and an application that could put
// its own function under one of them would not merely be lying to itself.
// `__deis_runtime_invoke` is what runs an invocation *and what sweeps up after
// one*: `__deis_runtime_wait_until_all` calls
// `__deis_runtime_sweep_invocation` at the end, which is where an application's
// timers and its outbound-fetch deadlines stop. `js/bootstrap.js` seals that
// sweep, and that is not enough on its own while the function that calls it can
// be replaced with one that does not -- timers would then go on firing on a
// runtime thread shared with every other application on it, long after the
// request that created them.
//
// `user_module` is deliberately not in this list: `runtime/isolate.rs` writes
// the application's exports there once the module has evaluated.
for (const name of [
  "__deis_runtime_invoke",
  "__deis_runtime_stream_begin",
  "__deis_runtime_stream_cancel",
  "__deis_runtime_stream_release_all",
  "__deis_runtime_invoke_queue",
  "__deis_runtime_invoke_scheduled",
  "__deis_runtime_wait_until_all",
  "__deis_runtime_set_response",
  "__deis_runtime_ws_servers",
  // What the module-evaluation glue calls to say that the top level of a
  // bundle answers no request. Sealed for the same reason the rest are: an
  // application that could replace it could make every stretch of its own code
  // claim to answer nothing, which is the permissive reading of the question
  // `Cache.put` asks.
  "__deis_runtime_answers_no_request",
]) {
  Object.defineProperty(globalThis, name, {
    value: globalThis[name],
    writable: false,
    enumerable: false,
    configurable: false,
  });
}

})();
