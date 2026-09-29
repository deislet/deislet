//! The Deno Deploy adapter.
//!
//! One adapter per target, and the crate is named after the `--target` value
//! it answers for: `deis build --target deno` is served by
//! `deis-build-adapter-deno`. That correspondence is the whole naming
//! rule, and [`VENDOR`] plus the test at the bottom of this file is what
//! keeps a rename from quietly breaking it.

/// The `--target` value this adapter answers for.
///
/// `deis-build` dispatches on it, so the string here and the crate name are
/// the same fact written twice; `the_crate_is_named_after_the_vendor` fails
/// if they ever stop agreeing.
pub const VENDOR: &str = "deno";

use deis_build_core::{
    javascript_runtime_module, javascript_string, AdapterArtifacts, ProjectConfig, StaticAsset,
};

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
    refuse_unplanned_resources(config)?;
    let environment_declarations =
        deis_build_core::resolve_environment_declaration_entries(config)?;
    let environment_declarations_json = serde_json::to_string(&environment_declarations)?;
    // 1. Generate Context Builder Code
    let context_builder_code = generate_context_builder_code(&environment_declarations_json);

    // 3. Generate Router Code
    let router_code =
        generate_router_code(routes, assets, node_runtime, &environment_declarations_json);

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
        config_files,
        support_files: vec![(
            "edge-canon-runtime.js".to_string(),
            javascript_runtime_module(!node_runtime),
        )],
    })
}

fn refuse_unplanned_resources(config: &ProjectConfig) -> anyhow::Result<()> {
    if !config.bindings.is_empty() || !config.callable_by.is_empty() {
        anyhow::bail!(
            "EC_BINDING_TARGET_UNIMPLEMENTED: the deno adapter requires a resolved standard binding plan"
        );
    }
    Ok(())
}

fn generate_router_code(
    routes: &[(String, String)],
    assets: &[StaticAsset],
    bundled_at_root: bool,
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

        let invocation = if bundled_at_root {
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

    let context_builder_import =
        "import { createContextBuilder } from './adapters/context-builder.ts';\n";
    let node_runtime_import = if bundled_at_root {
        "import { runWithEdgeCanonNodeEnv } from 'edge-canon:node-runtime';\nimport { edgeCanonProcessEnvironment } from './edge-canon-runtime.js';\n"
    } else {
        ""
    };

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
 * - Uses Deno.env for environment variables
 *
 * See: https://docs.deno.com/deploy/
 */

import {{ invokeEdgeCanonHandler, edgeCanonFailureResponse, matchRoute }} from './edge-canon-runtime.js';
{node_runtime_import}{imports}{context_builder_import}
const EDGE_CANON_ENVIRONMENT_DECLARATIONS = {environment_declarations_json};

const buildContext = createContextBuilder();

Deno.serve(async (request: Request): Promise<Response> => {{
  try {{
    const url = new URL(request.url);

    // Asset-first, the way this platform answers: a path that names a file in
    // the bundle is answered from the file and no handler runs.
    const asset = await serveAsset(url.pathname);
    if (asset !== null) return asset;

{route_handlers}
    // 404 Not Found
    return new Response(null, {{ status: 404 }});
  }} catch (_) {{
    return edgeCanonFailureResponse('EC_PROVIDER_CONTEXT_INVALID');
  }}
}});

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

fn generate_context_builder_code(environment_declarations_json: &str) -> String {
    format!(
        r#"/**
 * Edge Canon context bridge for Deno Deploy.
 *
 * Deno has no request-scoped waitUntil primitive; its server execution keeps
 * already-started promises alive. The shared boundary still tracks each task,
 * isolates rejection and closes registration with the response body.
 */

import {{ createEdgeCanonContext, extractParams, materializeEdgeCanonEnvironment }} from '../edge-canon-runtime.js';

const EDGE_CANON_ENVIRONMENT_DECLARATIONS = {environment_declarations_json};

export function createContextBuilder() {{
  return function buildContext(request: Request, route: string = '/', wildcardName: string | null = null) {{
    const url = new URL(request.url);
    const standardEnv: Record<string, unknown> = materializeEdgeCanonEnvironment(
      Deno.env.toObject(),
      EDGE_CANON_ENVIRONMENT_DECLARATIONS
    );
    return createEdgeCanonContext(
      request,
      standardEnv,
      extractParams(url.pathname, route, wildcardName),
      (promise: Promise<unknown>) => {{ void promise; }}
    );
  }};
}}
"#
    )
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
