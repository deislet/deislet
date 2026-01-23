use deforge_core::ProjectConfig;
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

pub fn generate(output_dir: &Path, config: &ProjectConfig) -> anyhow::Result<()> {
    // Collect all handlers for router
    let mut routes = Vec::new();
    let src_dir = output_dir;

    for entry in WalkDir::new(src_dir) {
        let entry = entry?;
        let path = entry.path();
        let filename = path.file_name().unwrap().to_string_lossy();

        // Skip generated entry points
        if filename == "index.ts" || filename == "index.js" || filename == "edgeone.toml" {
            continue;
        }

        if entry.file_type().is_file() {
            if let Some(ext) = path.extension() {
                if ext == "ts" || ext == "js" {
                    // Skip adapters directory
                    if path.to_string_lossy().contains("/adapters/") {
                        continue;
                    }

                    let relative = path.strip_prefix(src_dir).unwrap();
                    let route = filename_to_route(relative.to_string_lossy().as_ref());
                    let import_path = format!(
                        "./{}",
                        relative
                            .to_string_lossy()
                            .to_string()
                            .trim_end_matches(".ts")
                            .trim_end_matches(".js")
                    );
                    routes.push((route, import_path));
                }
            }
        }
    }

    // Generate main router
    generate_router(output_dir, &routes)?;

    // Generate edgeone.toml
    let edgeone_toml = format!(
        r#"name = "{}"
language = "typescript"
runtime = "edge-functions"
entry_point = "index.js"

[environments.staging]
url = "staging.{}.edgeone.ai"

[environments.production]
url = "{}.edgeone.ai"
"#,
        config.name, config.name, config.name
    );

    fs::write(output_dir.join("edgeone.toml"), edgeone_toml)?;

    // Create adapters directory
    let adapters_dir = output_dir.join("adapters");
    fs::create_dir_all(&adapters_dir)?;

    // Generate KV adapter if KV service is enabled
    if config
        .services
        .as_ref()
        .and_then(|s| s.kv.as_ref())
        .map(|kv| kv.enabled)
        .unwrap_or(false)
    {
        let kv_adapter = r#"/**
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
"#;
        fs::write(adapters_dir.join("kv-adapter.ts"), kv_adapter)?;
    }

    // Generate Context Builder
    let kv_import = if config
        .services
        .as_ref()
        .and_then(|s| s.kv.as_ref())
        .map(|kv| kv.enabled)
        .unwrap_or(false)
    {
        "import { createKVAdapter } from './kv-adapter';\n"
    } else {
        ""
    };

    let kv_usage = if config
        .services
        .as_ref()
        .and_then(|s| s.kv.as_ref())
        .map(|kv| kv.enabled)
        .unwrap_or(false)
    {
        "// Extract KV if present\n    const kv = typeof globalThis.EdgeKV !== 'undefined' ? createKVAdapter(globalThis.EdgeKV) : undefined;"
    } else {
        "const kv = undefined;"
    };

    let context_builder = format!(
        r#"/**
 * EdgeOne Context Builder
 */
{kv_import}
export function createContextBuilder(event: any) {{
  return function buildContext(request: Request, route: string = '/') {{
    const url = new URL(request.url);
    
    {kv_usage}

    return {{
      request,
      env: {{}}, // EdgeOne env vars usually injected globally
      params: extractParams(url.pathname, route),
      path: url.pathname,
      method: request.method,
      query: Object.fromEntries(url.searchParams),
      headers: request.headers,
      cookies: parseCookies(request),
      services: {{
        kv,
      }},
      log: console,
      waitUntil: (promise: Promise<any>) => event.waitUntil(promise),
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
    );
    fs::write(adapters_dir.join("context-builder.ts"), context_builder)?;

    Ok(())
}

fn generate_router(output_dir: &Path, routes: &[(String, String)]) -> anyhow::Result<()> {
    let mut imports = String::new();
    let mut route_handlers = String::new();

    for (i, (route, import_path)) in routes.iter().enumerate() {
        imports.push_str(&format!("import handler{i} from '{import_path}';\n"));

        route_handlers.push_str(&format!(
            r#"  if (matchRoute(pathname, '{route}')) {{
    const context = buildContext(request, '{route}');
    return handler{i}(context);
  }}
"#
        ));
    }

    let router = format!(
        r#"/**
 * Tencent EdgeOne Main Entry
 * Auto-generated router
 */

import {{ createContextBuilder }} from './adapters/context-builder';
{imports}

export default {{
  async fetch(request, event) {{
    const buildContext = createContextBuilder(event);
    const url = new URL(request.url);
    const pathname = url.pathname;

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

    fs::write(output_dir.join("router.ts"), router)?;

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
