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
    generate_router(output_dir, &routes, config)?;

    // Generate deno.json
    let deno_json = serde_json::json!({
        "tasks": {
            "start": "deno run -A main.ts",
            "dev": "deno run --watch -A main.ts"
        },
        "imports": {
            "std/": "https://deno.land/std@0.208.0/"
        }
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

    Ok(())
}

fn generate_router(output_dir: &Path, routes: &[(String, String)], config: &ProjectConfig) -> anyhow::Result<()> {
    let has_kv = config.services.as_ref()
        .and_then(|s| s.kv.as_ref())
        .map(|kv| kv.enabled)
        .unwrap_or(false);

    let mut imports = String::new();
    let mut route_handlers = String::new();

    for (i, (route, import_path)) in routes.iter().enumerate() {
        imports.push_str(&format!(
            "import handler{} from '{}';\n",
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
 * Deno Deploy Main Entry
 * Auto-generated router
 */

{}{}

const kv = {}await Deno.openKv() : undefined;

Deno.serve(async (request: Request): Promise<Response> => {{
  const url = new URL(request.url);

{}
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
"#,
        imports,
        if has_kv { "import { createKVAdapter } from './adapters/kv-adapter.ts';\n\n" } else { "" },
        if has_kv { "" } else { "undefined; // " },
        route_handlers
    );

    fs::write(output_dir.join("main.ts"), router)?;

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
