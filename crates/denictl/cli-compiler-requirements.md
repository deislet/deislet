# Edge Function CLI & Compiler Requirements

> 编译器和 CLI 工具的需求说明文档

---

## 概述

本文档定义了 Edge Function 生态系统中的两个核心工具的需求：

1. **CLI 工具** — 用户交互层，提供命令行接口用于初始化、开发、构建和部署
2. **编译器** — 后端核心，负责源码验证、转换、适配和优化

两者紧密协作，共同实现"一次编写、随处部署"的目标。

---

## 1. CLI 工具需求

### 1.1 定位和职责

CLI 工具是开发者与 Edge Function 生态的主要交互界面。主要职责包括：

- **项目管理** — 初始化、安装、构建项目
- **本地开发** — 启动开发服务器、热重载、本地模拟
- **编译发布** — 调用编译器，自动部署到各平台
- **配置管理** — 管理平台认证、部署参数等
- **诊断工具** — 验证规范、检查兼容性、查看日志

### 1.2 命令列表与详细需求

#### 1.2.1 `init [project-name]`

**功能**：初始化一个新的 Edge Function 项目

**参数**：
- `project-name` (可选)：项目名称，默认为 `edge-app`

**行为**：
1. 创建项目目录和标准文件结构
2. 生成模板 `.config.json`
3. 生成模板 `package.json`
4. 生成模板 `.env` 和 `.env.production`
5. 创建 `functions/` 目录和示例文件：
   - `functions/index.ts` — Hello World 示例
   - `functions/api/hello.ts` — API 示例
6. 生成 `.gitignore`
7. 生成 `README.md` 包含快速开始指南

**示例**：
```bash
$ init my-app
$ cd my-app
$ tree
my-app/
├── functions/
│   ├── index.ts
│   └── api/
│       └── hello.ts
├── .config.json
├── .env
├── .env.production
├── package.json
└── README.md
```

**输出**：项目初始化成功的提示和后续建议（如 `dev`、`deploy` 命令）

---

#### 1.2.2 `install`

**功能**：安装项目依赖

**行为**：
1. 检查 `package.json` 中的依赖
2. 执行 npm/yarn/pnpm 安装（基于项目中存在的 lock 文件）
3. 如果项目中包含 Deno 配置，也安装 Deno 依赖

**输出**：依赖安装进度和结果

---

#### 1.2.3 `dev [--port PORT] [--watch] [--debug]`

**功能**：启动本地开发服务器

**参数**：
- `--port PORT` (默认 8000)：监听端口
- `--watch` (默认启用)：监视文件变化自动重新加载
- `--debug` (可选)：输出详细日志

**行为**：
1. 验证项目结构和 `.config.json`
2. 加载 `.env` 文件到环境变量
3. 启动本地 HTTP 服务器
4. 监视 `functions/` 目录文件变化
5. 文件变化时：
   - 重新编译变化的 handler
   - 热重载（不需要重启服务器）
6. 模拟平台服务（KV、数据库等）
7. 监听 stdin，允许快捷命令：
   - `r` - 重新加载所有 handlers
   - `d` - 打开 REPL/调试模式
   - `c` - 清空日志
   - `q` - 退出

**输出**：
```
┌──────────────────────────────────────────────────────┐
│                  Edge Function Dev                   │
├──────────────────────────────────────────────────────┤
│ Local:   http://localhost:8000                       │
│ Network: http://192.168.1.100:8000                  │
├──────────────────────────────────────────────────────┤
│ Routes:                                              │
│   GET  /                                             │
│   GET  /api/hello                                    │
│                                                      │
│ Services:                                            │
│   ✓ KV Store (mocked)                               │
│   ✓ Database (SQLite)                               │
│   ✓ Cache                                            │
├──────────────────────────────────────────────────────┤
│ Watching: functions/                                 │
│ Press [r] reload, [d] debug, [c] clear, [q] quit   │
└──────────────────────────────────────────────────────┘
```

**本地模拟实现**：
- KV Store → 内存 Map + 持久化到 `.cache/kv.db`
- Database → SQLite (`.cache/dev.db`)
- Cache → 内存 LRU

---

#### 1.2.4 `validate [--strict]`

**功能**：验证项目是否符合规范

**参数**：
- `--strict` (可选)：严格模式，更多警告

