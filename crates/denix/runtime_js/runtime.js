(function() { const DenoCore = Deno.core; delete globalThis.Deno;
// Edge Canon Runtime (v0.2.0) for Denix
// Native Deno Runtime extensions enabled.

// ---------------------------------------------------------
// 1. WebSocket Support (Custom Bridge)
// ---------------------------------------------------------

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

if (!globalThis.__denix_ws_servers) {
  globalThis.__denix_ws_servers = {};
}

class WebSocketServer {
  constructor(ws_id) {
    this.ws_id = ws_id;
    this.readyState = 0; // CONNECTING
    this._accepted = false;
    // Register global reference for Rust ops to find
    globalThis.__denix_ws_servers[ws_id] = this;
  }

  // Called by user to accept the connection
  accept() {
    if (this._accepted) throw new Error("WebSocket already accepted");
    this._accepted = true;
    this.readyState = 1; // OPEN
    const result = DenoCore.ops.op_ws_server_accept(this.ws_id);
    if (result.success) {
      if (this.onopen) this.onopen({ type: "open" });
    } else {
      if (this.onerror) this.onerror({ type: "error", error: result.error });
    }
  }

  // Called by Rust when message received from client
  _handleMessage(message) {
    if (this.onmessage) {
        let data = message;
        // Check if binary
        try {
            const json = JSON.parse(message);
            if (json.type === "binary" && json.data) {
                // Decode base64
                const binaryString = atob(json.data);
                const bytes = new Uint8Array(binaryString.length);
                for (let i = 0; i < binaryString.length; i++) {
                    bytes[i] = binaryString.charCodeAt(i);
                }
                data = bytes.buffer; // Return ArrayBuffer
            }
        } catch (e) {}
        
        this.onmessage({ type: "message", data });
    }
  }

  // Called by Rust when connection closed
  _handleClose(code, reason) {
    this.readyState = 3; // CLOSED
    if (this.onclose) this.onclose({ type: "close", code, reason });
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

globalThis.__denix_wait_until_all = async function() {
  if (globalThis.__pending_wait_until && globalThis.__pending_wait_until.length > 0) {
    const promises = globalThis.__pending_wait_until;
    globalThis.__pending_wait_until = [];
    try {
        await Promise.all(promises);
    } catch (e) {
        console.error("Error in waitUntil promise:", e);
    }
    if (globalThis.__pending_wait_until && globalThis.__pending_wait_until.length > 0) {
      await globalThis.__denix_wait_until_all();
    }
  }
};

globalThis.__denix_invoke = async function(requestPayload) {
  try {
    let handler;
    // Support export default { fetch }
    if (globalThis.user_module && globalThis.user_module.default) {
      if (typeof globalThis.user_module.default.fetch === 'function') {
        handler = globalThis.user_module.default.fetch;
      }
    }
    
    if (!handler) {
      return { status: 500, headers: {}, body: "No fetch handler found in user module" };
    }

    const ctx = createContext(requestPayload);
    
    // Execute handler
    // Cloudflare style: fetch(request, env, ctx)
    const resp = await handler(ctx.request, ctx.env, ctx);
    
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
        headers["X-Denix-WebSocket-ID"] = webSocketId;
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
    console.error("Runtime invoke error:", e);
    return {
      status: 500,
      headers: {},
      body: `Runtime Error: ${e.message}`
    };
  }
};

globalThis.user_module = {}; globalThis.__denix_set_response = (json) => DenoCore.ops.op_set_response(json); 
})();