// Denix Runtime JavaScript Polyfills
// 这段代码运行在每个应用的 V8 isolate 中

// 全局 Deno API
globalThis.Deno = {
  core: Deno.core,
};

// KV API 封装
globalThis.kv = {
  get(key) {
    return Deno.core.ops.op_kv_get(key);
  },
  set(key, value) {
    Deno.core.ops.op_kv_set(key, value);
  },
  delete(key) {
    Deno.core.ops.op_kv_delete(key);
  },
  list(prefix = "") {
    return Deno.core.ops.op_kv_list(prefix);
  },
};

// Cache API 封装（高性能本地缓存）
globalThis.cache = {
  /**
   * 从缓存获取值
   * @param {string} key - 缓存键
   * @returns {Promise<string|null>} 缓存值或 null
   */
  async get(key) {
    return await Deno.core.ops.op_cache_get(key);
  },

  /**
   * 设置缓存值
   * @param {string} key - 缓存键
   * @param {string} value - 缓存值
   * @param {number} ttl - 过期时间（秒），0 表示使用默认 TTL
   */
  async set(key, value, ttl = 0) {
    await Deno.core.ops.op_cache_set(key, value, ttl);
  },

  /**
   * 删除缓存项
   * @param {string} key - 缓存键
   */
  async delete(key) {
    await Deno.core.ops.op_cache_delete(key);
  },

  /**
   * 检查缓存键是否存在
   * @param {string} key - 缓存键
   * @returns {Promise<boolean>} 是否存在
   */
  async has(key) {
    return await Deno.core.ops.op_cache_has(key);
  },

  /**
   * 清空所有缓存
   */
  async clear() {
    await Deno.core.ops.op_cache_clear();
  },

  /**
   * 获取缓存统计信息
   * @returns {Promise<{entry_count: number, weighted_size: number}>}
   */
  async stats() {
    return await Deno.core.ops.op_cache_stats();
  },
};

// Database API 封装（SQL 数据库）
globalThis.db = {
  /**
   * 执行 SQL 语句（INSERT, UPDATE, DELETE 等）
   * @param {string} sql - SQL 语句
   * @param {Array} params - 可选的参数数组
   * @returns {Promise<{rows_affected: number, last_insert_id?: number}>}
   */
  async execute(sql, params = null) {
    const paramsJson = params ? JSON.stringify(params) : null;
    return await Deno.core.ops.op_db_execute(sql, paramsJson);
  },

  /**
   * 查询 SQL（SELECT）
   * @param {string} sql - SQL 查询语句
   * @param {Array} params - 可选的参数数组
   * @returns {Promise<{rows: Array<Object>, columns: Array<string>}>}
   */
  async query(sql, params = null) {
    const paramsJson = params ? JSON.stringify(params) : null;
    return await Deno.core.ops.op_db_query(sql, paramsJson);
  },

  /**
   * 批量执行多条 SQL
   * @param {Array<{sql: string, params?: Array}>} statements - SQL 语句数组
   * @returns {Promise<Array<{rows_affected: number, last_insert_id?: number}>>}
   */
  async batch(statements) {
    const statementsJson = JSON.stringify(statements);
    return await Deno.core.ops.op_db_batch(statementsJson);
  },

  /**
   * 预处理 SQL 语句
   * @param {string} sql - SQL 语句
   * @returns {Promise<string>} 预处理语句 ID
   */
  async prepare(sql) {
    return await Deno.core.ops.op_db_prepare(sql);
  },

  /**
   * 执行预处理语句
   * @param {string} statementId - 预处理语句 ID
   * @param {Array} params - 可选的参数数组
   * @returns {Promise<{rows_affected: number, last_insert_id?: number}>}
   */
  async executePrepared(statementId, params = null) {
    const paramsJson = params ? JSON.stringify(params) : null;
    return await Deno.core.ops.op_db_execute_prepared(statementId, paramsJson);
  },

  /**
   * 查询预处理语句
   * @param {string} statementId - 预处理语句 ID
   * @param {Array} params - 可选的参数数组
   * @returns {Promise<{rows: Array<Object>, columns: Array<string>}>}
   */
  async queryPrepared(statementId, params = null) {
    const paramsJson = params ? JSON.stringify(params) : null;
    return await Deno.core.ops.op_db_query_prepared(statementId, paramsJson);
  },
};

