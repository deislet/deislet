# Deforge Architecture

本文档详细描述 Deforge 的技术架构和实现细节。

> **注**: Deforge 是一个专注的代码编译器，将标准代码翻译为平台特定代码。

## 整体架构

```
┌─────────────────────────────────────────────────────────────┐
│                        User Code                            │
│  functions/                                                 │
│  ├── index.ts          (Platform-agnostic handlers)        │
│  └── api/hello.ts                                          │
└─────────────────────────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                    Deforge Compiler                         │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐     │
│  │  Rolldown    │→ │  Validator   │→ │   Adapters   │     │
│  │   Bundler    │  │  (Optional)  │  │  (spec.mdc)  │     │
│  └──────────────┘  └──────────────┘  └──────────────┘     │
│         ↓                  ↓                  ↓             │
│  Bundle Code      Check Rules      Platform Code           │
└─────────────────────────────────────────────────────────────┘
                           │
           ┌───────────────┼───────────────┐
           ▼               ▼               ▼
    ┌──────────┐    ┌──────────┐    ┌──────────┐
    │Cloudflare│    │   Deno   │    │ Tencent  │
    │ Adapter  │    │ Adapter  │    │ Adapter  │
    └──────────┘    └──────────┘    └──────────┘
           │               │               │
           ▼               ▼               ▼
    ┌──────────┐    ┌──────────┐    ┌──────────┐
    │ Workers  │    │  Deploy  │    │ EdgeOne  │
    └──────────┘    └──────────┘    └──────────┘
```

## 核心模块

### 1. Types (@denictl/types)

定义所有核心类型和接口:

- **Context**: 统一的运行时上下文
- **Handler**: 处理函数签名
- **Services**: KV、Database、Cache 等服务接口
- **Config**: 项目配置结构
- **Compiler**: 编译器接口

**设计原则**:
- 平台无关: 不包含任何平台特定类型
- 类型安全: 完整的 TypeScript 类型定义
- 扩展性: 易于添加新的服务类型

### 2. CLI (@denictl/cli)

命令行工具,提供用户交互界面:

**命令结构**:
```
denictl
├── init          # 初始化项目
├── dev           # 开发服务器
├── validate      # 验证规范
├── build         # 构建项目
├── deploy        # 部署到平台
└── config        # 配置管理
    ├── set
    ├── get
    └── list
```

**依赖**:
- Commander.js: 命令行解析
- Chalk: 终端颜色输出
- Ora: 加载动画
- Inquirer: 交互式提示

### 3. Compiler (@denictl/compiler)

编译器核心,负责代码解析、验证和转换:

#### 3.1 Parser
解析 TypeScript/JavaScript 代码:

```typescript
class Parser {
  parse(code: string, filename: string): ParsedAST
}
```

**功能**:
- 使用 TypeScript Compiler API 解析代码
- 提取导出信息 (default export, named exports)
- 提取导入依赖
- 识别使用的服务 (KV, Database)

#### 3.2 Validator
验证代码是否符合规范:

```typescript
class Validator {
  validate(ast: ParsedAST, filename: string, config: ProjectConfig): ValidationResult
}
```

**检查项**:
1. **平台类型检测**: 禁止 `typeof Deno`、`typeof Cloudflare`
2. **条件导入**: 禁止动态 import 条件
3. **context.raw 访问**: 禁止访问底层平台对象
4. **平台 API 调用**: 禁止 `Deno.env`、`process.env` 等
5. **导出检查**: 必须有 default export 或 named exports

#### 3.3 Transformer
转换代码为平台特定实现:

```typescript
class Transformer {
  transform(code: string, filename: string, vendor: string, config: ProjectConfig): Promise<string>
  generateEntryPoint(vendor: string, handlers: string[]): string
}
```

**转换流程**:
1. 保持原始 handler 函数不变
2. 生成平台特定的入口点
3. 注入适配器导入
4. 构建 Context 对象

**示例转换**:

输入 (平台无关):
```typescript
export default async function handler(context: Context): Promise<Response> {
  const kv = context.services.kv;
  const value = await kv.get('key');
  return new Response(value);
}
```

输出 (Cloudflare):
```typescript
import { createContextBuilder } from './adapters/context-builder';

const buildContext = createContextBuilder(env, ctx);

export default {
  async fetch(request: Request, env: any, ctx: any) {
    const context = buildContext(request);
    return handler(context);
  }
};

async function handler(context: Context): Promise<Response> {
  const kv = context.services.kv;
  const value = await kv.get('key');
  return new Response(value);
}
```

### 4. Adapters (@denictl/adapters)

平台适配器,将统一接口映射到平台实现:

#### 4.1 Cloudflare Adapter

**KV Adapter**:
```typescript
function createKVAdapter(namespace: KVNamespace): KVStore {
  return {
    get: (key) => namespace.get(key),
    put: (key, value, options) => namespace.put(key, value, {
      expirationTtl: options?.expirationTtl
    }),
    // ...
  };
}
```

**Database Adapter** (D1):
```typescript
function createDatabaseAdapter(d1: D1Database): Database {
  return {
    query: (sql, params) => d1.prepare(sql).bind(...params).all(),
    execute: (sql, params) => d1.prepare(sql).bind(...params).run(),
    // ...
  };
}
```

