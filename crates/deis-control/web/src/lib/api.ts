/**
 * Control API Client
 *
 * Single entry point for every dashboard API call. It provides:
 * - bearer token authentication
 * - automatic handling of a 401 by clearing the key and returning to login
 * - uniform error handling
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
   * Read the stored API key.
   */
  getToken: (): string | null => {
    return localStorage.getItem("apiKey");
  },

  /**
   * Store the API key.
   */
  setToken: (token: string): void => {
    localStorage.setItem("apiKey", token);
  },

  /**
   * Forget the stored API key.
   */
  removeToken: (): void => {
    localStorage.removeItem("apiKey");
  },

  /**
   * The one place an HTTP request is issued.
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

      // Unauthenticated: drop the stale key and send the user back to login
      if (res.status === 401) {
        api.removeToken();
        window.location.href = "/";
        throw new ApiError(401, "Unauthorized");
      }

      // Any other non-2xx
      if (!res.ok) {
        const errorText = await res.text();
        throw new ApiError(res.status, errorText || res.statusText);
      }

      // No content
      if (res.status === 204) {
        return null as T;
      }

      // Parse the body
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

  // ==================== Nodes API ====================

  /**
   * List every registered node.
   */
  getNodes: () => api.request<NodeSummary[]>("/api/nodes"),

  /**
   * Fetch one node.
   */
  getNode: (id: string) => api.request<NodeSummary>(`/api/nodes/${id}`),

  /**
   * Fetch a node's heartbeat history.
   */
  getNodeHeartbeats: (id: string, limit = 100) =>
    api.request<Heartbeat[]>(`/api/nodes/${id}/heartbeats?limit=${limit}`),

  // ==================== Configs API ====================

  /**
   * List every configuration.
   */
  getConfigs: () => api.request<{ [nodeId: string]: { [key: string]: any } }>("/api/configs"),

  /**
   * Configuration counts.
   */
  getConfigStats: () => api.request<ConfigStats>("/api/configs/stats"),

  /**
   * Fetch one node's configuration.
   */
  getNodeConfig: (nodeId: string) =>
    api.request<{ [key: string]: any }>(`/api/configs/${nodeId}`),

  /**
   * Update one node's configuration.
   */
  updateNodeConfig: (nodeId: string, configs: { [key: string]: any }) =>
    api.request(`/api/configs/${nodeId}`, {
      method: "POST",
      body: JSON.stringify({ configs }),
    }),

  /**
   * Delete a node's whole configuration.
   */
  deleteNodeConfig: (nodeId: string) =>
    api.request(`/api/configs/${nodeId}`, { method: "DELETE" }),

  /**
   * Delete a single configuration key.
   */
  deleteConfigKey: (nodeId: string, key: string) =>
    api.request(`/api/configs/${nodeId}/${key}`, { method: "DELETE" }),

  /**
   * Configuration change history.
   */
  getConfigHistory: (nodeId: string, limit = 100) =>
    api.request<ConfigHistory[]>(`/api/configs/${nodeId}/history?limit=${limit}`),

  /**
   * Change history for one configuration key.
   */
  getConfigKeyHistory: (nodeId: string, key: string, limit = 50) =>
    api.request<ConfigHistory[]>(`/api/configs/${nodeId}/${key}/history?limit=${limit}`),

  /**
   * Roll a configuration back to an earlier version.
   */
  rollbackConfig: (nodeId: string, key: string, versionTimestamp: string) =>
    api.request(`/api/configs/${nodeId}/${key}/rollback`, {
      method: "POST",
      body: JSON.stringify({ version_timestamp: versionTimestamp }),
    }),

  /**
   * Push a configuration to a node.
   */
  pushConfig: (nodeId: string, key?: string) => {
    const endpoint = key
      ? `/api/configs/push/${nodeId}/${key}`
      : `/api/configs/push/${nodeId}`;
    return api.request(endpoint, { method: "POST" });
  },

  // ==================== Commands API ====================

  /**
   * List every command.
   */
  getCommands: () => api.request<Command[]>("/api/commands"),

  /**
   * List the commands issued to one node.
   */
  getNodeCommands: (nodeId: string) =>
    api.request<Command[]>(`/api/commands/node/${nodeId}`),

  /**
   * Fetch a command's status.
   */
  getCommandStatus: (commandId: string) =>
    api.request<Command>(`/api/commands/status/${commandId}`),

  /**
   * Issue a command.
   */
  executeCommand: (nodeId: string, command: ExecuteCommandRequest) =>
    api.request<{ command_id: string }>(`/api/commands/execute/${nodeId}`, {
      method: "POST",
      body: JSON.stringify(command),
    }),

  /**
   * Cancel a command.
   */
  cancelCommand: (commandId: string) =>
    api.request(`/api/commands/cancel/${commandId}`, { method: "POST" }),

  // ==================== Events API ====================

  /**
   * List events.
   */
  getEvents: (params?: EventsParams) => {
    const query = new URLSearchParams();
    if (params?.node_id) query.set("node_id", params.node_id);
    if (params?.severity) query.set("severity", params.severity);
    if (params?.limit) query.set("limit", params.limit.toString());
    if (params?.offset) query.set("offset", params.offset.toString());

    const queryString = query.toString();
    return api.request<Event[]>(`/api/events${queryString ? `?${queryString}` : ""}`);
  },

  /**
   * Event counts.
   */
  getEventStats: () => api.request<EventStats>("/api/events/stats"),

  // ==================== Metrics API ====================

  /**
   * Metrics summary.
   */
  getMetricsSummary: () => api.request<MetricsSummary>("/api/metrics/summary"),
};

// ==================== Types ====================

export interface NodeSummary {
  id: string;
  instance_id: string;
  node_role: string;
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
  node_id: string;
  instance_id: string;
  timestamp: string;
  power_reserve: number;
  latency_ms?: number;
}

export interface ConfigStats {
  node_count: number;
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
  node_id: string;
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
  node_id?: string;
  severity: "Info" | "Warning" | "Error";
  message: string;
  metadata?: any;
  timestamp: string;
}

export interface EventsParams {
  node_id?: string;
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
  nodes: {
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
