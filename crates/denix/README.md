# Denix - 多租户 Web 平台

基于 **Rust + deno_core** 的高性能多租户平台，运行 JavaScript 应用。

## 🎯 项目概述

Denix 是一个类似 Deno Deploy 的多租户 Web 平台，特点：
- 🦀 **Rust 核心** - 平台本身用 Rust 实现，高性能、低开销
- 📦 **JavaScript 应用** - 应用用 JavaScript 编写
- 🔒 **V8 隔离** - 每个应用运行在独立的 V8 isolate
- ⚡ **零拷贝** - deno_core 提供高效的 Rust ↔ JS 通信
- 🚀 **动态管理** - 运行时加载、更新、卸载应用

## 架构设计

```
┌─────────────────────────────────────────┐
│         Rust Core Platform              │
│  ┌────────────────────────────────┐     │
│  │   axum HTTP Server             │     │
│  └────────┬───────────────────────┘     │
│           │                              │
│  ┌────────▼───────────────────────┐     │
│  │   Application Manager           │     │
│  │   - 应用生命周期                  │     │
│  │   - Runtime 池管理               │     │
│  └────────┬───────────────────────┘     │
│           │                              │
│  ┌────────▼───────────────────────┐     │
│  │   V8 Runtime (deno_core)       │     │
│  │   - 每个应用独立 isolate         │     │
│  │   - Ops: Rust ↔ JS 通信         │     │
│  └────────┬───────────────────────┘     │
│           │                              │
│  ┌────────▼───────────────────────┐     │
│  │   Resource Layer (Ops)         │     │
│  │   - KV Storage                 │     │
│  │   - Cache (moka)               │     │
│  │   - 可扩展更多资源               │     │
│  └────────────────────────────────┘     │
└─────────────────────────────────────────┘
              ▲
              │
┌─────────────┴───────────────────────────┐
│       JavaScript Applications           │
│    export default {                     │
│      fetch(req) { return new Response() }│
│    }                                    │
└─────────────────────────────────────────┘
```

## 快速开始

### 1. 编译平台

```bash
# 开发模式
cargo build

# 生产模式 (推荐)
cargo build --release
```

**注意**: 首次编译会下载 V8 引擎，需要较长时间和良好的网络连接。

### 2. 启动平台

```bash
cargo run --release
```

### 3. 部署应用

应用使用 **JavaScript** (不是 TypeScript):

```bash
curl -X POST http://localhost:8000/api/apps \
  -H "Content-Type: application/json" \
  -d '{
    "id": "hello",
    "code": "export default { fetch() { return new Response(\"Hello from Rust!\") } }"
  }'
```

### 4. 访问应用

```bash
curl http://localhost:8000/apps/hello
```

## 应用开发

### 基本结构

应用必须导出包含 `fetch` 方法的默认对象：

```javascript
export default {
  async fetch(request, env) {
    return new Response("Hello from Denix!");
  }
};
```

### 使用 KV 存储

```javascript
export default {
  async fetch(request, env) {
    const url = new URL(request.url);

    if (url.pathname === "/set") {
      await env.kv.set("mykey", "myvalue");
      return new Response("Saved!");
    }

    if (url.pathname === "/get") {
      const value = await env.kv.get("mykey");
      return new Response(value || "not found");
    }

    return new Response("Welcome!");
  }
};
```

### 使用 Cache 缓存

高性能本地缓存，基于 **moka** (Rust 缓存库)：

```javascript
export default {
  async fetch(request, env) {
    const url = new URL(request.url);

    if (url.pathname === "/cache/set") {
      // 设置缓存 (TTL 参数预留，当前使用全局 10 分钟 TTL)
      await env.cache.set("user:123", "John Doe", 0);
      return new Response("Cached!");
    }

    if (url.pathname === "/cache/get") {
      // 获取缓存
      const value = await env.cache.get("user:123");
      return new Response(value || "cache miss");
    }

    if (url.pathname === "/cache/stats") {
      // 查看缓存统计
      const stats = await env.cache.stats();
      return new Response(JSON.stringify(stats));
    }

    return new Response("Cache API Demo");
  }
};
```

### 使用 HTTP Client (fetch API)

调用外部 HTTP 服务 (标准 Fetch API):

