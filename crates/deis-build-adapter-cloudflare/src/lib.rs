//! The Cloudflare Workers adapter.
//!
//! One adapter per vendor, and the crate is named after the `--vendor` value
//! it answers for: `deis build --vendor cloudflare` is served by
//! `deis-build-adapter-cloudflare`. That correspondence is the whole naming
//! rule, and [`VENDOR`] plus the test at the bottom of this file is what
//! keeps a rename from quietly breaking it.
//!
//! The adapter builds from the standard
//! [`binding plan`](deis_build_core::build_binding_plan): every logical
//! resource the project declares is mapped onto the provider section that
//! hosts it, and what the build emits for the resource itself is only the
//! plan's unresolved placeholder. The provider driver resolves those
//! placeholders from the deployment state at prepare time, before any
//! provider tool runs - so the build never invents a resource identifier,
//! and nothing it emits can deploy against the wrong one.

/// The `--vendor` value this adapter answers for.
///
/// `deis-build` dispatches on it, so the string here and the crate name are
/// the same fact written twice; `the_crate_is_named_after_the_vendor` fails
/// if they ever stop agreeing.
pub const VENDOR: &str = "cloudflare";

use deis_build_core::{
    AdapterArtifacts, BindingKind, ProjectConfig, StaticAsset, build_binding_plan,
    javascript_runtime_module, javascript_string,
};

/// Where the compiler stages this project's static files inside the Cloudflare
/// output directory, and what `wrangler.toml` points at.
///
/// One constant, because the two have to be the same directory: wrangler serves
/// whatever `[assets] directory` names and nothing else, so a copy that landed
/// anywhere else would be uploaded as inert weight.
pub const ASSETS_DIR: &str = "assets";

/// The file the adapter writes the binding plan into, beside `wrangler.toml`.
///
/// `wrangler.toml` has to be valid wrangler configuration, so it carries the
/// placeholders *inline* where wrangler's own schema wants an identifier. This
/// file is the machine-readable half the provider driver reads first: it names
/// every logical binding, its kind and its placeholder, so the driver resolves
/// from the plan instead of scraping TOML.
pub const BINDING_PLAN_FILE: &str = "binding-plan.json";

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
    // The plan is the gate and the source of truth. A project that asks for
    // something this target does not map - a service binding, `callableBy`,
    // an unnamed legacy service, a name the provider environment cannot
    // spell - fails here with `EC_BINDING_TARGET_UNIMPLEMENTED`, before any
    // artifact is emitted. Everything below plans only what the plan agreed
    // to.
    let plan = build_binding_plan(config)?;
    let plan_json = serde_json::to_string_pretty(&plan)?;

    // Everything above runs only for a planned Worker: the resources that
    // reach wrangler.toml come from the plan's placeholders, and nothing is
    // invented here.
    let environment_declarations =
        deis_build_core::resolve_environment_declaration_entries(config)?;
    let environment_declarations_json = serde_json::to_string(&environment_declarations)?;

    // Deployment-time `config` values become `[vars]`, one placeholder per
    // declared name. The build has no values - they are deployment-only and
    // deliberately kept out of canonical build identity - so the driver
    // resolves each one at prepare from the deployment's config, exactly as
    // it resolves resource identifiers. Secrets are never written here: they
    // go to the provider through its secret command, not through a file.
    let config_var_names = environment_declarations
        .iter()
        .filter(|declaration| declaration.kind == deis_wire::EnvironmentKind::Config)
        .map(|declaration| declaration.name.clone())
        .collect::<Vec<_>>();

    // 1. Generate Context Builder Code
    let context_builder_code = generate_context_builder_code(&environment_declarations_json);

    // 2. Generate Router Code
    let router_code = generate_router_code(routes, node_compat, &environment_declarations_json);

    // 3. Generate Config Files
    let mut config_files = Vec::new();

    // wrangler.toml
    //
    // Everything in it comes from the project file or the binding plan.
    // Three things used to be invented here instead, and all three were
    // wrong in the same way - the author read them back as their own:
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

    let wrangler_toml = generate_wrangler_toml(
        &plan,
        &config.name,
        &config_var_names,
        compatibility_date,
        assets,
        node_compat,
    );
    config_files.push(("wrangler.toml".to_string(), wrangler_toml));
    config_files.push((BINDING_PLAN_FILE.to_string(), plan_json));

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

