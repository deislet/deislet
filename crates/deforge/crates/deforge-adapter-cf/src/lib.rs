use deforge_core::ProjectConfig;
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

pub fn generate(output_dir: &Path, config: &ProjectConfig) -> anyhow::Result<()> {
    // Collect all handlers for router
    let mut routes = Vec::new();
    let src_dir = output_dir.join("src");

    if src_dir.exists() {
        for entry in WalkDir::new(&src_dir) {
            let entry = entry?;
            if entry.file_type().is_file() {
                let path = entry.path();
                if let Some(ext) = path.extension() {
                    if ext == "ts" || ext == "js" {
                        let relative = path.strip_prefix(&src_dir).unwrap();
                        let route = filename_to_route(relative.to_string_lossy().as_ref());
                        let import_path = relative
                            .to_string_lossy()
                            .to_string()
                            .trim_end_matches(".ts")
                            .trim_end_matches(".js")
                            .to_string();
                        routes.push((route, import_path));
                    }
                }
            }
        }
    }

    // Generate main router
    generate_router(output_dir, &routes)?;

    // Generate wrangler.toml
    let has_kv = config
        .services
        .as_ref()
        .and_then(|s| s.kv.as_ref())
        .map(|kv| kv.enabled)
        .unwrap_or(false);

    let has_database = config
        .services
        .as_ref()
        .and_then(|s| s.database.as_ref())
        .map(|db| db.enabled)
        .unwrap_or(false);

    let wrangler_toml = format!(
        r#"name = "{}"
main = "index.ts"
compatibility_date = "2024-01-01"

[env.staging]
route = "staging.{}.workers.dev"

[env.production]
route = "{}.workers.dev"
{}{}
"#,
        config.name,
        config.name,
        config.name,
        if has_kv {
            r#"
[[kv_namespaces]]
binding = "KV"
id = ""  # Add your KV namespace ID
"#
        } else {
            ""
        },
        if has_database {
            r#"
[[d1_databases]]
binding = "DB"
database_name = ""  # Add your D1 database name
database_id = ""    # Add your D1 database ID
"#
        } else {
            ""
        }
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

fn generate_router(output_dir: &Path, routes: &[(String, String)]) -> anyhow::Result<()> {
    let mut imports = String::new();
    let mut route_handlers = String::new();

    for (i, (route, import_path)) in routes.iter().enumerate() {
        imports.push_str(&format!("import handler{i} from './src/{import_path}';\n"));

        route_handlers.push_str(&format!(
            r#"  if (matchRoute(url.pathname, '{route}')) {{
    return handler{i}.fetch(request, env, ctx);
  }}
"#
        ));
    }

    let router = format!(
        r#"/**
 * Cloudflare Workers Router
 * Auto-generated entry point
 */

{imports}

export default {{
  async fetch(request, env, ctx) {{
    const url = new URL(request.url);

{route_handlers}
    // 404 Not Found
    return new Response('Not Found', {{ status: 404 }});
  }}
}};

function matchRoute(pathname: string, route: string): boolean {{
  // Exact match
  if (pathname === route) return true;

  // Dynamic parameter match
  const routeParts = route.split('/');
  const pathParts = pathname.split('/');

  if (routeParts.length !== pathParts.length) return false;

  return routeParts.every((part, i) => {{
    if (part.startsWith(':')) return true;
    return part === pathParts[i];
  }});
}}
"#
    );

    fs::write(output_dir.join("index.ts"), router)?;

    Ok(())
}

fn filename_to_route(filename: &str) -> String {
    let without_ext = filename.trim_end_matches(".ts").trim_end_matches(".js");

    let route = if without_ext == "index" {
        "/"
    } else if without_ext.ends_with("/index") {
        without_ext.trim_end_matches("/index")
    } else {
        without_ext
    };

    let route = route.replace("[", ":").replace("]", "");

    if route.starts_with('/') {
        route.to_string()
    } else {
        format!("/{route}")
    }
}
