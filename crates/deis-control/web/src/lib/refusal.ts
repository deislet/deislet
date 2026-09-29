/**
 * What the console makes of a refused call.
 *
 * A refusal on `/api/v1` and `/api/platform` arrives in the envelope of
 * `contracts/v1/control-error.schema.json`: a stable kebab-case `code`, a
 * sentence written for a person, the request id an operator greps the
 * control plane's log for, whether an identical request may succeed later,
 * and - when the server has an opinion - how long to wait before asking
 * again. This module is the client half of that contract.
 *
 * It is a module of its own, and deliberately free of `fetch`, `window` and
 * React, for two reasons. The first is that it can be exercised directly:
 * `refusal.test.ts` next to it feeds it the bodies the control plane really
 * writes and checks what the console would show. The second is that the
 * distinction it draws has to be drawn in exactly one place. A console that
 * renders every refusal as the same red sentence tells an operator nothing
 * about which refusal they are looking at, and the four cases want four
 * different actions from them: a scope refusal needs a different key, a
 * conflict needs the page re-read, a rate limit needs waiting as long as the
 * server asked rather than hammering it every two seconds, and a 500 needs
 * the request id carried to whoever can read the log. ADR-016 lists those as
 * states the console tells apart, and `describeRefusal` is where they are
 * told apart.
 */

import type { components } from "@/generated/control-api";

type Schemas = components["schemas"];

/**
 * One field a request got wrong, as the control plane names it in the
 * envelope's `violations`.
 */
export type Violation = Schemas["ErrorViolation"];

/**
 * The body of a refusal, which the document renders as `ErrorBody` around
 * `ErrorEnvelope`. Read leniently below: the guard asks only for the code
 * and the message, so a member the document promises but a body leaves out
 * does not stop those two from being read.
 */
type ErrorBody = Schemas["ErrorBody"];

/**
 * The response header carrying the id the control plane logged a request
 * under. The error envelope repeats it as `requestId`; on a refusal from
 * outside the envelope (the authorization gate) the header is all there is.
 */
export const REQUEST_ID_HEADER = "x-request-id";

/**
 * The standard header a 429 or a 503 may carry. The control plane puts its
 * advice in the envelope as well, and the envelope wins; this is what is
 * left to read when the refusal came from something in front of the control
 * plane - a reverse proxy shedding load - which answers no envelope at all.
 */
export const RETRY_AFTER_HEADER = "retry-after";

/** Everything about a refusal beyond its status and its sentence. */
export interface RefusalDetail {
  code?: string;
  requestId?: string;
  violations?: Violation[];
  retryable?: boolean;
  retryAfterSeconds?: number;
}

/**
 * A refused call.
 *
 * `message` is the sentence to show a person, with the request id appended
 * when the control plane gave one - that id is what an operator searches the
 * control plane's log for, so a toast or a thrown error carries it along.
 * `sentence` is the same text without the id, for the places that show the
 * id on a line of its own. `code` is the stable class from the envelope
 * (`not-found`, `insufficient-scope`, ...), absent on a refusal that arrived
 * in the flat `{ "error": ... }` shape or with no body at all.
 */
export class ApiError extends Error {
  status: number;
  code?: string;
  requestId?: string;
  violations: Violation[];
  retryable: boolean;
  /**
   * How long the server asked the caller to wait, in seconds. Only ever set
   * from a retryable refusal, because the envelope only means it there; a
   * caller that has none picks its own backoff.
   */
  retryAfterSeconds?: number;
  /** The control plane's sentence, without the request id appended. */
  sentence: string;

  constructor(status: number, message: string, detail: RefusalDetail = {}) {
    super(detail.requestId ? `${message} (request ${detail.requestId})` : message);
    this.name = "ApiError";
    this.status = status;
    this.sentence = message;
    this.code = detail.code;
    this.requestId = detail.requestId;
    this.violations = detail.violations ?? [];
    this.retryable = detail.retryable ?? false;
    this.retryAfterSeconds = detail.retryable ? detail.retryAfterSeconds : undefined;
  }
}

