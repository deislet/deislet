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

    // Minimal configuration based on user example
    let edgeone_json = serde_json::json!({
        "name": config.name,
        // "entry": "edge-functions/index.js", // Use convention
    });

    config_files.push((
        "edgeone.json".to_string(),
        serde_json::to_string_pretty(&edgeone_json).unwrap(),
    ));

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
        imports.push_str(&format!("import handler{i} from '{import_path}';\n"));

        route_handlers.push_str(&format!(
            r#"  if (matchRoute(pathname, '{route}')) {{
    const enhancedContext = enhanceContext(context, '{route}');
    return handler{i}(enhancedContext);
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
 * Tencent EdgeOne Main Entry
 * Auto-generated router
 */

import {{ enhanceContext }} from './adapters/context-builder';
{imports}

export function onRequest(context) {{
  const {{ request }} = context;
  const url = new URL(request.url);
  const pathname = url.pathname;

{route_handlers}
  // 404 Not Found
  return new Response('Not Found', {{ status: 404 }});
}}

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

    let kv_usage = if has_kv {
        "// Extract KV if present\n  const kv = typeof globalThis.EdgeKV !== 'undefined' ? createKVAdapter(globalThis.EdgeKV) : undefined;"
    } else {
        "const kv = undefined;"
    };

    format!(
        r#"/**
 * EdgeOne Context Enhancer
 * Enhances the EdgeOne context with additional utilities
 */
{kv_import}
export function enhanceContext(context: any, route: string = '/') {{
  const {{ request, env }} = context;
  const url = new URL(request.url);

  {kv_usage}

  return {{
    ...context,
    params: extractParams(url.pathname, route),
    path: url.pathname,
    method: request.method,
    query: Object.fromEntries(url.searchParams),
    cookies: parseCookies(request),
    services: {{
      kv,
    }},
    log: console,
    waitUntil: (promise: Promise<any>) => {{
      // EdgeOne may not support waitUntil, provide no-op fallback
      if (typeof context.waitUntil === 'function') {{
        context.waitUntil(promise);
      }}
    }},
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
 * Tencent EdgeOne KV Adapter
 */

export function createKVAdapter(kvNamespace: any) {
  return {
    async get(key: string, options: any = {}): Promise<string | null> {
      return kvNamespace.get(key, options.type || 'text');
    },

    async getJSON<T = any>(key: string): Promise<T | null> {
      return kvNamespace.get(key, 'json');
    },

    async put(key: string, value: string, options: any = {}): Promise<void> {
      await kvNamespace.put(key, value, options);
    },

    async putJSON<T = any>(key: string, value: T, options: any = {}): Promise<void> {
      await kvNamespace.put(key, JSON.stringify(value), options);
    },

    async delete(key: string): Promise<void> {
      await kvNamespace.delete(key);
    },

    async list(options: any = {}) {
      const result = await kvNamespace.list(options);
      return {
        keys: result.keys || [],
        list_complete: result.list_complete ?? true,
        cursor: result.cursor,
      };
    },

    async exists(key: string): Promise<boolean> {
      const value = await kvNamespace.get(key);
      return value !== null;
    },
  };
}
"#
    .to_string()
}
