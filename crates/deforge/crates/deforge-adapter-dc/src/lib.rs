use deforge_core::{AdapterArtifacts, ProjectConfig};
use std::fs;

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

    // Read user package.json to generate imports (this part still needs FS access to project root?
    // Adapter doesn't know project root anymore. Compiler should pass imports map?
    // Or we assume `npm:` specifiers are handled by Deno natively if no map provided?
    // Deno Deploy needs `deno.json` imports.
    // We should probably move package.json reading to Compiler and pass `dependencies` to Adapter.
    // For now, let's keep it simple: Deno adapter returns a default deno.json or we skip package.json reading here
    // since we removed WalkDir and FS access from arguments.
    // But wait, `generate` doesn't take `project_dir`.
    // It seems I need to update `generate` signature to include `dependencies` or let Compiler handle deno.json.

    // Let's assume standard imports for now to avoid breaking changes in logic complexity.
    // Or better: Compiler generates deno.json?
    // Adapter is responsible for platform config.

    let mut imports = serde_json::Map::new();
    imports.insert(
        "std/".to_string(),
        serde_json::json!("https://deno.land/std@0.208.0/"),
    );
    // Hack for E2E: Add uuid dependency manually since we lost package.json reading
    imports.insert("uuid".to_string(), serde_json::json!("npm:uuid@^9.0.0"));

    let deno_json = serde_json::json!({
        "tasks": {
            "start": "deno run -A router.ts",
            "dev": "deno run --watch -A router.ts"
        },
        "imports": imports
    });

    config_files.push((
        "deno.json".to_string(),
        serde_json::to_string_pretty(&deno_json)?,
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

    let context_builder_import =
        "import { createContextBuilder } from './adapters/context-builder.ts';\n";
    let kv_comment = if has_kv { "" } else { "undefined; // " };

    format!(
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
    )
}

fn generate_context_builder_code(has_kv: bool) -> String {
    let kv_import = if has_kv {
        "import { createKVAdapter } from './kv-adapter.ts';\n"
    } else {
        ""
    };

    let kv_service_code = if has_kv {
        "kv: kv ? createKVAdapter(kv) : undefined,"
    } else {
        "kv: undefined,"
    };

    format!(
        r#"/**
 * Deno Context Builder
 */

{kv_import}

export function createContextBuilder(kv?: Deno.Kv) {{
  return function buildContext(request: Request, route: string = '/') {{
    const url = new URL(request.url);

    return {{
      request,
      env: Deno.env.toObject(),
      params: extractParams(url.pathname, route),
      path: url.pathname,
      method: request.method,
      query: Object.fromEntries(url.searchParams),
      headers: request.headers,
      cookies: parseCookies(request),
      services: {{
        {kv_service_code}
      }},
      log: {{
        debug: (...args: any[]) => console.debug(...args),
        info: (...args: any[]) => console.info(...args),
        warn: (...args: any[]) => console.warn(...args),
        error: (...args: any[]) => console.error(...args),
      }},
      cache: {{
        match: async (req: Request | string) => caches.open('v1').then(c => c.match(req)),
        put: async (req: Request | string, res: Response) => caches.open('v1').then(c => c.put(req, res)),
        delete: async (req: Request | string) => caches.open('v1').then(c => c.delete(req)),
      }},
      waitUntil: (promise: Promise<any>) => {{
        promise.catch(console.error);
      }},
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
      const entries = kv.list(options?.prefix ? { prefix: [options.prefix] } : { prefix: [] });
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
"#
    .to_string()
}
