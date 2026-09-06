/**
 * Control API Client
 *
 * Single entry point for every console API call. It provides:
 * - API-key authentication
 * - one place that reacts to a 401, by dropping the key and telling the app
 * - one place that reads the control plane's error envelope
 * - the two path prefixes the control plane serves: `API_V1` for the
 *   product API and `API_PLATFORM` for platform diagnostics, which is every
 *   route this console reads today
 */

const API_BASE = import.meta.env.VITE_API_BASE || "";

/**
 * Prefix of the product API: applications, versions, environments,
 * deployments, secrets, accounts, tokens and the audit log. Nothing in this
 * console reads it yet; `request` is the way in when something does.
 */
export const API_V1 = "/api/v1";

/**
 * Prefix of the platform diagnostics API: control channels, the runtime
 * fleet, configurations, commands, events and the metrics summary.
 */
export const API_PLATFORM = "/api/platform";

/**
 * The response header carrying the id the control plane logged a request
 * under. The error envelope repeats it as `requestId`; on a refusal from
 * outside the envelope (the authorization gate) the header is all there is.
 */
const REQUEST_ID_HEADER = "x-request-id";

/**
 * One field a request got wrong, as the control plane names it in the
 * envelope's `violations`.
 */
export interface Violation {
  field: string;
  code: string;
  message: string;
}

/**
 * A refused call.
 *
 * `message` is the sentence to show a person, with the request id appended
 * when the control plane gave one - that id is what an operator searches the
 * control plane's log for. `code` is the stable kebab-case class from the
 * envelope (`not-found`, `invalid-argument`, ...), absent on a refusal that
 * arrived in the flat legacy shape or with no body at all.
 */
export class ApiError extends Error {
  status: number;
  code?: string;
  requestId?: string;
  violations: Violation[];
  retryable: boolean;

  constructor(status: number, message: string, detail: RefusalDetail = {}) {
    super(detail.requestId ? `${message} (request ${detail.requestId})` : message);
    this.name = "ApiError";
    this.status = status;
    this.code = detail.code;
    this.requestId = detail.requestId;
    this.violations = detail.violations ?? [];
    this.retryable = detail.retryable ?? false;
  }
}

interface RefusalDetail {
  code?: string;
  requestId?: string;
  violations?: Violation[];
  retryable?: boolean;
}

/**
 * The body of a refusal on `/api/v1` and `/api/platform` -
 * `contracts/v1/control-error.schema.json`. Read leniently: the members the
 * console shows are the code, the message, the request id and the
 * violations, and a member added there later must not stop them from being
 * read.
 */
interface ErrorEnvelope {
  error: {
    code: string;
    message: string;
    requestId?: string;
    violations?: Violation[];
    retryable?: boolean;
  };
}

function isErrorEnvelope(value: unknown): value is ErrorEnvelope {
  const error = asPlainObject(asPlainObject(value)?.error);
  return typeof error?.code === "string" && typeof error?.message === "string";
}

/**
 * Turn a non-2xx answer into an `ApiError`, whichever shape it arrived in:
 * the versioned envelope, the flat legacy `{ "error": "<message>" }` that
 * the authorization gate still answers with, or no JSON at all.
 */
function refusal(res: Response, text: string): ApiError {
  const headerId = res.headers.get(REQUEST_ID_HEADER) ?? undefined;
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    parsed = undefined;
  }
  if (isErrorEnvelope(parsed)) {
    const { code, message, requestId, violations, retryable } = parsed.error;
    return new ApiError(res.status, message, {
      code,
      requestId: requestId ?? headerId,
      violations: Array.isArray(violations) ? violations : [],
      retryable: retryable === true,
    });
  }
  const flat = asPlainObject(parsed)?.error;
  const message = typeof flat === "string" ? flat : text || res.statusText;
  return new ApiError(res.status, message, { requestId: headerId });
}

/**
 * One page of the control plane's unified list envelope -
 * `contracts/v1/control-list-page.schema.json`. Every paginated route on
 * both prefixes answers this exact shape.
 */
export interface ListPage<T> {
  schemaVersion: number;
  items: T[];
  page: PageMeta;
}

