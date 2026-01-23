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
        if entry.file_type().is_file() {
            let path = entry.path();
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
    generate_router(output_dir, &routes, config)?;

    // Read user package.json to generate imports
    let mut imports = serde_json::Map::new();
    // Default std imports
    imports.insert(
        "std/".to_string(),
        serde_json::json!("https://deno.land/std@0.208.0/"),
    );

    // Try to read project package.json
    let project_root = output_dir.parent().unwrap().parent().unwrap();
    let package_json_path = project_root.join("package.json");

    if package_json_path.exists() {
        if let Ok(content) = fs::read_to_string(package_json_path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(deps) = json.get("dependencies").and_then(|d| d.as_object()) {
                    for (pkg, ver) in deps {
                        let ver_str = ver.as_str().unwrap_or("latest");
                        // Clean version string (remove ^ or ~) to be safe with npm: specifiers,
                        // though deno supports semver ranges in npm specifiers too.
                        imports.insert(
                            pkg.clone(),
                            serde_json::json!(format!("npm:{}@{}", pkg, ver_str)),
                        );
                    }
                }
            }
        }
    }

    // Generate deno.json
    let deno_json = serde_json::json!({
        "tasks": {
            "start": "deno run -A main.bundle.js",
            "dev": "deno run --watch -A main.bundle.js"
        },
        "imports": imports
    });

    fs::write(
        output_dir.join("deno.json"),
        serde_json::to_string_pretty(&deno_json)?,
    )?;

    // Generate KV adapter
    let kv_adapter = r#"/**
 * Deno KV Adapter
 */

export function createKVAdapter(kv: Deno.Kv) {
  return {
    async get(key: string): Promise<string | null> {
      const result = await kv.get([key]);
      return result.value ? String(result.value) : null;
    },

    async getJSON<T = any>(key: string): Promise<T | null> {
      const result = await kv.get([key]);
      return result.value as T | null;
    },

    async put(key: string, value: string, options?: any): Promise<void> {
      const kvOptions: any = {};
      if (options?.expirationTtl) {
        kvOptions.expireIn = options.expirationTtl * 1000;
      }
      await kv.set([key], value, kvOptions);
    },

    async putJSON<T = any>(key: string, value: T, options?: any): Promise<void> {
      const kvOptions: any = {};
      if (options?.expirationTtl) {
        kvOptions.expireIn = options.expirationTtl * 1000;
      }
      await kv.set([key], value, kvOptions);
    },

    async delete(key: string): Promise<void> {
      await kv.delete([key]);
    },

    async list(options?: any) {
      const entries = kv.list(options?.prefix ? { prefix: [options.prefix] } : {});
      const keys: any[] = [];

      for await (const entry of entries) {
        keys.push({
          name: entry.key[0],
          metadata: entry.versionstamp
        });
      }

      return {
        keys,
        list_complete: true,
        cursor: undefined
      };
    },

    async exists(key: string): Promise<boolean> {
      const result = await kv.get([key]);
      return result.value !== null;
    }
  };
}
"#;

    let adapters_dir = output_dir.join("adapters");
    fs::create_dir_all(&adapters_dir)?;
    fs::write(adapters_dir.join("kv-adapter.ts"), kv_adapter)?;

    // Generate Context builder
    let context_builder = r#"/**
 * Deno Context Builder
 */

import { createKVAdapter } from './kv-adapter.ts';

export function createContextBuilder(kv?: Deno.Kv) {
  return function buildContext(request: Request, route: string = '/') {
    const url = new URL(request.url);

    return {
      request,
      env: Deno.env.toObject(),
      params: extractParams(url.pathname, route),
      path: url.pathname,
      method: request.method,
      query: Object.fromEntries(url.searchParams),
      headers: request.headers,
      cookies: parseCookies(request),
      services: {
        kv: kv ? createKVAdapter(kv) : undefined,
      },
      log: {
        debug: (...args: any[]) => console.debug(...args),
        info: (...args: any[]) => console.info(...args),
        warn: (...args: any[]) => console.warn(...args),
        error: (...args: any[]) => console.error(...args),
      },
      // Deno Deploy Cache API is standard Web Cache API
      cache: {
        match: async (req: Request | string) => caches.open('v1').then(c => c.match(req)),
        put: async (req: Request | string, res: Response) => caches.open('v1').then(c => c.put(req, res)),
        delete: async (req: Request | string) => caches.open('v1').then(c => c.delete(req)),
      },
      waitUntil: (promise: Promise<any>) => {
        // Deno Deploy doesn't strictly require waitUntil for background tasks
        // but we can just not await it here.
        promise.catch(console.error);
      },
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

fn generate_router(
    output_dir: &Path,
    routes: &[(String, String)],
    config: &ProjectConfig,
) -> anyhow::Result<()> {
    let has_kv = config
        .services
        .as_ref()
        .and_then(|s| s.kv.as_ref())
        .map(|kv| kv.enabled)
        .unwrap_or(false);

    let mut imports = String::new();
    let mut route_handlers = String::new();

    for (i, (route, import_path)) in routes.iter().enumerate() {
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
        "import { createKVAdapter } from './adapters/kv-adapter.ts';\n"
    } else {
        ""
    };

    // Always import context builder
    let context_builder_import =
        "import { createContextBuilder } from './adapters/context-builder.ts';\n";

    let kv_comment = if has_kv { "" } else { "undefined; // " };

    let router = format!(
        r#"/**
 * Deno Deploy Main Entry
 * Auto-generated router
 */

{imports}{kv_import}{context_builder_import}

const kv = {kv_comment}await Deno.openKv();
const buildContext = createContextBuilder(kv);

Deno.serve(async (request: Request): Promise<Response> => {{
  const url = new URL(request.url);

{route_handlers}
  // 404 Not Found
  return new Response('Not Found', {{ status: 404 }});
}});

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

console.log('🚀 Deno Deploy server running...');
"#
    );

    fs::write(output_dir.join("main.ts"), router)?;

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
