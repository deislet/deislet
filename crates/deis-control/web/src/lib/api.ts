/**
 * Control API Client
 *
 * 统一的 API 请求封装，支持：
 * - Bearer Token 认证
 * - 自动 401 处理
 * - 错误处理
 */

const API_BASE = import.meta.env.VITE_API_BASE || "";

export class ApiError extends Error {
  status: number;

  constructor(status: number, message: string) {
    super(message);
    this.status = status;
    this.name = "ApiError";
  }
}

export const api = {
  /**
   * 获取存储的 API Key
   */
  getToken: (): string | null => {
    return localStorage.getItem("apiKey");
  },

  /**
   * 设置 API Key
   */
  setToken: (token: string): void => {
    localStorage.setItem("apiKey", token);
  },

  /**
   * 移除 API Key
   */
  removeToken: (): void => {
    localStorage.removeItem("apiKey");
  },

  /**
   * 统一的 HTTP 请求方法
   */
  request: async <T = any>(
    endpoint: string,
    options: RequestInit = {}
  ): Promise<T> => {
    const token = api.getToken();

    const headers: HeadersInit = {
      "Content-Type": "application/json",
      ...(token ? { "X-API-Key": token } : {}),
      ...options.headers,
    };

    const url = `${API_BASE}${endpoint}`;

    try {
      const res = await fetch(url, {
        ...options,
        headers,
      });

      // 401 未认证 - 自动跳转登录
      if (res.status === 401) {
        api.removeToken();
        window.location.href = "/";
        throw new ApiError(401, "Unauthorized");
      }

      // HTTP 错误
      if (!res.ok) {
        const errorText = await res.text();
        throw new ApiError(res.status, errorText || res.statusText);
      }

      // 204 无内容
      if (res.status === 204) {
        return null as T;
      }

      // 解析响应
      const contentType = res.headers.get("content-type");
      if (contentType && contentType.includes("application/json")) {
        return await res.json();
      }

      return (await res.text()) as T;
    } catch (error) {
      if (error instanceof ApiError) {
        throw error;
      }
      throw new ApiError(0, error instanceof Error ? error.message : "Network error");
    }
  },

  // ==================== Workers API ====================

  /**
   * 获取所有 workers
   */
  getWorkers: () => api.request<Worker[]>("/api/workers"),

  /**
   * 获取单个 worker 详情
   */
  getWorker: (id: string) => api.request<Worker>(`/api/workers/${id}`),

  /**
   * 获取 worker 心跳历史
   */
  getWorkerHeartbeats: (id: string, limit = 100) =>
    api.request<Heartbeat[]>(`/api/workers/${id}/heartbeats?limit=${limit}`),

  // ==================== Configs API ====================

  /**
   * 获取所有配置
   */
  getConfigs: () => api.request<{ [workerId: string]: { [key: string]: any } }>("/api/configs"),

  /**
   * 获取配置统计
   */
  getConfigStats: () => api.request<ConfigStats>("/api/configs/stats"),

  /**
   * 获取 worker 配置
   */
  getWorkerConfig: (workerId: string) =>
    api.request<{ [key: string]: any }>(`/api/configs/${workerId}`),

  /**
   * 更新 worker 配置
   */
  updateWorkerConfig: (workerId: string, configs: { [key: string]: any }) =>
    api.request(`/api/configs/${workerId}`, {
      method: "POST",
      body: JSON.stringify({ configs }),
    }),

  /**
   * 删除 worker 配置
   */
  deleteWorkerConfig: (workerId: string) =>
    api.request(`/api/configs/${workerId}`, { method: "DELETE" }),

  /**
   * 删除单个配置项
   */
  deleteConfigKey: (workerId: string, key: string) =>
    api.request(`/api/configs/${workerId}/${key}`, { method: "DELETE" }),

  /**
   * 获取配置历史
   */
  getConfigHistory: (workerId: string, limit = 100) =>
    api.request<ConfigHistory[]>(`/api/configs/${workerId}/history?limit=${limit}`),

  /**
   * 获取配置键历史
   */
  getConfigKeyHistory: (workerId: string, key: string, limit = 50) =>
    api.request<ConfigHistory[]>(`/api/configs/${workerId}/${key}/history?limit=${limit}`),

  /**
   * 回滚配置
   */
  rollbackConfig: (workerId: string, key: string, versionTimestamp: string) =>
    api.request(`/api/configs/${workerId}/${key}/rollback`, {
      method: "POST",
      body: JSON.stringify({ version_timestamp: versionTimestamp }),
    }),

  /**
   * 推送配置到 worker
   */
  pushConfig: (workerId: string, key?: string) => {
    const endpoint = key
      ? `/api/configs/push/${workerId}/${key}`
      : `/api/configs/push/${workerId}`;
    return api.request(endpoint, { method: "POST" });
  },

  // ==================== Commands API ====================

  /**
   * 获取所有命令
   */
  getCommands: () => api.request<Command[]>("/api/commands"),

  /**
   * 获取 worker 的命令
   */
  getWorkerCommands: (workerId: string) =>
    api.request<Command[]>(`/api/commands/worker/${workerId}`),

  /**
   * 获取命令状态
   */
  getCommandStatus: (commandId: string) =>
    api.request<Command>(`/api/commands/status/${commandId}`),

  /**
   * 执行命令
   */
  executeCommand: (workerId: string, command: ExecuteCommandRequest) =>
    api.request<{ command_id: string }>(`/api/commands/execute/${workerId}`, {
      method: "POST",
      body: JSON.stringify(command),
    }),

  /**
   * 取消命令
   */
  cancelCommand: (commandId: string) =>
    api.request(`/api/commands/cancel/${commandId}`, { method: "POST" }),

  // ==================== Events API ====================

  /**
   * 获取事件列表
   */
  getEvents: (params?: EventsParams) => {
    const query = new URLSearchParams();
    if (params?.worker_id) query.set("worker_id", params.worker_id);
    if (params?.severity) query.set("severity", params.severity);
    if (params?.limit) query.set("limit", params.limit.toString());
    if (params?.offset) query.set("offset", params.offset.toString());

    const queryString = query.toString();
    return api.request<Event[]>(`/api/events${queryString ? `?${queryString}` : ""}`);
  },

  /**
   * 获取事件统计
   */
  getEventStats: () => api.request<EventStats>("/api/events/stats"),

  // ==================== Metrics API ====================

  /**
   * 获取指标摘要
   */
  getMetricsSummary: () => api.request<MetricsSummary>("/api/metrics/summary"),
};

