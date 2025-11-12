/**
 * Cloudflare Context Builder
 * Constructs unified Context object from Cloudflare Workers request
 */

import type { Context, CookieStore } from '@denictl/types';
import { createKVAdapter } from './kv-adapter.js';
import { createDatabaseAdapter } from './database-adapter.js';

/**
 * Parse cookies from request headers
 */
function parseCookies(request: Request): CookieStore {
  const cookies: Record<string, string> = {};
  const cookieHeader = request.headers.get('Cookie');

  if (cookieHeader) {
    cookieHeader.split(';').forEach(cookie => {
      const [name, ...rest] = cookie.split('=');
      cookies[name.trim()] = rest.join('=').trim();
    });
  }

  return {
    get(name: string) {
      return cookies[name];
    },
    set(name: string, value: string) {
      cookies[name] = value;
    },
    delete(name: string) {
      delete cookies[name];
    },
    getAll() {
      return { ...cookies };
    }
  };
}

/**
 * Extract route parameters from URL path
 */
function extractParams(pathname: string, route: string): Record<string, string> {
  // Simple parameter extraction (can be improved with proper routing)
  const params: Record<string, string> = {};
  const routeParts = route.split('/');
  const pathParts = pathname.split('/');

  routeParts.forEach((part, i) => {
    if (part.startsWith(':')) {
      const paramName = part.slice(1);
      params[paramName] = pathParts[i] || '';
    }
  });

  return params;
}

/**
 * Create Context builder for Cloudflare Workers
 */
export function createContextBuilder(env: any, ctx: any) {
  return function buildContext(request: Request, route: string = '/'): Context {
    const url = new URL(request.url);

    return {
      request,
      env: {}, // Environment variables from env object (filtered for non-bindings)
      params: extractParams(url.pathname, route),
      path: url.pathname,
      method: request.method as any,
      query: Object.fromEntries(url.searchParams),
      headers: request.headers,
      cookies: parseCookies(request),
      services: {
        kv: env.KV ? createKVAdapter(env.KV) : undefined,
        database: env.DB ? createDatabaseAdapter(env.DB) : undefined
      },
      log: {
        debug: (...args: any[]) => console.debug(...args),
        info: (...args: any[]) => console.info(...args),
        warn: (...args: any[]) => console.warn(...args),
        error: (...args: any[]) => console.error(...args)
      },
      cache: {
        match: async (req: Request | string) => {
          const cache = caches.default;
          return cache.match(req);
        },
        put: async (req: Request | string, res: Response) => {
          const cache = caches.default;
          return cache.put(req, res);
        },
        delete: async (req: Request | string) => {
          const cache = caches.default;
          return cache.delete(req);
        }
      },
      waitUntil: (promise: Promise<any>) => {
        ctx.waitUntil(promise);
      }
    };
  };
}
