// Core type definitions

/**
 * Application state
 */
export type AppState = "loading" | "running" | "stopped" | "error";

/**
 * Permission configuration
 */
export interface Permissions {
  net?: boolean | string[]; // Network access
  read?: boolean | string[]; // File read
  write?: boolean | string[]; // File write
  env?: boolean | string[]; // Environment variables
  run?: boolean | string[]; // Subprocesses
}

/**
 * Resource limits
 */
export interface ResourceLimits {
  maxMemoryMB?: number; // Maximum memory (MB)
  maxCpuTimeMS?: number; // Maximum CPU time (milliseconds)
  maxConcurrent?: number; // Maximum concurrent requests
  requestTimeoutMS?: number; // Request timeout (milliseconds)
}

/**
 * Application configuration
 */
export interface AppConfig {
  routes?: string[]; // Route paths
  permissions?: Permissions; // Permission configuration
  limits?: ResourceLimits; // Resource limits
  resources?: string[]; // List of required resources
  env?: Record<string, string>; // Environment variables
}

/**
 * Application definition
 */
export interface App {
  id: string; // Application ID
  code: string; // Application code
  state: AppState; // Application state
  config: AppConfig; // Application configuration
  createdAt: Date; // Creation time
  updatedAt: Date; // Last update time
  error?: string; // Error message
}

/**
 * Worker request message
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
 * Worker response message
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
 * Resource interface
 */
export interface Resource {
  type: string; // Resource type
  name: string; // Resource name
  config: Record<string, any>; // Resource configuration
}

/**
 * Resource provider interface
 */
export interface ResourceProvider {
  type: string; // Resource type
  create(appId: string, config: Record<string, any>): Promise<any>; // Create a resource instance
  destroy(instance: any): Promise<void>; // Destroy a resource instance
}

/**
 * Application handler interface
 */
export interface AppHandler {
  fetch(request: Request, env?: Record<string, any>): Promise<Response>;
}

/**
 * Route configuration
 */
export interface RouteConfig {
  appId: string;
  pattern: string | RegExp;
}