// Fetch API 封装（HTTP 客户端）
/**
 * 发送 HTTP 请求到外部服务
 * @param {string|Request} input - URL 字符串或 Request 对象
 * @param {Object} init - 请求配置（可选）
 * @returns {Promise<Response>} HTTP 响应对象
 */
globalThis.fetch = async function (input, init = {}) {
  let url, method, headers, body;

  // 解析参数
  if (typeof input === "string") {
    url = input;
    method = init.method || "GET";
    headers = init.headers || {};
    body = init.body || null;
  } else if (input instanceof Request) {
    url = input.url;
    method = input.method;
    headers = input.headers;
    body = input.body;
  } else {
    throw new Error("fetch: invalid input type");
  }

  // 构建请求对象
  const request = {
    url,
    method: method.toUpperCase(),
    headers: headers || {},
    body,
    timeout: init.timeout || null,
  };

  // 调用底层 op
  const result = await Deno.core.ops.op_fetch(JSON.stringify(request));

  // 检查错误
  if (result.error) {
    throw new Error(`fetch failed: ${result.error}`);
  }

  // 创建 Response 对象
  return new Response(result.body, {
    status: result.status,
    statusText: result.statusText,
    headers: result.headers,
  });
};

// WebSocket 类（客户端）
class WebSocket {
  constructor(url, protocols = null) {
    this.url = url;
    this.protocols = protocols;
    this.readyState = 0; // CONNECTING
    this.ws_id = `ws-${Date.now()}-${Math.random().toString(36).slice(2)}`;
    this.onopen = null;
    this.onmessage = null;
    this.onerror = null;
    this.onclose = null;

    // 异步连接
    this._connect();
  }

  async _connect() {
    try {
      const request = {
        ws_id: this.ws_id,
        url: this.url,
        protocols: this.protocols
      };

      const result = await Deno.core.ops.op_ws_connect(JSON.stringify(request));

      if (result.success) {
        this.readyState = 1; // OPEN
        if (this.onopen) {
          this.onopen({ type: "open" });
        }
      } else {
        this.readyState = 3; // CLOSED
        if (this.onerror) {
          this.onerror({ type: "error", error: result.error });
        }
      }
    } catch (error) {
      this.readyState = 3; // CLOSED
      if (this.onerror) {
        this.onerror({ type: "error", error: error.message });
      }
    }
  }

  async send(data) {
    if (this.readyState !== 1) {
      throw new Error("WebSocket is not open");
    }

    const message = typeof data === "string" ? data : JSON.stringify(data);
    const result = await Deno.core.ops.op_ws_send(this.ws_id, message);

    if (!result.success) {
      throw new Error(`Failed to send message: ${result.error}`);
    }
  }

  async close(code = 1000, reason = "") {
    if (this.readyState === 2 || this.readyState === 3) {
      return; // Already closing or closed
    }

    this.readyState = 2; // CLOSING

    const result = await Deno.core.ops.op_ws_close(this.ws_id);

    this.readyState = 3; // CLOSED

    if (this.onclose) {
      this.onclose({ type: "close", code, reason });
    }
  }
}

// WebSocket 常量
WebSocket.CONNECTING = 0;
WebSocket.OPEN = 1;
WebSocket.CLOSING = 2;
WebSocket.CLOSED = 3;

globalThis.WebSocket = WebSocket;

// WebSocket Server 全局注册表
if (!globalThis.__denix_ws_servers) {
  globalThis.__denix_ws_servers = {};
}