**行为**：
1. 检查项目结构：
   - `functions/` 目录是否存在
   - `.config.json` 是否存在且有效
   - handler 文件是否符合命名规范
2. 检查 `.config.json` 配置：
   - 必需字段是否存在
   - 版本号是否兼容
   - vendors 配置是否有效
3. 解析所有 handler 文件，检查：
   - 是否有默认导出或命名导出
   - 导出签名是否符合规范
   - TypeScript 语法是否正确
4. **规范检查**（重要）：
   - 扫描代码检查是否有平台特定代码：
     - 检测 `typeof Deno`、`typeof Cloudflare` 等
     - 检测条件导入 (e.g., `import(condition ? ...)`)
     - 检测直接 `env` 访问模式 (e.g., `env.MY_NAMESPACE`)
     - 检测 `context.raw` 访问
   - 报告所有违规代码位置和错误
5. 检查服务兼容性：
   - 检查所有使用的 KV/Database 功能是否被配置的 vendors 支持
   - 如果某个 vendor 不支持某个功能，给出警告

**输出示例**：

```
✓ Project structure valid
✓ .config.json valid
✓ 5 handler files found
✗ Validation failed: 2 errors, 1 warning

Errors:
  functions/api/users.ts:12:5
  Error: Platform-specific code detected
  > if (typeof Deno !== 'undefined') {
    ^^^
  You cannot check for platform type. Use unified interfaces only.

  functions/api/posts.ts:45:10
  Error: Direct env access detected
  > const kv = env.MY_KV;
    ^^^
  Use context.services.kv instead. Direct env access is not allowed.

Warnings:
  functions/cache.ts:20:5
  Warning: Database transaction used but Deno vendor might not support it fully
  (This is just a warning, it should work but may have limitations)

Fix the errors above and run 'validate' again.
```

---

#### 1.2.5 `build [--vendor cloudflare|deno|tencent|all] [--watch]`

**功能**：构建项目，生成各平台的编译产物

**参数**：
- `--vendor cloudflare|deno|tencent|all` (默认 all)：只构建特定平台或全部
- `--watch` (可选)：监视源文件，变化时自动重新构建

**行为**：
1. 运行 `validate` 检查
2. 如果 validate 失败，停止构建并报错
3. 根据 `.config.json` 的 `vendors` 配置和 CLI 参数，决定要构建的平台
4. 对于每个要构建的平台：
   - 调用编译器生成适配产物
   - 输出到 `./dist/{vendor}/` 目录
5. 生成构建报告

**输出示例**：

```
Building for: cloudflare, deno, tencent

[1/3] Building for Cloudflare...
  ✓ Compiled 7 handlers
  ✓ Generated wrangler.toml
  ✓ Output: dist/cloudflare/ (245 KB)

[2/3] Building for Deno Deploy...
  ✓ Compiled 7 handlers
  ✓ Generated deno.json
  ✓ Output: dist/deno/ (198 KB)

[3/3] Building for Tencent EdgeOne...
  ✓ Compiled 7 handlers
  ✓ Generated edgeone.toml
  ✓ Output: dist/tencent/ (212 KB)

Build completed in 1.23s
Total output: 655 KB
```

---

#### 1.2.6 `test [--watch] [--coverage]`

**功能**：运行项目测试

**参数**：
- `--watch` (可选)：监视文件变化，自动重新运行测试
- `--coverage` (可选)：生成覆盖率报告

**行为**：
1. 在项目中查找 `*.test.ts`、`*.spec.ts` 或 `__tests__/` 目录
2. 使用 Vitest 或 Jest 运行测试
3. 在模拟的 Edge Function 环境中执行测试
4. 提供本地 Context mock，方便测试

**测试 helper 示例**：
```typescript
// test helper 由 CLI 提供
import { createMockContext } from '@edge-function/test-utils';

describe('api/users', () => {
  it('should list users', async () => {
    const context = createMockContext({
      method: 'GET',
      path: '/api/users',
      services: { database: mockDB }
    });
    
    const response = await handler(context);
    expect(response.status).toBe(200);
  });
});
```

---

#### 1.2.7 `deploy [--vendor cloudflare|deno|tencent|all] [--env production|staging] [--tag TAG] [--promote]`

**功能**：编译并部署项目到一个或多个平台

