# DeniCTL - Universal Edge Function CLI & Compiler

> 一次编写,随处部署 | Write Once, Deploy Anywhere

DeniCTL 是一个用于 Edge Function 开发和部署的统一工具链,支持多个 serverless 平台。

## 特性

- **统一规范**: 基于 [deislet/spec](https://github.com/deislet/spec) 的标准化 Edge Function 接口
- **多平台支持**: 一键部署到 Cloudflare Workers、Deno Deploy、Tencent EdgeOne 等平台
- **平台无关**: 禁止平台特定代码,编译器自动适配各平台实现
- **完整工具链**: 提供 init、dev、build、validate、deploy 等完整命令
- **类型安全**: 完整的 TypeScript 类型定义
- **高性能**: Rust 实现,快速的编译和验证

## 架构

```
denictl/
└── crates/
    └── denictl/           # CLI 可执行文件 (init / dev / validate / build / deploy / config)
```

解析、验证、编译和平台适配器代码生成全部由同一 workspace 的 `deforge` 提供，denictl 只是它的命令行外壳，另加本地开发服务器（Mock KV + 热重载）。

## 技术思路

### 1. 规范层
定义一套"通用 Serverless Function 项目规范":
- 统一的项目结构 (`functions/` 目录)
- 标准化的 handler 格式 (Context 接口)
- API 输入输出约定
- 环境变量和服务规范

### 2. 适配层
针对每个 vendor 实现适配器:
- **核心 handler 适配**: 将通用 Context 映射到平台特定实现
- **服务适配**: KV Store、Database、Cache 等服务的统一接口
- **配置生成**: 自动生成平台配置文件 (wrangler.toml, deno.json 等)

### 3. CLI 工具层
- 项目管理: `init`、`install`
- 本地开发: `dev` (开发服务器 + 热重载)
- 代码验证: `validate` (检查规范违规)
- 构建打包: `build` (生成各平台产物)
- 部署发布: `deploy` (一键部署到多平台)

## 快速开始

### 安装

```bash
# 克隆项目
git clone https://github.com/deislet/denictl.git
cd denictl

# 构建 (需要 Rust 1.70+)
cargo build --release

# 安装到系统
cargo install --path crates/denictl
```

### 使用

```bash
# 初始化新项目
denictl init my-edge-app
cd my-edge-app

# 安装依赖
npm install

# 启动开发服务器
denictl dev

# 验证项目规范
denictl validate

# 构建所有平台
denictl build

# 部署到生产环境
denictl deploy
```

## CLI 命令

### `init [project-name]`
初始化一个新的 Edge Function 项目,生成标准项目结构。

### `dev [--port PORT]`
启动本地开发服务器,支持热重载和服务模拟。

### `validate [--strict]`
验证项目是否符合规范,检查平台特定代码等违规情况。

### `build [--vendor cloudflare|deno|tencent|all]`
构建项目,生成各平台的编译产物到 `dist/` 目录。

### `deploy [--vendor all] [--env staging|production]`
编译并部署项目到指定平台。

### `config set/get <key> <value>`
管理 CLI 配置 (平台认证、部署参数等)。

## 项目示例

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

## 核心概念

### Context 对象
统一的运行时上下文,包含:
- `request`: 原始 Request 对象
- `env`: 环境变量
- `params`: 路由参数
- `query`: 查询参数
- `services`: 统一服务接口 (KV, Database, Cache)
- `log`: 结构化日志
- `waitUntil`: 后台任务注册

### 平台适配
编译器自动将统一的 handler 代码转换为各平台实现:

```typescript
// 源码 (平台无关)
const kv = context.services.kv;
await kv.put('key', 'value');

// Cloudflare Workers (自动生成)
const kv = createKVAdapter(env.KV_STORE);
await kv.put('key', 'value');

// Deno Deploy (自动生成)
const kv = createKVAdapter(await Deno.openKv());
await kv.put('key', 'value');
```

## 规范检查

编译器会检测并禁止以下模式:

❌ **平台类型检测**
```typescript
if (typeof Deno !== 'undefined') { }  // Error!
```

❌ **条件导入**
```typescript
const mod = await import(condition ? 'a' : 'b');  // Error!
```

❌ **直接访问 context.raw**
```typescript
const cfEnv = context.raw.cloudflare.env;  // Error!
```

❌ **平台特定 API**
```typescript
Deno.env.get('VAR');  // Error! 使用 context.env
```

✅ **正确的方式**
```typescript
const apiKey = context.env.API_KEY;
const kv = context.services.kv;
```

## 开发状态

这是一个 **Rust 实现**版本,包含:

- ✅ 单一 CLI crate，编译链路复用 `deforge`
- ✅ CLI 工具 (使用 clap)
- ✅ TypeScript/JavaScript 解析器 (使用 SWC)
- ✅ 代码验证器 (平台特定代码检测)
- ✅ 代码转换器框架
- ✅ Cloudflare/Deno/Tencent 适配器代码生成
- 🚧 本地开发服务器 (待实现)
- 🚧 完整的代码转换和打包 (待完善)
- 🚧 实际部署集成 (待实现)
- 🚧 测试套件 (待添加)

**技术栈**: Rust + SWC (JavaScript/TypeScript 解析器)

## 路线图

- [ ] 完整的本地开发服务器实现
- [ ] 完善代码转换和优化
- [ ] 实现各平台的实际部署集成
- [ ] 添加完整的测试套件
- [ ] 支持更多平台 (Vercel Edge, Netlify Edge)
- [ ] 插件系统
- [ ] 中间件支持
- [ ] 性能监控和分析

## 参考文档

- [Edge Function 规范](https://github.com/deislet/spec)
- [CLI 和编译器需求](./cli-compiler-requirements.md)

## 贡献

欢迎提交 Issue 和 Pull Request!

## 许可证

MIT License
