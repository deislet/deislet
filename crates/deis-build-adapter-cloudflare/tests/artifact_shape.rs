/**
 * Cloudflare target artifact shape tests (not conformance tests)
 *
 * These tests preserve output assumptions recorded in the unpinned spec.mdc
 * research snapshot. They are not Edge Canon conformance or support evidence.
 */
use deis_build_adapter_cloudflare::{generate, ASSETS_DIR};
use deis_build_core::{ProjectConfig, StaticAsset};

/// The date the fixtures pin. wrangler will not deploy a Worker without one, so
/// this adapter refuses to build without one, and every fixture here has to
/// carry it.
const PINNED_DATE: &str = "2026-09-01";

fn test_config(name: &str, kv_enabled: bool) -> ProjectConfig {
    let mut v = serde_json::json!({
        "version": "1.0",
        "name": name,
        "runtime": "edge",
        "language": "typescript",
        "compatibilityDate": PINNED_DATE,
    });
    if kv_enabled {
        v["services"] = serde_json::json!({ "kv": { "enabled": true } });
    }
    serde_json::from_value(v).expect("valid test project config")
}

fn wrangler_toml(result: &deis_build_core::AdapterArtifacts) -> &str {
    result
        .config_files
        .iter()
        .find(|(name, _)| name == "wrangler.toml")
        .map(|(_, content)| content.as_str())
        .expect("the adapter writes wrangler.toml")
}

#[test]
fn test_entry_point_format() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-worker", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Verify modules syntax entry point (per spec.mdc)
    assert!(result.router_code.contains("export default"));
    assert!(result
        .router_code
        .contains("async fetch(request, env, ctx)"));

    // Should NOT use service worker syntax
    assert!(!result.router_code.contains("addEventListener"));
}

#[test]
fn test_handler_parameters() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-worker", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Verify all three handler parameters are present (per spec.mdc)
    assert!(result.router_code.contains("request"));
    assert!(result.router_code.contains("env"));
    assert!(result.router_code.contains("ctx"));
}

#[test]
fn test_context_includes_env_bindings() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-worker", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Verify env is passed through (not empty object)
    assert!(result.context_builder_code.contains("env,"));
    assert!(!result.context_builder_code.contains("env: {}"));
}

#[test]
fn test_context_includes_cf_property() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-worker", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Verify request.cf is exposed (per spec.mdc - Cloudflare-specific data)
    assert!(result.context_builder_code.contains("cf: request.cf"));
}

#[test]
fn test_context_execution_methods() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-worker", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Verify waitUntil is present (per spec.mdc)
    assert!(result.context_builder_code.contains("waitUntil"));

    // Verify passThroughOnException is present (per spec.mdc)
    assert!(result
        .context_builder_code
        .contains("passThroughOnException"));
}

#[test]
fn test_kv_adapter_when_enabled() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-worker", true);

    let result = generate(&routes, &config, &[]).unwrap();

    // Verify KV adapter is generated when enabled
    assert!(result.kv_adapter_code.is_some());

    let kv_code = result.kv_adapter_code.unwrap();

    // Verify KV methods match spec.mdc examples
    assert!(kv_code.contains("async get(key: string)"));
    assert!(kv_code.contains("async getJSON"));
    assert!(kv_code.contains("async put"));
    assert!(kv_code.contains("async delete"));
    assert!(kv_code.contains("async list"));
}

#[test]
fn test_wrangler_toml_format() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-worker", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Find wrangler.toml in config files
    let wrangler_toml = result
        .config_files
        .iter()
        .find(|(name, _)| name == "wrangler.toml")
        .map(|(_, content)| content);

    assert!(wrangler_toml.is_some());

    let content = wrangler_toml.unwrap();

    // Verify required fields per spec.mdc
    assert!(content.contains("name ="));
    assert!(content.contains("main ="));
    assert!(content.contains("compatibility_date ="));
}

#[test]
fn test_error_handling_present() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-worker", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Verify error handling is present (per best practices)
    assert!(result.router_code.contains("try"));
    assert!(result.router_code.contains("catch"));
    assert!(result.support_files[0].1.contains("console.error"));
}

