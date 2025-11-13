# DeniCTL 快速开始指南

本指南将帮助您在 5 分钟内创建并部署您的第一个 Edge Function。

## 前置要求

- Rust 1.70+ ([安装 Rust](https://rustup.rs/))
- Node.js 18+ (用于项目依赖管理)

## 1. 安装 DeniCTL

```bash
# 克隆项目
git clone https://github.com/deislet/denictl.git
cd denictl

# 构建并安装
cargo build --release
cargo install --path crates/denictl

# 验证安装
denictl --version
```

## 2. 创建新项目

```bash
# 初始化项目
denictl init my-first-app
cd my-first-app

# 查看项目结构
tree
```

您将看到:
```
my-first-app/
├── functions/          # 边缘函数代码
│   ├── index.ts       # 默认路由 (/)
│   └── api/
│       └── hello.ts   # API 端点 (/api/hello)
├── .config.json       # 项目配置
├── .env               # 环境变量
├── package.json
├── tsconfig.json
└── README.md
```

## 3. 查看示例代码

### functions/index.ts - 默认路由
```typescript
export default async function handler(context: Context): Promise<Response> {
  return new Response('Hello from Edge Function!', {
    headers: { 'Content-Type': 'text/plain' }
  });
}
```

### functions/api/hello.ts - API 端点
```typescript
export default async function handler(context: Context): Promise<Response> {
  const { query } = context;
  const name = query.name || 'World';

  const response = {
    message: `Hello, ${name}!`,
    timestamp: new Date().toISOString(),
    path: context.path
  };

  return new Response(JSON.stringify(response, null, 2), {
    headers: { 'Content-Type': 'application/json' }
  });
}
```

## 4. 验证项目

```bash
# 验证代码是否符合规范
denictl validate

# 输出:
# ✓ Validation passed
# ✓ Project structure valid
# ✓ .config.json valid
# ✓ All handlers valid
```

## 5. 构建项目

```bash
# 构建所有平台
denictl build

# 或只构建特定平台
denictl build --vendor cloudflare
```

您将在 `dist/` 目录看到构建产物:
```
dist/
├── cloudflare/
│   ├── index.ts           # 路由器入口
│   ├── wrangler.toml      # Cloudflare 配置
│   ├── src/               # 转换后的 handlers
│   └── adapters/          # 平台适配器
├── deno/
│   └── ...
└── tencent/
    └── ...
```

## 6. 查看生成的代码

### dist/cloudflare/index.ts - 自动生成的路由器
```typescript
import handler0 from './src/index';
import handler1 from './src/api/hello';

export default {
  async fetch(request, env, ctx) {
    const url = new URL(request.url);

    if (matchRoute(url.pathname, '/')) {
      return handler0.fetch(request, env, ctx);
    }
    if (matchRoute(url.pathname, '/api/hello')) {
      return handler1.fetch(request, env, ctx);
    }

    return new Response('Not Found', { status: 404 });
  }
};
```

### dist/cloudflare/src/index.ts - 转换后的 handler
```typescript
import { createContextBuilder } from '../adapters/context-builder';

// 原始 handler 代码保持不变
export default async function handler(context: Context): Promise<Response> {
  return new Response('Hello from Edge Function!', {
    headers: { 'Content-Type': 'text/plain' }
  });
}

// Cloudflare Workers 包装
export default {
  async fetch(request, env, ctx) {
    const buildContext = createContextBuilder(env, ctx);
    const context = buildContext(request, '/');
    return await handler(context);
  }
};
```

## 7. 部署到 Cloudflare Workers

```bash
# 配置 Cloudflare 凭证
denictl config set cloudflare.api-token YOUR_API_TOKEN

# 部署到 staging 环境
denictl deploy --vendor cloudflare --env staging

# 部署到 production 环境
denictl deploy --vendor cloudflare --env production
```

或者手动使用 wrangler:
```bash
cd dist/cloudflare
npx wrangler deploy
```

## 8. 添加 KV Store 功能

### 修改 .config.json
```json
{
  "services": {
    "kv": { "enabled": true },
    "database": { "enabled": false }
  }
}
```

### 创建 functions/api/count.ts
```typescript
export default async function handler(context: Context): Promise<Response> {
  const { kv } = context.services;

  // 获取当前计数
  const count = await kv.getJSON<number>('visit_count') || 0;

  // 增加计数
  const newCount = count + 1;
  await kv.putJSON('visit_count', newCount);

  return new Response(JSON.stringify({
    count: newCount,
    message: 'Visit count updated'
  }), {
    headers: { 'Content-Type': 'application/json' }
  });
}
```

### 重新构建
```bash
denictl build --vendor cloudflare
```

## 9. 添加动态路由

### 创建 functions/api/users/[id].ts
```typescript
export default async function handler(context: Context): Promise<Response> {
  const { params } = context;
  const userId = params.id;

  // 模拟从数据库获取用户
  const user = {
    id: userId,
    name: `User ${userId}`,
    email: `user${userId}@example.com`
  };

  return new Response(JSON.stringify(user), {
    headers: { 'Content-Type': 'application/json' }
  });
}
```

这将自动映射到路由: `/api/users/:id`

访问: `https://your-worker.workers.dev/api/users/123`

## 10. 本地开发 (即将推出)

```bash
# 启动本地开发服务器
denictl dev

# 服务器将在 http://localhost:8000 启动
# 支持热重载和 Mock 服务
```

## 常见任务

### 验证代码规范
```bash
denictl validate --strict
```

### 构建特定平台
```bash
denictl build --vendor cloudflare
denictl build --vendor deno
denictl build --vendor tencent
```

### 查看部署历史
```bash
denictl deployments list
```

### 回滚部署
```bash
denictl rollback --deployment <deployment-id>
```

## 最佳实践

### 1. 保持代码平台无关

✅ **正确**:
```typescript
const apiKey = context.env.API_KEY;
const kv = context.services.kv;
```

❌ **错误**:
```typescript
if (typeof Deno !== 'undefined') { }
const kv = env.MY_KV;  // 直接访问平台 binding
```

### 2. 使用统一的服务接口

```typescript
// KV Store
await context.services.kv.put('key', 'value');
const value = await context.services.kv.get('key');

// Database
const result = await context.services.database.query(
  'SELECT * FROM users WHERE id = ?',
  [userId]
);
```

### 3. 结构化日志

```typescript
context.log.info('Processing request', { userId, action: 'login' });
context.log.error('Failed to process', { error: error.message });
```

### 4. 使用环境变量

```typescript
const apiEndpoint = context.env.API_ENDPOINT;
const logLevel = context.env.LOG_LEVEL;
```

## 项目配置详解

### .config.json
```json
{
  "version": "1.0",
  "name": "my-app",
  "runtime": "standard-v1",
  "language": "typescript",
  "functionRoot": "./functions",

  "environment": {
    "variables": {
      "LOG_LEVEL": "info",
      "API_BASE": "https://api.example.com"
    },
    "secrets": ["API_KEY", "DB_PASSWORD"]
  },

  "services": {
    "kv": { "enabled": true },
    "database": { "enabled": false }
  },

  "vendors": {
    "cloudflare": { "enabled": true },
    "deno": { "enabled": true },
    "tencent": { "enabled": false }
  },

  "build": {
    "outDir": "./dist",
    "minify": true,
    "sourcemap": true
  }
}
```

## 故障排除

### 验证失败
```bash
# 查看详细错误
denictl validate --strict

# 常见错误:
# - 平台类型检测: typeof Deno
# - 直接访问 context.raw
# - 使用平台特定 API
```

### 构建失败
```bash
# 检查 TypeScript 语法
npx tsc --noEmit

# 查看详细构建日志
denictl build --vendor all
```

### 部署失败
```bash
# 验证配置
denictl config get cloudflare.api-token

# 手动部署
cd dist/cloudflare
npx wrangler deploy
```

## 下一步

- 阅读 [完整文档](./README.md)
- 查看 [示例项目](./examples/)
- 浏览 [API 参考](./docs/api.md)
- 加入 [社区讨论](https://github.com/deislet/denictl/discussions)

## 获取帮助

- [GitHub Issues](https://github.com/deislet/denictl/issues)
- [讨论区](https://github.com/deislet/denictl/discussions)
- [规范文档](https://github.com/deislet/spec)

祝您使用愉快! 🚀
