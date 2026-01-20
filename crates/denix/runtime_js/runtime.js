// Edge Canon Runtime (v0.1.0) for Denix
// Compliance Level: Strict

// ---------------------------------------------------------
// 1. Core Classes & Polyfills
// ---------------------------------------------------------

// Console
globalThis.console = {
  log: (...args) => Deno.core.ops.op_log("info", args.join(" ")),
  info: (...args) => Deno.core.ops.op_log("info", args.join(" ")),
  warn: (...args) => Deno.core.ops.op_log("warn", args.join(" ")),
  error: (...args) => Deno.core.ops.op_log("error", args.join(" ")),
  debug: (...args) => Deno.core.ops.op_log("debug", args.join(" ")),
};

// Headers Class (Minimal)
class Headers {
  constructor(init = {}) {
    this._map = new Map();
    if (init instanceof Headers) {
      init.forEach((v, k) => this._map.set(k, v));
    } else if (Array.isArray(init)) {
      init.forEach(([k, v]) => this._map.set(k.toLowerCase(), v));
    } else {
      for (const [k, v] of Object.entries(init)) {
        this._map.set(k.toLowerCase(), v);
      }
    }
  }
  get(name) { return this._map.get(name.toLowerCase()) || null; }
  set(name, value) { this._map.set(name.toLowerCase(), value); }
  has(name) { return this._map.has(name.toLowerCase()); }
  forEach(callback) { this._map.forEach((v, k) => callback(v, k)); }
}
globalThis.Headers = Headers;

// Request Class
class Request {
  constructor(input, init = {}) {
    if (typeof input === 'string') {
      this.url = input;
    } else {
      this.url = input.url;
      this.method = input.method;
      this.headers = new Headers(input.headers);
      this.body = input.body;
    }
    this.method = init.method || this.method || 'GET';
    this.headers = new Headers(init.headers || this.headers || {});
    this.body = init.body || this.body;
  }
  async text() { return this.body || ""; }
  async json() { return JSON.parse(this.body || "{}"); }
}

// Response Class
class Response {
  constructor(body, init = {}) {
    this.body = typeof body === 'object' ? JSON.stringify(body) : body;
    this.status = init.status || 200;
    // Normalize headers to object for Rust serde compatibility
    this.headers = {};
    if (init.headers) {
        if (init.headers instanceof Headers) {
            init.headers.forEach((v, k) => this.headers[k] = v);
        } else {
            Object.assign(this.headers, init.headers);
        }
    }
    
    // WebSocket Upgrade Support
    if (init.webSocket) {
      this.status = 101;
      this.headers["Upgrade"] = "websocket";
      if (init.webSocket.ws_id) {
        this.headers["X-Denix-WebSocket-ID"] = init.webSocket.ws_id;
      }
    }
  }
}

globalThis.Request = Request;
globalThis.Response = Response;

