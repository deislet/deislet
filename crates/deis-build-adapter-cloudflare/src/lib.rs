//! The Cloudflare Workers adapter.
//!
//! One adapter per vendor, and the crate is named after the `--vendor` value
//! it answers for: `deis build --vendor cloudflare` is served by
//! `deis-build-adapter-cloudflare`. That correspondence is the whole naming
//! rule, and [`VENDOR`] plus the test at the bottom of this file is what
//! keeps a rename from quietly breaking it.

/// The `--vendor` value this adapter answers for.
///
/// `deis-build` dispatches on it, so the string here and the crate name are
/// the same fact written twice; `the_crate_is_named_after_the_vendor` fails
/// if they ever stop agreeing.
pub const VENDOR: &str = "cloudflare";

use deis_build_core::{AdapterArtifacts, ProjectConfig, StaticAsset};

/// Where the compiler stages this project's static files inside the Cloudflare
/// output directory, and what `wrangler.toml` points at.
///
/// One constant, because the two have to be the same directory: wrangler serves
/// whatever `[assets] directory` names and nothing else, so a copy that landed
/// anywhere else would be uploaded as inert weight.
pub const ASSETS_DIR: &str = "assets";

pub fn generate(
    routes: &[(String, String)],
    config: &ProjectConfig,
    assets: &[StaticAsset],
) -> anyhow::Result<AdapterArtifacts> {
    generate_with_node_compat(routes, config, assets, false)
}

