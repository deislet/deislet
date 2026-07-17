use denictl_core::ProjectConfig;
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

    // Generate KV adapter if KV service is enabled
    if config.services.as_ref()
        .and_then(|s| s.kv.as_ref())
        .map(|kv| kv.enabled)
        .unwrap_or(false)
    {
        let kv_adapter = r#"/**
 * Tencent EdgeOne KV Adapter
 * Wraps EdgeOne KV binding with unified interface
 */

export function createKVAdapter(kvNamespace) {
  return {
    async get(key, options = {}) {
      const value = await kvNamespace.get(key, options.type || 'text');
      return value;
    },

    async getJSON(key) {
      const value = await kvNamespace.get(key, 'json');
      return value;
    },

    async getWithMetadata(key) {
      const value = await kvNamespace.getWithMetadata(key);
      return {
        value: value.value,
        metadata: value.metadata || null,
      };
    },

    async put(key, value, options = {}) {
      await kvNamespace.put(key, value, options);
    },

    async putJSON(key, value, options = {}) {
      await kvNamespace.put(key, JSON.stringify(value), options);
    },

    async delete(key) {
      await kvNamespace.delete(key);
    },

    async deleteMultiple(keys) {
      await Promise.all(keys.map(key => kvNamespace.delete(key)));
    },

    async list(options = {}) {
      const result = await kvNamespace.list(options);
      return {
        keys: result.keys || [],
        list_complete: result.list_complete ?? true,
        cursor: result.cursor,
      };
    },

    async exists(key) {
      const value = await kvNamespace.get(key);
      return value !== null;
    },
  };
}
"#;
        fs::write(adapters_dir.join("kv-adapter.js"), kv_adapter)?;
    }

    Ok(())
}

fn generate_router(output_dir: &Path, routes: &[(String, String)]) -> anyhow::Result<()> {
    let mut imports = String::new();
    let mut route_handlers = String::new();

    for (i, (route, import_path)) in routes.iter().enumerate() {
        // Import onRequest handlers from transformed files
        imports.push_str(&format!(
            "import {{ onRequest as handler{} }} from '{}';\n",
            i, import_path
        ));

        route_handlers.push_str(&format!(
            r#"  if (matchRoute(pathname, '{}')) {{
    // Update params with matched route parameters
    context.params = extractParams(pathname, '{}');
    return handler{}(context);
  }}
"#,
            route, route, i
        ));
    }

    let router = format!(
        r#"/**
 * Tencent EdgeOne Main Entry
 * Auto-generated router
 * Uses EdgeOne's EventContext pattern
 */

{}

// EdgeOne main router using onRequest pattern
export function onRequest(context) {{
  // EdgeOne provides: {{ request, params, env, waitUntil }}
  const url = new URL(context.request.url);
  const pathname = url.pathname;

{}
  // 404 Not Found
  return new Response('Not Found', {{ status: 404 }});
}}

function matchRoute(pathname, route) {{
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

function extractParams(pathname, route) {{
  const params = {{}};
  const routeParts = route.split('/');
  const pathParts = pathname.split('/');

  routeParts.forEach((part, i) => {{
    if (part.startsWith(':')) {{
      params[part.slice(1)] = pathParts[i] || '';
    }}
  }});

  return params;
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