function asPlainRecord(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

function isErrorBody(value: unknown): value is ErrorBody {
  const error = asPlainRecord(asPlainRecord(value)?.error);
  return typeof error?.code === "string" && typeof error?.message === "string";
}

/**
 * Whether a status is one an identical request may get past by waiting. Used
 * only when the refusal carried no envelope to say so: within the envelope
 * `retryable` is a property of the code and is taken as written.
 */
function statusIsRetryable(status: number): boolean {
  return status === 0 || status === 408 || status === 429 || status >= 500;
}

/**
 * Read a `Retry-After` header. RFC 9110 allows two spellings, and both turn
 * up in front of a control plane: a count of seconds, and an HTTP date. A
 * date in the past, or anything unparseable, answers `undefined` rather than
 * a negative wait.
 */
function parseRetryAfter(header: string | undefined, now: number): number | undefined {
  if (header === undefined) return undefined;
  const trimmed = header.trim();
  if (trimmed === "") return undefined;
  if (/^\d+$/.test(trimmed)) return Number(trimmed);
  const at = Date.parse(trimmed);
  if (Number.isNaN(at)) return undefined;
  return Math.max(0, Math.round((at - now) / 1000));
}

/** The headers a refusal is read out of, named rather than passed as a `Headers`. */
export interface RefusalHeaders {
  requestId?: string;
  retryAfter?: string;
}

/**
 * Turn a non-2xx answer into an `ApiError`, whichever shape it arrived in:
 * the versioned envelope, the flat `{ "error": "<message>" }` that
 * the authorization gate still answers with, or no JSON at all.
 *
 * `now` is a parameter so that an HTTP-date `Retry-After` can be measured
 * against a fixed clock in a test rather than against the wall clock.
 */
export function refusalFrom(
  status: number,
  statusText: string,
  headers: RefusalHeaders,
  text: string,
  now: number = Date.now()
): ApiError {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    parsed = undefined;
  }
  const advised = parseRetryAfter(headers.retryAfter, now);
  if (isErrorBody(parsed)) {
    const { code, message, requestId, violations, retryable, retryAfterSeconds } = parsed.error;
    return new ApiError(status, message, {
      code,
      requestId: requestId ?? headers.requestId,
      violations: Array.isArray(violations) ? violations : [],
      retryable: retryable === true,
      // The envelope's own advice first; the header is the fallback for a
      // body that carries no `retryAfterSeconds`.
      retryAfterSeconds: retryAfterSeconds ?? advised,
    });
  }
  const flat = asPlainRecord(parsed)?.error;
  const message = typeof flat === "string" ? flat : text || statusText;
  return new ApiError(status, message, {
    requestId: headers.requestId,
    retryable: statusIsRetryable(status),
    retryAfterSeconds: advised,
  });
}

/**
 * Which of the refusals a console has to tell apart this one is. The classes
 * are drawn where the operator's next move differs, which is why 503 is not
 * folded into the other 5xx (it comes back on its own, and often quickly)
 * and why 401 and 403 are not one class (one needs any key, the other needs
 * a wider one).
 */
export type RefusalKind =
  | "offline"
  | "unauthorized"
  | "forbidden"
  | "missing"
  | "conflict"
  | "rate-limited"
  | "unavailable"
  | "server"
  | "refused";

/** A refusal, in the terms a page renders it in. */
export interface RefusalState {
  kind: RefusalKind;
  status: number;
  code?: string;
  /** A headline naming what could not be read and why. */
  title: string;
  /** The control plane's own sentence, with no request id glued to it. */
  message: string;
  requestId?: string;
  retryable: boolean;
  retryAfterSeconds?: number;
  violations: Violation[];
}

function kindOf(status: number, code: string | undefined): RefusalKind {
  if (status === 0) return "offline";
  if (status === 401) return "unauthorized";
  if (status === 403) return "forbidden";
  if (status === 404) return "missing";
  if (status === 409) return "conflict";
  if (status === 429) return "rate-limited";
  if (status === 503) return "unavailable";
  // A dependency the control plane itself dials can be reported with a 500;
  // the code is the more precise of the two, so it wins.
  if (code === "backend-unavailable" || code === "database-unavailable") return "unavailable";
  if (status >= 500) return "server";
  return "refused";
}

