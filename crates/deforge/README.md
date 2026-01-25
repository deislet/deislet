# Deforge - Universal Edge Function Compiler

> 标准代码到平台代码的纯粹翻译器 | Pure Translator from Standard to Platform-Specific Code

**Deforge** 是一个专注的代码编译器，将符合 [Edge Canon](https://github.com/deislet/edge-canon) 标准的 Edge Function 代码翻译为特定平台的可部署代码。

## 🎯 核心定位

**Deforge 只做一件事：翻译代码**

```
输入：符合 Edge Canon 标准的代码库
输出：特定平台（Cloudflare/Deno/Tencent/Deislet）的可部署代码
```

## ✨ 特性

- **纯粹翻译器**: 专注于代码转换，不涉及项目管理、开发服务器或部署
- **多平台支持**: 一次编写，翻译到 Cloudflare Workers、Deno Deploy、Tencent EdgeOne、Deislet
- **标准验证**: 确保输入代码符合 Edge Canon 规范，检测平台特定代码
- **平台能力验证**: 编译时检查目标平台是否支持使用的扩展特性
- **自动适配**: 自动生成路由器、适配器和配置文件
- **类型安全**: 基于 Rust + SWC，保证转换的正确性

## 📦 架构

```
deforge/
├── src/
│   ├── main.rs              # CLI 入口 (validate + build)
│   ├── compiler/            # 编译器逻辑
│   ├── parser/              # TypeScript/JavaScript 解析器
│   └── commands/            # CLI 命令实现
├── crates/
│   ├── deforge-core/        # 核心类型、配置、平台能力系统
│   ├── deforge-adapter-cf/  # Cloudflare Workers 适配器
│   ├── deforge-adapter-dc/  # Deno Deploy 适配器
│   └── deforge-adapter-eo/  # EdgeOne Pages 适配器
└── BASIC_SERVICES.md        # 基础服务文档
```

### 关键组件

- **平台能力系统** (`platform_capabilities.rs`): 编译时验证平台特性支持
- **适配器系统**: 为每个平台生成符合其规范的代码
- **验证器**: 检测平台特定代码和规范违规

## 🚀 快速开始

### 安装

```bash
# 克隆项目
git clone https://github.com/deislet/deforge.git
cd deforge

# 构建 (需要 Rust 1.70+)
cargo build --release

# 安装到系统
cargo install --path crates/deforge
```

### 使用

#### 1. 验证代码

检查代码是否符合标准，检测平台特定代码：

```bash
deforge validate [--strict]
```

输出示例：
```
✓ Validating functions/api/users.ts
✓ Validating functions/index.ts
✗ Error in functions/bad.ts:10:5
  Platform-specific code detected: typeof Deno !== 'undefined'

2/3 files passed, 1 error
```

#### 2. 构建项目

将标准代码翻译为特定平台代码：

```bash
# 构建所有平台
deforge build

# 构建特定平台
deforge build --vendor cloudflare
deforge build --vendor deno
deforge build --vendor tencent

# 监视模式（开发时使用）
deforge build --watch
```

输出目录结构：
```
dist/
├── cloudflare/
│   ├── index.ts              # 自动生成的路由器
│   ├── wrangler.toml         # Cloudflare 配置
│   ├── src/                  # 转换后的 handlers
│   └── adapters/             # KV/Context 适配器
├── deno/
│   ├── index.ts              # Deno.serve 入口点
│   ├── deno.json             # Deno 配置
│   └── src/                  # 转换后的代码
└── tencent/
    ├── edge-functions/
    │   └── index.js          # EdgeOne Pages 入口点
    ├── edgeone.json          # EdgeOne 配置
    └── src/                  # 转换后的代码和静态资源
```

## 📝 输入格式

Deforge 期望符合以下规范的代码：

### 项目结构

```
your-project/
├── functions/              # 必须：函数目录
│   ├── index.ts           # 根路由 /
│   ├── api/
│   │   ├── users.ts       # /api/users
│   │   └── [id].ts        # /api/:id （动态路由）
│   └── admin/
│       └── index.ts       # /admin
└── .config.json           # 必须：项目配置
```

### Handler 格式

```typescript
// functions/api/users.ts
import type { Context } from '@denictl/types';

export default async function handler(context: Context): Promise<Response> {
  const { kv } = context.services;
  const userId = context.params.id;

  // 使用统一的 KV 接口 (自动适配各平台)
  const user = await kv.getJSON(`user:${userId}`);

  if (!user) {
    return new Response('User not found', { status: 404 });
  }

  return new Response(JSON.stringify(user), {
    headers: { 'Content-Type': 'application/json' }
  });
}
```

### Context 对象

统一的运行时上下文：

```typescript
interface Context {
  request: Request;                // 原始 Request 对象
  env: Record<string, string>;     // 环境变量
  params: Record<string, string>;  // 路由参数

  // 基础服务（所有平台必须支持）
  services: {
    kv: KVStore;                   // 键值存储（无需配置）
    cache: Cache;                  // HTTP 缓存（无需配置）

    // 扩展服务（需要配置，平台可选支持）
    database?: Database;           // SQL 数据库
    blob?: BlobStore;              // 对象存储
    queue?: Queue;                 // 消息队列
  };

  log: Logger;                     // 结构化日志
  waitUntil: (promise: Promise<any>) => void;
}
```

> **注意**: KV Storage 和 Cache API 是基础服务，无需在配置中启用。详见 [BASIC_SERVICES.md](./BASIC_SERVICES.md)

## 🔍 验证规则

Deforge 会检测并禁止以下模式：

❌ **平台类型检测**
```typescript
if (typeof Deno !== 'undefined') { }  // Error!
```

❌ **直接访问 context.raw**
```typescript
const cfEnv = context.raw.cloudflare.env;  // Error!
```

❌ **平台特定 API**
```typescript
Deno.env.get('VAR');  // Error! 使用 context.env
process.env.VAR;      // Error! 使用 context.env
```

✅ **正确的方式**
```typescript
const apiKey = context.env.API_KEY;
const kv = context.services.kv;
```

## 🎨 输出示例

### 输入 (functions/api/hello.ts)
```typescript
export default async function handler(context: Context): Promise<Response> {
  const name = context.query.get('name') || 'World';
  return new Response(`Hello, ${name}!`);
}
```

### 输出 (dist/cloudflare/src/api/hello.ts)
```typescript
/**
 * Cloudflare Workers Handler
 * Generated from: functions/api/hello.ts
 * Route: /api/hello
 */

import { createContextBuilder } from '../adapters/context-builder';

// Original handler code (保持不变)
export default async function handler(context: Context): Promise<Response> {
  const name = context.query.get('name') || 'World';
  return new Response(`Hello, ${name}!`);
}

// Cloudflare Workers export (自动生成)
export default {
  async fetch(request, env, ctx) {
    const buildContext = createContextBuilder(env, ctx);
    const context = buildContext(request, '/api/hello');
    return await handler(context);
  }
};
```

### 输出 (dist/cloudflare/index.ts) - 自动路由器
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

## 🛠️ 工作流程集成

Deforge 专注于编译，可与其他工具配合使用：

```bash
# 1. 验证代码
deforge validate

# 2. 构建目标平台代码
deforge build --vendor cloudflare

# 3. 使用平台工具部署（不是 Deforge 的职责）
cd dist/cloudflare
npx wrangler deploy
```

## 🔧 配置文件

必需的 `.config.json` 配置文件（Edge Canon 标准）：

```json
{
  "version": "0.1.0",
  "name": "my-edge-app",
  "runtime": "standard-v1",
  "language": "typescript",
  "functionRoot": "./functions",

  "services": {
    // 扩展服务（需要显式启用）
    "database": {
      "enabled": true,
      "binding": "DB"
    }
  },

  "vendors": {
    "cloudflare": { "enabled": true },
    "deno": { "enabled": true },
    "tencent": { "enabled": true }
  },

  "build": {
    "outDir": "./dist",
    "minify": true,
    "sourceMap": false
  }
}
```

> **注意**: KV 和 Cache 无需配置，详见 [BASIC_SERVICES.md](./BASIC_SERVICES.md)

## 🏗️ 与其他工具的关系

**Deforge 不做的事情：**
- ❌ 项目初始化 → 使用 `create-deislet-app`
- ❌ 开发服务器 → 使用 `deislet-dev`
- ❌ 部署管理 → 使用 wrangler/deployctl 或 `deislet-deploy`

**Deforge 专注于：**
- ✅ 代码验证
- ✅ 代码转换
- ✅ 适配器生成
- ✅ 配置文件生成

## 📊 性能

- **验证速度**: ~2s (100 个文件)
- **构建速度**: ~5s (100 个handlers)
- **二进制大小**: ~15MB (release)
- **内存占用**: ~30MB

## 📚 相关文档

### Edge Canon 规范
- [基础规范 (Basic)](https://github.com/deislet/edge-canon/blob/main/SPECIFICATION_BASIC.md) - 所有平台必须支持的能力
- [扩展规范 (Extended)](https://github.com/deislet/edge-canon/blob/main/SPECIFICATION_EXT.md) - 平台可选支持的特性
- [平台支持矩阵](https://github.com/deislet/edge-canon/blob/main/PLATFORM_MATRIX.md) - 详细兼容性信息

### Deforge 文档
- [BASIC_SERVICES.md](./BASIC_SERVICES.md) - KV 和 Cache 基础服务说明
- [ARCHITECTURE.md](./ARCHITECTURE.md) - 架构设计文档
- [ERROR_SYSTEM.md](./crates/deforge-core/ERROR_SYSTEM.md) - 错误分类系统

### 平台适配器
每个平台适配器都有详细的规范文档 (`spec.mdc`)：
- **Cloudflare Workers** (`crates/deforge-adapter-cf/spec.mdc`)
- **Deno Deploy** (`crates/deforge-adapter-dc/spec.mdc`)
- **EdgeOne Pages** (`crates/deforge-adapter-eo/spec.mdc`)

## 🤝 贡献

欢迎贡献！请查看 [CONTRIBUTING.md](./CONTRIBUTING.md)

## 📄 许可证

Apache-2.0 License

---

**Deforge**: Pure. Focused. Efficient.
