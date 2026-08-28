# 快速开始指南

本指南将帮助你在 5 分钟内使用 Build 构建你的第一个 Edge Function。

## 前置要求

- **Rust** 1.70+ ([安装 Rust](https://rustup.rs/))
- 符合 [Edge Canon](https://github.com/deislet/edge-canon) 标准的项目

## 安装 Build

### 方式 1: 从源码构建

```bash
# 克隆项目
git clone https://github.com/deislet/deis-build.git
cd deis-build

# 构建并安装
cargo build --release
cargo install --path .
```

### 方式 2: 使用 cargo install (待发布)

```bash
cargo install deis-build
```

### 验证安装

```bash
deis-build --version
# 输出: deis-build 0.1.0
```

## 创建示例项目

### 1. 项目结构

创建以下目录结构：

```
my-edge-app/
├── functions/
│   ├── index.ts          # 根路由 /
│   └── api/
│       └── hello.ts      # /api/hello
└── .config.json          # 项目配置
```

### 2. 配置文件 (.config.json)

```json
{
  "version": "0.1.0",
  "name": "my-edge-app",
  "runtime": "standard-v1",
  "language": "typescript",
  "functionRoot": "./functions",

  "vendors": {
    "cloudflare": { "enabled": true },
    "deno": { "enabled": true }
  },

  "build": {
    "outDir": "./dist",
    "minify": true
  }
}
```

### 3. 创建 Handler

**functions/index.ts**:

```typescript
import type { Context } from '@edge-canon/types';

export default async function handler(context: Context): Promise<Response> {
  return new Response('Hello from Build!', {
    headers: { 'Content-Type': 'text/plain' }
  });
}
```

**functions/api/hello.ts**:

```typescript
import type { Context } from '@edge-canon/types';

export default async function handler(context: Context): Promise<Response> {
  const name = new URL(context.request.url).searchParams.get('name') || 'World';

  // 使用基础服务 - KV Storage
  const { kv } = context.services;
  await kv.put('last-visitor', name);

  return new Response(JSON.stringify({
    message: `Hello, ${name}!`,
    timestamp: new Date().toISOString()
  }), {
    headers: { 'Content-Type': 'application/json' }
  });
}
```

## 使用 Build

### 1. 验证代码

在项目目录下运行：

```bash
deis-build validate
```

输出示例：
```
✓ Validating functions/index.ts
✓ Validating functions/api/hello.ts

Validation Result:
  2/2 files passed
  ✓ No errors found
```

### 2. 构建项目

#### 构建所有平台

```bash
deis-build build
```

输出：
```
Building for cloudflare...
  ⚡ Bundling for cloudflare...
  ✓ Bundled to index.js
  ✓ Build complete: dist/cloudflare

Building for deno...
  ✓ Build complete: dist/deno

Build Result:
  ✓ cloudflare: 2 files, 15.2 KB
  ✓ deno: 2 files, 8.3 KB
```

#### 构建特定平台

```bash
# 仅构建 Cloudflare Workers
deis-build build --vendor cloudflare

# 仅构建 Deno Deploy
deis-build build --vendor deno
```

### 3. 查看输出

```bash
tree dist/
```

输出结构：
```
dist/
├── cloudflare/
│   ├── index.js           # 打包后的入口文件
│   └── wrangler.toml      # Cloudflare 配置
└── deno/
    ├── _deis_internal/
    │   ├── router.ts      # Deno 路由器
    │   └── adapters/
    │       ├── context-builder.ts
    │       └── kv-adapter.ts
    ├── src/               # 转换后的源文件
    │   ├── index.ts
    │   └── api/
    │       └── hello.ts
    └── deno.json          # Deno 配置
```

## 部署到平台

Build 只负责构建，部署需要使用各平台的工具。

### Cloudflare Workers

```bash
cd dist/cloudflare
npx wrangler deploy
```

### Deno Deploy

```bash
cd dist/deno
deployctl deploy --project=my-app _deis_internal/router.ts
```

## 使用库模式

如果你想在 Rust 代码中使用 Build：

```toml
# Cargo.toml
[dependencies]
deis-build = "0.1"
tokio = { version = "1", features = ["full"] }
```

```rust
use deis-build::{Compiler, CompilerOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let options = CompilerOptions {
        project_dir: "./my-edge-app".to_string(),
        target_vendor: Some("cloudflare".to_string()),
        ..Default::default()
    };

    let compiler = Compiler::with_options(options);
    let result = compiler.build().await?;

    println!("✓ 构建成功: {}", result.builds["cloudflare"].output_path);
    Ok(())
}
```

详见 [库模式使用指南](./library-usage.md)。

## 下一步

- 📖 阅读 [架构设计](./architecture.md) 了解 Build 工作原理
- 📖 查看 [输出模式](./output-modes.md) 了解 Files/Memory/Both 模式
- 📖 阅读 [基础服务](./basic-services.md) 了解 KV 和 Cache 使用
- 📖 查看 [Edge Canon 规范](https://github.com/deislet/edge-canon)

## 常见问题

### 验证失败怎么办？

```bash
# 使用严格模式查看详细错误
deis-build validate --strict
```

### 如何启用扩展服务？

在 `.config.json` 中配置：

```json
{
  "services": {
    "database": {
      "enabled": true,
      "binding": "DB"
    }
  }
}
```

> **注意**: 扩展服务需要目标平台支持，否则构建时会报错。

### 如何添加环境变量？

环境变量在部署时配置，不在 Build 的职责范围内：

- Cloudflare: 使用 `wrangler.toml` 或 Dashboard
- Deno: 使用 `deployctl` 或环境文件

---

**遇到问题？** 查看 [Issues](https://github.com/deislet/deis-build/issues) 或提交新问题。
