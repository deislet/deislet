const DenoCore = Deno.core; delete globalThis.Deno;
// Edge Canon Runtime (v0.2.0) for Runtime
// Native Deno Runtime extensions enabled.

// ---------------------------------------------------------
// 0. Encoding
// ---------------------------------------------------------
//
// TextEncoder, TextDecoder, btoa and atob, which SPECIFICATION_BASIC.md
// section 8.1 lists among the Web APIs every platform must provide. They are
// written out here rather than pulled from deno_core because this runtime
// boots the bare `deno_core` engine: nothing but `console` and
// `queueMicrotask` comes with it.
//
// They are not decoration. Several things below already reached for them and
// therefore threw ReferenceError instead of doing their job: reading a binary
// request body, `request.arrayBuffer()`, `blob.put(key, "a string")`,
// `(await blob.get(key)).text()` and a binary WebSocket frame.
//
// UTF-8 is the only encoding implemented, which is the only one the canon
// asks for; any other label is refused rather than silently decoded as UTF-8.

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
    constructor(label = "utf-8", options = {}) {
      const encoding = String(label).trim().toLowerCase();
      if (!UTF8_LABELS.has(encoding)) {
        throw new RangeError(
          "TextDecoder on this platform decodes UTF-8 only; '" + label + "' is not supported",
        );
      }
      this._fatal = !!options.fatal;
      this._ignoreBOM = !!options.ignoreBOM;
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

if (typeof globalThis.EventTarget === "undefined") {
  class EventTarget {
    constructor() {
      this.__listeners = new Map();
    }
    addEventListener(type, listener) {
      if (!listener) return;
      const listeners = this.__listeners.get(type) || new Set();
      listeners.add(listener);
      this.__listeners.set(type, listeners);
    }
    removeEventListener(type, listener) {
      const listeners = this.__listeners.get(type);
      if (!listeners) return;
      listeners.delete(listener);
      if (listeners.size === 0) this.__listeners.delete(type);
    }
    dispatchEvent(event) {
      if (!event || !event.type) return false;
      const listeners = this.__listeners.get(event.type);
      if (!listeners) return false;
      for (const listener of listeners) {
        listener.call(this, event);
      }
      return true;
    }
  }
  globalThis.EventTarget = EventTarget;
}

if (typeof globalThis.Headers === "undefined") {
  class Headers {
    constructor(init = {}) {
      this._map = new Map();
      if (init instanceof Headers) {
        init.forEach((value, key) => this._map.set(key.toLowerCase(), String(value)));
      } else if (Array.isArray(init)) {
        for (const [key, value] of init) {
          this._map.set(String(key).toLowerCase(), String(value));
        }
      } else if (init && typeof init === "object") {
        for (const key of Object.keys(init)) {
          this._map.set(String(key).toLowerCase(), String(init[key]));
        }
      }
    }
    set(key, value) {
      this._map.set(String(key).toLowerCase(), String(value));
    }
    get(key) {
      const value = this._map.get(String(key).toLowerCase());
      return value === undefined ? null : value;
    }
    forEach(callback) {
      for (const [key, value] of this._map.entries()) {
        callback(value, key, this);
      }
    }
    entries() {
      return this._map.entries();
    }
  }
  globalThis.Headers = Headers;
}

// Render whatever the runtime handed down as a body into text. It is a
// string today, but a clone or a handler-built Request can carry bytes.
function bodyToText(body) {
  if (body === null || body === undefined) return "";
  if (typeof body === "string") return body;
  if (body instanceof Uint8Array) return new TextDecoder().decode(body);
  if (body instanceof ArrayBuffer) return new TextDecoder().decode(new Uint8Array(body));
  if (ArrayBuffer.isView(body)) {
    return new TextDecoder().decode(new Uint8Array(body.buffer, body.byteOffset, body.byteLength));
  }
  return String(body);
}

if (typeof globalThis.Request === "undefined") {
  class Request {
    constructor(url, init = {}) {
      this.url = String(url);
      this.method = (init.method || "GET").toUpperCase();
      this.headers = init.headers instanceof Headers ? init.headers : new Headers(init.headers || {});
      this._body = init.body ?? null;
      this._bodyUsed = false;
    }
    // Reading the body. A handler that posts JSON has no other way in: the
    // runtime hands the body down as a string, and without these a call to
    // request.text() threw "not a function" and the whole request failed
    // with a 500 that named the polyfill rather than the app.
    //
    // `bodyUsed` follows the standard: a body may be read once, and a second
    // read is a bug in the handler worth surfacing rather than answering with
    // the same bytes twice.
    async text() {
      if (this._bodyUsed) {
        throw new TypeError("Request body has already been read");
      }
      this._bodyUsed = true;
      return bodyToText(this._body);
    }
    async json() {
      return JSON.parse(await this.text());
    }
    async arrayBuffer() {
      const text = await this.text();
      return new TextEncoder().encode(text).buffer;
    }
    get bodyUsed() {
      return this._bodyUsed;
    }
    get body() {
      return this._body;
    }
    clone() {
      return new Request(this.url, { method: this.method, headers: this.headers, body: this._body });
    }
  }
  globalThis.Request = Request;
}

if (typeof globalThis.Response === "undefined") {
  class Response {
    constructor(body = null, init = {}) {
      this.status = init.status || 200;
      this.headers = init.headers instanceof Headers ? init.headers : new Headers(init.headers || {});
      this._body = body;
      if (init && init.webSocket) {
        this.webSocket = init.webSocket;
      }
    }
    async text() {
      return bodyToText(this._body);
    }
    async json() {
      return JSON.parse(await this.text());
    }
    clone() {
      return new Response(this._body, { status: this.status, headers: this.headers });
    }
    get body() {
      return this._body;
    }
  }
  globalThis.Response = Response;
}

if (typeof globalThis.URLSearchParams === "undefined") {
  class URLSearchParams {
    constructor(query = "") {
      this._map = new Map();
      let raw = String(query);
      if (raw.startsWith("?")) raw = raw.slice(1);
      if (!raw) return;
      for (const part of raw.split("&")) {
        if (!part) continue;
        const [key, value = ""] = part.split("=");
        const decodedKey = decodeURIComponent(key.replace(/\+/g, " "));
        const decodedValue = decodeURIComponent(value.replace(/\+/g, " "));
        this._map.set(decodedKey, decodedValue);
      }
    }
    get(key) {
      return this._map.has(key) ? this._map.get(key) : null;
    }
  }
  globalThis.URLSearchParams = URLSearchParams;
}

if (typeof globalThis.URL === "undefined") {
  class URL {
    constructor(url) {
      const href = String(url);
      this.href = href;
      this.pathname = URL._extractPathname(href);
      this.search = URL._extractSearch(href);
      this.searchParams = new URLSearchParams(this.search);
    }
    toString() {
      return this.href;
    }
    static _extractPathname(href) {
      if (!href) return "/";
      let path = "/";
      const schemeIndex = href.indexOf("://");
      if (schemeIndex >= 0) {
        const pathIndex = href.indexOf("/", schemeIndex + 3);
        if (pathIndex >= 0) {
          path = href.slice(pathIndex);
        }
      } else if (href.startsWith("/")) {
        path = href;
      }
      const queryIndex = path.indexOf("?");
      if (queryIndex >= 0) path = path.slice(0, queryIndex);
      const hashIndex = path.indexOf("#");
      if (hashIndex >= 0) path = path.slice(0, hashIndex);
      return path || "/";
    }
    static _extractSearch(href) {
      if (!href) return "";
      const queryIndex = href.indexOf("?");
      if (queryIndex < 0) return "";
      const hashIndex = href.indexOf("#", queryIndex);
      if (hashIndex >= 0) return href.slice(queryIndex, hashIndex);
      return href.slice(queryIndex);
    }
  }
  globalThis.URL = URL;
}

if (typeof globalThis.setTimeout === "undefined") {
  globalThis.setTimeout = (callback, _ms, ...args) => {
    const id = Math.random();
    Promise.resolve().then(() => callback(...args));
    return id;
  };
  globalThis.clearTimeout = (_id) => {};
}

if (globalThis.Response && !globalThis.Response.__deis_runtime_patched) {
  const OriginalResponse = globalThis.Response;
  class ResponseWrapper extends OriginalResponse {
    constructor(body = null, init = {}) {
      super(body, init);
      if (init && init.webSocket) {
        this.webSocket = init.webSocket;
      }
    }
  }
  ResponseWrapper.__deis_runtime_patched = true;
  globalThis.Response = ResponseWrapper;
}

// BridgeWebSocket: A placeholder WebSocket for the client side of a WebSocketPair.
// It mainly serves to carry the ws_id back to the platform via Response.
class BridgeWebSocket extends EventTarget {
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

class WebSocketServer extends EventTarget {
  constructor(ws_id) {
    super();
    this.ws_id = ws_id;
    this.readyState = 0; // CONNECTING
    this._accepted = false;
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
      const event = { type: "open" };
      if (this.onopen) this.onopen(event);
      this.dispatchEvent(event);
    } else {
      if (this.onerror) this.onerror({ type: "error", error: result.error });
    }
  }

  // Called by Rust when message received from client
  _handleMessage(message) {
    let data = message;
    try {
      const json = JSON.parse(message);
      if (json.type === "binary" && json.data) {
        const binaryString = atob(json.data);
        const bytes = new Uint8Array(binaryString.length);
        for (let i = 0; i < binaryString.length; i++) {
          bytes[i] = binaryString.charCodeAt(i);
        }
        data = bytes.buffer;
      }
    } catch (e) {}

    const event = { type: "message", data };
    if (this.onmessage) this.onmessage(event);
    this.dispatchEvent(event);
  }

  // Called by Rust when connection closed
  _handleClose(code, reason) {
    this.readyState = 3; // CLOSED
    const event = { type: "close", code, reason };
    if (this.onclose) this.onclose(event);
    this.dispatchEvent(event);
  }

  async send(data) {
    if (this.readyState !== 1) throw new Error("WebSocket is not open");
    const message = typeof data === "string" ? data : JSON.stringify(data);
    const result = await DenoCore.ops.op_ws_server_send(this.ws_id, message);
    if (!result.success) throw new Error(`Failed to send: ${result.error}`);
  }

  async close(code = 1000, reason = "") {
    this.readyState = 2; // CLOSING
    const request = { ws_id: this.ws_id, code, reason };
    await DenoCore.ops.op_ws_server_close(JSON.stringify(request));
    this.readyState = 3; // CLOSED
    if (this.onclose) this.onclose({ type: "close", code, reason });
  }
}

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
// request's URL, read it back, drop it. Reached two ways, both of them this
// same store: `context.services.cache`, which is what the canon specifies, and
// `globalThis.caches.default`, which is what a handler ported from Cloudflare
// reaches for.
//
// Scope: one store per isolate, so an entry is visible to later requests for
// the same application on the same node and to nothing else - not another
// node, not the replacement isolate a new version is loaded into. A hit is an
// optimisation; nothing may depend on one.

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
    const body = await response.clone().text();
    const meta = {
      status: response.status,
      headers: Object.fromEntries(response.headers.entries()),
      body: body
    };
    await DenoCore.ops.op_cache_set(key, JSON.stringify(meta), freshnessSeconds(response));
  }
  async match(request) {
    const key = cacheKeyOf(request);
    const val = await DenoCore.ops.op_cache_get(key);
    if (!val) return undefined;
    try {
      const meta = JSON.parse(val);
      return new Response(meta.body, {
        status: meta.status,
        headers: meta.headers
      });
    } catch {
      return undefined;
    }
  }
  async delete(request) {
    return await DenoCore.ops.op_cache_delete(cacheKeyOf(request));
  }
}