// URL Class (Regex Fallback Only)
class URL {
  constructor(url, base) {
    let fullUrl = url;
    if (base && !url.match(/^[a-z]+:\/\//)) {
       fullUrl = base.replace(/\/+$/, '') + '/' + url.replace(/^\/+/, '');
    }

    const match = fullUrl.match(/^(https?:)\/\/([^/:]+)(:(\d+))?([^?#]*)(\?[^#]*)?(#.*)?$/);
    if (match) {
      this.href = fullUrl;
      this.protocol = match[1];
      this.hostname = match[2];
      this.port = match[4] || "";
      this.pathname = match[5] || "/";
      this.search = match[6] || "";
      this.hash = match[7] || "";
      this.searchParams = new URLSearchParams(this.search);
    } else {
      this.href = fullUrl;
      this.pathname = fullUrl;
      this.search = "";
      this.searchParams = new URLSearchParams("");
    }
  }
}

// URLSearchParams Polyfill
if (!globalThis.URLSearchParams) {
  globalThis.URLSearchParams = class {
    constructor(search) {
      this.params = new Map();
      if (search && search.startsWith('?')) {
        search.slice(1).split('&').forEach(pair => {
          const [k, v] = pair.split('=');
          if (k) this.params.set(decodeURIComponent(k), decodeURIComponent(v || ''));
        });
      }
    }
    get(name) { return this.params.get(name); }
  };
}

globalThis.URL = URL;

// ---------------------------------------------------------
// 2. WebSocket Support
// ---------------------------------------------------------

if (!globalThis.__denix_ws_servers) {
  globalThis.__denix_ws_servers = {};
}

class WebSocketServer {
  constructor(ws_id) {
    this.ws_id = ws_id || `wss-${Date.now()}-${Math.random().toString(36).slice(2)}`;
    this.readyState = 0; // CONNECTING
    this.onopen = null;
    this.onmessage = null;
    this.onerror = null;
    this.onclose = null;
    this._accepted = false;

    globalThis.__denix_ws_servers[this.ws_id] = this;
  }

  accept() {
    if (this._accepted) throw new Error("WebSocket already accepted");
    this._accepted = true;
    this.readyState = 1; // OPEN
    const result = Deno.core.ops.op_ws_server_accept(this.ws_id);
    if (result.success) {
      if (this.onopen) this.onopen({ type: "open" });
    } else {
      this.readyState = 3;
      if (this.onerror) this.onerror({ type: "error", error: result.error });
    }
  }

  async send(data) {
    if (this.readyState !== 1) throw new Error("WebSocket is not open");
    const message = typeof data === "string" ? data : JSON.stringify(data);
    const result = await Deno.core.ops.op_ws_server_send(this.ws_id, message);
    if (!result.success) throw new Error(`Failed to send: ${result.error}`);
  }

  async close(code = 1000, reason = "") {
    this.readyState = 2; // CLOSING
    const request = { ws_id: this.ws_id, code, reason };
    await Deno.core.ops.op_ws_server_close(JSON.stringify(request));
    this.readyState = 3; // CLOSED
    if (this.onclose) this.onclose({ type: "close", code, reason });
  }

  _handleMessage(data) {
    if (this.onmessage) this.onmessage({ type: "message", data });
  }

  _handleClose(code, reason) {
    this.readyState = 3;
    if (this.onclose) this.onclose({ type: "close", code, reason });
  }

  addEventListener(type, listener) {
    if (type === 'message') {
      this.onmessage = (event) => listener(event);
    } else if (type === 'close') {
      this.onclose = (event) => listener(event);
    } else if (type === 'error') {
      this.onerror = (event) => listener(event);
    } else if (type === 'open') {
      this.onopen = (event) => listener(event);
    }
  }
}

class WebSocketPair {
  constructor() {
    const ws_id = `wsp-${Date.now()}-${Math.random().toString(36).slice(2)}`;
    this.server = new WebSocketServer(`${ws_id}-server`);
    this.client = { ws_id: `${ws_id}-server` };
    return { 0: this.client, 1: this.server }; 
  }
}

globalThis.WebSocketServer = WebSocketServer;
globalThis.WebSocketPair = WebSocketPair;

// ---------------------------------------------------------
// 3. Service Adapters (The Bridge)
// ---------------------------------------------------------

class NativeKV {
  async get(key) {
    return await Deno.core.ops.op_kv_get(key);
  }
  async put(key, value) {
    await Deno.core.ops.op_kv_set(key, value); 
  }
  async delete(key) {
    await Deno.core.ops.op_kv_delete(key);
  }
}

class NativeDB {
  async execute(sql, params = []) {
    return await Deno.core.ops.op_db_execute(sql, JSON.stringify(params));
  }
  async query(sql, params = []) {
    const res = await Deno.core.ops.op_db_query(sql, JSON.stringify(params));
    return res.rows; 
  }
  async transaction(callback) {
    return await callback(this); 
  }
}

// ---------------------------------------------------------
// 4. The Context & Invocation Logic
// ---------------------------------------------------------

function createContext(requestData) {
  return {
    request: new Request(requestData.url, {
      method: requestData.method,
      headers: requestData.headers,
      body: requestData.body
    }),
    env: Deno.core.ops.op_get_env() || {}, 
    params: requestData.params || {},      
    services: {
      kv: new NativeKV(),
      database: new NativeDB(),
    },
    log: {
      info: (msg) => console.info(typeof msg === 'string' ? msg : JSON.stringify(msg)),
      error: (msg) => console.error(typeof msg === 'string' ? msg : JSON.stringify(msg)),
      warn: (msg) => console.warn(typeof msg === 'string' ? msg : JSON.stringify(msg)),
      debug: (msg) => console.debug(typeof msg === 'string' ? msg : JSON.stringify(msg)),
    },
    waitUntil: (promise) => {
      if (!globalThis.__pending_wait_until) globalThis.__pending_wait_until = [];
      globalThis.__pending_wait_until.push(promise);
    }
  };
}

globalThis.__denix_invoke = async function(requestPayload) {
  try {
    let handler;
    const userMod = globalThis.user_module || {};
    
    // Priority 1: globalThis.user_module.default
    if (typeof userMod.default === 'function') {
      handler = userMod.default;
    } 
    // Priority 2: globalThis.user_module.default.fetch
    else if (userMod.default?.fetch) {
      handler = (ctx) => userMod.default.fetch(ctx.request, ctx.env, ctx);
    }
    // Priority 3: globalThis.user_module.onRequest
    else if (typeof userMod.onRequest === 'function') {
      handler = userMod.onRequest;
    }
    // Legacy Fallback: globalThis.default
    else if (globalThis.default?.fetch) {
       handler = (ctx) => globalThis.default.fetch(ctx.request, ctx.env, ctx);
    }
    
    if (!handler) {
      throw new Error("No valid handler found. Export 'default function handler(ctx)' or 'onRequest'.");
    }

    const context = createContext(requestPayload);
    const response = await handler(context);

    return {
      status: response.status || 200,
      headers: response.headers || {},
      body: response.body || ""
    };

  } catch (e) {
    console.error("Runtime Error:", e);
    return {
      status: 500,
      headers: {},
      body: JSON.stringify({ error: { code: "INTERNAL_ERROR", message: e.message } })
    };
  }
};

globalThis.user_module = {}; 
