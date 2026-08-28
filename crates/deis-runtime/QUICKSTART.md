# Runtime 快速开始指南

这是一个快速开始指南，帮助你在几分钟内运行 Runtime 平台并部署你的第一个应用。

## 1. 启动 Runtime 平台

```bash
# 开发模式（带自动重载）
deno task dev

# 或者生产模式
deno task start
```

启动后，你应该看到：
```
🚀 Starting Runtime Platform...
=====================================
[Gateway] Starting HTTP server on port 8000...
[Gateway] Server started on http://localhost:8000
=====================================
✅ Runtime Platform is ready!
📡 HTTP Server: http://localhost:8000
📊 Health Check: http://localhost:8000/health
📚 API Docs: http://localhost:8000/
=====================================
```

## 2. 部署你的第一个应用

### 方法 1: 使用 curl 命令

```bash
# 部署一个简单的 Hello World 应用
curl -X POST http://localhost:8000/api/apps \
  -H "Content-Type: application/json" \
  -d '{
    "id": "hello",
    "code": "export default { fetch(req) { return new Response(\"Hello from Runtime!\") } }"
  }'
```

### 方法 2: 部署示例应用

```bash
# 部署所有示例应用
deno run --allow-all scripts/deploy_examples.ts
```

## 3. 访问你的应用

```bash
# 访问 Hello World 应用
curl http://localhost:8000/apps/hello

# 如果部署了示例应用，可以尝试：
curl http://localhost:8000/apps/counter/increment
curl http://localhost:8000/apps/echo
curl http://localhost:8000/apps/todos/todos
```

## 4. 管理应用

### 列出所有应用
```bash
curl http://localhost:8000/api/apps
```

### 获取应用信息
```bash
curl http://localhost:8000/api/apps/hello
```

### 更新应用
```bash
curl -X PUT http://localhost:8000/api/apps/hello \
  -H "Content-Type: application/json" \
  -d '{
    "code": "export default { fetch(req) { return new Response(\"Hello Updated!\") } }"
  }'
```

### 删除应用
```bash
curl -X DELETE http://localhost:8000/api/apps/hello
```

## 5. 编写自己的应用

创建一个新文件 `my_app.ts`:

```typescript
// 基本应用结构
export default {
  async fetch(request: Request, env: any): Promise<Response> {
    const url = new URL(request.url);

    // 路由处理
    if (url.pathname === "/") {
      return new Response("Welcome to my app!");
    }

    // 使用 KV 存储
    if (url.pathname === "/save") {
      await env.kv.set("mykey", "myvalue");
      return new Response("Saved!");
    }

    if (url.pathname === "/load") {
      const value = await env.kv.get("mykey");
      return new Response(`Value: ${value}`);
    }

    return new Response("Not Found", { status: 404 });
  }
};
```

部署你的应用：
```bash
APP_CODE=$(cat my_app.ts)
curl -X POST http://localhost:8000/api/apps \
  -H "Content-Type: application/json" \
  -d "{\"id\":\"myapp\",\"code\":$(echo "$APP_CODE" | jq -Rs .)}"
```

## 6. 使用资源

Runtime 支持为应用注入资源。目前支持：

### KV 存储
```typescript
export default {
  async fetch(request: Request, env: any): Promise<Response> {
    // env.kv 是自动注入的 KV 存储
    await env.kv.set("key", "value");
    const value = await env.kv.get("key");
    await env.kv.delete("key");
    const keys = await env.kv.list("prefix:");

    return new Response(value);
  }
};
```

### 指定资源配置
```bash
curl -X POST http://localhost:8000/api/apps \
  -H "Content-Type: application/json" \
  -d '{
    "id": "myapp",
    "code": "...",
    "config": {
      "resources": ["kv:storage"]
    }
  }'
```

## 7. 运行测试

```bash
# 给测试脚本添加执行权限
chmod +x scripts/test_platform.sh

# 运行集成测试
./scripts/test_platform.sh
```

## 8. 查看平台状态

在浏览器中打开 http://localhost:8000 查看：
- 平台统计信息
- 已部署的应用列表
- API 文档

或使用命令行：
```bash
# 健康检查
curl http://localhost:8000/health

# 获取统计信息
curl http://localhost:8000/api/apps | jq
```

## 示例应用说明

### Hello World (`examples/hello_world.ts`)
最简单的应用，返回 "Hello from Runtime!"
```bash
curl http://localhost:8000/apps/hello
```

### Counter (`examples/counter_app.ts`)
使用 KV 存储的计数器应用
```bash
# 查看计数
curl http://localhost:8000/apps/counter/count

# 增加计数
curl http://localhost:8000/apps/counter/increment

# 重置计数
curl http://localhost:8000/apps/counter/reset
```

### Echo (`examples/echo_app.ts`)
返回请求详细信息的应用
```bash
curl http://localhost:8000/apps/echo
curl -X POST http://localhost:8000/apps/echo -d "test data"
```

### TODO App (`examples/todo_app.ts`)
完整的 CRUD 应用
```bash
# 列出所有 TODOs
curl http://localhost:8000/apps/todos/todos

# 创建 TODO
curl -X POST http://localhost:8000/apps/todos/todos \
  -H "Content-Type: application/json" \
  -d '{"title":"Buy milk"}'

# 更新 TODO
curl -X PUT http://localhost:8000/apps/todos/todos/{id} \
  -H "Content-Type: application/json" \
  -d '{"completed":true}'

# 删除 TODO
curl -X DELETE http://localhost:8000/apps/todos/todos/{id}
```

## 故障排查

### 端口已被占用
```bash
# 使用不同的端口
PORT=3000 deno task start
```

### 应用无法启动
检查应用代码是否导出了正确的格式：
```typescript
export default {
  fetch(request: Request, env?: any): Promise<Response> {
    // ...
  }
}
```

### Worker 初始化失败
确保应用代码是有效的 TypeScript/JavaScript，并且没有语法错误。

## 下一步

- 阅读 [ARCHITECTURE.md](./ARCHITECTURE.md) 了解平台架构
- 查看 [examples/](./examples/) 目录获取更多示例
- 实现自定义资源提供者
- 添加中间件支持
- 实现持久化存储

## 需要帮助？

- 查看 [README.md](./README.md) 了解更多信息
- 提交 Issue 报告问题
- 贡献代码改进平台