/// Render `wrangler.toml` from the binding plan.
///
/// Every provider resource identifier this writes is the plan's unresolved
/// placeholder - never an empty string, never an invented one. The comment
/// above each says what resolves it, so a person reading the file meets the
/// same contract the driver enforces.
fn generate_wrangler_toml(
    plan: &deis_build_core::BindingPlan,
    project_name: &str,
    config_var_names: &[String],
    compatibility_date: &str,
    assets: &[StaticAsset],
    node_compat: bool,
) -> String {
    // Assembled section by section rather than formatted in one giant
    // literal: a stable section order is what keeps the output deterministic
    // across builds (and across the driver's placeholder rewrites), and each
    // section's tests can assert on its own slice.
    let mut sections = String::new();

    sections.push_str(&format!(
        "name = {}\nmain = \"index.js\"\ncompatibility_date = \"{compatibility_date}\"\n",
        javascript_string(project_name),
    ));

    if node_compat {
        sections.push_str("compatibility_flags = [\"nodejs_compat\"]\n");
    }

    // Cron triggers, verbatim from the project file.
    if !plan.crons.is_empty() {
        let crons = plan
            .crons
            .iter()
            .map(|cron| javascript_string(cron))
            .collect::<Vec<_>>()
            .join(", ");
        sections.push_str(&format!("\n[triggers]\ncrons = [{crons}]\n"));
    }

    for binding in &plan.bindings {
        match binding.kind {
            BindingKind::Kv => sections.push_str(&format!(
                "\n[[kv_namespaces]]\nbinding = {}\n# Resolved by the deis provider driver at \
                 prepare time from the deployment state; this placeholder is replaced, never \
                 deployed.\nid = {}\n",
                javascript_string(&binding.name),
                javascript_string(&binding.provider_resource_ref),
            )),
            BindingKind::Database => sections.push_str(&format!(
                "\n[[d1_databases]]\nbinding = {}\ndatabase_name = {}\n# Resolved by the deis \
                 provider driver at prepare time from the deployment state; this placeholder is \
                 replaced, never deployed.\ndatabase_id = {}\n",
                javascript_string(&binding.name),
                javascript_string(&binding.name),
                javascript_string(&binding.provider_resource_ref),
            )),
            BindingKind::Blob => sections.push_str(&format!(
                "\n[[r2_buckets]]\nbinding = {}\n# The bucket name is the driver's provisioning \
                 decision; the placeholder is replaced at prepare time from the deployment \
                 state.\nbucket_name = {}\n",
                javascript_string(&binding.name),
                javascript_string(&binding.provider_resource_ref),
            )),
            BindingKind::Queue => sections.push_str(&format!(
                "\n[[queues.producers]]\nbinding = {}\n# The queue is named, not identified: the \
                 driver provisions a queue with this logical name at prepare time.\nqueue = {}\n",
                javascript_string(&binding.name),
                javascript_string(&binding.provider_resource_ref),
            )),
        }
    }

    for consumer in &plan.queue_consumers {
        sections.push_str(&format!(
            "\n[[queues.consumers]]\nqueue = {}\nmax_retries = {}\n",
            javascript_string(&consumer.queue),
            consumer.max_retries,
        ));
        if let Some(batch_size) = consumer.max_batch_size {
            sections.push_str(&format!("max_batch_size = {batch_size}\n"));
        }
        if let Some(concurrency) = consumer.max_concurrency {
            sections.push_str(&format!("max_concurrency = {concurrency}\n"));
        }
    }

    if !config_var_names.is_empty() {
        sections.push_str("\n[vars]\n");
        for name in config_var_names {
            sections.push_str(&format!(
                "{name} = {}\n",
                javascript_string(&format!("deis:unresolved/config/{name}"))
            ));
        }
    }

    if !assets.is_empty() {
        // What makes wrangler upload and serve the project's static files.
        // Without it the directory is deployed and never looked at, which
        // is what happened to every Cloudflare build before this: the files
        // were not even copied.
        sections.push_str(&format!("\n[assets]\ndirectory = \"./{ASSETS_DIR}\"\n"));
    }

    // One piece, top to bottom.
    sections
}

