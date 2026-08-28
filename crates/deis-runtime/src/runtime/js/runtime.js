const DenoCore = Deno.core; delete globalThis.Deno;
// Edge Canon Runtime (v0.2.0) for Runtime
// Native Deno Runtime extensions enabled.

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

if (typeof globalThis.Request === "undefined") {
  class Request {
    constructor(url, init = {}) {
      this.url = String(url);
      this.method = (init.method || "GET").toUpperCase();
      this.headers = init.headers instanceof Headers ? init.headers : new Headers(init.headers || {});
      this._body = init.body ?? null;
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
      return this._body == null ? "" : String(this._body);
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

class Cache {
  async put(request, response) {
    const url = typeof request === 'string' ? request : request.url;
    // Clone response to avoid consuming the body if reused
    const body = await response.clone().text(); 
    const meta = {
      status: response.status,
      headers: Object.fromEntries(response.headers.entries()),
      body: body
    };
    await DenoCore.ops.op_cache_set(url, JSON.stringify(meta));
  }
  async match(request) {
    const url = typeof request === 'string' ? request : request.url;
    const val = await DenoCore.ops.op_cache_get(url);
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
    const url = typeof request === 'string' ? request : request.url;
    await DenoCore.ops.op_cache_delete(url);
    return true; 
  }
}

globalThis.caches = {
  default: new Cache()
};

class NativeKV {
  async get(key) {
    return await DenoCore.ops.op_kv_get(key);
  }
  async put(key, value) {
    await DenoCore.ops.op_kv_set(key, value); 
  }
  async delete(key) {
    await DenoCore.ops.op_kv_delete(key);
  }
}

class NativeDB {
  async execute(sql, params = []) {
    return await DenoCore.ops.op_db_execute(sql, JSON.stringify(params));
  }
  async query(sql, params = []) {
    const res = await DenoCore.ops.op_db_query(sql, JSON.stringify(params));
    return res.rows; 
  }
  async transaction(callback) {
    // Not implemented in op layer yet
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

function createContext(requestData) {
  return {
    request: new Request(requestData.url, {
      method: requestData.method,
      headers: requestData.headers,
      body: requestData.body
    }),
    env: DenoCore.ops.op_get_env() || {}, 
    params: requestData.params || {},      
    services: {
      kv: new NativeKV(),
      database: new NativeDB(),
      blob: new BlobStore(),
      queue: new Queue("default"),
    },
    waitUntil: (promise) => {
      if (!globalThis.__pending_wait_until) globalThis.__pending_wait_until = [];
      globalThis.__pending_wait_until.push(promise);
    }
  };
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

globalThis.user_module = {}; globalThis.__deis_runtime_set_response = (json) => DenoCore.ops.op_set_response(json);