export interface PageMeta {
  /** The page size actually applied - the server's default when the request named none. */
  limit: number;
  /** How many rows this page carries; equal to `items.length`. */
  count: number;
  /** How many rows the same query has in total, counted in the same read. */
  total: number;
  /** The token that reads the page after this one; `null` on the last. */
  nextCursor: string | null;
}

/** What a caller may say about which page of a list it wants. */
export interface PageQuery {
  limit?: number;
  cursor?: string | null;
}

/**
 * How many pages `listAll` follows before giving up on a control plane that
 * keeps handing out a cursor. At the server's default page size that is
 * ten thousand rows, more than any diagnostic list this console walks.
 */
const MAX_LIST_PAGES = 100;

/**
 * Read a paginated route to its end by following `nextCursor`.
 *
 * For the few reads that want the whole list rather than its newest page. The
 * polling pages deliberately do not use it: they re-read the first page every
 * few seconds, and walking a history that has no ceiling on every tick is
 * exactly what a cursor is meant to spare the control plane.
 */
export async function listAll<T>(
  readPage: (cursor: string | null) => Promise<ListPage<T>>
): Promise<T[]> {
  const items: T[] = [];
  let cursor: string | null = null;
  for (let page = 1; page <= MAX_LIST_PAGES; page += 1) {
    const read: ListPage<T> = await readPage(cursor);
    items.push(...read.items);
    cursor = read.page.nextCursor;
    if (cursor === null) return items;
    if (read.items.length === 0) {
      throw new ApiError(0, "the control plane answered an empty page and still offered another");
    }
  }
  throw new ApiError(0, `the control plane is still offering more pages after ${MAX_LIST_PAGES}`);
}

/** Append `limit=` and `cursor=` to a route, keeping any query it already has. */
function paged(route: string, query: PageQuery = {}): string {
  const params = new URLSearchParams();
  if (query.limit !== undefined) params.set("limit", String(query.limit));
  if (query.cursor) params.set("cursor", query.cursor);
  const encoded = params.toString();
  if (!encoded) return route;
  return `${route}${route.includes("?") ? "&" : "?"}${encoded}`;
}

/**
 * Whoever wants to know that the control plane refused the stored key.
 *
 * A 401 used to be answered with `window.location.href = "/"`. That is a full
 * page load of the console itself, and the console starts polling five
 * endpoints the moment it mounts - so with `api_auth_enabled = true`, which is
 * the posture an operator is told to run, the first poll answered 401, the tab
 * reloaded, the poll ran again, and the console reloaded itself for as long as
 * it was open. Nothing was ever rendered that a person could put a key into,
 * and the control plane took a fresh round of requests every reload for as
 * long as the tab was left there.
 *
 * So a 401 is an event now. The key is dropped, the listeners are told, and
 * nothing navigates: the app decides what to show, which is the one screen
 * that can get the console out of this state.
 */
type UnauthorizedListener = () => void;

const unauthorizedListeners = new Set<UnauthorizedListener>();

function announceUnauthorized(): void {
  // A copy, because a listener may unsubscribe itself while being told.
  for (const listener of [...unauthorizedListeners]) {
    listener();
  }
}

/**
 * The one place an HTTP request is issued. `url` is already absolute within
 * the console's origin; `request` and `platformRequest` below put the prefix
 * on.
 *
 * `unknown`, not `any`, when a caller does not name `T`: an unread response
 * needs no shape, and one that is read has to be narrowed first, which is
 * what catches a caller assuming a field that is not actually there.
 */