**参数**：
- `--vendor` (默认从 `.config.json` 的 `vendors.*.enabled`)：部署到哪些平台
- `--env` (默认 staging)：部署环境
- `--tag` (可选)：版本标签，用于部署历史追踪
- `--promote` (可选)：部署后立即升级为生产环境

**行为**：
1. 运行 `build`
2. 如果 build 失败，停止部署
3. 对于每个要部署的平台：
   - 检查平台认证配置（如 Cloudflare API Token）
   - 调用平台的 CLI 或 API 进行部署
   - 等待部署完成，检查状态
4. 生成部署报告，包含：
   - 部署 ID
   - 时间戳
   - 各平台的部署 URL
   - 当前版本标签

**部署前检查**：
- 检查必需的环境变量（如 `CF_API_TOKEN`、`DENO_DEPLOY_TOKEN` 等）
- 提示用户确认部署到的平台和环境
- 给出部署后可能的影响

**输出示例**：

```
🔨 Building...
  ✓ Cloudflare build completed
  ✓ Deno build completed
  ✓ Tencent build completed

📤 Deploying to staging...

[1/3] Deploying to Cloudflare...
  ✓ Uploaded to edge-functions-staging
  ✓ URL: https://staging.myapp.workers.dev
  ✓ Deployment ID: abc123def456

[2/3] Deploying to Deno Deploy...
  ✓ Uploaded to myapp-staging
  ✓ URL: https://myapp-staging.deno.dev
  ✓ Deployment ID: deno-deploy-123

[3/3] Deploying to Tencent EdgeOne...
  ✓ Uploaded to myapp-staging
  ✓ URL: https://myapp-staging.edgeone.ai
  ✓ Deployment ID: tencent-456

✅ Deployment completed successfully
📋 Deployment ID: edge-func-20250113-140559
🏷️  Tag: v1.0.0
🌍 All platforms deployed
```

---

#### 1.2.8 `deployments list [--vendor cloudflare] [--limit 10] [--json]`

**功能**：查看部署历史

**参数**：
- `--vendor` (可选)：只显示特定平台的部署历史
- `--limit` (默认 10)：最多显示的部署记录数
- `--json` (可选)：输出为 JSON 格式

**行为**：
1. 从本地 `.edge-function/deployments.json` 读取部署历史
2. 从各平台的 API 查询最新部署信息
3. 合并、排序并展示

**输出示例**：

```
Recent deployments:

  ID                          Time              Env         Status  Vendors
  ─────────────────────────────────────────────────────────────────────────
  edge-func-20250113-140559   2025-01-13 14:05  staging     ✓       all
  edge-func-20250113-120315   2025-01-13 12:03  production  ✓       all
  edge-func-20250112-185642   2025-01-12 18:56  staging     ✓       cf, deno
  edge-func-20250112-140201   2025-01-12 14:02  production  ✗       cf (failed)
  edge-func-20250111-093012   2025-01-11 09:30  production  ✓       all

Run 'preview --deployment <id>' to preview a deployment
Run 'rollback --deployment <id>' to rollback to a deployment
```

---

#### 1.2.9 `preview --deployment <id>`

**功能**：预览特定部署的状态

**参数**：
- `--deployment <id>` (必需)：部署 ID

**行为**：
1. 从部署历史中查找部署
2. 连接到各平台查询部署状态和日志
3. 显示：
   - 部署信息（时间、环境、版本）
   - 各平台的部署 URL
   - 近期日志摘要
   - 是否可以回滚

**输出示例**：

```
Deployment: edge-func-20250113-140559
Created: 2025-01-13 14:05:32 UTC
Environment: staging
Tag: v1.0.0
Status: ✓ Active

Platforms:
  ✓ Cloudflare  - https://staging.myapp.workers.dev
  ✓ Deno Deploy - https://myapp-staging.deno.dev
  ✓ Tencent     - https://myapp-staging.edgeone.ai

Recent logs:
  [14:05:45] Handler compiled successfully
  [14:05:48] Deployed to Cloudflare
  [14:05:52] Deployed to Deno Deploy
  [14:06:01] Deployed to Tencent EdgeOne
  [14:06:05] All platforms online

You can rollback to this deployment with:
  rollback --deployment edge-func-20250113-140559
```

---

#### 1.2.10 `rollback [--deployment <id>] [--vendor cloudflare|all]`

**功能**：回滚到上一个（或指定的）部署

