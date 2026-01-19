# Denix Rust 版本快速开始

这是 Denix 的 Rust 实现版本，使用 Rust + deno_core 构建高性能多租户平台，运行 TypeScript/JavaScript 应用。

## 系统要求

- Rust 1.70+ (推荐最新稳定版)
- Linux/macOS (Windows 需要 WSL)

## 1. 编译平台

```bash
# 开发模式（快速编译，包含调试信息）
cargo build

# 生产模式（优化编译，性能最佳）
cargo build --release
```

## 2. 启动平台

```bash
# 开发模式
cargo run

# 或使用已编译的二进制
./target/debug/denix

# 生产模式
./target/release/denix
```

启动后，你应该看到：
```
🚀 Starting Denix Platform (Rust)
====================================
====================================
✅ Denix Platform is ready!
📡 HTTP Server: http://localhost:8000
📊 Health Check: http://localhost:8000/health
📚 API Docs: http://localhost:8000/
====================================
```

## 3. 部署应用

### 方法 1: 使用脚本部署示例应用

```bash
# 给脚本添加执行权限
chmod +x scripts/deploy_rust_examples.sh

# 部署所有示例应用
./scripts/deploy_rust_examples.sh
```

### 方法 2: 手动部署

```bash
# 部署 Hello World 应用
APP_CODE=$(cat apps/hello.ts | jq -Rs .)
curl -X POST http://localhost:8000/api/apps \
  -H "Content-Type: application/json" \
  -d "{\"id\":\"hello\",\"code\":$APP_CODE}"
```

## 4. 访问应用

```bash
# Hello World
curl http://localhost:8000/apps/hello

# Counter (需要路径)
curl http://localhost:8000/apps/counter/count
curl http://localhost:8000/apps/counter/increment

# Echo
curl http://localhost:8000/apps/echo

# TODO App
curl http://localhost:8000/apps/todos/todos
curl -X POST http://localhost:8000/apps/todos/todos \
  -H "Content-Type: application/json" \
  -d '{"title":"Test Todo"}'
```

## 5. 管理应用

### 列出所有应用
```bash
curl http://localhost:8000/api/apps | jq
```

### 获取应用信息
```bash
curl http://localhost:8000/api/apps/hello | jq
```

### 更新应用
```bash
NEW_CODE=$(cat apps/hello.ts | jq -Rs .)
curl -X PUT http://localhost:8000/api/apps/hello \
  -H "Content-Type: application/json" \
  -d "{\"code\":$NEW_CODE}"
```

### 删除应用
```bash
curl -X DELETE http://localhost:8000/api/apps/hello
```

## 6. 编写自己的应用

创建 `my_app.ts`:

```typescript
export default {
  async fetch(request: Request, env: any): Promise<Response> {
    const url = new URL(request.url);

    // 使用 KV 存储
    if (url.pathname === "/save") {
      await env.kv.set("mykey", "myvalue");
      return new Response("Saved!");
    }

    if (url.pathname === "/load") {
      const value = await env.kv.get("mykey");
      return new Response(`Value: ${value}`);
    }

    return new Response("Welcome!");
  }
};
```

部署：
```bash
APP_CODE=$(cat my_app.ts | jq -Rs .)
curl -X POST http://localhost:8000/api/apps \
  -H "Content-Type: application/json" \
  -d "{\"id\":\"myapp\",\"code\":$APP_CODE}"
```

## 7. 应用可用的 API

### KV Storage
```typescript
// 应用中可以使用 env.kv
const value = await env.kv.get("key");
await env.kv.set("key", "value");
await env.kv.delete("key");
const keys = await env.kv.list("prefix:");
```

### Console Logging
```typescript
console.log("Hello");     // 会输出到平台日志
console.info("Info");
console.warn("Warning");
console.error("Error");
```

### Web Standards API
```typescript
// Request, Response, URL 等标准 API
const url = new URL(request.url);
return new Response("Hello", {
  status: 200,
  headers: { "content-type": "text/plain" }
});
```

## 8. 性能特性

### Rust 平台优势
- **启动速度**: 毫秒级平台启动
- **内存效率**: 低内存占用，高并发支持
- **V8 隔离**: 每个应用独立 V8 isolate
- **零拷贝**: deno_core 高效数据传递

### 并发处理
平台使用 tokio 异步运行时，支持：
- 多应用并发执行
- 每个应用并发请求处理
- 非阻塞 I/O 操作

## 9. 开发调试

### 启用详细日志
```bash
RUST_LOG=debug cargo run
```

### 监控应用状态
```bash
# 查看平台统计
curl http://localhost:8000/health | jq

# 查看应用列表
curl http://localhost:8000/api/apps | jq
```

### 常见问题

#### 编译错误
```bash
# 更新 Rust 工具链
rustup update

# 清理缓存重新编译
cargo clean
cargo build
```

#### 应用加载失败
- 检查 TypeScript 语法是否正确
- 确保导出了 default 对象和 fetch 方法
- 查看平台日志了解详细错误

#### 端口占用
```bash
# 使用不同端口
PORT=3000 cargo run
```

## 10. 架构说明

```
┌─────────────────────────────────────┐
│     Rust Platform (denix)           │
│  ┌────────────────────────────┐     │
│  │  axum HTTP Server          │     │
│  └────────┬───────────────────┘     │
│           │                          │
│  ┌────────▼───────────────────┐     │
│  │  Application Manager        │     │
│  └────────┬───────────────────┘     │
│           │                          │
│  ┌────────▼───────────────────┐     │
│  │  V8 Runtime (deno_core)    │     │
│  │  - Isolate per app         │     │
│  │  - Ops for Rust ↔ JS       │     │
│  └────────────────────────────┘     │
└─────────────────────────────────────┘
              ▲
              │
┌─────────────┴───────────────┐
│  TypeScript/JavaScript Apps  │
│  export default {            │
│    fetch(req) { ... }        │
│  }                           │
└─────────────────────────────┘
```

## 11. 与 Deno 版本对比

| 特性 | Deno 版本 | Rust 版本 |
|------|----------|-----------|
| 平台语言 | TypeScript | Rust |
| 应用语言 | TypeScript | TypeScript |
| 性能 | 好 | 更好 |
| 内存 | 中等 | 更低 |
| 启动速度 | 快 | 更快 |
| 开发速度 | 快 | 中等 |
| 生产就绪 | 原型 | 更接近 |

## 12. 生产部署建议

### 编译优化
```bash
# 使用发布模式
cargo build --release

# 启动生产服务
./target/release/denix
```

### 使用 systemd（Linux）
创建 `/etc/systemd/system/denix.service`:
```ini
[Unit]
Description=Denix Platform
After=network.target

[Service]
Type=simple
User=denix
WorkingDirectory=/opt/denix
ExecStart=/opt/denix/target/release/denix
Restart=always

[Install]
WantedBy=multi-user.target
```

启动服务：
```bash
sudo systemctl enable denix
sudo systemctl start denix
```

## 13. 下一步

- 添加更多 ops（数据库、HTTP 客户端等）
- 实现应用持久化存储
- 添加监控和指标收集
- 实现 Runtime snapshot 优化启动
- 支持应用间通信

## 需要帮助？

- 查看 [README_RUST.md](./README_RUST.md) 了解架构
- 查看 [ARCHITECTURE_RUST.md](./ARCHITECTURE_RUST.md) 了解设计细节
- 提交 Issue 报告问题
