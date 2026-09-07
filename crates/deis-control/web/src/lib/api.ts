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
 *
 * Every shape the control plane answers is taken from
 * `@/generated/control-api`, which `scripts/generate-openapi-types.py`
 * renders from `contracts/openapi/v1.json`, and nothing below restates one.
 * The names this module used to define by hand (`SessionSummary`,
 * `FleetNode`, `Command`, ...) are kept as aliases of the generated types so
 * that the hooks and pages read as before; what they name is now what the
 * document says, and a page that disagrees with the document fails to
 * compile rather than failing at runtime. The request side - the prefixes,
 * `PageQuery`, `paged`, `send` - is the thin wrapper the ADR allows.
 */

import type { components, operations } from "@/generated/control-api";

/** The document's schemas by name; `Schemas["FleetResponse"]` and so on. */
type Schemas = components["schemas"];

/**
 * What one operation answers with one status:
 * `Answer<"listRuntimeFleet", 200>` is
 * `operations["listRuntimeFleet"]["responses"][200]`, spelled shorter.
 * Naming the operation and the status here is what ties each method below
 * to the document, so a route whose answer changes shape changes the
 * method's return type with it.
 */
type Answer<
  Id extends keyof operations,
  Status extends keyof operations[Id]["responses"],
> = operations[Id]["responses"][Status];

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
export type Violation = Schemas["ErrorViolation"];

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
 * `contracts/v1/control-error.schema.json`, which the document renders as
 * `ErrorBody` around `ErrorEnvelope`. Read leniently: the guard asks only
 * for the code and the message, which is what the console shows, so a
 * member the document promises but an older control plane leaves out does
 * not stop those two from being read.
 */
type ErrorBody = Schemas["ErrorBody"];

