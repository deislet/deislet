use denictl_core::ProjectConfig;
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

pub fn generate(output_dir: &Path, config: &ProjectConfig) -> anyhow::Result<()> {
    // Collect all handlers for router
    let mut routes = Vec::new();
    let src_dir = output_dir;

    for entry in WalkDir::new(&src_dir) {
        let entry = entry?;
        if entry.file_type().is_file() {
            let path = entry.path();
            if let Some(ext) = path.extension() {
                if ext == "ts" || ext == "js" {
                    // Skip adapters directory
                    if path.to_string_lossy().contains("/adapters/") {
                        continue;
                    }

                    let relative = path.strip_prefix(&src_dir).unwrap();
                    let route = filename_to_route(relative.to_string_lossy().as_ref());
                    let import_path = format!("./{}", relative.to_string_lossy().to_string()
                        .trim_end_matches(".ts")
                        .trim_end_matches(".js"));
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
entry_point = "index.ts"

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

    // Generate context builder
    let context_builder = r#"/**
 * Tencent EdgeOne Context Builder
 */

export function createContextBuilder() {
  return function buildContext(request: Request, route: string) {
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
      services: {},
      log: {
        debug: (...args: any[]) => console.debug(...args),
        info: (...args: any[]) => console.info(...args),
        warn: (...args: any[]) => console.warn(...args),
        error: (...args: any[]) => console.error(...args),
      },
      cache: {
        match: async () => undefined,
        put: async () => {},
        delete: async () => false,
      },
      waitUntil: () => {},
    };
  };
}

function extractParams(pathname: string, route: string): Record<string, string> {
  const params: Record<string, string> = {};
  const routeParts = route.split('/');
  const pathParts = pathname.split('/');

  routeParts.forEach((part, i) => {
    if (part.startsWith(':')) {
      params[part.slice(1)] = pathParts[i] || '';
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
    get: (name: string) => cookies[name],
    set: (name: string, value: string) => { cookies[name] = value; },
    delete: (name: string) => { delete cookies[name]; },
    getAll: () => ({ ...cookies }),
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
        imports.push_str(&format!(
            "import {{ handleRequest as handler{} }} from '{}';\n",
            i, import_path
        ));

        route_handlers.push_str(&format!(
            r#"  if (matchRoute(url.pathname, '{}')) {{
    return handler{}(request);
  }}
"#,
            route, i
        ));
    }

    let router = format!(
        r#"/**
 * Tencent EdgeOne Main Entry
 * Auto-generated router
 */

{}

export async function handleRequest(request: Request): Promise<Response> {{
  const url = new URL(request.url);

{}
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
"#,
        imports,
        route_handlers
    );

    fs::write(output_dir.join("index.ts"), router)?;

    Ok(())
}

fn filename_to_route(filename: &str) -> String {
    let without_ext = filename
        .trim_end_matches(".ts")
        .trim_end_matches(".js");

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
        format!("/{}", route)
    }
}