**参数**：
- `--deployment <id>` (可选)：回滚到指定的部署 ID。如果不指定，回滚到上一个部署
- `--vendor` (默认 all)：只回滚特定平台

**行为**：
1. 获取部署历史
2. 确定要回滚到的版本
3. 提示用户确认（显示版本号、时间、内容摘要）
4. 调用各平台的回滚 API
5. 等待回滚完成

**输出示例**：

```
Rolling back from edge-func-20250113-140559 to edge-func-20250113-120315...

Current:  v1.0.1 (2025-01-13 14:05)
Target:   v1.0.0 (2025-01-13 12:03)

Confirm? (y/n): y

[1/3] Rolling back Cloudflare...
  ✓ Reverted to v1.0.0

[2/3] Rolling back Deno Deploy...
  ✓ Reverted to v1.0.0

[3/3] Rolling back Tencent EdgeOne...
  ✓ Reverted to v1.0.0

✅ Rollback completed successfully
```

---

#### 1.2.11 `logs [--deployment <id>] [--vendor cloudflare] [--tail] [--lines 100]`

**功能**：查看部署的日志

**参数**：
- `--deployment <id>` (可选)：查看特定部署的日志，默认为当前部署
- `--vendor` (可选)：只显示特定平台的日志
- `--tail` (可选)：实时尾随日志
- `--lines` (默认 100)：显示最近 N 行日志

**行为**：
1. 连接到各平台的日志服务
2. 获取并聚合日志
3. 按时间戳排序显示

**输出示例**：

```
Logs for deployment: edge-func-20250113-140559

[Cloudflare]
2025-01-13T14:05:45.123Z  GET   /api/users         200  45ms
2025-01-13T14:05:46.456Z  POST  /api/users         201  52ms
2025-01-13T14:05:47.789Z  GET   /api/posts/1       200  38ms
2025-01-13T14:05:48.012Z  GET   /                  200  12ms

[Deno Deploy]
2025-01-13T14:05:45.134Z  GET   /api/users         200  48ms
2025-01-13T14:05:46.467Z  POST  /api/users         201  55ms
2025-01-13T14:05:47.790Z  GET   /api/posts/1       200  41ms

[Tencent EdgeOne]
2025-01-13T14:05:45.145Z  GET   /api/users         200  42ms
2025-01-13T14:05:46.478Z  POST  /api/users         201  58ms

Press Ctrl+C to exit
```

---

#### 1.2.12 `config set <key> <value>` 和 `config get <key>`

**功能**：管理 CLI 配置（平台认证、部署参数等）

**参数**：
- `<key>`：配置键（如 `cloudflare.api-token`, `deno.project-name`）
- `<value>`：配置值（仅 `set` 命令需要）

**行为**：
1. 配置存储在 `~/.edge-function/config.json` 或项目级 `.edge-function/config.json`
2. `config set` — 存储配置（敏感信息加密存储）
3. `config get` — 读取配置

**常见配置键**：
```
cloudflare.api-token           # Cloudflare API Token
cloudflare.account-id          # Cloudflare Account ID
cloudflare.zone-id             # Cloudflare Zone ID

deno.project-name              # Deno Deploy 项目名
deno.access-token              # Deno Deploy Token

tencent.secret-id              # 腾讯 EdgeOne Secret ID
tencent.secret-key             # 腾讯 EdgeOne Secret Key
tencent.project-id             # 腾讯 EdgeOne Project ID
```

**示例**：
```bash
$ config set cloudflare.api-token sk_live_xxxx
✓ Configuration saved

$ config get cloudflare.api-token
sk_live_xxxx
```

---

### 1.3 CLI 的技术要求

#### 实现语言
- **推荐**：Node.js + TypeScript（或 Rust）
- **原因**：生态成熟、易于扩展、与前端开发者习惯一致

#### 框架和库
- 命令行框架：Commander.js 或 Yargs
- 文件监视：Chokidar
- 日志：Winston 或 Pino
- 表格显示：Chalk + Table 库
- API 调用：Fetch API 或 Axios

#### 本地模拟
- KV Store：SQLite3
- Database：SQLite3 + sql.js
- HTTP 服务器：Express 或 Hono
- 文件系统模拟：内存 FS

#### 配置文件格式
- 全局配置：`~/.edge-function/config.json`
- 项目配置：`.edge-function/config.json` 或 `./config.json`
- 敏感信息加密存储（使用 crypto）