// One instance, reached under both names, so an application that writes
// through `caches.default` and reads through `context.services.cache` sees
// its own entry.
const defaultCache = new Cache();

globalThis.caches = {
  default: defaultCache
};

class NativeKV {
  async get(key) {
    return await DenoCore.ops.op_kv_get(key);
  }
  // getWithMetadata(key) -> { value, metadata, expiration } | null
  //
  // null means the key does not exist or has already lapsed. A key that was
  // written without metadata comes back as a record whose metadata is null:
  // "no such key" and "no metadata on this key" are different answers.
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
  async getWithMetadata(key) {
    const read = await DenoCore.ops.op_kv_get_with_metadata(key);
    if (!read) return null;
    let metadata = null;
    if (read.metadata !== undefined && read.metadata !== null) {
      try {
        metadata = JSON.parse(read.metadata);
      } catch {
        metadata = read.metadata;
      }
    }
    return {
      value: read.value,
      metadata: metadata,
      expiration: read.expiration === undefined ? null : read.expiration
    };
  }
  // put(key, value, { expirationTtl, metadata })
  //
  // expirationTtl is a lifetime in whole seconds, counted from the moment
  // deis-store accepts the write; leave it out and the entry never lapses.
  //
  // metadata is stored alongside the value and returned untouched by
  // deis-store; getWithMetadata() reads it back. It is JSON-encoded here,
  // strings included, so that decoding it there is a plain JSON.parse and an
  // object, a number and a string all round-trip as themselves.
  async put(key, value, options) {
    const opts = options || {};
    let ttl = null;
    if (opts.expirationTtl !== undefined && opts.expirationTtl !== null) {
      ttl = Number(opts.expirationTtl);
      if (!Number.isFinite(ttl) || Math.floor(ttl) !== ttl || ttl <= 0) {
        throw new TypeError(
          "kv.put: expirationTtl must be a whole number of seconds greater than 0, got " +
            String(opts.expirationTtl)
        );
      }
    }

    let metadata = null;
    if (opts.metadata !== undefined && opts.metadata !== null) {
      metadata = JSON.stringify(opts.metadata);
    }

    await DenoCore.ops.op_kv_set(key, value, { ttlSeconds: ttl, metadata: metadata });
  }
  async delete(key) {
    await DenoCore.ops.op_kv_delete(key);
  }
}

