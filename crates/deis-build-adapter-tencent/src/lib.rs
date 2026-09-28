//! The Tencent EdgeOne adapter.
//!
//! One adapter per target, and the crate is named after the `--target` value
//! it answers for: `deis build --target tencent` is served by
//! `deis-build-adapter-tencent`. That correspondence is the whole naming
//! rule, and [`VENDOR`] plus the test at the bottom of this file is what
//! keeps a rename from quietly breaking it.

/// The `--target` value this adapter answers for.
///
/// `deis-build` dispatches on it, so the string here and the crate name are
/// the same fact written twice; `the_crate_is_named_after_the_vendor` fails
/// if they ever stop agreeing.
pub const VENDOR: &str = "tencent";

use deis_build_core::{
    javascript_runtime_module, javascript_string, AdapterArtifacts, ProjectConfig, StaticAsset,
};

/// Where the compiler stages this project's static files inside the EdgeOne
/// output directory: the deployment root itself.
///
/// EdgeOne Pages serves the build output directory as the site and reserves
/// `edge-functions/` for the functions, so `public/css/app.css` has to land at
/// `css/app.css` to be served at `/css/app.css`. An `assets/` subdirectory
/// would have served it at `/assets/css/app.css`, which is not the path the
/// same file answers on any other target.
pub const ASSETS_DIR: &str = "";

pub fn generate(
    routes: &[(String, String)],
    config: &ProjectConfig,
    _assets: &[StaticAsset],
) -> anyhow::Result<AdapterArtifacts> {
    generate_with_node_runtime(routes, config, _assets, false)
}

/// Generate EdgeOne's Node API Build Output for a validated Node/npm graph.
/// The application-facing router and Context stay identical; only the host
/// execution directory and its versioned route manifest differ.
pub fn generate_with_node_runtime(
    routes: &[(String, String)],
    config: &ProjectConfig,
    _assets: &[StaticAsset],
    node_runtime: bool,
) -> anyhow::Result<AdapterArtifacts> {
    refuse_unplanned_resources(config)?;
    let environment_declarations =
        deis_build_core::resolve_environment_declaration_entries(config)?;
    let environment_declarations_json = serde_json::to_string(&environment_declarations)?;
    let has_kv = config
        .services
        .as_ref()
        .and_then(|s| s.kv.as_ref())
        .map(|kv| kv.enabled)
        .unwrap_or(false);

    // 1. Generate Context Builder Code
    let context_builder_code =
        generate_context_builder_code(has_kv, &environment_declarations_json);

    // 2. Generate KV Adapter Code
    let kv_adapter_code = if has_kv {
        Some(generate_kv_adapter_code())
    } else {
        None
    };

    // 3. Generate Router Code
    let router_code =
        generate_router_code(routes, has_kv, node_runtime, &environment_declarations_json);

    // 4. Generate Config Files
    let mut config_files = Vec::new();

    // Minimal configuration based on EdgeOne Pages spec.
    //
    // The static files need no entry here: EdgeOne Pages serves whatever is in
    // the deployment root, which is where the compiler stages them (see
    // [`ASSETS_DIR`]). Naming them a second time in this file would be a list
    // that could disagree with the directory.
    let edgeone_json = serde_json::json!({
        "name": config.name,
    });

    config_files.push((
        "edgeone.json".to_string(),
        serde_json::to_string_pretty(&edgeone_json).unwrap(),
    ));
    if node_runtime {
        let route_config = node_function_route_config(routes)?;
        config_files.push((
            "cloud-functions/api-node/config.json".to_string(),
            serde_json::to_string_pretty(&route_config)?,
        ));
    }

    Ok(AdapterArtifacts {
        router_code,
        context_builder_code,
        kv_adapter_code,
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
            "EC_BINDING_TARGET_UNIMPLEMENTED: the tencent adapter requires a resolved standard binding plan"
        );
    }
    Ok(())
}

fn generate_router_code(
    routes: &[(String, String)],
    _has_kv: bool,
    node_runtime: bool,
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

        let invocation = if node_runtime {
            format!(
                "runWithEdgeCanonNodeEnv(edgeCanonProcessEnvironment(enhancedContext.env, EDGE_CANON_ENVIRONMENT_DECLARATIONS), () => handler{i}(enhancedContext))"
            )
        } else {
            format!("handler{i}(enhancedContext)")
        };
        route_handlers.push_str(&format!(
            r#"  if (matchRoute(pathname, {route_literal})) {{
    const lifecycle = buildEnhancedContext(context, {route_literal}, {wildcard_name});
    const enhancedContext = lifecycle.context;
    return await invokeEdgeCanonHandler(lifecycle, () => {invocation});
  }}
"#
        ));
    }

    let node_runtime_import = if node_runtime {
        "import { runWithEdgeCanonNodeEnv } from 'edge-canon:node-runtime';\nimport { edgeCanonProcessEnvironment } from './edge-canon-runtime.js';\n"
    } else {
        ""
    };
    format!(
        r#"/**
 * EdgeOne Pages {} Entry Point
 * Auto-generated by deis_build
 *
 * Artifact assumptions: spec.mdc research snapshot (former pages-llms.mdc lines 814-855)
 * - Edge function structure follows EdgeOne Pages standard
 * - Context object contains: request, env, params (line 819-822)
 * - Only uses Web APIs from approved list (line 899-928)
 */

import {{ invokeEdgeCanonHandler, edgeCanonFailureResponse, matchRoute }} from './edge-canon-runtime.js';
import {{ buildEnhancedContext }} from './adapters/context-builder';
{node_runtime_import}{imports}
const EDGE_CANON_ENVIRONMENT_DECLARATIONS = {environment_declarations_json};

// Main entry point as per EdgeOne spec (line 847)
export async function onRequest(context) {{
  try {{
    const pathname = context.request.url ? new URL(context.request.url).pathname : '/';

{route_handlers}
    // 404 Not Found
    return new Response(null, {{ status: 404 }});
  }} catch (_) {{
    return edgeCanonFailureResponse('EC_PROVIDER_CONTEXT_INVALID');
  }}
}}

"#,
        if node_runtime {
            "Node Function"
        } else {
            "Edge Function"
        }
    )
}