// WebSocket Server 类（服务端）
class WebSocketServer {
  constructor(ws_id) {
    this.ws_id = ws_id || `wss-${Date.now()}-${Math.random().toString(36).slice(2)}`;
    this.readyState = 0; // CONNECTING
    this.onopen = null;
    this.onmessage = null;
    this.onerror = null;
    this.onclose = null;
    this._accepted = false;

    // 注册到全局表
    globalThis.__denix_ws_servers[this.ws_id] = this;
  }

  accept() {
    if (this._accepted) {
      throw new Error("WebSocket already accepted");
    }

    this._accepted = true;
    this.readyState = 1; // OPEN

    const result = Deno.core.ops.op_ws_server_accept(this.ws_id);

    if (result.success) {
      if (this.onopen) {
        this.onopen({ type: "open" });
      }
    } else {
      this.readyState = 3; // CLOSED
      if (this.onerror) {
        this.onerror({ type: "error", error: result.error });
      }
    }
  }

  async send(data) {
    if (this.readyState !== 1) {
      throw new Error("WebSocket is not open");
    }

    const message = typeof data === "string" ? data : JSON.stringify(data);
    const result = await Deno.core.ops.op_ws_server_send(this.ws_id, message);

    if (!result.success) {
      throw new Error(`Failed to send message: ${result.error}`);
    }
  }

  async close(code = 1000, reason = "") {
    if (this.readyState === 2 || this.readyState === 3) {
      return; // Already closing or closed
    }

    this.readyState = 2; // CLOSING

    const request = {
      ws_id: this.ws_id,
      code: code || 1000,
      reason: reason || ""
    };
    await Deno.core.ops.op_ws_server_close(JSON.stringify(request));

    this.readyState = 3; // CLOSED

    if (this.onclose) {
      this.onclose({ type: "close", code, reason });
    }
  }

  // 内部方法：接收消息（由 gateway 调用）
  _handleMessage(data) {
    if (this.onmessage) {
      this.onmessage({ type: "message", data });
    }
  }

  // 内部方法：处理关闭（由 gateway 调用）
  _handleClose(code, reason) {
    this.readyState = 3; // CLOSED
    if (this.onclose) {
      this.onclose({ type: "close", code, reason });
    }
  }
}

// WebSocket Server 常量
WebSocketServer.CONNECTING = 0;
WebSocketServer.OPEN = 1;
WebSocketServer.CLOSING = 2;
WebSocketServer.CLOSED = 3;

globalThis.WebSocketServer = WebSocketServer;

// WebSocket Pair（用于创建一对连接的 WebSocket）
class WebSocketPair {
  constructor() {
    const ws_id = `wsp-${Date.now()}-${Math.random().toString(36).slice(2)}`;

    // 创建服务端 WebSocket
    this.server = new WebSocketServer(`${ws_id}-server`);

    // 客户端由 gateway 处理，这里只保存 ID 用于标识
    // 创建一个包含 ws_id 的对象，用于 Response 集成
    this.client = {
      ws_id: `${ws_id}-server`, // 使用 server 的 ID，Gateway 会通过这个 ID 找到对应的 WebSocketServer
      toString() {
        return this.ws_id;
      }
    };

    // 返回一个包含客户端和服务端的对象
    // 索引访问方式（Cloudflare Workers 兼容）
    return {
      0: this.server,   // 服务端（JavaScript 可访问）
      1: this.client,   // 客户端对象（包含 ws_id）
      server: this.server,
      client: this.client
    };
  }
}

globalThis.WebSocketPair = WebSocketPair;

// Console API
globalThis.console = {
  log(...args) {
    Deno.core.ops.op_log("info", args.map(String).join(" "));
  },
  info(...args) {
    Deno.core.ops.op_log("info", args.map(String).join(" "));
  },
  warn(...args) {
    Deno.core.ops.op_log("warn", args.map(String).join(" "));
  },
  error(...args) {
    Deno.core.ops.op_log("error", args.map(String).join(" "));
  },
  debug(...args) {
    Deno.core.ops.op_log("debug", args.map(String).join(" "));
  },
};

// Request 类（简化版）
class Request {
  constructor(url, options = {}) {
    this.url = url;
    this.method = options.method || "GET";
    this.headers = options.headers || {};
    this.body = options.body;
  }