```javascript
export default {
  async fetch(request, env) {
    const url = new URL(request.url);

    if (url.pathname === "/proxy") {
      // 调用外部 API
      const response = await fetch("https://api.github.com/users/github", {
        method: "GET",
        headers: {
          "User-Agent": "Denix/1.0"
        },
        timeout: 10  // 超时时间（秒）
      });

      const data = await response.text();
      return new Response(data, {
        headers: { "Content-Type": "application/json" }
      });
    }

    if (url.pathname === "/post-data") {
      // 发送 POST 请求
      const response = await fetch("https://httpbin.org/post", {
        method: "POST",
        headers: {
          "Content-Type": "application/json"
        },
        body: JSON.stringify({ key: "value" })
      });

      return new Response(await response.text());
    }

    return new Response("Fetch API Demo");
  }
};
```

### 使用 WebSocket Client

连接到外部 WebSocket 服务：

```javascript
export default {
  async fetch(request, env) {
    const url = new URL(request.url);

    if (url.pathname === "/ws-connect") {
      // 创建 WebSocket 连接
      const ws = new WebSocket("wss://echo.websocket.org/");

      ws.onopen = (event) => {
        console.log("WebSocket connected");
        ws.send("Hello from Denix!");
      };

      ws.onmessage = (event) => {
        console.log("Received:", event.data);
      };

      ws.onerror = (event) => {
        console.error("WebSocket error:", event.error);
      };

      ws.onclose = (event) => {
        console.log("WebSocket closed");
      };

      return new Response("WebSocket connection initiated");
    }

    if (url.pathname === "/ws-send") {
      // 发送消息到已建立的 WebSocket
      const ws = new WebSocket("wss://api.example.com/realtime");

      ws.onopen = async () => {
        await ws.send(JSON.stringify({ action: "subscribe", channel: "updates" }));
        // 关闭连接
        await ws.close();
      };

      return new Response("Message sent");
    }

    return new Response("WebSocket Client Demo");
  }
};
```

### 使用 WebSocket Server

接收客户端 WebSocket 连接：

```javascript
export default {
  async fetch(request, env) {
    const url = new URL(request.url);

    // 检查是否是 WebSocket 升级请求
    if (request.headers.get("Upgrade") === "websocket") {
      // 创建 WebSocket Pair
      const pair = new WebSocketPair();
      const [client, server] = Object.values(pair);

      // 处理服务端 WebSocket
      handleWebSocket(server);

      // 返回客户端 WebSocket
      return new Response(null, {
        status: 101,
        webSocket: client
      });
    }

    return new Response("Expected WebSocket connection");
  }
};

function handleWebSocket(ws) {
  // 接受连接
  ws.accept();

  // 处理消息
  ws.onmessage = (event) => {
    console.log("Received message:", event.data);

    // Echo 回客户端
    ws.send(`Echo: ${event.data}`);
  };

  // 处理关闭
  ws.onclose = (event) => {
    console.log("WebSocket closed:", event.code, event.reason);
  };

  // 处理错误
  ws.onerror = (event) => {
    console.error("WebSocket error:", event.error);
  };
}
```

**当前状态**:

✅ **已实现**:
- JavaScript 应用可以使用 WebSocketPair 创建 WebSocket 连接
- Response 支持 `webSocket` 选项返回 status 101
- Gateway 检测 WebSocket 升级响应并提取 ws_id
- WebSocket 协议升级完整支持

⏳ **进行中**:
- Gateway 与 JavaScript Isolate 的消息桥接
- 需要扩展 ApplicationManager API 来路由 WebSocket 消息

**测试**:

```bash
# 安装 wscat (如果需要)
npm install -g wscat

# 连接到应用的 WebSocket 端点
wscat -c ws://localhost:8000/apps/myapp

# 发送消息（当前会收到 echo 响应）
> Hello WebSocket!
< Echo: Hello WebSocket!
```

**架构说明**:

```
客户端 WebSocket
     ↓
Gateway (handle_websocket_bridged)
     ↓
[消息桥接层 - 待完善]
     ↓
WsServerManager (in Runtime Isolate)
     ↓
JavaScript WebSocketServer.onmessage
```

当前限制：WsServerManager 在 runtime isolate 内部，Gateway 无法直接访问。需要通过 ApplicationManager 添加消息路由 API。

### 使用定时任务 (Cron)

定时执行任务：

