//! The Deno Deploy adapter.
//!
//! One adapter per vendor, and the crate is named after the `--vendor` value
//! it answers for: `deis build --vendor deno` is served by
//! `deis-build-adapter-deno`. That correspondence is the whole naming
//! rule, and [`VENDOR`] plus the test at the bottom of this file is what
//! keeps a rename from quietly breaking it.

/// The `--vendor` value this adapter answers for.
///
/// `deis-build` dispatches on it, so the string here and the crate name are
/// the same fact written twice; `the_crate_is_named_after_the_vendor` fails
/// if they ever stop agreeing.
pub const VENDOR: &str = "deno";

use deis_build_core::{AdapterArtifacts, ProjectConfig, StaticAsset};

/// Where the compiler stages this project's static files inside the Deno
/// output directory.
///
/// Deno Deploy has no static layer of its own - nothing serves a file unless
/// the program reads it - so the generated router serves them from here. The
/// router reaches them as `../assets/<path>` because it lives one level down in
/// `_deis_internal/`.
pub const ASSETS_DIR: &str = "assets";

pub fn generate(
    routes: &[(String, String)],
    config: &ProjectConfig,
    assets: &[StaticAsset],
) -> anyhow::Result<AdapterArtifacts> {
    generate_with_node_runtime(routes, config, assets, false)
}

