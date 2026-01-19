// 核心类型定义

/**
 * 应用状态
 */
export type AppState = "loading" | "running" | "stopped" | "error";

/**
 * 权限配置
 */
export interface Permissions {
  net?: boolean | string[]; // 网络访问
  read?: boolean | string[]; // 文件读取
  write?: boolean | string[]; // 文件写入
  env?: boolean | string[]; // 环境变量
  run?: boolean | string[]; // 子进程
}

/**
 * 资源限制
 */
export interface ResourceLimits {
  maxMemoryMB?: number; // 最大内存 (MB)
  maxCpuTimeMS?: number; // 最大 CPU 时间 (毫秒)
  maxConcurrent?: number; // 最大并发请求
  requestTimeoutMS?: number; // 请求超时 (毫秒)
}

/**
 * 应用配置
 */
export interface AppConfig {
  routes?: string[]; // 路由路径
  permissions?: Permissions; // 权限配置
  limits?: ResourceLimits; // 资源限制
  resources?: string[]; // 所需资源列表
  env?: Record<string, string>; // 环境变量
}

/**
 * 应用定义
 */
export interface App {
  id: string; // 应用 ID
  code: string; // 应用代码
  state: AppState; // 应用状态
  config: AppConfig; // 应用配置
  createdAt: Date; // 创建时间
  updatedAt: Date; // 更新时间
  error?: string; // 错误信息
}

/**
 * Worker 请求消息
 */
export interface WorkerRequest {
  type: "execute" | "ping";
  requestId: string;
  request?: {
    url: string;
    method: string;
    headers: Record<string, string>;
    body?: string;
  };
  env?: Record<string, any>;
}

/**
 * Worker 响应消息
 */
export interface WorkerResponse {
  type: "response" | "error" | "pong";
  requestId: string;
  response?: {
    status: number;
    statusText: string;
    headers: Record<string, string>;
    body?: string;
  };
  error?: string;
}

/**
 * 资源接口
 */
export interface Resource {
  type: string; // 资源类型
  name: string; // 资源名称
  config: Record<string, any>; // 资源配置
}

/**
 * 资源提供者接口
 */
export interface ResourceProvider {
  type: string; // 资源类型
  create(appId: string, config: Record<string, any>): Promise<any>; // 创建资源实例
  destroy(instance: any): Promise<void>; // 销毁资源实例
}

/**
 * 应用处理函数接口
 */
export interface AppHandler {
  fetch(request: Request, env?: Record<string, any>): Promise<Response>;
}

/**
 * 路由配置
 */
export interface RouteConfig {
  appId: string;
  pattern: string | RegExp;
}
