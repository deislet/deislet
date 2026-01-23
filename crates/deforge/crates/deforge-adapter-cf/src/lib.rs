use deforge_core::{AdapterArtifacts, ProjectConfig};

pub fn generate(
    routes: &[(String, String)],
    config: &ProjectConfig,
) -> anyhow::Result<AdapterArtifacts> {
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

    // 1. Generate Context Builder Code
    let context_builder_code = generate_context_builder_code(has_kv);

    // 2. Generate KV Adapter Code
    let kv_adapter_code = if has_kv {
        Some(generate_kv_adapter_code())
    } else {
        None
    };

    // 3. Generate Router Code
    let router_code = generate_router_code(routes, has_kv);

    // 4. Generate Config Files
    let mut config_files = Vec::new();

    // wrangler.toml
    let wrangler_toml = format!(
        r#"name = "{}"
main = "index.js"
compatibility_date = "2024-09-23"
compatibility_flags = ["nodejs_compat"]

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
    config_files.push(("wrangler.toml".to_string(), wrangler_toml));

    // package.json
    let package_json = format!(
        r#"{{
  "name": "{}",
  "private": true,
  "main": "index.js"
}}"#,
        config.name
    );
    config_files.push(("package.json".to_string(), package_json));

    Ok(AdapterArtifacts {
        router_code,
        context_builder_code,
        kv_adapter_code,
        config_files,
    })
}

fn generate_router_code(routes: &[(String, String)], has_kv: bool) -> String {
    let mut imports = String::new();
    let mut route_handlers = String::new();

    for (i, (route, import_path)) in routes.iter().enumerate() {
        // Use provided import path directly (absolute path handled by Compiler)
        imports.push_str(&format!("import handler{i} from '{import_path}';\n"));

        route_handlers.push_str(&format!(
            r#"  if (matchRoute(url.pathname, '{route}')) {{
    const context = buildContext(request, '{route}');
    return handler{i}(context);
  }}
"#
        ));
    }

    let kv_import = if has_kv {
        "import { createKVAdapter } from './adapters/kv-adapter';\n"
    } else {
        ""
    };

    format!(
        r#"/**
 * Cloudflare Workers Router
 * Auto-generated entry point
 */

import {{ createContextBuilder }} from './adapters/context-builder';
{imports}

export default {{
  async fetch(request, env, ctx) {{
    const buildContext = createContextBuilder(env, ctx);
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
    )
}

fn generate_context_builder_code(has_kv: bool) -> String {
    let kv_import = if has_kv {
        "import { createKVAdapter } from './kv-adapter';\n"
    } else {
        ""
    };

    format!(
        r#"/**
 * Cloudflare Context Builder
 */

{kv_import}

export function createContextBuilder(env: any, ctx: any) {{
  return function buildContext(request: Request, route: string = '/') {{
    const url = new URL(request.url);

    return {{
      request,
      env: {{}},
      params: extractParams(url.pathname, route),
      path: url.pathname,
      method: request.method,
      query: Object.fromEntries(url.searchParams),
      headers: request.headers,
      cookies: parseCookies(request),
      services: {{
        kv: env.KV ? createKVAdapter(env.KV) : undefined,
      }},
      log: {{
        debug: (...args: any[]) => console.debug(...args),
        info: (...args: any[]) => console.info(...args),
        warn: (...args: any[]) => console.warn(...args),
        error: (...args: any[]) => console.error(...args),
      }},
      cache: {{
        match: async (req: Request | string) => caches.default.match(req),
        put: async (req: Request | string, res: Response) => caches.default.put(req, res),
        delete: async (req: Request | string) => caches.default.delete(req),
      }},
      waitUntil: (promise: Promise<any>) => ctx.waitUntil(promise),
    }};
  }};
}}

function extractParams(pathname: string, route: string): Record<string, string> {{
  const params: Record<string, string> = {{}};
  const routeParts = route.split('/');
  const pathParts = pathname.split('/');

  routeParts.forEach((part, i) => {{
    if (part.startsWith(':')) {{
      const paramName = part.slice(1);
      params[paramName] = pathParts[i] || '';
    }}
  }});

  return params;
}}

function parseCookies(request: Request): any {{
  const cookies: Record<string, string> = {{}};
  const cookieHeader = request.headers.get('Cookie');

  if (cookieHeader) {{
    cookieHeader.split(';').forEach(cookie => {{
      const [name, ...rest] = cookie.split('=');
      cookies[name.trim()] = rest.join('=').trim();
    }});
  }}

  return {{
    get(name: string) {{ return cookies[name]; }},
    set(name: string, value: string) {{ cookies[name] = value; }},
    delete(name: string) {{ delete cookies[name]; }},
    getAll() {{ return {{ ...cookies }}; }}
  }};
}}
"#
    )
}

fn generate_kv_adapter_code() -> String {
    r#"/**
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
"#
    .to_string()
}