#### 性能要求
- `init` — < 2 秒
- `dev` 启动 — < 5 秒
- `build` — < 30 秒（对于 10+ handlers）
- `validate` — < 10 秒
- 文件变化到热重载 — < 2 秒

---

## 2. 编译器需求

### 2.1 定位和职责

编译器是 Edge Function 生态的核心。主要职责包括：

- **源码解析** — 解析 TypeScript/JavaScript handler 代码
- **规范检查** — 检测并禁止平台特定代码
- **代码转换** — 将统一接口的代码转换为平台特定实现
- **依赖管理** — 管理和编译依赖
- **优化** — Tree-shaking、minify、代码分割
- **输出生成** — 生成各平台的编译产物（包含适配器、配置等）

### 2.2 编译流程

#### 第一阶段：验证

```
源码
  ↓
[1] 解析 .config.json
  ├─ 检查必需字段
  ├─ 验证版本兼容性
  └─ 加载 vendor 配置
  ↓
[2] 扫描 functions/ 目录
  ├─ 发现所有 handler 文件
  ├─ 建立路由映射
  └─ 检查命名规范
  ↓
[3] 解析每个 handler 文件
  ├─ TypeScript 类型检查
  ├─ 代码 AST 分析
  └─ 检查导出签名
  ↓
[4] 规范检查
  ├─ 扫描平台检测代码 → 错误
  ├─ 扫描条件导入 → 错误
  ├─ 扫描 context.raw 访问 → 错误
  ├─ 扫描直接 env 访问 → 错误
  └─ 如有违规，停止编译并报错
  ↓
[5] 依赖分析
  ├─ 提取所有 import 语句
  ├─ 检查 node_modules 依赖
  └─ 检查是否有平台特定依赖 → 错误
  ↓
✓ 验证通过 或 ✗ 验证失败
```

#### 第二阶段：为每个 vendor 生成编译产物

```
对于每个 vendor (cloudflare / deno / tencent):
  ↓
[6] 加载平台适配器
  ├─ KV 适配器
  ├─ Database 适配器
  ├─ Cache 适配器
  └─ HTTP 适配器
  ↓
[7] 转换 handler 代码
  ├─ 替换 Context 对象构造
  ├─ 重定向 services 访问到适配器
  ├─ 重定向 log 方法到平台实现
  └─ 生成平台特定的入口点
  ↓
[8] 打包依赖
  ├─ 执行 Tree-shaking
  ├─ 合并依赖到 bundle
  └─ 生成 vendor 特定的 package.json / deno.json
  ↓
[9] 生成平台配置文件
  ├─ Cloudflare: wrangler.toml
  ├─ Deno: deno.json
  └─ Tencent: edgeone.toml
  ↓
[10] 优化输出
  ├─ Minify 代码（如果启用）
  ├─ Source maps（如果启用）
  └─ 生成优化统计信息
  ↓
[11] 输出到 dist/{vendor}/
```

### 2.3 核心模块详细需求

#### 2.3.1 代码解析模块 (Parser)

**功能**：解析 TypeScript/JavaScript 代码，提取信息

**输入**：handler 文件路径和内容

**输出**：AST 和元数据

**需要检测的信息**：
- 导出类型（default export、named exports）
- 导出函数签名（onRequest、onRequestGet 等）
- 使用的 Context 属性（哪些 services、env 字段）
- 导入的依赖（第三方库）
- 平台特定代码（需要禁止）

**实现**：
- 使用 TypeScript Compiler API 或 Babel 进行解析
- 构建 AST
- 遍历 AST 提取信息

#### 2.3.2 规范检查模块 (Linter)

**功能**：检测代码中的规范违规

**检查项**：

1. **平台类型检测**
   ```typescript
   // ❌ Error
   if (typeof Deno !== 'undefined') { }
   if (typeof Cloudflare !== 'undefined') { }
   if (globalThis.Deno) { }
   ```

2. **条件导入**
   ```typescript
   // ❌ Error
   const mod = await import(condition ? 'a' : 'b');
   ```

3. **直接 env 访问（平台命名空间）**
   ```typescript
   // ❌ Error
   const kv = env.MY_KV;  // 假设 MY_KV 是平台 binding
   const db = env.DB;     // 假设 DB 是平台 binding
   
   // ✓ OK (通用环境变量)
   const apiBase = env.API_BASE;
   const logLevel = env.LOG_LEVEL;
   ```
   
   **启发式规则**：
   - 如果 env 键在 `.config.json` 的 `environment.variables` 或 `environment.secrets` 中声明 → OK
   - 否则 → 可能是平台特定 → 警告或错误

