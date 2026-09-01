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
  
  send(data) { console.warn("BridgeWebSocket.send ignored"); }
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

// How long the response asked to stay fresh, in whole seconds, or 0 when it
// named no lifetime and the store's default should apply.
//
// `s-maxage` wins over `max-age` where both are present: this is a shared
// cache, and that is what the directive is for.
function freshnessSeconds(response) {
  const headers = response && response.headers;
  const control = headers && typeof headers.get === "function"
    ? headers.get("cache-control")
    : null;
  if (!control) return 0;

  const shared = /(?:^|[,\s])s-maxage\s*=\s*"?(\d+)"?/i.exec(control);
  const own = /(?:^|[,\s])max-age\s*=\s*"?(\d+)"?/i.exec(control);
  const chosen = shared || own;
  if (!chosen) return 0;

  const seconds = Number(chosen[1]);
  if (!Number.isFinite(seconds) || seconds <= 0) return 0;
  // The op takes a u32; a longer lifetime than that is indistinguishable from
  // "as long as this isolate lives" anyway.
  return Math.min(Math.floor(seconds), 4294967295);
}

function refusesToBeStored(response) {
  const headers = response && response.headers;
  const control = headers && typeof headers.get === "function"
    ? headers.get("cache-control")
    : null;
  if (!control) return null;
  if (/(?:^|[,\s])no-store(?:\s*(?:,|$))/i.test(control)) return "no-store";
  if (/(?:^|[,\s])private(?:\s*(?:,|$))/i.test(control)) return "private";
  return null;
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
  async put(request, response) {
    const key = cacheKeyOf(request);

    // A response that declares itself uncacheable is not stored, and the
    // caller is told rather than left to believe it was. Cloudflare's Cache
    // API refuses the same two directives, and silently keeping a `private`
    // response in a cache every later request reads is how one user's data
    // reaches another.
    const refusal = refusesToBeStored(response);
    if (refusal) {
      throw new TypeError(
        "cache.put: this response carries Cache-Control: " + refusal +
          ", so it was not stored"
      );
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
    const meta = {
      status: response.status,
      headers: Object.fromEntries(response.headers.entries())
    };
    await DenoCore.ops.op_cache_set(
      this.name,
      key,
      JSON.stringify(meta),
      body,
      freshnessSeconds(response)
    );
  }
  async match(request) {
    const key = cacheKeyOf(request);
    const hit = await DenoCore.ops.op_cache_get(this.name, key);
    if (!hit) return undefined;
    try {
      const meta = JSON.parse(hit.meta);
      // A 204 or a 304 may not carry a body at all, and the constructor
      // refuses an empty buffer as loudly as a full one -- so an entry with
      // nothing in it is rebuilt with a null body rather than a zero-length
      // one, which is also what a bodiless response was to begin with.
      const body = hit.body.byteLength === 0 ? null : hit.body;
      return new Response(body, {
        status: meta.status,
        headers: meta.headers
      });
    } catch {
      return undefined;
    }
  }
  async delete(request) {
    return await DenoCore.ops.op_cache_delete(this.name, cacheKeyOf(request));
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
        console.warn(
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
  const collected = {};
  const absorb = (source) => {
    if (!source) return;
    if (typeof source.forEach === "function" && typeof source.entries === "function") {
      // A Headers, or anything shaped like one.
      source.forEach((value, name) => {
        collected[String(name).toLowerCase()] = String(value);
      });
      return;
    }
    if (Array.isArray(source)) {
      for (const pair of source) {
        if (Array.isArray(pair) && pair.length === 2) {
          collected[String(pair[0]).toLowerCase()] = String(pair[1]);
        }
      }
      return;
    }
    if (typeof source === "object") {
      for (const name of Object.keys(source)) {
        collected[name.toLowerCase()] = String(source[name]);
      }
    }
  };
  // The request's own headers first, then the init's, so an init that names a
  // header overrides the request it was built from -- which is what `fetch`
  // does.
  if (request && request.headers) absorb(request.headers);
  if (init && init.headers) absorb(init.headers);
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
    const answer = await DenoCore.ops.op_deis_service_call(
      this.#name,
      {
        method:
          (init && init.method) ||
          (request && typeof request.method === "string" ? request.method : "GET"),
        url: serviceTargetOf(this.#name, input),
        headers: serviceHeadersOf(init, request)
      },
      await serviceBodyOf(this.#name, init, request)
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
        console.warn(
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
      debug: (...args) => console.debug(...args),
      info: (...args) => console.info(...args),
      warn: (...args) => console.warn(...args),
      error: (...args) => console.error(...args),
    },
    waitUntil: (promise) => {
      if (!globalThis.__pending_wait_until) globalThis.__pending_wait_until = [];
      globalThis.__pending_wait_until.push(promise);
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
  return Object.assign(createServiceContext(), {
    request: new Request(requestData.url, {
      method: requestData.method,
      headers: requestData.headers,
      body: body ?? null
    }),
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
    const promises = globalThis.__pending_wait_until;
    globalThis.__pending_wait_until = [];
    try {
        await Promise.all(promises);
    } catch (e) {
        console.error("Error in waitUntil promise:", e);
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
  // The deadline timers go first and silently: they are this runtime's own
  // bookkeeping, one per outbound fetch, and an invocation that ends with one
  // still armed has done nothing wrong. Leaving them would mean a later
  // request in this same isolate carried an abort timer belonging to an
  // invocation that is already over.
  globalThis.__deis_runtime_sweep_fetch_budgets();

  // An interval still running at this point would keep firing inside every
  // later request this isolate serves; see the timers section of
  // js/bootstrap.js for why that is unbounded and why timeouts are left
  // alone.
  const swept = globalThis.__deis_runtime_sweep_intervals();
  if (swept > 0) {
    console.warn(
      swept + " interval(s) were still running when this invocation ended and have been " +
      "cleared. An isolate is reused across requests, so an interval that outlives the " +
      "request that created it keeps firing inside later ones; clear it before returning, " +
      "or do the work in the request itself"
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
  const messages = (payload && payload.messages) || [];
  const allIds = messages.map((m) => m.id);

  let handler = globalThis.user_module && globalThis.user_module.queue;
  if (typeof handler !== "function") {
    const dflt = globalThis.user_module && globalThis.user_module.default;
    if (dflt && typeof dflt.queue === "function") handler = dflt.queue;
  }

  if (typeof handler !== "function") {
    console.error(
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
    console.error("Queue handler error on '" + payload.queue + "':", errorText);
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
  let handler = globalThis.user_module && globalThis.user_module.scheduled;
  if (typeof handler !== "function") {
    const dflt = globalThis.user_module && globalThis.user_module.default;
    if (dflt && typeof dflt.scheduled === "function") handler = dflt.scheduled;
  }

  if (typeof handler !== "function") {
    console.error(
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
    console.error("Scheduled handler error on '" + payload.cron + "':", errorText);
    return { ok: false, error: errorText };
  }

  return { ok: true };
};

// Response headers that belong to the connection a body arrived on rather
// than to the body itself. See where this is used, in __deis_runtime_invoke.
// `content-encoding` is in the list because `fetch` already decoded the body
// before the handler saw it: telling the client it is still gzip would leave
// it trying to inflate bytes that are no longer compressed.
const REFRAMED_HEADERS = new Set([
  "connection",
  "content-encoding",
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

// Headers leave this isolate as a list of [name, value] pairs, never as an
// object.
//
// One name may legitimately appear twice, and for `Set-Cookie` it has to: the
// Fetch specification exempts that one header from being combined, so
// iterating a `Headers` yields one entry per cookie. An object keeps one value
// per key, so an answer that set a session cookie and a CSRF cookie used to
// arrive at the client with exactly one of them and nothing said so.
//
// Whether the repeats survive is not decided here. The node decides it, once,
// from the application's compatibility date -- see
// `runtime/response_headers.rs` -- so an application written before the fix
// still gets the bytes it was written against. This side's job is only to stop
// throwing the information away before anyone can choose.
function headerPairs(headers) {
  if (Array.isArray(headers)) return headers;
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
// ends: an outbound `fetch` left in flight is aborted, an interval left armed
// is cleared. A stream is the one thing that outlives its stretch on purpose,
// and the call feeding it is usually the very `fetch` the handler made. So the
// stream takes ownership of what the handler left open (`StreamScope.adopt`),
// files what its producer opens afterwards in the same place
// (`StreamScope.enter`), and gives all of it back when the stream ends
// (`StreamScope.release`). See the ownership section in `js/bootstrap.js`.
const StreamScope = globalThis.__deis_runtime_stream_scope;

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
// `__deis_runtime_stream_begin`. Cleared at the start of every invocation, so
// a stream that was never begun cannot become the next request's body.
let pendingStreamBody = null;

// The streams this isolate is producing right now, by the id the node gave
// them. Several at once is ordinary: an application serving server-sent events
// has one per connected browser.
const openStreams = new NativeMap();

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
function retireStream(id, entry) {
  if (entry.closed) return false;
  entry.closed = true;
  openStreams.delete(id);
  StreamScope.release(entry.owner);
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
  const source = pendingStreamBody;
  pendingStreamBody = null;

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
  const owner = StreamScope.adopt();

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

  const entry = { reader, owner, closed: false };
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

globalThis.__deis_runtime_invoke = async function(requestPayload) {
  // A stream the previous invocation stashed and never began would otherwise
  // become this request's body. Ownership goes back to the invocation for the
  // same reason: what this handler opens is this handler's, and is swept when
  // it ends.
  pendingStreamBody = null;
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
      return { status: resp.status, headers: headerPairs(headers), stream: true };
    }

    let body = null;
    if (!webSocketId && resp.body) {
        body = new Uint8Array(await resp.arrayBuffer());
    }

    return respond(resp.status, headers, body);
  } catch (e) {
    const errorText = e && e.stack ? e.stack : String(e);
    console.error("Runtime invoke error:", errorText);
    return respond(500, {}, `Runtime Error: ${errorText}`);
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
  };

  const bridged = (level, method) =>
    function (...args) {
      forward(level, args);
      if (native && typeof native[method] === "function") {
        native[method].apply(native, args);
      }
    };

  globalThis.console = Object.assign(Object.create(native ?? null), native, {
    log: bridged("info", "log"),
    info: bridged("info", "info"),
    debug: bridged("debug", "debug"),
    warn: bridged("warn", "warn"),
    error: bridged("error", "error"),
    trace: bridged("debug", "trace"),
  });
}

globalThis.user_module = {}; globalThis.__deis_runtime_set_response = (json) => DenoCore.ops.op_set_response(json);

// The names the Rust side calls by, sealed in one place now that they all
// exist.
//
// These are not the application's surface, and an application that could put
// its own function under one of them would not merely be lying to itself.
// `__deis_runtime_invoke` is what runs an invocation *and what sweeps up
// after one*: it calls `__deis_runtime_sweep_intervals` and
// `__deis_runtime_sweep_fetch_budgets` at the end, which is where an
// application's `setInterval` callbacks and outbound-fetch deadlines stop.
// `js/bootstrap.js` seals those two, and that is not enough on its own while
// the function that calls them can be replaced with one that does not --
// timers would then go on firing on a runtime thread shared with every other
// application on it, long after the request that created them.
//
// `user_module` is deliberately not in this list: `runtime/isolate.rs` writes
// the application's exports there once the module has evaluated.
for (const name of [
  "__deis_runtime_invoke",
  "__deis_runtime_stream_begin",
  "__deis_runtime_stream_cancel",
  "__deis_runtime_invoke_queue",
  "__deis_runtime_invoke_scheduled",
  "__deis_runtime_wait_until_all",
  "__deis_runtime_set_response",
  "__deis_runtime_ws_servers",
]) {
  Object.defineProperty(globalThis, name, {
    value: globalThis[name],
    writable: false,
    enumerable: false,
    configurable: false,
  });
}

})();