```javascript
export default {
  async fetch(request, env) {
    return new Response("HTTP handler");
  },

  // 定时任务处理器
  async scheduled(event, env) {
    console.log("Cron triggered at:", new Date(event.scheduledTime));
    console.log("Cron expression:", event.cron);

    // 执行定时任务逻辑
    const now = Date.now().toString();
    await env.kv.set("last_cron_run", now);

    // 清理过期缓存
    const stats = await env.cache.stats();
    console.log("Cache stats:", stats);

    // 执行数据库维护
    await env.db.execute(
      "DELETE FROM logs WHERE created_at < datetime('now', '-30 days')",
      null
    );

    return { success: true };
  }
};
```

### 使用 Database 数据库

SQL 数据库支持 (基于 gRPC，类似 Cloudflare D1)：

```javascript
export default {
  async fetch(request, env) {
    const url = new URL(request.url);

    if (url.pathname === "/db/create") {
      // 创建表
      await env.db.execute(
        "CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY, name TEXT)",
        null
      );
      return new Response("Table created!");
    }

    if (url.pathname === "/db/insert") {
      // 插入数据
      const result = await env.db.execute(
        "INSERT INTO users (name) VALUES (?)",
        ["Alice"]
      );
      return new Response(`Inserted: ${result.last_insert_id}`);
    }

    if (url.pathname === "/db/query") {
      // 查询数据
      const result = await env.db.query(
        "SELECT * FROM users WHERE name = ?",
        ["Alice"]
      );
      return new Response(JSON.stringify(result.rows));
    }

    return new Response("Database API Demo");
  }
};
```

### 可用的 API

- `Request` / `Response` - Web 标准 API
- `URL` - URL 解析
- `console.log()` - 日志输出到平台
- `fetch(url, options)` - HTTP 客户端 (标准 Fetch API)
  - 支持方法: GET, POST, PUT, DELETE, PATCH, HEAD, OPTIONS
  - 自定义请求头
  - 请求体支持
  - 超时控制 (默认 30 秒)
- `WebSocket` - WebSocket 客户端 (标准 WebSocket API)
  - 连接外部 WebSocket 服务
  - 事件: onopen, onmessage, onerror, onclose
  - 方法: send(data), close(code, reason)
  - 状态: CONNECTING, OPEN, CLOSING, CLOSED
- `WebSocketServer` / `WebSocketPair` - WebSocket 服务端
  - 接收客户端 WebSocket 连接
  - WebSocketPair 创建客户端和服务端对
  - accept() 接受连接
  - send(data) 发送消息
  - close(code, reason) 关闭连接
- 定时任务 (Cron)
  - 导出 `scheduled(event, env)` 函数
  - event: { scheduledTime: number, cron?: string }
  - 由 denira supervisor 触发
- `env.kv` - KV 存储
  - `get(key)` - 获取值
  - `set(key, value)` - 设置值
  - `delete(key)` - 删除键
  - `list(prefix)` - 列出前缀匹配的键
- `env.cache` - 高性能缓存 (基于 moka)
  - `get(key)` - 获取缓存值
  - `set(key, value, ttl)` - 设置缓存 (ttl 参数预留)
  - `delete(key)` - 删除缓存
  - `has(key)` - 检查键是否存在
  - `clear()` - 清空所有缓存
  - `stats()` - 获取统计信息 (entry_count, weighted_size)
- `env.db` - SQL 数据库 (基于 gRPC)
  - `execute(sql, params)` - 执行 SQL，返回 { rows_affected, last_insert_id? }
  - `query(sql, params)` - 查询 SQL，返回 { rows, columns }
  - `batch(statements)` - 批量执行
  - `prepare(sql)` - 预处理语句
  - `executePrepared(stmtId, params)` - 执行预处理
  - `queryPrepared(stmtId, params)` - 查询预处理

## 示例应用

```bash
# 部署示例应用 (需要先转换为 JavaScript)
./scripts/deploy_rust_examples.sh

# 或手动测试
curl http://localhost:8000/apps/hello
curl http://localhost:8000/apps/counter/increment
curl http://localhost:8000/apps/echo
```

## 项目结构

```
denix/
├── Cargo.toml              # Rust 依赖
├── src/
│   ├── main.rs             # 入口
│   ├── runtime/            # V8 Runtime 层
│   │   ├── isolate.rs      # V8 isolate 封装
│   │   └── ops.rs          # Rust ↔ JS ops
│   ├── app/                # 应用管理
│   │   ├── manager.rs      # 应用生命周期
│   │   └── types.rs        # 类型定义
│   ├── resource/           # 资源管理
│   └── gateway/            # HTTP 网关
│       ├── mod.rs          # 网关核心
│       └── handlers.rs     # 请求处理
├── runtime_js/
│   └── runtime.js          # JS Runtime API
├── apps/                   # JavaScript 应用示例
├── tests/                  # 测试套件
└── scripts/                # 工具脚本
```

