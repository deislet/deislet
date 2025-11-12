/**
 * Context interface - Unified runtime context for edge functions
 * This is the primary interface that user code interacts with
 */

import type { Services } from './services.js';

export type HTTPMethod = 'GET' | 'POST' | 'PUT' | 'DELETE' | 'PATCH' | 'HEAD' | 'OPTIONS';

export interface CookieStore {
  get(name: string): string | undefined;
  set(name: string, value: string, options?: CookieOptions): void;
  delete(name: string): void;
  getAll(): Record<string, string>;
}

export interface CookieOptions {
  expires?: Date;
  maxAge?: number;
  domain?: string;
  path?: string;
  secure?: boolean;
  httpOnly?: boolean;
  sameSite?: 'strict' | 'lax' | 'none';
}

export interface Logger {
  debug(message: string, ...args: any[]): void;
  info(message: string, ...args: any[]): void;
  warn(message: string, ...args: any[]): void;
  error(message: string, ...args: any[]): void;
}

/**
 * Context object passed to all edge function handlers
 * IMPORTANT: No `raw` field - platform-specific access is forbidden
 */
export interface Context {
  /** The original Request object */
  request: Request;

  /** Environment variables (universal only, no platform bindings) */
  env: Record<string, string>;

  /** Dynamic route parameters extracted from the URL */
  params: Record<string, string>;

  /** Request path (e.g., /api/users/123) */
  path: string;

  /** HTTP method */
  method: HTTPMethod;

  /** Query string parameters */
  query: Record<string, string | string[]>;

  /** Request headers */
  headers: Headers;

  /** Cookie management */
  cookies: CookieStore;

  /** Unified service interfaces (KV, Database, Cache) */
  services: Services;

  /** Structured logging */
  log: Logger;

  /** Cache API */
  cache: {
    match(request: Request | string): Promise<Response | undefined>;
    put(request: Request | string, response: Response): Promise<void>;
    delete(request: Request | string): Promise<boolean>;
  };

  /** Register background tasks (for deferred execution) */
  waitUntil(promise: Promise<any>): void;
}
