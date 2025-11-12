use denictl_core::ProjectConfig;
use std::fs;
use std::path::Path;

pub fn generate(output_dir: &Path, config: &ProjectConfig) -> anyhow::Result<()> {
    // Generate wrangler.toml
    let wrangler_toml = format!(
        r#"name = "{}"
main = "src/index.ts"
compatibility_date = "2024-01-01"

[env.staging]
route = "staging.{}.workers.dev"

[env.production]
route = "{}.workers.dev"
"#,
        config.name, config.name, config.name
    );

    fs::write(output_dir.join("wrangler.toml"), wrangler_toml)?;

    // Generate KV adapter
    let kv_adapter = r#"/**
 * Cloudflare KV Adapter
 */

export function createKVAdapter(namespace: any) {
  return {
    async get(key: string): Promise<string | null> {
      return namespace.get(key);
    },

    async getJSON<T = any>(key: string): Promise<T | null> {
      return namespace.get(key, 'json');
    },

    async put(key: string, value: string, options?: any): Promise<void> {
      await namespace.put(key, value, {
        expirationTtl: options?.expirationTtl,
        metadata: options?.metadata
      });
    },

    async putJSON<T = any>(key: string, value: T, options?: any): Promise<void> {
      await namespace.put(key, JSON.stringify(value), {
        expirationTtl: options?.expirationTtl,
        metadata: options?.metadata
      });
    },

    async delete(key: string): Promise<void> {
      await namespace.delete(key);
    },

    async list(options?: any) {
      const result = await namespace.list(options);
      return {
        keys: result.keys,
        list_complete: result.list_complete,
        cursor: result.cursor
      };
    },

    async exists(key: string): Promise<boolean> {
      return (await namespace.get(key)) !== null;
    }
  };
}
"#;

    let adapters_dir = output_dir.join("adapters");
    fs::create_dir_all(&adapters_dir)?;
    fs::write(adapters_dir.join("kv-adapter.ts"), kv_adapter)?;

    // Generate Context builder
    let context_builder = r#"/**
 * Cloudflare Context Builder
 */

import { createKVAdapter } from './kv-adapter';

export function createContextBuilder(env: any, ctx: any) {
  return function buildContext(request: Request, route: string = '/') {
    const url = new URL(request.url);

    return {
      request,
      env: {},
      params: extractParams(url.pathname, route),
      path: url.pathname,
      method: request.method,
      query: Object.fromEntries(url.searchParams),
      headers: request.headers,
      cookies: parseCookies(request),
      services: {
        kv: env.KV ? createKVAdapter(env.KV) : undefined,
      },
      log: {
        debug: (...args: any[]) => console.debug(...args),
        info: (...args: any[]) => console.info(...args),
        warn: (...args: any[]) => console.warn(...args),
        error: (...args: any[]) => console.error(...args),
      },
      cache: {
        match: async (req: Request | string) => caches.default.match(req),
        put: async (req: Request | string, res: Response) => caches.default.put(req, res),
        delete: async (req: Request | string) => caches.default.delete(req),
      },
      waitUntil: (promise: Promise<any>) => ctx.waitUntil(promise),
    };
  };
}

function extractParams(pathname: string, route: string): Record<string, string> {
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

function parseCookies(request: Request): any {
  const cookies: Record<string, string> = {};
  const cookieHeader = request.headers.get('Cookie');

  if (cookieHeader) {
    cookieHeader.split(';').forEach(cookie => {
      const [name, ...rest] = cookie.split('=');
      cookies[name.trim()] = rest.join('=').trim();
    });
  }

  return {
    get(name: string) { return cookies[name]; },
    set(name: string, value: string) { cookies[name] = value; },
    delete(name: string) { delete cookies[name]; },
    getAll() { return { ...cookies }; }
  };
}
"#;

    fs::write(adapters_dir.join("context-builder.ts"), context_builder)?;

    Ok(())
}