**Context Builder**:
```typescript
function createContextBuilder(env, ctx) {
  return (request: Request): Context => ({
    request,
    env: filterEnv(env),
    services: {
      kv: env.KV ? createKVAdapter(env.KV) : undefined,
      database: env.DB ? createDatabaseAdapter(env.DB) : undefined
    },
    // ...
  });
}
```

#### 4.2 Deno Adapter

**KV Adapter**:
```typescript
function createKVAdapter(kv: Deno.Kv): KVStore {
  return {
    get: (key) => kv.get([key]).then(r => r.value),
    put: (key, value, options) => kv.set([key], value, {
      expireIn: options?.expirationTtl * 1000
    }),
    // ...
  };
}
```

#### 4.3 Tencent Adapter

待完善,基本结构已建立。

## 编译流程

### 完整编译流程

```
1. 加载项目配置
   ↓
2. 发现所有 handler 文件
   ↓
3. 验证阶段
   ├─ 解析每个 handler
   ├─ 运行规范检查
   └─ 收集错误和警告
   ↓
4. 如果验证失败 → 停止
   ↓
5. 确定目标平台
   ↓
6. 对每个平台:
   ├─ 转换所有 handlers
   ├─ 生成适配器代码
   ├─ 生成平台配置
   ├─ 打包依赖
   └─ 输出到 dist/{vendor}/
   ↓
7. 生成构建报告
```

### 文件路由映射

```
functions/index.ts           → /
functions/about.ts           → /about
functions/api/users.ts       → /api/users
functions/api/posts/[id].ts  → /api/posts/:id
functions/[[catch]].ts       → /* (catch-all)
```

## 配置系统

### 项目配置 (.config.json)

```json
{
  "version": "1.0",
  "name": "my-app",
  "runtime": "standard-v1",
  "language": "typescript",
  "functionRoot": "./functions",
  "environment": {
    "variables": { "LOG_LEVEL": "info" },
    "secrets": ["API_KEY"]
  },
  "services": {
    "kv": { "enabled": true },
    "database": { "enabled": false }
  },
  "vendors": {
    "cloudflare": { "enabled": true },
    "deno": { "enabled": true }
  },
  "build": {
    "outDir": "./dist",
    "minify": true,
    "sourcemap": true
  }
}
```

### 全局配置 (~/.edge-function/config.json)

存储平台认证信息:
```json
{
  "cloudflare": {
    "api-token": "...",
    "account-id": "..."
  },
  "deno": {
    "access-token": "..."
  }
}
```

## 错误处理

### 验证错误级别

1. **Error** (严重错误,停止编译):
   - 平台特定代码检测
   - 语法错误
   - 缺少必需导出

2. **Warning** (警告,继续编译):
   - 平台不支持的功能
   - 性能问题提示

3. **Info** (信息):
   - 编译统计
   - 优化建议

### 错误报告格式

```
✗ Validation failed: 2 errors, 1 warning

Errors:
  functions/api/users.ts:12:5
  Error: Platform-specific code detected
  > if (typeof Deno !== 'undefined') {
    ^^^
  Suggestion: Use unified interfaces from context.services
```

## 性能考虑

### 编译缓存
- 基于文件 hash 缓存已编译的 handlers
- 增量编译:仅重新编译变化的文件
- 依赖分析结果缓存

### 性能目标
- 解析 1000 行代码: < 100ms
- 验证 10 个 handlers: < 500ms
- 完整编译 10 handlers: < 5s

## 扩展性

### 添加新平台

1. 实现平台适配器:
   ```typescript
   // packages/adapters/src/newplatform/
   ├── index.ts
   ├── kv-adapter.ts
   ├── database-adapter.ts
   └── context-builder.ts
   ```

2. 在 Transformer 中添加入口点生成:
   ```typescript
   private generateNewPlatformEntry(handlers: string[]): string {
     // 生成平台特定入口代码
   }
   ```

3. 更新配置类型:
   ```typescript
   interface VendorsConfig {
     newplatform?: VendorConfig;
   }
   ```

### 添加新服务

1. 在 types 中定义接口:
   ```typescript
   export interface Queue {
     send(message: any): Promise<void>;
     receive(): Promise<any>;
   }
   ```

2. 在 Services 中添加:
   ```typescript
   export interface Services {
     queue?: Queue;
   }
   ```

3. 为每个平台实现适配器:
   ```typescript
   export function createQueueAdapter(platformQueue: any): Queue {
     // ...
   }
   ```

## 测试策略

### 单元测试
- Parser: 测试 AST 解析正确性
- Validator: 测试规范检查规则
- Transformer: 测试代码转换输出
- Adapters: 测试接口映射正确性

### 集成测试
- 完整编译流程测试
- 跨平台一致性测试
- CLI 命令执行测试

### E2E 测试
- 实际部署到各平台
- 运行时行为验证
- 性能基准测试

## 未来优化

1. **增量编译**: 只重新编译修改的文件
2. **并行编译**: 多个平台并行构建
3. **Tree-shaking**: 移除未使用代码
4. **Code splitting**: 按需加载
5. **Source maps**: 调试支持
6. **Hot reload**: 开发服务器热重载
7. **Plugin system**: 用户自定义转换