// ==================== 类型定义 ====================

export interface Worker {
  id: string;
  instance_id: string;
  worker_type: string;
  version: string;
  host: string;
  ports: number[];
  state: "Connected" | "Registered" | "Active" | "Disconnected";
  connected_at: string;
  last_heartbeat: string;
  power_reserve: number;
  metadata?: any;
}

export interface Heartbeat {
  worker_id: string;
  instance_id: string;
  timestamp: string;
  power_reserve: number;
  latency_ms?: number;
}

export interface ConfigStats {
  worker_count: number;
  total_configs: number;
}

export interface ConfigHistory {
  id: number;
  config_key: string;
  config_value: any;
  version_timestamp: string;
  operation: "create" | "update" | "delete";
  created_at: string;
}

export interface Command {
  id: string;
  worker_id: string;
  command_type: string;
  command_data: any;
  status: "Pending" | "Sent" | "Acknowledged" | "Completed" | "Failed" | "Timeout";
  created_at: string;
  sent_at?: string;
  completed_at?: string;
  timeout_at?: string;
  response_data?: any;
  error_message?: string;
}

export interface ExecuteCommandRequest {
  command_type: string;
  command_data?: any;
  timeout?: number;
}

export interface Event {
  event_type: string;
  worker_id?: string;
  severity: "Info" | "Warning" | "Error";
  message: string;
  metadata?: any;
  timestamp: string;
}

export interface EventsParams {
  worker_id?: string;
  severity?: string;
  limit?: number;
  offset?: number;
}

export interface EventStats {
  total: number;
  by_severity: { [key: string]: number };
  by_type: { [key: string]: number };
}

export interface MetricsSummary {
  workers: {
    total: number;
    active: number;
    connected: number;
    registered: number;
    disconnected: number;
    by_type: { [key: string]: number };
  };
  system: {
    uptime_seconds: number;
    total_events: number;
    total_commands: number;
    total_heartbeats: number;
  };
}