4. **context.raw 访问**
   ```typescript
   // ❌ Error
   if (context.raw?.cloudflare) { }
   const cfEnv = context.raw.cloudflare.env;
   ```

5. **平台特定 API 调用**
   ```typescript
   // ❌ Error
   Deno.env.get('VAR');  // 应使用 context.env.VAR
   Cloudflare.requestHeaders;
   ```

**输出**：违规报告，包含：
- 违规位置（文件、行号、列号）
- 违规代码片段
- 错误/警告描述
- 修复建议

#### 2.3.3 代码转换模块 (Transformer)

**功能**：将统一的 handler 代码转换为平台特定实现

**转换示例**：

**源码**：
```typescript
// functions/api/users.ts
export default async function handler(context: Context): Promise<Response> {
  const kv = context.services.kv;
  const userId = context.params.id;
  
  const cached = await kv.get(`user:${userId}`);
  if (cached) return new Response(cached);
  
  const user = { id: userId, name: 'John' };
  await kv.putJSON(`user:${userId}`, user, { expirationTtl: 3600 });
  
  return new Response(JSON.stringify(user), {
    headers: { 'Content-Type': 'application/json' }
  });
}
```

**转换为 Cloudflare**：
```typescript
// dist/cloudflare/src/api/users.ts
import { createKVAdapter } from '../adapters/kv';

export default {
  fetch: async (request: Request, env: any, ctx: any) => {
    // 构建统一的 Context 对象
    const url = new URL(request.url);
    const context = {
      request,
      env,
      params: extractParams(url.pathname, '/api/users/:id'),
      path: url.pathname,
      method: request.method as any,
      query: Object.fromEntries(url.searchParams),
      headers: request.headers,
      cookies: new CookieStore(request),
      cache: caches,
      log: console,
      waitUntil: (p) => ctx.waitUntil(p),
      services: {
        kv: createKVAdapter(env.KV_STORE),
      }
    };
    
    // 调用原始 handler
    return handler(context);
  }
};

async function handler(context: Context): Promise<Response> {
  // 原始代码，不变
  const kv = context.services.kv;
  const userId = context.params.id;
  
  const cached = await kv.get(`user:${userId}`);
  if (cached) return new Response(cached);
  
  const user = { id: userId, name: 'John' };
  await kv.putJSON(`user:${userId}`, user, { expirationTtl: 3600 });
  
  return new Response(JSON.stringify(user), {
    headers: { 'Content-Type': 'application/json' }
  });
}
```

**转换为 Deno**：
```typescript
// dist/deno/functions/api/users.ts
import { createKVAdapter } from '../adapters/kv.ts';

const kv = createKVAdapter(await Deno.openKv());

export async function onRequest(request: Request): Promise<Response> {
  const url = new URL(request.url);
  const context = {
    request,
    env: Deno.env.toObject(),
    params: extractParams(url.pathname, '/api/users/:id'),
    path: url.pathname,
    method: request.method as any,
    query: Object.fromEntries(url.searchParams),
    headers: request.headers,
    cookies: new CookieStore(request),
    cache: new DenoCache(),
    log: console,
    waitUntil: (p) => {},
    services: { kv }
  };
  
  return handler(context);
}

async function handler(context: Context): Promise<Response> {
  // 原始代码，不变
  const kv = context.services.kv;
  const userId = context.params.id;
  
  const cached = await kv.get(`user:${userId}`);
  if (cached) return new Response(cached);
  
  const user = { id: userId, name: 'John' };
  await kv.putJSON(`user:${userId}`, user, { expirationTtl: 3600 });
  
  return new Response(JSON.stringify(user), {
    headers: { 'Content-Type': 'application/json' }
  });
}
```

**关键点**：
- handler 函数本身保持不变
- 仅改变入口点和 Context 构建方式
- 适配器将统一的 services 接口映射到平台 API

#### 2.3.4 适配器生成模块 (AdapterGenerator)

**功能**：为每个 vendor 生成服务适配器代码

**生成的适配器**：