class NativeDB {
  // The database ops report failure as an `error` field rather than by
  // throwing, so a caller that did not look for it would see `undefined`
  // rows and no reason why. Turn it into an exception, which is what the
  // other services do and what a handler's try/catch expects.
  #check(result, call) {
    if (result && result.error) {
      throw new Error("database." + call + " failed: " + result.error);
    }
    return result;
  }
  async execute(sql, params = []) {
    return this.#check(
      await DenoCore.ops.op_db_execute(sql, JSON.stringify(params)),
      "execute"
    );
  }
  async query(sql, params = []) {
    const res = this.#check(
      await DenoCore.ops.op_db_query(sql, JSON.stringify(params)),
      "query"
    );
    return res.rows;
  }
  // Edge Canon's Database interface names a transaction, and this platform
  // has no session-scoped one: deis-store serves each call on whichever
  // pooled connection is free, so there is no place to hold an open BEGIN
  // across the awaits inside a callback.
  //
  // It used to accept the callback, never call it, and resolve. An
  // application whose two writes were "in a transaction" therefore performed
  // neither of them and was told nothing - the worst of the three possible
  // answers. This one is the least bad: the work does not happen, and the
  // caller finds out on the spot.
  async transaction(_callback) {
    throw new Error(
      "database.transaction is not available on this platform: deis-store has no " +
        "session-scoped transaction. Run the statements with database.execute(), or " +
        "write them so each one is safe on its own."
    );
  }
}

