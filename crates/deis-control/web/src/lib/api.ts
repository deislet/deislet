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

  // ==================== Control channels ====================

  /**
   * List the nodes currently holding a control channel open to the control
   * plane. This is not the runtime fleet - see `getFleet`.
   */
  getSessions: () => api.request<{ sessions: SessionSummary[]; total: number }>("/api/sessions"),

  /**
   * Fetch one open control channel.
   */
  getSession: (id: string) => api.request<SessionSummary>(`/api/sessions/${id}`),

  /**
   * Fetch a node's heartbeat history.
   */
  getSessionHeartbeats: (id: string, limit = 100) =>
    api.request<Heartbeat[]>(`/api/sessions/${id}/heartbeats?limit=${limit}`),

  // ==================== Runtime fleet ====================

  /**
   * List the runtime nodes that receive deployments and run cron
   * occurrences, whether they came from the control plane's configuration
   * file or registered themselves.
   */
  getFleet: () => api.request<{ nodes: FleetNode[] }>("/api/runtime"),

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
    api.request<{ success: boolean; command_id: string; message: string }>(
      `/api/commands/execute/${nodeId}`,
      { method: "POST", body: JSON.stringify(command) }
    ),

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

/**
 * One node holding a control channel open to the control plane, as
 * `/api/sessions` reports it.
 */
export interface SessionSummary {
  id: string;
  instance_id: string;
  node_role: string;
  host: string;
  ports: number[];
  state: "Connected" | "Registered" | "Active" | "Disconnected";
  connected_at: string;
  last_heartbeat: string;
}

/**
 * One member of the runtime fleet, as `/api/runtime` reports it. A node is
 * here because the control plane's configuration names it or because it
 * registered itself, which is what `source` says - holding a control channel
 * open has nothing to do with it.
 */
export interface FleetNode {
  id: string;
  addr: string;
  source: "configured" | "registered";
  registered_at: string | null;
  last_registered_at: string | null;
  healthy: boolean;
  version: string | null;
  last_checked: string | null;
  last_error: string | null;
}

/**
 * One recorded heartbeat. It carries no load figure: the wire's heartbeat
 * message is a status string, so there is nothing else to report.
 */
export interface Heartbeat {
  node_id: string;
  instance_id: string;
  timestamp: string;
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

/**
 * Exactly the body `POST /api/commands/execute/:node_id` deserializes.
 *
 * `command_type` is a tagged enum: the named commands are plain strings, and
 * anything else is `{ custom: "<name>" }`. The payload field is `data`, not
 * `command_data` - the server rejects the whole request with 422 if it is
 * spelled any other way, which is what used to happen to the Restart button.
 */
export type CommandType =
  | "shutdown"
  | "restart"
  | "reload"
  | "get_status"
  | "get_metrics"
  | "execute_scheduled"
  | { custom: string };

export interface ExecuteCommandRequest {
  command_type: CommandType;
  data?: unknown;
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