/// Generate the same Worker artifact while enabling Cloudflare's native Node
/// implementation for an already-validated Edge Canon Node/npm graph. The
/// source validator, not this provider flag, decides which builtins/exports an
/// application may use.
pub fn generate_with_node_compat(
    routes: &[(String, String)],
    config: &ProjectConfig,
    assets: &[StaticAsset],
    node_compat: bool,
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
    let router_code = generate_router_code(routes, has_kv, node_compat);

    // 4. Generate Config Files
    let mut config_files = Vec::new();

    // wrangler.toml
    //
    // Everything in it comes from the project file. Three things used to be
    // invented here instead, and all three were wrong in the same way - the
    // author read them back as their own:
    //
    //   * `compatibility_date = "2024-09-23"`, a fixed day, whatever date the
    //     project pinned. A compatibility date is the whole mechanism for
    //     deciding which of a runtime's behaviours an application gets, so an
    //     author who pinned 2026-09-01 and deployed to Cloudflare was running
    //     under semantics two years older than the ones they tested against.
    //   * `compatibility_flags = ["nodejs_compat"]`, which nobody wrote. It
    //     turns on Node built-ins in the Worker; the bundler here targets the
    //     browser platform and emits no Node imports, so it bought nothing and
    //     was never asked for.
    //   * `[env.staging]` and `[env.production]` with `route =
    //     "<name>.workers.dev"`. `route` binds a Worker to a hostname on a zone
    //     the account controls, and `workers.dev` is not one of those, so the
    //     two blocks named deployment environments the author never declared
    //     and pointed them at routes that cannot be taken.
    let compatibility_date = config.compatibility_date.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "the cloudflare target needs a compatibility date and this project pins none. wrangler \
             requires `compatibility_date` to deploy a Worker, and this build will not choose one on \
             your behalf: a date filled in by the build would move on its own, so the same unchanged \
             source would run under different semantics the next time it was built. Add \
             \"compatibilityDate\": \"YYYY-MM-DD\" to .config.json"
        )
    })?;
    if !config.compatibility_flags.is_empty() {
        return Err(anyhow::anyhow!(
            "this project declares the compatibility flag(s) {:?}, which name behaviours of this \
             platform (`deis_bundle::compatibility::FLAGS`) and mean nothing to Cloudflare Workers, \
             whose flags are its own. Carrying them over would produce a wrangler.toml wrangler \
             refuses, and dropping them would build a Worker that behaves differently from the one \
             you pinned. Remove the flags, or drop `cloudflare` from `vendors`",
            config.compatibility_flags
        ));
    }

    let wrangler_toml = format!(
        r#"name = "{}"
main = "index.js"
compatibility_date = "{compatibility_date}"
{}{}{}{}"#,
        config.name,
        if node_compat {
            "compatibility_flags = [\"nodejs_compat\"]\n"
        } else {
            ""
        },
        if assets.is_empty() {
            String::new()
        } else {
            // What makes wrangler upload and serve the project's static files.
            // Without it the directory is deployed and never looked at, which
            // is what happened to every Cloudflare build before this: the files
            // were not even copied.
            format!(
                r#"
[assets]
directory = "./{ASSETS_DIR}"
"#
            )
        },
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

fn generate_router_code(routes: &[(String, String)], has_kv: bool, node_compat: bool) -> String {
    let mut imports = String::new();
    let mut route_handlers = String::new();

    for (i, (route, import_path)) in routes.iter().enumerate() {
        let import_literal = serde_json::to_string(import_path).expect("serialize import path");
        let route_literal = serde_json::to_string(route).expect("serialize route");
        let wildcard_name = deis_build_core::routes::wildcard_name(
            deis_build_core::routes::staged_handler_path(import_path),
        )
        .map(|name| serde_json::to_string(&name).expect("serialize wildcard name"))
        .unwrap_or_else(|| "null".to_string());
        imports.push_str(&format!("import handler{i} from {import_literal};\n"));

        let invocation = if node_compat {
            format!("runWithEdgeCanonNodeEnv(env, () => handler{i}(context))")
        } else {
            format!("handler{i}(context)")
        };
        route_handlers.push_str(&format!(
            r#"  if (matchRoute(url.pathname, {route_literal})) {{
    const context = buildContext(request, {route_literal}, {wildcard_name});
    return {invocation};
  }}
"#
        ));
    }

    let _kv_import = if has_kv {
        "import { createKVAdapter } from './adapters/kv-adapter';\n"
    } else {
        ""
    };
    let node_runtime_import = if node_compat {
        "import { runWithEdgeCanonNodeEnv } from 'edge-canon:node-runtime';\n"
    } else {
        ""
    };

    format!(
        r#"/**
 * Cloudflare Workers Entry Point
 * Auto-generated by deis_build
 *
 * Artifact assumptions: deis-build-adapter-cloudflare/spec.mdc (research snapshot)
 * - Entry format: Modules syntax (export default {{ fetch() }})
 * - Handler params: request, env, ctx per Cloudflare spec
 * - Context builder provides enhanced context with utilities
 *
 * See: https://developers.cloudflare.com/workers/
 */

import {{ createContextBuilder }} from './adapters/context-builder';
{node_runtime_import}{imports}

export default {{
  async fetch(request, env, ctx) {{
    try {{
      const buildContext = createContextBuilder(env, ctx);
      const url = new URL(request.url);

{route_handlers}
      // 404 Not Found
      return new Response('Not Found', {{ status: 404 }});
    }} catch (error) {{
      console.error('Worker Error:', error);
      return new Response('Internal Server Error', {{
        status: 500,
        headers: {{ 'content-type': 'text/plain' }}
      }});
    }}
  }}
}};

function matchRoute(pathname: string, route: string): boolean {{
  const routeParts = route.split('/').filter(Boolean);
  const pathParts = pathname.split('/').filter(Boolean);
  for (let i = 0; i < routeParts.length; i++) {{
    const part = routeParts[i];
    if (part === '*') return i === routeParts.length - 1;
    if (i >= pathParts.length) return false;
    if (part.startsWith(':')) continue;
    if (decodeSegment(part) !== decodeSegment(pathParts[i])) return false;
  }}
  return routeParts.length === pathParts.length;
}}

function decodeSegment(segment: string): string {{
  try {{ return decodeURIComponent(segment); }}
  catch (_) {{ return segment; }}
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
 * Cloudflare Workers Context Builder
 *
 * Artifact assumptions: deis-build-adapter-cloudflare/spec.mdc (research snapshot)
 * - Passes through env (all bindings: KV, D1, R2, DO, vars, secrets)
 * - Passes through ctx (execution context with waitUntil, passThroughOnException)
 * - Exposes request.cf (Cloudflare-specific data)
 * - Provides utility methods for common operations
 *
 * See: https://developers.cloudflare.com/workers/runtime-apis/handlers/fetch/
 */

{kv_import}

export function createContextBuilder(env: any, ctx: any) {{
  return function buildContext(request: Request, route: string = '/', wildcardName: string | null = null) {{
    const url = new URL(request.url);

    return {{
      // Core Cloudflare context (per spec.mdc)
      request,
      env,  // Pass through all environment bindings
      ctx,  // Execution context

      // Cloudflare-specific request properties
      cf: request.cf,  // Cloudflare data (colo, country, city, etc.)

      // Convenience fields
      params: extractParams(url.pathname, route, wildcardName),
      path: url.pathname,
      method: request.method,
      query: Object.fromEntries(url.searchParams),
      headers: request.headers,
      cookies: parseCookies(request),

      // Services (backwards compatibility)
      services: {{
        kv: env.KV ? createKVAdapter(env.KV) : undefined,
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
        match: async (req: Request | string) => caches.default.match(req),
        put: async (req: Request | string, res: Response) => caches.default.put(req, res),
        delete: async (req: Request | string) => caches.default.delete(req),
      }},

      // Context methods
      waitUntil: (promise: Promise<any>) => ctx.waitUntil(promise),
      passThroughOnException: () => ctx.passThroughOnException(),
    }};
  }};
}}

function extractParams(pathname: string, route: string, wildcardName: string | null): Record<string, string> {{
  const params: Record<string, string> = {{}};
  const routeParts = route.split('/').filter(Boolean);
  const pathParts = pathname.split('/').filter(Boolean);

  routeParts.forEach((part, i) => {{
    if (part === '*') {{
      const rest = pathParts.slice(i).map(decodeSegment).join('/');
      params['*'] = rest;
      if (wildcardName !== null) params[wildcardName] = rest;
    }} else if (part.startsWith(':')) {{
      const paramName = part.slice(1);
      params[paramName] = decodeSegment(pathParts[i] || '');
    }}
  }});

  return params;
}}

function decodeSegment(segment: string): string {{
  try {{ return decodeURIComponent(segment); }}
  catch (_) {{ return segment; }}
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

#[cfg(test)]
mod naming {
    /// A crate whose name no longer matches the vendor it serves sends the
    /// next reader looking for `deis-build-adapter-cloudflare` in the wrong
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
