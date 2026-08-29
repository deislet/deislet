/**
 * Cloudflare Workers Adapter Spec Compliance Tests
 *
 * These tests verify that the generated code complies with
 * deis-build-adapter-cloudflare/spec.mdc requirements.
 */
use deis_build_adapter_cloudflare::generate;
use deis_build_core::ProjectConfig;

fn test_config(name: &str, kv_enabled: bool) -> ProjectConfig {
    let mut v = serde_json::json!({
        "version": "1.0",
        "name": name,
        "runtime": "edge",
        "language": "typescript",
    });
    if kv_enabled {
        v["services"] = serde_json::json!({ "kv": { "enabled": true } });
    }
    serde_json::from_value(v).expect("valid test project config")
}

#[test]
fn test_entry_point_format() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-worker", false);

    let result = generate(&routes, &config).unwrap();

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

    let result = generate(&routes, &config).unwrap();

    // Verify all three handler parameters are present (per spec.mdc)
    assert!(result.router_code.contains("request"));
    assert!(result.router_code.contains("env"));
    assert!(result.router_code.contains("ctx"));
}

#[test]
fn test_context_includes_env_bindings() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-worker", false);

    let result = generate(&routes, &config).unwrap();

    // Verify env is passed through (not empty object)
    assert!(result.context_builder_code.contains("env,"));
    assert!(!result.context_builder_code.contains("env: {}"));
}

#[test]
fn test_context_includes_cf_property() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-worker", false);

    let result = generate(&routes, &config).unwrap();

    // Verify request.cf is exposed (per spec.mdc - Cloudflare-specific data)
    assert!(result.context_builder_code.contains("cf: request.cf"));
}

#[test]
fn test_context_execution_methods() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-worker", false);

    let result = generate(&routes, &config).unwrap();

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

    let result = generate(&routes, &config).unwrap();

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

    let result = generate(&routes, &config).unwrap();

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

    let result = generate(&routes, &config).unwrap();

    // Verify error handling is present (per best practices)
    assert!(result.router_code.contains("try"));
    assert!(result.router_code.contains("catch"));
    assert!(result.router_code.contains("console.error"));
}

#[test]
fn test_spec_reference_in_comments() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-worker", false);

    let result = generate(&routes, &config).unwrap();

    // Verify spec compliance is documented
    assert!(result.router_code.contains("spec.mdc"));
    assert!(result.context_builder_code.contains("spec.mdc"));
}