1. **KV 适配器** — 统一 KVStore 接口到平台实现
2. **Database 适配器** — 统一 Database 接口到平台实现
3. **Cache 适配器** — 统一 CacheAPI 接口到平台实现

**适配器要求**：
- 100% 实现所有接口方法
- 处理平台差异（如 TTL 的时间单位）
- 错误处理和重试逻辑
- 性能优化（如批量操作）

**示例 - Cloudflare KV 适配器**：

```typescript
import type { KVStore, PutOptions, GetOptions, ListOptions, ListResult } from '../types';

export function createKVAdapter(namespace: any): KVStore {
  return {
    async get(key: string, options?: GetOptions): Promise<string | null> {
      return namespace.get(key);
    },
    
    async getJSON<T = any>(key: string, options?: GetOptions): Promise<T | null> {
      const value = await namespace.get(key, 'json');
      return value as T | null;
    },
    
    async getWithMetadata(key: string) {
      const { value, metadata } = await namespace.getWithMetadata(key);
      return { value, metadata };
    },
    
    async put(key: string, value: string, options?: PutOptions): Promise<void> {
      await namespace.put(key, value, {
        expirationTtl: options?.expirationTtl,
        metadata: options?.metadata
      });
    },
    
    async putJSON<T = any>(key: string, value: T, options?: PutOptions): Promise<void> {
      await namespace.put(key, JSON.stringify(value), {
        expirationTtl: options?.expirationTtl,
        metadata: options?.metadata
      });
    },
    
    async delete(key: string): Promise<void> {
      await namespace.delete(key);
    },
    
    async deleteMultiple(keys: string[]): Promise<void> {
      await Promise.all(keys.map(k => namespace.delete(k)));
    },
    
    async list(options?: ListOptions): Promise<ListResult> {
      const result = await namespace.list(options);
      return {
        keys: result.keys,
        list_complete: result.list_complete,
        cursor: result.cursor
      };
    },
    
    async exists(key: string): Promise<boolean> {
      return (await namespace.get(key)) !== null;
    }
  };
}
```

#### 2.3.5 配置生成模块 (ConfigGenerator)

**功能**：为每个 vendor 生成配置文件

**输出文件**：

**Cloudflare (wrangler.toml)**：
```toml
name = "my-edge-app"
main = "src/index.ts"
compatibility_date = "2024-01-01"

[env.staging]
route = "staging.myapp.workers.dev"
zone_id = "..."

[env.production]
route = "myapp.workers.dev"
zone_id = "..."

[[kv_namespaces]]
binding = "KV_STORE"
id = "..."
preview_id = "..."
```

**Deno (deno.json)**：
```json
{
  "tasks": {
    "start": "deno serve -A functions/index.ts"
  },
  "imports": {
    "std/": "https://deno.land/std@0.208.0/"
  }
}
```

**Tencent (edgeone.toml)**：
```toml
name = "my-edge-app"
language = "typescript"
runtime = "nodejs"
entry_point = "functions/index.ts"

[environments.staging]
url = "staging.myapp.edgeone.ai"

[environments.production]
url = "myapp.edgeone.ai"
```

#### 2.3.6 优化模块 (Optimizer)

**功能**：优化编译输出

**优化项**：
- Tree-shaking — 移除未使用的代码
- Minification — 压缩代码
- Code splitting — 按需加载
- Dependency analysis — 分析和优化依赖

**输出统计**：
```
Before optimization:
  Total: 245 KB
  Handlers: 187 KB
  Dependencies: 58 KB
  
After optimization (minify + tree-shake):
  Total: 64 KB (73% reduction)
  Handlers: 42 KB
  Dependencies: 22 KB
```

### 2.4 编译器的技术要求

#### 实现语言
- **推荐**：Rust （性能好，大文件快速处理）或 Node.js (TypeScript)

#### 核心库
- 代码解析：
  - Node.js: TypeScript Compiler API / Babel
  - Rust: tree-sitter 或 swc
- 代码生成：ts-morph (Node.js) 或手工 AST 操作
- 打包：Esbuild / Rollup / Webpack
- 类型检查：TypeScript

#### 性能指标
- 解析 1000 行代码：< 100ms
- 规范检查 10 个 handlers：< 500ms
- 完整编译 10 handlers：< 5s
- 依赖分析：< 1s

#### 缓存策略
- 缓存已编译的 handlers（hash-based）
- 缓存依赖解析结果
- 增量编译（仅重新编译变化的文件）

