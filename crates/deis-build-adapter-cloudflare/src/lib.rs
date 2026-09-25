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

use deis_build_core::{
    javascript_runtime_module, javascript_string, AdapterArtifacts, ProjectConfig, StaticAsset,
};

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
    refuse_unplanned_resources(config)?;
    // Everything below runs only for a stateless Worker: KV, D1, Blob, Queue
    // and explicit bindings were all rejected above, so there is no branch on
    // `has_kv`/`has_database` any more and no template that would emit a
    // provider resource identifier the build could not know.
    let environment_declarations =
        deis_build_core::resolve_environment_declaration_entries(config)?;
    let environment_declarations_json = serde_json::to_string(&environment_declarations)?;

    // 1. Generate Context Builder Code
    let context_builder_code = generate_context_builder_code(&environment_declarations_json);

    // 2. Generate Router Code
    let router_code = generate_router_code(routes, node_compat, &environment_declarations_json);

    // 3. Generate Config Files
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
{}{}"#,
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
        kv_adapter_code: None,
        config_files,
        support_files: vec![(
            "edge-canon-runtime.js".to_string(),
            javascript_runtime_module(false),
        )],
    })
}

fn refuse_unplanned_resources(config: &ProjectConfig) -> anyhow::Result<()> {
    let enabled_service = config.services.as_ref().is_some_and(|services| {
        [
            services.kv.as_ref(),
            services.database.as_ref(),
            services.blob.as_ref(),
            services.queue.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|service| service.enabled)
    });
    if !config.bindings.is_empty() || enabled_service || !config.callable_by.is_empty() {
        anyhow::bail!(
            "EC_BINDING_TARGET_UNIMPLEMENTED: the cloudflare adapter requires a resolved standard binding plan; it does not emit empty provider resource identifiers"
        );
    }
    Ok(())
}

fn generate_router_code(
    routes: &[(String, String)],
    node_compat: bool,
    environment_declarations_json: &str,
) -> String {
    let mut imports = String::new();
    let mut route_handlers = String::new();

    for (i, (route, import_path)) in routes.iter().enumerate() {
        let import_literal = javascript_string(import_path);
        let route_literal = javascript_string(route);
        let wildcard_name = deis_build_core::routes::wildcard_name(
            deis_build_core::routes::staged_handler_path(import_path),
        )
        .map(|name| serde_json::to_string(&name).expect("serialize wildcard name"))
        .unwrap_or_else(|| "null".to_string());
        imports.push_str(&format!("import handler{i} from {import_literal};\n"));

        let invocation = if node_compat {
            format!(
                "runWithEdgeCanonNodeEnv(edgeCanonProcessEnvironment(context.env, EDGE_CANON_ENVIRONMENT_DECLARATIONS), () => handler{i}(context))"
            )
        } else {
            format!("handler{i}(context)")
        };
        route_handlers.push_str(&format!(
            r#"  if (matchRoute(url.pathname, {route_literal})) {{
    const lifecycle = buildContext(request, {route_literal}, {wildcard_name});
    const context = lifecycle.context;
    return await invokeEdgeCanonHandler(lifecycle, () => {invocation});
  }}
"#
        ));
    }

    let node_runtime_import = if node_compat {
        "import { runWithEdgeCanonNodeEnv } from 'edge-canon:node-runtime';\nimport { edgeCanonProcessEnvironment } from './edge-canon-runtime.js';\n"
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

import {{ invokeEdgeCanonHandler, edgeCanonFailureResponse, matchRoute }} from './edge-canon-runtime.js';
import {{ createContextBuilder }} from './adapters/context-builder';
{node_runtime_import}{imports}
const EDGE_CANON_ENVIRONMENT_DECLARATIONS = {environment_declarations_json};

export default {{
  async fetch(request, env, ctx) {{
    try {{
      const buildContext = createContextBuilder(env, ctx);
      const url = new URL(request.url);

{route_handlers}
      // 404 Not Found
      return new Response(null, {{ status: 404 }});
    }} catch (error) {{
      return edgeCanonFailureResponse('EC_PROVIDER_CONTEXT_INVALID');
    }}
  }}
}};

"#
    )
}

fn generate_context_builder_code(environment_declarations_json: &str) -> String {
    format!(
        r#"/**
 * Edge Canon context bridge for Cloudflare Workers.
 *
 * Provider objects remain behind this bridge. The application receives only
 * request, env, params and waitUntil, as required by EC-WEB v1.
 */

import {{ createEdgeCanonContext, extractParams, materializeEdgeCanonEnvironment }} from '../edge-canon-runtime.js';

const EDGE_CANON_ENVIRONMENT_DECLARATIONS = {environment_declarations_json};

export function createContextBuilder(env: any, ctx: any) {{
  return function buildContext(request: Request, route: string = '/', wildcardName: string | null = null) {{
    const url = new URL(request.url);
    const standardEnv: Record<string, unknown> = materializeEdgeCanonEnvironment(
      env,
      EDGE_CANON_ENVIRONMENT_DECLARATIONS
    );
    return createEdgeCanonContext(
      request,
      standardEnv,
      extractParams(url.pathname, route, wildcardName),
      ctx && typeof ctx.waitUntil === 'function' ? ctx.waitUntil.bind(ctx) : undefined
    );
  }};
}}
"#
    )
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