## 技术栈

- **Rust**: tokio, axum, deno_core, serde, moka
- **V8**: 通过 deno_core 集成
- **应用**: JavaScript (ES modules)

## 性能特点

- **启动速度**: 毫秒级平台启动
- **内存效率**: 低内存占用，每个应用独立 isolate
- **并发性能**: 基于 tokio 的高吞吐量
- **真正隔离**: V8 isolate 级别隔离

## 测试

```bash
# 运行所有测试
cargo test

# 运行特定测试
cargo test ops_test
cargo test integration_test

# 详细输出
cargo test -- --nocapture
```

查看 [README_TESTS.md](./README_TESTS.md) 了解详细的测试文档。

## API 文档

### 管理 API

#### 创建应用
```bash
POST /api/apps
Content-Type: application/json

{
  "id": "myapp",
  "code": "export default { fetch() { ... } }"
}
```

#### 列出应用
```bash
GET /api/apps
```

#### 获取应用
```bash
GET /api/apps/:id
```

#### 更新应用
```bash
PUT /api/apps/:id
Content-Type: application/json

{
  "code": "export default { fetch() { ... } }"
}
```

#### 删除应用
```bash
DELETE /api/apps/:id
```

### 应用路由

```bash
GET/POST/PUT/DELETE /apps/:id/*
```

## 编译说明

### 系统要求

- Rust 1.70+ (推荐最新稳定版)
- 4GB+ RAM (编译 V8 需要)
- 良好的网络连接 (下载 V8 二进制)

### 编译问题

如果遇到 V8 编译问题：

```bash
# 清理缓存重新编译
cargo clean
cargo build --release

# 或从源码编译 V8 (需要更长时间)
V8_FROM_SOURCE=1 cargo build --release
```

## 文档

- [QUICKSTART_RUST.md](./QUICKSTART_RUST.md) - 快速开始指南
- [ARCHITECTURE_RUST.md](./ARCHITECTURE_RUST.md) - 架构设计文档
- [README_RUST.md](./README_RUST.md) - 详细说明
- [README_TESTS.md](./README_TESTS.md) - 测试文档

## 开发状态

🚧 **原型阶段**

已完成：
- ✅ Rust 核心平台
- ✅ V8 Runtime 集成
- ✅ Ops 系统 (KV Storage, Cache, Database, HTTP Client, WebSocket Client/Server, Cron)
- ✅ gRPC 客户端 (KV + Database)
- ✅ HTTP Client (Fetch API)
- ✅ WebSocket Client (标准 WebSocket API)
- ✅ WebSocket Server (WebSocketPair, accept, send, close)
- ✅ WebSocket Gateway 协议升级 (JavaScript Response 集成)
- ✅ 定时任务 (Cron) 支持
- ✅ 应用管理器
- ✅ HTTP 网关
- ✅ Denira Supervisor 集成客户端
- ✅ 完整测试套件 (53 单元测试 + 9 集成测试)

进行中：
- 🔧 WebSocket 消息桥接 (Gateway ↔ Runtime Isolate)
  - ✅ JavaScript WebSocketPair API
  - ✅ Response webSocket 选项支持
  - ✅ Gateway WebSocket 升级检测
  - ⏳ 消息路由层（需要扩展 ApplicationManager API）

计划中：
- ⏳ Runtime snapshot 优化
- ⏳ 持久化存储
- ⏳ 监控和日志系统

## 与其他平台对比

| 特性 | Denix | Deno Deploy | Cloudflare Workers |
|------|-------|-------------|-------------------|
| 开源 | ✅ | ❌ | ❌ |
| 自部署 | ✅ | ❌ | ❌ |
| 平台语言 | Rust | Rust | C++ |
| 应用语言 | JavaScript | TypeScript/JavaScript | JavaScript |
| V8 隔离 | ✅ | ✅ | ✅ |
| 可定制 | 完全可控 | 闭源 | 闭源 |

## License

MIT

## 贡献

欢迎提交 Issue 和 PR！

提交 PR 前请确保：
1. 代码通过 `cargo fmt` 格式化
2. 通过 `cargo clippy` 检查
3. 所有测试通过 `cargo test`