fn node_function_route_config(routes: &[(String, String)]) -> anyhow::Result<serde_json::Value> {
    let mut route_entries = routes
        .iter()
        .map(|(route, _)| {
            let (src, dest) = route_to_build_output(route)?;
            Ok(serde_json::json!({"src": src, "dest": dest}))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    // The filesystem boundary tells EdgeOne: function routes above
    // this consume matching requests; everything else falls through
    // to static file serving.  Without it, a catch-all like ^/(.*)$
    // claims every path and no static file is ever reachable.
    route_entries.push(serde_json::json!({"handle": "filesystem"}));
    Ok(serde_json::json!({"version": 3, "routes": route_entries}))
}

fn route_to_build_output(route: &str) -> anyhow::Result<(String, String)> {
    if !route.starts_with('/') {
        anyhow::bail!("EdgeOne Node Function route must start with '/': {route:?}");
    }
    if route == "/" {
        return Ok(("^/$".to_string(), "/".to_string()));
    }
    let mut source = String::from("^");
    let mut destination = String::new();
    let mut capture = 0usize;
    for segment in route.split('/') {
        if segment.is_empty() {
            continue;
        }
        source.push('/');
        destination.push('/');
        if segment.starts_with(':') {
            capture += 1;
            source.push_str("([^/]+)");
            destination.push('$');
            destination.push_str(&capture.to_string());
        } else if segment.starts_with('*') {
            capture += 1;
            source.push_str("(.*)");
            destination.push('$');
            destination.push_str(&capture.to_string());
        } else {
            for character in segment.chars() {
                if matches!(
                    character,
                    '.' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\'
                ) {
                    source.push('\\');
                }
                source.push(character);
                destination.push(character);
            }
        }
    }
    source.push('$');
    Ok((source, destination))
}

fn generate_context_builder_code(has_kv: bool, environment_declarations_json: &str) -> String {
    let kv_init = if has_kv {
        r#"
  // Initialize KV adapter if available
  let kv = undefined;
  if (typeof globalThis.EdgeKV !== 'undefined') {
    kv = createKVAdapter(globalThis.EdgeKV);
  }"#
    } else {
        r#"
  const kv = undefined;"#
    };

    let kv_import = if has_kv {
        "import { createKVAdapter } from './kv-adapter';\n"
    } else {
        ""
    };
    format!(
        r#"/**
 * Edge Canon context bridge for EdgeOne.
 *
 * Provider fields and globals remain behind this bridge. The application sees
 * exactly request, env, params and waitUntil.
 */
{kv_import}import {{ createEdgeCanonContext, extractParams, materializeEdgeCanonEnvironment }} from '../edge-canon-runtime.js';

const EDGE_CANON_ENVIRONMENT_DECLARATIONS = {environment_declarations_json};

export function buildEnhancedContext(context, route, wildcardName) {{
  const request = context.request;
  const url = new URL(request.url);
{kv_init}
  const standardEnv = materializeEdgeCanonEnvironment(
    context.env,
    EDGE_CANON_ENVIRONMENT_DECLARATIONS
  );
  if (kv !== undefined) standardEnv.KV = kv;
  const params = extractParams(url.pathname, route, wildcardName);
  return createEdgeCanonContext(
    request,
    standardEnv,
    params,
    context && typeof context.waitUntil === 'function'
      ? context.waitUntil.bind(context)
      : undefined
  );
}}
"#
    )
}

fn generate_kv_adapter_code() -> String {
    r#"/**
 * EdgeOne KV Adapter
 *
 * Provides a simplified interface to EdgeOne KV storage
 * Spec: EdgeOne KV may be available via globalThis.EdgeKV
 */

export function createKVAdapter(kvNamespace) {
  return {
    get: function(key, options) {
      options = options || {};
      return kvNamespace.get(key, options.type || 'text');
    },

    getJSON: function(key) {
      return kvNamespace.get(key, 'json');
    },

    put: function(key, value, options) {
      options = options || {};
      return kvNamespace.put(key, value, options);
    },

    putJSON: function(key, value, options) {
      options = options || {};
      return kvNamespace.put(key, JSON.stringify(value), options);
    },

    delete: function(key) {
      return kvNamespace.delete(key);
    },

    list: function(options) {
      options = options || {};
      return kvNamespace.list(options).then(function(result) {
        return {
          keys: result.keys || [],
          list_complete: result.list_complete !== false,
          cursor: result.cursor
        };
      });
    },

    exists: function(key) {
      return kvNamespace.get(key).then(function(value) {
        return value !== null;
      });
    }
  };
}
"#
    .to_string()
}

#[cfg(test)]
mod naming {
    /// A crate whose name no longer matches the vendor it serves sends the
    /// next reader looking for `deis-build-adapter-tencent` in the wrong
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