async function send<T = unknown>(url: string, options: RequestInit = {}): Promise<T> {
  const token = api.getToken();

  const headers: HeadersInit = {
    Accept: "application/json",
    "Content-Type": "application/json",
    ...(token ? { "X-API-Key": token } : {}),
    ...options.headers,
  };

  try {
    const res = await fetch(url, {
      ...options,
      headers,
    });

    // Unauthenticated: drop the stale key and tell the app, which shows the
    // screen that can replace it. Deliberately no navigation - see
    // `unauthorizedListeners` above for the reload loop that was.
    if (res.status === 401) {
      api.removeToken();
      announceUnauthorized();
      throw new ApiError(401, "Unauthorized", {
        requestId: res.headers.get(REQUEST_ID_HEADER) ?? undefined,
      });
    }

    // Any other non-2xx
    if (!res.ok) {
      throw refusal(res, await res.text());
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
   * Listen for the control plane refusing the stored key. Returns the
   * function that stops listening, so a React effect can return it directly.
   */
  onUnauthorized: (listener: UnauthorizedListener): (() => void) => {
    unauthorizedListeners.add(listener);
    return () => {
      unauthorizedListeners.delete(listener);
    };
  },

  /**
   * Put the key down deliberately, and take the console back to the key
   * screen. The same path a refusal takes, so there is one way out of an
   * authenticated session rather than two that can drift apart.
   */
  signOut: (): void => {
    api.removeToken();
    announceUnauthorized();
  },

  /**
   * A call to the product API. `route` is spelled without a prefix
   * (`/apps/demo`); `API_V1` is put in front of it here.
   */
  request: <T = unknown>(route: string, options: RequestInit = {}): Promise<T> =>
    send<T>(`${API_BASE}${API_V1}${route}`, options),

  /**
   * A call to the platform diagnostics API, prefixed with `API_PLATFORM`.
   */
  platformRequest: <T = unknown>(route: string, options: RequestInit = {}): Promise<T> =>
    send<T>(`${API_BASE}${API_PLATFORM}${route}`, options),

  // ==================== Control channels ====================

  /**
   * List the nodes currently holding a control channel open to the control
   * plane. This is not the runtime fleet - see `getFleet`.
   */
  getSessions: () =>
    api.platformRequest<{ sessions: SessionSummary[]; total: number }>("/sessions"),

  /**
   * Fetch one open control channel.
   */
  getSession: (id: string) => api.platformRequest<SessionSummary>(`/sessions/${id}`),

  /**
   * One page of a node's heartbeat history, newest first. Follow
   * `page.nextCursor` (or use `listAll`) for the rest.
   */
  getSessionHeartbeats: (id: string, query: PageQuery = {}) =>
    api.platformRequest<ListPage<Heartbeat>>(paged(`/sessions/${id}/heartbeats`, query)),

  // ==================== Runtime fleet ====================

  /**
   * List the runtime nodes that receive deployments and run cron
   * occurrences, whether they came from the control plane's configuration
   * file or registered themselves.
   */
  getFleet: () => api.platformRequest<{ nodes: FleetNode[] }>("/runtime"),

  // ==================== Configs API ====================
  //
  // The point reads and mutations here answer `{ success, ... }`, not the
  // bare payload - `crates/deis-control/src/api/configs.rs`; the two history
  // routes answer the unified `ListPage`. A configuration value itself is
  // whatever JSON an operator pushed, stored as `serde_json::Value` with no
  // schema (`crates/deis-control/src/config_manager/mod.rs`), so it comes
  // through as `ConfigValue` (`unknown`) here; narrow it with
  // `asPlainObject` before reading a field out of it.

  /**
   * List every configuration, for every node.
   */
  getConfigs: () =>
    api.platformRequest<{
      success: boolean;
      data: { [nodeId: string]: NodeConfigMap };
      count: number;
    }>("/configs"),

  /**
   * Configuration counts.
   */
  getConfigStats: () =>
    api.platformRequest<{ success: boolean; node_count: number; total_configs: number }>(
      "/configs/stats"
    ),

  /**
   * Fetch one node's configuration.
   */
  getNodeConfig: (nodeId: string) =>
    api.platformRequest<{ success: boolean; node_id: string; data: NodeConfigMap; count: number }>(
      `/configs/${nodeId}`
    ),

  /**
   * Update one node's configuration.
   */
  updateNodeConfig: (nodeId: string, configs: NodeConfigMap) =>
    api.platformRequest<{
      success: boolean;
      node_id: string;
      version_timestamp: string;
      updated_keys: string[];
      message: string;
    }>(`/configs/${nodeId}`, {
      method: "POST",
      body: JSON.stringify({ configs }),
    }),

  /**
   * Delete a node's whole configuration.
   */
  deleteNodeConfig: (nodeId: string) =>
    api.platformRequest<{ success: boolean; node_id: string; message: string }>(
      `/configs/${nodeId}`,
      { method: "DELETE" }
    ),

  /**
   * Delete a single configuration key.
   */
  deleteConfigKey: (nodeId: string, key: string) =>
    api.platformRequest<{ success: boolean; node_id: string; config_key: string; message: string }>(
      `/configs/${nodeId}/${key}`,
      { method: "DELETE" }
    ),

  /**
   * One page of a node's configuration change history, newest first.
   */
  getConfigHistory: (nodeId: string, query: PageQuery = {}) =>
    api.platformRequest<ListPage<ConfigHistory>>(paged(`/configs/${nodeId}/history`, query)),

  /**
   * One page of the change history of one configuration key, newest first.
   */
  getConfigKeyHistory: (nodeId: string, key: string, query: PageQuery = {}) =>
    api.platformRequest<ListPage<ConfigHistory>>(
      paged(`/configs/${nodeId}/${key}/history`, query)
    ),

  /**
   * Roll a configuration back to an earlier version.
   */
  rollbackConfig: (nodeId: string, key: string, versionTimestamp: string) =>
    api.platformRequest<{
      success: boolean;
      node_id: string;
      config_key: string;
      rolled_back_to: string;
      new_version: string;
      value: ConfigValue;
      message: string;
    }>(`/configs/${nodeId}/${key}/rollback`, {
      method: "POST",
      body: JSON.stringify({ version_timestamp: versionTimestamp }),
    }),

  /**
   * Push a configuration to a node.
   *
   * One key hits `push_config` (`{ ..., config_key, message }`); the whole
   * node hits `push_all_configs` (`{ ..., pushed_count, message }`) - two
   * different handlers, so two different shapes back.
   */
  pushConfig: (nodeId: string, key?: string) => {
    if (key) {
      return api.platformRequest<{
        success: boolean;
        node_id: string;
        config_key: string;
        message: string;
      }>(`/configs/push/${nodeId}/${key}`, { method: "POST" });
    }
    return api.platformRequest<{
      success: boolean;
      node_id: string;
      pushed_count: number;
      message: string;
    }>(`/configs/push/${nodeId}`, { method: "POST" });
  },

  // ==================== Commands API ====================

  /**
   * One page of the whole fleet's command history, newest first.
   */
  getCommands: (query: PageQuery = {}) =>
    api.platformRequest<ListPage<Command>>(paged("/commands", query)),

  /**
   * One page of the commands issued to one node, newest first.
   */
  getNodeCommands: (nodeId: string, query: PageQuery = {}) =>
    api.platformRequest<ListPage<Command>>(paged(`/commands/node/${nodeId}`, query)),

  /**
   * Fetch a command's status - just the status string, not the command
   * itself (`crates/deis-control/src/command/mod.rs::get_status`).
   */
  getCommandStatus: (commandId: string) =>
    api.platformRequest<{ success: boolean; command_id: string; status: string }>(
      `/commands/status/${commandId}`
    ),

  /**
   * Issue a command.
   */
  executeCommand: (nodeId: string, command: ExecuteCommandRequest) =>
    api.platformRequest<{
      success: boolean;
      command_id: string;
      node_id: string;
      command_type: string;
      message: string;
    }>(`/commands/execute/${nodeId}`, {
      method: "POST",
      body: JSON.stringify(command),
    }),

  /**
   * Cancel a command.
   */
  cancelCommand: (commandId: string) =>
    api.platformRequest<{ success: boolean; command_id: string; message: string }>(
      `/commands/cancel/${commandId}`,
      { method: "POST" }
    ),

  // ==================== Events API ====================

  /**
   * One page of the event log, newest first, optionally narrowed to one
   * node or one severity. The route refuses `offset`; the way further back
   * is `page.nextCursor`.
   */
  getEvents: (params: EventsParams = {}) => {
    const query = new URLSearchParams();
    if (params.node_id) query.set("node_id", params.node_id);
    if (params.severity) query.set("severity", params.severity);
    const filter = query.toString();
    return api.platformRequest<ListPage<Event>>(
      paged(`/events${filter ? `?${filter}` : ""}`, params)
    );
  },

  /**
   * Event counts.
   */
  getEventStats: () =>
    api.platformRequest<{ success: boolean; stats: EventStats }>("/events/stats"),

  // ==================== Metrics API ====================

  /**
   * Metrics summary.
   */
  getMetricsSummary: () =>
    api.platformRequest<{ success: boolean; metrics: MetricsSummary }>("/metrics/summary"),
};

// ==================== Types ====================

/**
 * A value with no fixed shape on the wire - `serde_json::Value` on the
 * control plane. `unknown`, not `any`: a caller has to narrow it (see
 * `asPlainObject` below) before it can read a field out of one.
 */
export type ConfigValue = unknown;

/** One node's configuration: config key -> whatever value is stored for it. */
export type NodeConfigMap = { [key: string]: ConfigValue };

/**
 * Narrow a `ConfigValue` (or any other `serde_json::Value`-shaped field, such
 * as a command's payload or an event's metadata) down to the "plain JSON
 * object" case the console actually renders. Everything else a JSON value
 * can be - a string, a number, an array, `null` - answers `undefined`, so a
 * caller cannot accidentally run `Object.keys` over something that is not a
 * record.
 */
export function asPlainObject(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

/**
 * One node holding a control channel open to the control plane, as
 * `/api/platform/sessions` reports it.
 */
export interface SessionSummary {
  id: string;
  instance_id: string;
  node_role: string;
  host: string;
  ports: number[];
  /**
   * `Unresponsive` is a node that is still holding its control channel open
   * but has not been heard from within `heartbeat_timeout`. It is not the
   * same as `Disconnected`: that node hung up and is presumably restarting,
   * while this one is still connected and its host or its network has stopped
   * answering.
   */
  state: "Connected" | "Registered" | "Active" | "Unresponsive" | "Disconnected";
  connected_at: string;
  last_heartbeat: string;
}

/**
 * One member of the runtime fleet, as `/api/platform/runtime` reports it. A
 * node is here because the control plane's configuration names it or because
 * it registered itself, which is what `source` says - holding a control
 * channel open has nothing to do with it.
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
 * One recorded heartbeat, as `/api/platform/sessions/:id/heartbeats` lists
 * it. It carries no load figure: the wire's heartbeat message is a status
 * string, so there is nothing else to report.
 */
export interface Heartbeat {
  id: number;
  /** RFC 3339, as recorded. */
  timestamp: string;
  latency_ms: number | null;
}

export interface ConfigStats {
  node_count: number;
  total_configs: number;
}

export interface ConfigHistory {
  id: number;
  config_key: string;
  config_value: ConfigValue;
  version_timestamp: string;
  operation: "create" | "update" | "delete";
  created_at: string;
}

/**
 * One command, as `/api/platform/commands` and `.../commands/node/:id` list
 * it - `crates/deis-control/src/command/mod.rs::CommandSummary`. The
 * summary lists exactly these fields, so a payload, a result, a timeout or
 * an error message is not part of this type - nothing in this API returns
 * them today.
 */
export interface Command {
  id: string;
  node_id: string;
  command_type: string;
  status: "Pending" | "Sent" | "Acknowledged" | "Completed" | "Failed" | "Timeout";
  created_at: string;
  sent_at?: string | null;
  completed_at?: string | null;
}

/**
 * Exactly the body `POST /api/platform/commands/execute/:node_id`
 * deserializes.
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

/** One line of the event log, as `/api/platform/events` lists it. */
export interface Event {
  id: number;
  event_type: string;
  node_id?: string | null;
  severity: "Info" | "Warning" | "Error";
  message: string;
  metadata?: ConfigValue;
  timestamp: string;
}

export interface EventsParams extends PageQuery {
  node_id?: string;
  /** Exactly `Info`, `Warning` or `Error`; anything else is refused. */
  severity?: string;
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
    /** Still connected, but silent past `heartbeat_timeout`. */
    unresponsive: number;
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