function isErrorBody(value: unknown): value is ErrorBody {
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
  if (isErrorBody(parsed)) {
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
 *
 * The document renders the envelope once per item type
 * (`ControlListPage_CommandSummary`, `ControlListPage_EventItem`, ...),
 * all of them the same three members around a different `items`. This is
 * that envelope with the item type abstracted, built from one of the
 * rendered ones so that `schemaVersion` and `page` stay what the document
 * says; every `ControlListPage_*` is assignable to it, which is what lets
 * `listAll` walk any of them.
 */
export type ListPage<T> = Omit<Schemas["ControlListPage_EventItem"], "items"> & { items: T[] };

/** How much of the collection one page covers. */
export type PageMeta = Schemas["PageMetadata"];

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
  getSessions: () => api.platformRequest<Answer<"listSessions", 200>>("/sessions"),

  /**
   * Fetch one open control channel.
   */
  getSession: (id: string) => api.platformRequest<Answer<"getSession", 200>>(`/sessions/${id}`),

  /**
   * One page of a node's heartbeat history, newest first. Follow
   * `page.nextCursor` (or use `listAll`) for the rest.
   */
  getSessionHeartbeats: (id: string, query: PageQuery = {}) =>
    api.platformRequest<Answer<"listSessionHeartbeats", 200>>(
      paged(`/sessions/${id}/heartbeats`, query)
    ),

  // ==================== Runtime fleet ====================

  /**
   * List the runtime nodes that receive deployments and run cron
   * occurrences, whether they came from the control plane's configuration
   * file or registered themselves. The answer also carries `lost` (nodes
   * that stopped refreshing their registration) and `contested` (node ids
   * two machines claimed); the fleet a page shows is `nodes`.
   */
  getFleet: () => api.platformRequest<Answer<"listRuntimeFleet", 200>>("/runtime"),

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
  getConfigs: () => api.platformRequest<Answer<"listNodeConfigs", 200>>("/configs"),

  /**
   * Configuration counts.
   */
  getConfigStats: () => api.platformRequest<Answer<"getNodeConfigStats", 200>>("/configs/stats"),

  /**
   * Fetch one node's configuration.
   */
  getNodeConfig: (nodeId: string) =>
    api.platformRequest<Answer<"getNodeConfigs", 200>>(`/configs/${nodeId}`),

  /**
   * Update one node's configuration.
   */
  updateNodeConfig: (nodeId: string, configs: NodeConfigMap) => {
    const body: operations["updateNodeConfigs"]["requestBody"] = { configs };
    return api.platformRequest<Answer<"updateNodeConfigs", 200>>(`/configs/${nodeId}`, {
      method: "POST",
      body: JSON.stringify(body),
    });
  },

  /**
   * Delete a node's whole configuration.
   */
  deleteNodeConfig: (nodeId: string) =>
    api.platformRequest<Answer<"deleteNodeConfigs", 200>>(`/configs/${nodeId}`, {
      method: "DELETE",
    }),

  /**
   * Delete a single configuration key.
   */
  deleteConfigKey: (nodeId: string, key: string) =>
    api.platformRequest<Answer<"deleteNodeConfigValue", 200>>(`/configs/${nodeId}/${key}`, {
      method: "DELETE",
    }),

  /**
   * One page of a node's configuration change history, newest first.
   */
  getConfigHistory: (nodeId: string, query: PageQuery = {}) =>
    api.platformRequest<Answer<"listNodeConfigHistory", 200>>(
      paged(`/configs/${nodeId}/history`, query)
    ),

  /**
   * One page of the change history of one configuration key, newest first.
   */
  getConfigKeyHistory: (nodeId: string, key: string, query: PageQuery = {}) =>
    api.platformRequest<Answer<"listNodeConfigKeyHistory", 200>>(
      paged(`/configs/${nodeId}/${key}/history`, query)
    ),

  /**
   * Roll a configuration back to an earlier version.
   */
  rollbackConfig: (nodeId: string, key: string, versionTimestamp: string) => {
    const body: operations["rollbackNodeConfigValue"]["requestBody"] = {
      version_timestamp: versionTimestamp,
    };
    return api.platformRequest<Answer<"rollbackNodeConfigValue", 200>>(
      `/configs/${nodeId}/${key}/rollback`,
      { method: "POST", body: JSON.stringify(body) }
    );
  },

  /**
   * Push a configuration to a node.
   *
   * One key is `pushNodeConfigValue` (`{ ..., config_key, message }`); the
   * whole node is `pushNodeConfigs` (`{ ..., pushed_count, message }`) - two
   * different handlers, so two different shapes back.
   */
  pushConfig: (nodeId: string, key?: string) => {
    if (key) {
      return api.platformRequest<Answer<"pushNodeConfigValue", 200>>(
        `/configs/push/${nodeId}/${key}`,
        { method: "POST" }
      );
    }
    return api.platformRequest<Answer<"pushNodeConfigs", 200>>(`/configs/push/${nodeId}`, {
      method: "POST",
    });
  },

  // ==================== Commands API ====================

  /**
   * One page of the whole fleet's command history, newest first.
   */
  getCommands: (query: PageQuery = {}) =>
    api.platformRequest<Answer<"listNodeCommands", 200>>(paged("/commands", query)),

  /**
   * One page of the commands issued to one node, newest first.
   */
  getNodeCommands: (nodeId: string, query: PageQuery = {}) =>
    api.platformRequest<Answer<"listNodeCommandsOfNode", 200>>(
      paged(`/commands/node/${nodeId}`, query)
    ),

  /**
   * Fetch a command's status - just the status string, not the command
   * itself (`crates/deis-control/src/command/mod.rs::get_status`).
   */
  getCommandStatus: (commandId: string) =>
    api.platformRequest<Answer<"getNodeCommandStatus", 200>>(`/commands/status/${commandId}`),

  /**
   * Issue a command. The control plane answers 202: the command is queued
   * for the node, not carried out, and `command_id` is the handle for the
   * status and cancel routes.
   */
  executeCommand: (nodeId: string, command: ExecuteCommandRequest) =>
    api.platformRequest<Answer<"executeNodeCommand", 202>>(`/commands/execute/${nodeId}`, {
      method: "POST",
      body: JSON.stringify(command),
    }),

  /**
   * Cancel a command.
   */
  cancelCommand: (commandId: string) =>
    api.platformRequest<Answer<"cancelNodeCommand", 200>>(`/commands/cancel/${commandId}`, {
      method: "POST",
    }),

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
    return api.platformRequest<Answer<"listEvents", 200>>(
      paged(`/events${filter ? `?${filter}` : ""}`, params)
    );
  },

  /**
   * Event counts.
   */
  getEventStats: () => api.platformRequest<Answer<"getEventStats", 200>>("/events/stats"),

  // ==================== Metrics API ====================

  /**
   * Metrics summary.
   */
  getMetricsSummary: () =>
    api.platformRequest<Answer<"getMetricsSummary", 200>>("/metrics/summary"),
};