/// Generate a Deno Deploy artifact for the validated Node/npm subset. npm
/// packages are bundled by the compiler; only approved Node builtins remain
/// external for Deno's compatibility layer.
pub fn generate_with_node_runtime(
    routes: &[(String, String)],
    config: &ProjectConfig,
    assets: &[StaticAsset],
    node_runtime: bool,
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
    let router_code = generate_router_code(routes, has_kv, assets, node_runtime);

    // 4. Generate Config Files
    let mut config_files = Vec::new();

    // The import map carries only what this adapter itself knows about.
    //
    // `generate` is not given the project directory, so it cannot read the
    // application's package.json and cannot know which bare specifiers the
    // code imports. Guessing them is worse than leaving them out: this map
    // used to declare `"uuid": "npm:uuid@^9.0.0"` for every application ever
    // built for Deno, whether or not it had heard of uuid, which pins a
    // version on behalf of a project that never asked for one.
    //
    // npm package resolution is a compiler responsibility under Edge Canon;
    // the deployable must not independently resolve a different version.
    let imports = serde_json::Map::new();

    let entry = if node_runtime {
        "index.js"
    } else {
        "router.ts"
    };
    let deno_json = serde_json::json!({
        "tasks": {
            "start": format!("deno run -A {entry}"),
            "dev": format!("deno run --watch -A {entry}")
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

fn generate_router_code(
    routes: &[(String, String)],
    has_kv: bool,
    assets: &[StaticAsset],
    bundled_at_root: bool,
) -> String {
    let mut imports = String::new();
    let mut route_handlers = String::new();

    for (i, (route, import_path)) in routes.iter().enumerate() {
        imports.push_str(&format!("import handler{i} from '{import_path}';\n"));

        let invocation = if bundled_at_root {
            format!("runWithEdgeCanonNodeEnv(context.env, () => handler{i}(context))")
        } else {
            format!("handler{i}(context)")
        };
        route_handlers.push_str(&format!(
            r#"  if (matchRoute(url.pathname, '{route}')) {{
    const context = buildContext(request, '{route}');
    return {invocation};
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
    let node_runtime_import = if bundled_at_root {
        "import { runWithEdgeCanonNodeEnv } from 'edge-canon:node-runtime';\n"
    } else {
        ""
    };
    let kv_comment = if has_kv { "" } else { "undefined; // " };

    // The static files, as a table rather than as a directory listing at
    // request time.
    //
    // Deno Deploy has no asset layer: a file is served only if the program
    // reads it, which is why every Deno build used to leave the project's
    // `public/` unserved and unmentioned. The table is written by the build
    // because only the build knows which files there are and what media type
    // each one gets - the type comes from `deis_bundle::guess_content_type`,
    // the same table the bundle manifest and deis-runtime use, so one file is
    // not served as two different types depending on which target ran.
    //
    // It also bounds what can be read: a path that is not a key is a 404, so no
    // request can walk out of the assets directory and no generated file
    // beside it (`deno.json`, `src/`) is reachable.
    let asset_types: std::collections::BTreeMap<&str, &str> = assets
        .iter()
        .map(|asset| (asset.path.as_str(), asset.content_type.as_str()))
        .collect();
    let asset_table =
        serde_json::to_string_pretty(&asset_types).unwrap_or_else(|_| "{}".to_string());

    let assets_dir = ASSETS_DIR;
    let assets_prefix = if bundled_at_root { "./" } else { "../" };
    format!(
        r#"/**
 * Deno Deploy Entry Point
 * Auto-generated by deis_build
 *
 * Artifact assumptions: deis-build-adapter-deno/spec.mdc (research snapshot)
 * - Entry format: Deno.serve() per Deno Deploy spec
 * - Handler receives Request, returns Promise<Response>
 * - Uses Deno.openKv() for key-value storage
 * - Uses Deno.env for environment variables
 *
 * See: https://docs.deno.com/deploy/
 */

{node_runtime_import}{imports}{kv_import}{context_builder_import}

// Initialize Deno KV database
const kv = {kv_comment}await Deno.openKv();
const buildContext = createContextBuilder(kv);

Deno.serve(async (request: Request): Promise<Response> => {{
  try {{
    const url = new URL(request.url);

    // Asset-first, the way this platform answers: a path that names a file in
    // the bundle is answered from the file and no handler runs.
    const asset = await serveAsset(url.pathname);
    if (asset !== null) return asset;

{route_handlers}
    // 404 Not Found
    return new Response('Not Found', {{ status: 404 }});
  }} catch (error) {{
    console.error('Deno Deploy Error:', error);
    return new Response(
      // `error` is `unknown` in a catch clause, so reaching for `.message`
      // without narrowing first is a type error: `deno check` refused this
      // module, which is exactly the smoke test the build runs on it.
      JSON.stringify({{ error: error instanceof Error ? error.message : String(error) }}),
      {{
        status: 500,
        headers: {{ 'content-type': 'application/json' }}
      }}
    );
  }}
}});

function matchRoute(pathname: string, route: string): boolean {{
  // Exact match
  if (pathname === route) return true;

  // Dynamic parameter match (e.g., /users/:id)
  const routeParts = route.split('/');
  const pathParts = pathname.split('/');

  if (routeParts.length !== pathParts.length) return false;

  return routeParts.every((part, i) => {{
    if (part.startsWith(':')) return true;
    return part === pathParts[i];
  }});
}}

// The static files this build carried, and the media type each one is served
// as. Written by the compiler from the project's assets directory.
const ASSETS: Record<string, string> = {asset_table};

async function serveAsset(pathname: string): Promise<Response | null> {{
  let key = pathname.replace(/^\/+/, '');
  // A directory names its index, so `/` is `index.html` and `/docs/` is
  // `docs/index.html` - the same rule the bundle's asset index applies.
  if (key === '' || key.endsWith('/')) key = key + 'index.html';

  const contentType = ASSETS[key];
  if (contentType === undefined) return null;

  try {{
    const bytes = await Deno.readFile(new URL('{assets_prefix}{assets_dir}/' + key, import.meta.url));
    return new Response(bytes, {{ headers: {{ 'content-type': contentType }} }});
  }} catch (error) {{
    // The table said the file was carried and it could not be read, so the
    // deployment is not what the build produced. A 404 here would read as "no
    // such page" and hide that, so say which file and let the route table have
    // its turn.
    console.error('asset ' + key + ' is in the manifest and could not be read:', error);
    return null;
  }}
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
 * Deno Deploy Context Builder
 *
 * Artifact assumptions: deis-build-adapter-deno/spec.mdc (research snapshot)
 * - Uses Deno.env.toObject() for environment variables
 * - Provides Deno.Kv access through services.kv
 * - Implements Cache API via caches.open()
 * - waitUntil implementation logs errors but doesn't block response
 *
 * See: https://docs.deno.com/deploy/api/runtime-request
 */

{kv_import}

export function createContextBuilder(kv?: Deno.Kv) {{
  return function buildContext(request: Request, route: string = '/') {{
    const url = new URL(request.url);

    return {{
      // Core Deno Deploy context
      request,
      env: Deno.env.toObject(),  // All environment variables

      // Convenience fields
      params: extractParams(url.pathname, route),
      path: url.pathname,
      method: request.method,
      query: Object.fromEntries(url.searchParams),
      headers: request.headers,
      cookies: parseCookies(request),

      // Services
      services: {{
        {kv_service_code}
      }},

      // Logging utilities
      log: {{
        debug: (...args: any[]) => console.debug(...args),
        info: (...args: any[]) => console.info(...args),
        warn: (...args: any[]) => console.warn(...args),
        error: (...args: any[]) => console.error(...args),
      }},

      // Cache API wrapper
      cache: {{
        match: async (req: Request | string) => caches.open('v1').then(c => c.match(req)),
        put: async (req: Request | string, res: Response) => caches.open('v1').then(c => c.put(req, res)),
        delete: async (req: Request | string) => caches.open('v1').then(c => c.delete(req)),
      }},

      // waitUntil polyfill (Deno Deploy doesn't have native waitUntil)
      waitUntil: (promise: Promise<any>) => {{
        // Run async work without blocking response
        promise.catch(err => console.error('Background task error:', err));
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

#[cfg(test)]
mod generated_config {
    use deis_build_core::ProjectConfig;

    /// The import map must declare nothing the project did not ask for.
    ///
    /// This adapter used to add `"uuid": "npm:uuid@^9.0.0"` to every
    /// `deno.json` it wrote, marked "hack for E2E". Every application built
    /// for Deno therefore shipped a pinned dependency on a package it had
    /// very likely never imported, and the pin travelled in the deploy.
    #[test]
    fn the_import_map_invents_no_dependencies() {
        let config: ProjectConfig = serde_json::from_str(
            r#"{"name":"demo","version":"1.0.0","runtime":"standard-v1","language":"typescript"}"#,
        )
        .expect("a minimal config");

        let artifacts = super::generate(&[("/".to_string(), "index.ts".to_string())], &config, &[])
            .expect("the adapter generates");

        let (_, deno_json) = artifacts
            .config_files
            .iter()
            .find(|(name, _)| name == "deno.json")
            .expect("the adapter writes deno.json");

        let parsed: serde_json::Value = serde_json::from_str(deno_json).expect("deno.json parses");
        let imports = parsed["imports"]
            .as_object()
            .expect("deno.json has an imports map");

        assert!(
            imports.is_empty(),
            "an adapter with a closed local graph must not introduce a remote resolver: {imports:?}"
        );
    }
}

#[cfg(test)]
mod naming {
    /// A crate whose name no longer matches the vendor it serves sends the
    /// next reader looking for `deis-build-adapter-deno` in the wrong
    /// place - which is exactly what the `-cf` / `-dc` / `-eo` names used to
    /// do.
    #[test]
    fn the_crate_is_named_after_the_vendor() {
        assert_eq!(
            env!("CARGO_PKG_NAME"),
            format!("deis-build-adapter-{}", super::VENDOR)
        );
    }
}