---

## 3. 集成与交互

### 3.1 CLI 与编译器的交互

```
CLI (index.ts)
    ↓
    ├─→ build 命令
    │     ↓
    │     [加载 .config.json]
    │     ↓
    │     [调用编译器.validate()]
    │     ├─ 验证失败 → 输出错误，停止
    │     └─ 验证成功 → 继续
    │     ↓
    │     [调用编译器.compile(vendors)]
    │     ├─ 对于每个 vendor：
    │     │   ├─ 调用 transform()
    │     │   ├─ 调用 generateAdapters()
    │     │   ├─ 调用 generateConfig()
    │     │   ├─ 调用 bundle()
    │     │   └─ 输出到 dist/{vendor}/
    │     └─ 编译完成
    │     ↓
    │     [输出构建报告]
    │
    └─→ deploy 命令
          ↓
          [调用 build()]
          ↓
          [对于每个 vendor，调用平台 CLI/API]
          ├─ Cloudflare: wrangler publish
          ├─ Deno: deno deploy
          └─ Tencent: edgeone deploy
          ↓
          [输出部署报告]
```

### 3.2 错误处理

#### 编译器错误级别

1. **Fatal Error** — 停止编译
   - 语法错误
   - 规范违规（平台特定代码）
   - 类型检查错误

2. **Warning** — 继续编译但输出警告
   - 某个 vendor 不支持的功能
   - 潜在的性能问题

3. **Info** — 仅输出信息
   - 编译统计
   - 优化建议

#### CLI 错误处理
- 捕获编译器抛出的错误
- 友好地输出错误信息
- 提供修复建议
- 返回非零 exit code

---

## 4. 扩展点

### 4.1 支持更多平台

添加新平台的步骤：
1. 实现平台特定的适配器（KV、DB、Cache）
2. 实现配置生成器（生成平台配置文件）
3. 实现平台兼容性检查
4. 测试完整编译和部署流程

### 4.2 插件系统

未来可支持用户自定义：
- 自定义中间件
- 自定义优化规则
- 自定义验证规则

---

## 5. 测试策略

### 5.1 编译器单元测试

- 代码解析：正确解析各种 handler 写法
- 规范检查：正确检测和禁止平台特定代码
- 代码转换：正确转换为各平台实现
- 适配器生成：适配器正确实现所有接口

### 5.2 集成测试

- 完整编译流程：源码 → 各平台产物
- 编译产物验证：产物可在各平台正确运行
- 跨平台一致性：同一源码在各平台表现一致

### 5.3 CLI 集成测试

- 命令执行：各命令正确执行
- 部署流程：完整部署到实际平台
- 错误处理：错误场景的恰当处理

---

## 6. 文档需求

### 6.1 用户文档
- CLI 命令参考
- 快速开始指南
- 最佳实践

### 6.2 开发者文档
- 编译器架构
- 适配器开发指南
- 扩展指南

---

## 附录 A：配置文件示例

`.config.json`:
```json
{
  "version": "1.0",
  "name": "my-app",
  "runtime": "standard-v1",
  "language": "typescript",
  "functionRoot": "./functions",
  "environment": {
    "variables": { "LOG_LEVEL": "info" },
    "secrets": ["API_KEY", "DB_URL"]
  },
  "services": {
    "kv": { "enabled": true },
    "database": { "enabled": true }
  },
  "vendors": {
    "cloudflare": { "enabled": true },
    "deno": { "enabled": true },
    "tencent": { "enabled": true }
  }
}
```

---

## 附录 B：编译器 API

```typescript
interface Compiler {
  // 验证源码
  validate(projectDir: string): ValidationResult;
  
  // 编译为各平台
  compile(projectDir: string, vendors: string[]): Promise<CompilationResult>;
  
  // 单个转换
  transformHandler(handler: string, vendor: string): Promise<string>;
  
  // 生成适配器
  generateAdapters(vendor: string): Promise<AdapterCode>;
}

interface ValidationResult {
  valid: boolean;
  errors: ValidationError[];
  warnings: ValidationWarning[];
}

interface CompilationResult {
  success: boolean;
  vendors: Record<string, VendorBuild>;
  duration: number;
}

interface VendorBuild {
  path: string;
  files: string[];
  size: number;
  errors?: string[];
}
```

---

**文档完成**