// ==================== Types ====================
//
// Every name here is an alias of a type in `@/generated/control-api`, kept
// under the name the hooks and pages have always imported. None of them
// restates a shape: what a page can read off a `FleetNode` is exactly what
// `contracts/openapi/v1.json` says a `NodeView` carries, and the comments
// here add what the document does not say about how the console uses it.

/**
 * A value with no fixed shape on the wire - `serde_json::Value` on the
 * control plane, `Value` in the document. `unknown`, not `any`: a caller
 * has to narrow it (see `asPlainObject` below) before it can read a field
 * out of one.
 */
export type ConfigValue = Schemas["Value"];

/** One node's configuration: config key -> whatever value is stored for it. */
export type NodeConfigMap = Schemas["NodeConfigs"]["data"];

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
 *
 * `state` is one of `Connected`, `Registered`, `Active`, `Unresponsive` and
 * `Disconnected`, which the document leaves as a string. `Unresponsive` is a
 * node that is still holding its control channel open but has not been
 * heard from within `heartbeat_timeout`. It is not the same as
 * `Disconnected`: that node hung up and is presumably restarting, while this
 * one is still connected and its host or its network has stopped answering.
 */
export type SessionSummary = Schemas["SessionView"];

/**
 * One member of the runtime fleet, as `/api/platform/runtime` reports it. A
 * node is here because the control plane's configuration names it or because
 * it registered itself, which is what `source` says - holding a control
 * channel open has nothing to do with it. The health members (`healthy`,
 * `version`, `last_checked`, `last_error`) are the last probe's answer, and
 * every one but `healthy` may be absent as well as null.
 */
export type FleetNode = Schemas["NodeView"];

/**
 * One recorded heartbeat, as `/api/platform/sessions/:id/heartbeats` lists
 * it. It carries no load figure: the wire's heartbeat message is a status
 * string, so there is nothing else to report.
 */
export type Heartbeat = Schemas["HeartbeatView"];

/** How much configuration the control plane holds, with the `success` flag. */
export type ConfigStats = Schemas["ConfigStatsResponse"];

/**
 * One recorded configuration change. `operation` is `create`, `update` or
 * `delete`, which the document leaves as a string.
 */
export type ConfigHistory = Schemas["ConfigHistoryItem"];

/**
 * One command, as `/api/platform/commands` and `.../commands/node/:id` list
 * it - `crates/deis-control/src/command/mod.rs::CommandSummary`. The
 * summary lists exactly these fields, so a payload, a result, a timeout or
 * an error message is not part of this type - nothing in this API returns
 * them today. `status` is `Pending`, `Sent`, `Acknowledged`, `Completed`,
 * `Failed`, `Timeout` or `Cancelled`, which the document leaves as a string.
 */
export type Command = Schemas["CommandSummary"];

/**
 * The command a node is asked to run, as `POST
 * /api/platform/commands/execute/:node_id` deserializes it.
 *
 * `command_type` is a tagged enum: the named commands are plain strings, and
 * anything else is `{ custom: "<name>" }`. The payload field is `data`, not
 * `command_data` - the server rejects the whole request with 422 if it is
 * spelled any other way, which is what used to happen to the Restart button.
 */
export type CommandType = Schemas["CommandType"];

export type ExecuteCommandRequest = Schemas["ExecuteCommandRequest"];

/**
 * One line of the event log, as `/api/platform/events` lists it.
 * `severity` is `Info`, `Warning` or `Error`, which the document leaves as
 * a string.
 */
export type Event = Schemas["EventItem"];

/**
 * What `getEvents` takes: the page to read, plus the two filters the route
 * accepts, spelled as the document spells them. `severity` is exactly
 * `Info`, `Warning` or `Error`; anything else is refused.
 */
export type EventsParams = PageQuery & Pick<operations["listEvents"]["query"], "node_id" | "severity">;

export type EventStats = Schemas["EventStats"];

/**
 * What `/api/platform/metrics/summary` carries: the control channels by
 * state, the process-wide counters and the health of the cursor table.
 */
export type MetricsSummary = Schemas["MetricsSummaryView"];
