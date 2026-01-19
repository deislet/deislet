# Denix 架构设计文档

## 设计理念

Denix 的核心目标是提供一个轻量级、安全、易扩展的多租户 Web 平台。设计遵循以下原则：

1. **隔离优先**: 每个租户应用必须完全隔离，互不影响
2. **动态灵活**: 支持运行时动态加载、更新应用，无需重启平台
3. **资源可控**: 细粒度的资源访问控制和配额管理
4. **简单高效**: API 简洁，性能开销最小化

## 核心架构

### 1. Runtime Isolate (运行时隔离层)

**职责**: 为每个应用提供独立的 JavaScript 运行时环境

**实现方式**:
- 使用 Deno Worker API 创建独立的 V8 isolate
- 每个 Worker 运行在独立的线程中
- Worker 之间通过消息传递通信

**核心接口**:
```typescript
class RuntimeIsolate {
  constructor(appId: string, code: string, permissions: Permissions)
  async execute(request: Request, env: Record<string, any>): Promise<Response>
  terminate(): void
}
```

**隔离特性**:
- V8 isolate 级别的内存隔离
- 独立的全局作用域
- 可配置的权限 (网络、文件系统、环境变量)
- 资源限制 (内存、CPU 时间)

**通信协议**:
```typescript
// 主线程 -> Worker
type WorkerRequest = {
  type: "execute";
  requestId: string;
  request: SerializedRequest;
  env: Record<string, any>;
}

// Worker -> 主线程
type WorkerResponse = {
  type: "response" | "error";
  requestId: string;
  response?: SerializedResponse;
  error?: string;
}
```

### 2. Application Manager (应用管理层)

**职责**: 管理应用的完整生命周期

**核心功能**:
- 应用注册与发现
- 应用加载与卸载
- 应用更新 (热更新)
- 路由映射管理

**状态管理**:
```typescript
type AppState = "loading" | "running" | "stopped" | "error";

interface App {
  id: string;
  code: string;
  state: AppState;
  isolate: RuntimeIsolate | null;
  routes: string[];
  config: AppConfig;
  createdAt: Date;
  updatedAt: Date;
}
```

**生命周期**:
```
[Create] -> [Loading] -> [Running] -> [Stopping] -> [Stopped]
                |            |
                v            v
            [Error]      [Updating]
```

**API**:
```typescript
class ApplicationManager {
  async loadApp(id: string, code: string, config: AppConfig): Promise<void>
  async unloadApp(id: string): Promise<void>
  async updateApp(id: string, code: string): Promise<void>
  async executeApp(id: string, request: Request): Promise<Response>
  getApp(id: string): App | undefined
  listApps(): App[]
}
```

### 3. Resource Manager (资源管理层)

**职责**: 管理应用可访问的外部资源

**资源类型**:
- KV Storage (键值存储)
- Database (数据库连接)
- HTTP Client (外部 HTTP 请求)
- File System (文件系统访问)
- Environment Variables (环境变量)

**资源抽象**:
```typescript
interface Resource {
  type: string;
  name: string;
  config: Record<string, any>;
}

interface ResourceProvider {
  type: string;
  create(config: Record<string, any>): any;
  destroy(instance: any): Promise<void>;
}
```

**注入机制**:
资源通过 `env` 对象注入到应用中：
```typescript
// 应用代码
export default {
  async fetch(request, env) {
    // env.kv - KV 存储
    // env.db - 数据库
    // env.fetch - HTTP 客户端
    await env.kv.set("key", "value");
  }
}
```

**访问控制**:
- 每个应用声明所需资源
- 平台验证并注入授权的资源
- 资源配额限制 (请求数、存储量等)

### 4. HTTP Gateway (HTTP 网关层)

**职责**: 接收外部 HTTP 请求并路由到对应应用

**路由策略**:
1. **路径路由**: `/apps/{appId}/*`
2. **域名路由**: `{appId}.denix.local`
3. **自定义路由**: 应用自定义路由规则

**请求处理流程**:
```
[HTTP Request]
    |
    v
[Route Matching] -> 404 Not Found
    |
    v
[App Lookup] -> 404 App Not Found
    |
    v
[Execute in Isolate] -> 500 Internal Error
    |
    v
[Return Response]
```

**特性**:
- 健康检查: `GET /health`
- 管理 API: `/api/*`
- 应用路由: `/apps/{appId}/*`
- 超时控制
- 错误处理

### 5. 数据流向

```
HTTP Request
    │
    ▼
┌─────────────────┐
│  HTTP Gateway   │
│   (路由解析)     │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│App Manager      │
│ (查找应用)       │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│Resource Manager │
│ (注入资源)       │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│Runtime Isolate  │
│ (执行应用代码)   │
└────────┬────────┘
         │
         ▼
    Response
```

## 安全模型

### 1. 隔离边界
- **进程级隔离**: 主进程 vs Worker 进程
- **内存隔离**: 独立的 V8 isolate
- **权限隔离**: Deno 权限系统

### 2. 权限控制
```typescript
interface Permissions {
  net?: boolean | string[];  // 网络访问
  read?: boolean | string[]; // 文件读取
  write?: boolean | string[]; // 文件写入
  env?: boolean | string[];  // 环境变量
  run?: boolean | string[];  // 子进程
}
```

### 3. 资源限制
```typescript
interface ResourceLimits {
  maxMemoryMB?: number;      // 最大内存
  maxCpuTimeMS?: number;     // 最大 CPU 时间
  maxConcurrent?: number;    // 最大并发请求
  requestTimeoutMS?: number; // 请求超时
}
```

## 扩展性设计

### 1. 资源插件系统
- 标准化的 ResourceProvider 接口
- 动态注册新资源类型
- 支持第三方资源插件

### 2. 中间件系统
- 请求前置处理
- 响应后置处理
- 全局错误处理

### 3. 监控与日志
- 应用性能指标
- 资源使用统计
- 错误日志收集

## 性能优化

### 1. Worker 池管理
- 预创建 Worker 池
- Worker 复用机制
- 按需扩缩容

### 2. 代码缓存
- 应用代码编译缓存
- 模块依赖缓存

### 3. 请求优化
- Keep-Alive 连接
- HTTP/2 支持
- 响应压缩

## 后续演进方向

1. **分布式部署**: 支持多节点集群
2. **持久化存储**: 应用配置和状态持久化
3. **监控面板**: Web UI 管理界面
4. **CI/CD 集成**: Git 集成，自动部署
5. **边缘计算**: 支持边缘节点部署
6. **容器集成**: 可选的 Docker 容器隔离

## 参考实现

- Deno Deploy: https://deno.com/deploy
- Cloudflare Workers: https://workers.cloudflare.com/
- AWS Lambda: https://aws.amazon.com/lambda/