#[test]
fn test_research_snapshot_reference_in_comments() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-worker", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Preserve a pointer to the artifact research snapshot.
    assert!(result.router_code.contains("spec.mdc"));
    assert!(result.context_builder_code.contains("spec.mdc"));
}

#[test]
fn the_wrangler_toml_carries_the_date_the_project_pinned() {
    // It used to say `compatibility_date = "2024-09-23"` whatever the project
    // pinned, so a Worker ran under semantics its author had never tested
    // against and nothing anywhere said so.
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let result = generate(&routes, &test_config("dated", false), &[]).unwrap();
    let content = wrangler_toml(&result);

    assert!(
        content.contains(&format!("compatibility_date = \"{PINNED_DATE}\"")),
        "{content}"
    );
    assert!(
        !content.contains("2024-09-23"),
        "the fixed date must be gone: {content}"
    );
}

#[test]
fn nothing_the_author_did_not_write_is_in_the_wrangler_toml() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let result = generate(&routes, &test_config("plain", false), &[]).unwrap();
    let content = wrangler_toml(&result);

    // `nodejs_compat` turns on Node built-ins in the Worker. The bundler
    // targets the browser platform and emits no Node imports, so it was never
    // needed and was never asked for.
    assert!(
        !content.contains("compatibility_flags"),
        "no flags were declared, so none may be written: {content}"
    );
    // `route = "<name>.workers.dev"` under invented `[env.staging]` and
    // `[env.production]` sections: `route` binds a Worker to a hostname on a
    // zone the account controls, and workers.dev is not one.
    assert!(
        !content.contains("[env."),
        "the author declared no environments: {content}"
    );
    assert!(!content.contains("workers.dev"), "{content}");
}

#[test]
fn a_project_with_no_compatibility_date_is_refused_and_told_what_to_add() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let mut config = test_config("undated", false);
    config.compatibility_date = None;

    let error = generate(&routes, &config, &[])
        .expect_err("wrangler cannot deploy a Worker without a compatibility date")
        .to_string();
    assert!(error.contains("compatibilityDate"), "{error}");
    assert!(error.contains("cloudflare"), "{error}");
}

#[test]
fn a_project_with_compatibility_flags_is_refused_rather_than_half_translated() {
    // The names are this platform's (`deis_bundle::compatibility::FLAGS`), not
    // Cloudflare's. Carrying them over builds a wrangler.toml wrangler refuses;
    // dropping them builds a Worker that behaves differently from the one the
    // author pinned. Neither is something to do quietly.
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let mut config = test_config("flagged", false);
    config.compatibility_flags = vec!["single_set_cookie_header".to_string()];

    let error = generate(&routes, &config, &[])
        .expect_err("a flag this target cannot honour is not dropped in silence")
        .to_string();
    assert!(error.contains("single_set_cookie_header"), "{error}");
}

#[test]
fn the_static_files_are_declared_so_wrangler_serves_them() {
    // Every Cloudflare build used to produce a directory with the project's
    // `public/` nowhere in it, and said nothing about it.
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let assets = vec![StaticAsset {
        path: "css/app.css".to_string(),
        content_type: "text/css; charset=utf-8".to_string(),
    }];

    let with_assets = generate(&routes, &test_config("site", false), &assets).unwrap();
    let content = wrangler_toml(&with_assets);
    assert!(content.contains("[assets]"), "{content}");
    assert!(
        content.contains(&format!("directory = \"./{ASSETS_DIR}\"")),
        "the declaration has to name the directory the compiler stages into: {content}"
    );

    // And a project with no static files gets no empty declaration, which
    // wrangler would read as a directory it could not find.
    let without = generate(&routes, &test_config("site", false), &[]).unwrap();
    assert!(!wrangler_toml(&without).contains("[assets]"));
}

#[test]
fn a_validated_node_graph_enables_the_provider_runtime_explicitly() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let result = deis_build_adapter_cloudflare::generate_with_node_compat(
        &routes,
        &test_config("node-app", false),
        &[],
        true,
    )
    .expect("generate Node-compatible Worker");
    assert!(wrangler_toml(&result).contains("compatibility_flags = [\"nodejs_compat\"]"));
}