fn generate_router_code(
    routes: &[(String, String)],
    node_compat: bool,
    environment_declarations_json: &str,
) -> String {
    let mut imports = String::new();
    let mut route_handlers = String::new();
    let mut queue_import: Option<String> = None;
    let mut scheduled_import: Option<String> = None;

    for (i, (route, import_path)) in routes.iter().enumerate() {
        // A root `_queue` or `_scheduled` file is an event handler the
        // platform calls, not a request route. It gets an export of its own
        // below, never a place in the fetch table.
        match deis_build_core::routes::handler_kind(import_path) {
            deis_build_core::routes::HandlerKind::Queue => {
                queue_import = Some(import_path.clone());
                continue;
            }
            deis_build_core::routes::HandlerKind::Scheduled => {
                scheduled_import = Some(import_path.clone());
                continue;
            }
            deis_build_core::routes::HandlerKind::Reserved
            | deis_build_core::routes::HandlerKind::Http => {}
        }

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

    // The platform-called handlers. On this target the Workers runtime calls
    // them with the native `(batch|event, env, ctx)`; the exports below are
    // the bridge that builds the standard context from the native pieces and
    // hands the handler the same three arguments Edge Canon fixes -
    // `(batch, env, context)` and `(event, env, context)` - so a handler
    // written to the canon runs here exactly as it runs on the self-hosted
    // target (SPECIFICATION_EXT.md sections 3.1 and 3.2).
    let mut event_exports = String::new();
    if let Some(import) = &queue_import {
        event_exports.push_str(&format!(
            "\n// Queue consumer. Workers hands it a batch of messages; the export builds the\n\
             \x20// standard context the canon's queue handler is defined against.\n\
             import queueHandler from {};\n\
             export async function queue(batch, env, ctx) {{\n\
             \x20 const context = buildEventContext(env, ctx);\n\
             \x20 return queueHandler(batch, context.env, context);\n\
             }}\n",
            javascript_string(import)
        ));
    }
    if let Some(import) = &scheduled_import {
        event_exports.push_str(&format!(
            "\n// Cron handler. Workers triggers it on the `[triggers]` schedule; the export\n\
             \x20// builds the standard context the canon's scheduled handler is defined against.\n\
             import scheduledHandler from {};\n\
             export async function scheduled(event, env, ctx) {{\n\
             \x20 const context = buildEventContext(env, ctx);\n\
             \x20 return scheduledHandler(event, context.env, context);\n\
             }}\n",
            javascript_string(import)
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

import {{ invokeEdgeCanonHandler, edgeCanonFailureResponse, matchRoute, createEdgeCanonContext, materializeEdgeCanonEnvironment }} from './edge-canon-runtime.js';
import {{ createContextBuilder }} from './adapters/context-builder';
{node_runtime_import}{imports}
const EDGE_CANON_ENVIRONMENT_DECLARATIONS = {environment_declarations_json};

// The context a queue batch or a cron tick is answered with: no request, no
// path, the provider's execution context for `waitUntil`.
function buildEventContext(env, ctx) {{
  return createEdgeCanonContext(
    null,
    materializeEdgeCanonEnvironment(env, EDGE_CANON_ENVIRONMENT_DECLARATIONS),
    {{}},
    ctx && typeof ctx.waitUntil === 'function' ? ctx.waitUntil.bind(ctx) : undefined
  );
}}
{event_exports}
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