class BlobStore {
  async get(key) {
    const data = await DenoCore.ops.op_blob_get(key);
    if (!data) return null;
    return {
      arrayBuffer: async () => data.buffer,
      text: async () => new TextDecoder().decode(data),
      json: async () => JSON.parse(new TextDecoder().decode(data))
    };
  }
  async put(key, data) {
    let buffer;
    if (typeof data === 'string') {
      buffer = new TextEncoder().encode(data);
    } else if (data instanceof ArrayBuffer) {
      buffer = new Uint8Array(data);
    } else if (ArrayBuffer.isView(data)) {
      buffer = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    } else {
      throw new Error("Invalid data type for BlobStore.put");
    }
    await DenoCore.ops.op_blob_put(key, buffer);
  }
  async delete(key) {
    await DenoCore.ops.op_blob_delete(key);
  }
}

class Queue {
  constructor(name = "default") {
    this.name = name;
  }
  async send(message) {
    const msgStr = typeof message === "string" ? message : JSON.stringify(message);
    await DenoCore.ops.op_queue_send(this.name, msgStr);
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
  return {
    env: DenoCore.ops.op_get_env() || {},
    services: {
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
function createContext(requestData) {
  return Object.assign(createServiceContext(), {
    request: new Request(requestData.url, {
      method: requestData.method,
      headers: requestData.headers,
      body: requestData.body
    }),
    params: requestData.params || {},
  });
}

globalThis.__deis_runtime_wait_until_all = async function() {
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
    }
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
// attempts and timestamp, plus json() for a JSON body. `body` is the message
// exactly as it was sent - text, not a parsed object - and `json()` is the
// parse. Delivery is at-least-once, so a handler MUST be idempotent:
// `attempts` above 1 means this message has been delivered before.
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
    messages: messages.map((m) => ({
      id: m.id,
      body: m.body,
      attempts: m.attempts,
      timestamp: m.timestamp,
      json() { return JSON.parse(m.body); },
      retry() { failed.add(m.id); },
      ack() { failed.delete(m.id); },
    })),
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

globalThis.__deis_runtime_invoke = async function(requestPayload) {
  try {
    let handler;
    if (globalThis.user_module && typeof globalThis.user_module.default === 'function') {
        handler = globalThis.user_module.default;
    }
    
    if (!handler) {
      return { status: 500, headers: {}, body: "No default export handler found (Edge Canon requires 'export default function(context)')" };
    }

    const ctx = createContext(requestPayload);
    
    let resp;
    resp = await handler(ctx);
    
    if (!(resp instanceof Response)) {
       return { status: 500, headers: {}, body: "Handler did not return a Response object" };
    }

    // Prepare response
    const headers = {};
    resp.headers.forEach((v, k) => headers[k] = v);
    
    // Check for WebSocket upgrade
    // Check if user attached webSocket to the response object dynamically
    let webSocketId = undefined;
    
    if (resp.webSocket && resp.webSocket.ws_id) {
        webSocketId = resp.webSocket.ws_id;
        headers["X-Runtime-WebSocket-ID"] = webSocketId;
    }

    // Read body
    let body = null;
    if (!webSocketId && resp.body) {
        body = await resp.text();
    }

    return {
      status: resp.status,
      headers: headers,
      body: body
    };
  } catch (e) {
    const errorText = e && e.stack ? e.stack : String(e);
    console.error("Runtime invoke error:", errorText);
    return {
      status: 500,
      headers: {},
      body: `Runtime Error: ${errorText}`
    };
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