  async text() {
    return this.body || "";
  }

  async json() {
    return JSON.parse(this.body || "{}");
  }
}

// Response 类（简化版）
class Response {
  constructor(body, options = {}) {
    this.body = typeof body === "string" ? body : JSON.stringify(body);
    this.status = options.status || 200;
    this.statusText = options.statusText || "OK";
    this.headers = options.headers || {};

    // WebSocket 支持
    if (options.webSocket) {
      this.status = 101;
      this.statusText = "Switching Protocols";
      // 将 WebSocketServer 的 ID 存储在特殊 header 中
      if (options.webSocket.ws_id) {
        this.headers["X-Denix-WebSocket-ID"] = options.webSocket.ws_id;
      }
    }
  }

  async text() {
    return this.body;
  }

  async json() {
    return JSON.parse(this.body);
  }
}

globalThis.Request = Request;
globalThis.Response = Response;

// URL 类（简化版）
class URL {
  constructor(url, base) {
    // 简单的 URL 解析
    try {
      const parsed = new globalThis.URL(url, base);
      this.href = parsed.href;
      this.protocol = parsed.protocol;
      this.hostname = parsed.hostname;
      this.port = parsed.port;
      this.pathname = parsed.pathname;
      this.search = parsed.search;
      this.hash = parsed.hash;
      this.searchParams = parsed.searchParams;
    } catch {
      // Fallback 简单解析
      const match = url.match(/^(https?:)\/\/([^/:]+)(:(\d+))?([^?#]*)(\?[^#]*)?(#.*)?$/);
      if (match) {
        this.href = url;
        this.protocol = match[1];
        this.hostname = match[2];
        this.port = match[4] || "";
        this.pathname = match[5] || "/";
        this.search = match[6] || "";
        this.hash = match[7] || "";
      } else {
        this.href = url;
        this.pathname = url;
      }
    }
  }
}

globalThis.URL = URL;

// 内部函数：执行应用的 fetch
globalThis.__denix_fetch = async function (request) {
  // 确保应用已导出 default
  if (!globalThis.default || typeof globalThis.default.fetch !== "function") {
    return {
      status: 500,
      headers: { "content-type": "text/plain" },
      body: "App must export default { fetch(request) { ... } }",
    };
  }

  try {
    // 构造 Request 对象
    const req = new Request(request.url, {
      method: request.method,
      headers: request.headers,
      body: request.body,
    });

    // 准备 env 对象（包含资源）
    const env = {
      kv: globalThis.kv,
      cache: globalThis.cache,
      db: globalThis.db,
    };

    // 调用应用的 fetch 函数
    const response = await globalThis.default.fetch(req, env);

    // 返回序列化的 response
    return {
      status: response.status,
      headers: response.headers || {},
      body: response.body,
    };
  } catch (error) {
    console.error("Error in app fetch:", error);
    return {
      status: 500,
      headers: { "content-type": "text/plain" },
      body: `Internal Error: ${error.message || String(error)}`,
    };
  }
};

// 内部函数：执行应用的定时任务
globalThis.__denix_scheduled = async function () {
  // 确保应用已导出 default 和 scheduled 函数
  if (!globalThis.default || typeof globalThis.default.scheduled !== "function") {
    console.log("No scheduled handler defined in app");
    return;
  }

  try {
    // 获取 cron 事件信息
    const event = Deno.core.ops.op_cron_get_event();

    if (!event) {
      console.error("No cron event data available");
      return;
    }

    // 准备 env 对象（包含资源）
    const env = {
      kv: globalThis.kv,
      cache: globalThis.cache,
      db: globalThis.db,
    };

    // 调用应用的 scheduled 函数
    await globalThis.default.scheduled(event, env);

    console.log("Scheduled task completed successfully");
  } catch (error) {
    console.error("Error in scheduled task:", error);
    throw error;
  }
};

// 标记 runtime 已加载
globalThis.__denix_runtime_loaded = true;