function capitalize(subject: string): string {
  return subject.charAt(0).toUpperCase() + subject.slice(1);
}

function titleOf(kind: RefusalKind, subject: string): string {
  switch (kind) {
    case "offline":
      return "Could not reach the control plane";
    case "unauthorized":
      return "The control plane refused this key";
    case "forbidden":
      return `This key may not read ${subject}`;
    case "missing":
      return `${capitalize(subject)} is not there`;
    case "conflict":
      return `${capitalize(subject)} changed while this page was reading it`;
    case "rate-limited":
      return `Reading ${subject} too often`;
    case "unavailable":
      return `The control plane cannot serve ${subject} yet`;
    case "server":
      return `The control plane failed while reading ${subject}`;
    case "refused":
      return `The control plane refused this read of ${subject}`;
  }
}

/**
 * Describe whatever a read threw. `subject` names what was being read, as a
 * noun phrase that fits into a sentence ("the runtime fleet"), so that a
 * page with two lists says which of them is the one in trouble.
 *
 * Anything that is not an `ApiError` reached here without going through the
 * request layer, which wraps even a network failure; it is reported as
 * unreachable rather than dropped, because a console that silently shows
 * stale rows is worse than one that says it stopped reading.
 */
export function describeRefusal(error: unknown, subject: string): RefusalState {
  if (error instanceof ApiError) {
    const kind = kindOf(error.status, error.code);
    return {
      kind,
      status: error.status,
      code: error.code,
      title: titleOf(kind, subject),
      message: error.sentence,
      requestId: error.requestId,
      retryable: error.retryable,
      retryAfterSeconds: error.retryAfterSeconds,
      violations: error.violations,
    };
  }
  return {
    kind: "offline",
    status: 0,
    title: titleOf("offline", subject),
    message: error instanceof Error ? error.message : `Could not read ${subject}.`,
    retryable: true,
    violations: [],
  };
}

/**
 * The longest a poll that keeps being refused pushes its next attempt out on
 * its own. A console is watched by a person, so it stays on the minute
 * rather than backing off into hours: it is the operator's window onto a
 * control plane that is in trouble, and it has to notice when the trouble
 * ends.
 */
export const MAX_POLL_BACKOFF_MS = 60_000;

/**
 * The longest the console waits on the server's own advice. The envelope
 * allows a day, which for a page someone is looking at means a dead tab; if
 * the advice is longer than this the console comes back anyway, and the
 * Refresh button is there in the meantime.
 */
export const MAX_ADVISED_WAIT_MS = 300_000;

/**
 * How long to wait before the next poll.
 *
 * A steady read keeps its cadence. A refused one backs off, because the two
 * refusals a control plane under strain answers - 429 and 503 - are made
 * worse by a console asking again half a second later, and the refusals that
 * will not change on their own (a scope refusal, a conflict) are pure waste
 * at two-second intervals. `retryAfterSeconds` is the server saying how long
 * it wants; nothing else is guessed at more finely than doubling.
 *
 * `consecutiveRefusals` counts the refusals since the last answer, so the
 * first read after a recovery is back at the normal cadence.
 */
export function nextPollDelayMs(
  intervalMs: number,
  consecutiveRefusals: number,
  refusal: RefusalState | null
): number {
  if (refusal === null || consecutiveRefusals <= 0) return intervalMs;
  if (refusal.retryAfterSeconds !== undefined) {
    const advised = Math.min(refusal.retryAfterSeconds * 1000, MAX_ADVISED_WAIT_MS);
    return Math.max(intervalMs, advised);
  }
  const doubled = intervalMs * 2 ** Math.min(consecutiveRefusals, 16);
  return Math.min(doubled, MAX_POLL_BACKOFF_MS);
}
