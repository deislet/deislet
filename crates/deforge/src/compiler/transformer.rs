use deforge_core::ProjectConfig;

pub struct Transformer {}

impl Transformer {
    pub fn new() -> Self {
        Self {}
    }

    pub fn transform(
        &self,
        code: &str,
        filename: &str,
        vendor: &str,
        config: &ProjectConfig,
    ) -> anyhow::Result<String> {
        // Extract route from filename
        let route = Self::filename_to_route(filename, &config.function_root);

        // Generate transformed code based on vendor
        let transformed = match vendor {
            "cloudflare" => self.transform_cloudflare(code, filename, &route, config)?,
            "deno" => self.transform_deno(code, filename, &route, config)?,
            "tencent" => self.transform_tencent(code, filename, &route, config)?,
            _ => return Err(anyhow::anyhow!("Unknown vendor: {vendor}")),
        };

        Ok(transformed)
    }

    fn transform_cloudflare(
        &self,
        code: &str,
        filename: &str,
        route: &str,
        config: &ProjectConfig,
    ) -> anyhow::Result<String> {
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

        let transformed = format!(
            r#"/**
 * Cloudflare Workers Handler
 * Generated from: {}
 * Route: {}
 */

import {{ createContextBuilder }} from '../adapters/context-builder';
{}{}

// Original handler code
{}

// Cloudflare Workers export
export default {{
  async fetch(request, env, ctx) {{
    const buildContext = createContextBuilder(env, ctx);
    const context = buildContext(request, '{}');

    // Call the handler
    return await handler(context);
  }}
}};
"#,
            filename,
            route,
            if has_kv {
                "import { createKVAdapter } from '../adapters/kv-adapter';\n"
            } else {
                ""
            },
            if has_database {
                "import { createDatabaseAdapter } from '../adapters/database-adapter';\n"
            } else {
                ""
            },
            code,
            route
        );

        Ok(transformed)
    }

    fn transform_deno(
        &self,
        code: &str,
        filename: &str,
        route: &str,
        config: &ProjectConfig,
    ) -> anyhow::Result<String> {
        let has_kv = config
            .services
            .as_ref()
            .and_then(|s| s.kv.as_ref())
            .map(|kv| kv.enabled)
            .unwrap_or(false);

        let transformed = format!(
            r#"/**
 * Deno Deploy Handler
 * Generated from: {}
 * Route: {}
 */

{}

// Original handler code
{}

// Deno Deploy handler
const kv = {}await Deno.openKv() : undefined;

export default async function(request: Request): Promise<Response> {{
  const url = new URL(request.url);
  const context = {{
    request,
    env: Deno.env.toObject(),
    params: extractParams(url.pathname, '{}'),
    path: url.pathname,
    method: request.method,
    query: Object.fromEntries(url.searchParams),
    headers: request.headers,
    cookies: parseCookies(request),
    services: {{
      kv: kv ? createKVAdapter(kv) : undefined,
    }},
    log: {{
      debug: (...args: any[]) => console.debug(...args),
      info: (...args: any[]) => console.info(...args),
      warn: (...args: any[]) => console.warn(...args),
      error: (...args: any[]) => console.error(...args),
    }},
    cache: {{ match: async () => undefined, put: async () => {{}}, delete: async () => false }},
    waitUntil: () => {{}},
  }};

  return handler(context);
}}

function extractParams(pathname: string, route: string): Record<string, string> {{
  const params: Record<string, string> = {{}};
  const routeParts = route.split('/');
  const pathParts = pathname.split('/');
  routeParts.forEach((part, i) => {{
    if (part.startsWith(':')) {{
      params[part.slice(1)] = pathParts[i] || '';
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
    get: (name: string) => cookies[name],
    set: (name: string, value: string) => {{ cookies[name] = value; }},
    delete: (name: string) => {{ delete cookies[name]; }},
    getAll: () => ({{ ...cookies }}),
  }};
}}
"#,
            filename,
            route,
            if has_kv {
                "import { createKVAdapter } from './adapters/kv-adapter.ts';\n"
            } else {
                ""
            },
            code,
            if has_kv { "" } else { "undefined; // " },
            route
        );

        Ok(transformed)
    }

    fn transform_tencent(
        &self,
        code: &str,
        filename: &str,
        route: &str,
        config: &ProjectConfig,
    ) -> anyhow::Result<String> {
        let has_kv = config
            .services
            .as_ref()
            .and_then(|s| s.kv.as_ref())
            .map(|kv| kv.enabled)
            .unwrap_or(false);

        // EdgeOne uses onRequest pattern with EventContext
        let transformed = format!(
            r#"/**
 * Tencent EdgeOne Handler
 * Generated from: {}
 * Route: {}
 */

{}
// Original handler code
{}

// Tencent EdgeOne export (EventContext pattern)
export function onRequest(context) {{
  // EdgeOne provides: {{ request, params, env, waitUntil }}
  // Extend context with our unified interface
  const enhancedContext = {{
    ...context,
    path: new URL(context.request.url).pathname,
    method: context.request.method,
    query: Object.fromEntries(new URL(context.request.url).searchParams),
    headers: context.request.headers,
    cookies: parseCookies(context.request),
    services: {{
      kv: {}context.env.KV ? createKVAdapter(context.env.KV) : undefined,
    }},
    log: {{
      debug: (...args) => console.debug(...args),
      info: (...args) => console.info(...args),
      warn: (...args) => console.warn(...args),
      error: (...args) => console.error(...args),
    }},
    cache: {{
      match: async () => undefined,
      put: async () => ({{}}),
      delete: async () => false
    }},
  }};

  return handler(enhancedContext);
}}

function parseCookies(request) {{
  const cookies = {{}};
  const cookieHeader = request.headers.get('Cookie');
  if (cookieHeader) {{
    cookieHeader.split(';').forEach(cookie => {{
      const [name, ...rest] = cookie.split('=');
      cookies[name.trim()] = rest.join('=').trim();
    }});
  }}
  return {{
    get: (name) => cookies[name],
    set: (name, value) => {{ cookies[name] = value; }},
    delete: (name) => {{ delete cookies[name]; }},
    getAll: () => ({{ ...cookies }}),
  }};
}}
"#,
            filename,
            route,
            if has_kv {
                "import { createKVAdapter } from './adapters/kv-adapter.js';\n"
            } else {
                ""
            },
            code,
            if has_kv { "" } else { "// " }
        );

        Ok(transformed)
    }

    fn filename_to_route(filename: &str, function_root: &str) -> String {
        // Remove function root prefix
        let relative = filename
            .trim_start_matches(function_root)
            .trim_start_matches('/');

        // Remove file extension
        let without_ext = relative
            .trim_end_matches(".ts")
            .trim_end_matches(".js")
            .trim_end_matches(".tsx")
            .trim_end_matches(".jsx");

        // Handle index files
        let route = if without_ext == "index" || without_ext.is_empty() {
            "/"
        } else if without_ext.ends_with("/index") {
            without_ext.trim_end_matches("/index")
        } else {
            without_ext
        };

        // Convert dynamic parameters: [id] -> :id
        let route = route.replace("[", ":").replace("]", "");

        // Ensure leading slash
        if route.starts_with('/') {
            route.to_string()
        } else {
            format!("/{route}")
        }
    }
}
