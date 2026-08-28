/**
 * Deno Deploy Adapter Spec Compliance Tests
 *
 * These tests verify that the generated code complies with
 * deis-build-adapter-dc/spec.mdc requirements.
 */
use deis_build_adapter_dc::generate;
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
    let config = test_config("test-app", false);

    let result = generate(&routes, &config).unwrap();

    // Verify Deno.serve entry point (per spec.mdc)
    assert!(result.router_code.contains("Deno.serve"));
    assert!(result
        .router_code
        .contains("async (request: Request): Promise<Response>"));
}

#[test]
fn test_deno_kv_usage() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", true);

    let result = generate(&routes, &config).unwrap();

    // Verify Deno.openKv() usage (per spec.mdc)
    assert!(result.router_code.contains("Deno.openKv()"));
}

#[test]
fn test_env_access() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config).unwrap();

    // Verify Deno.env.toObject() usage (per spec.mdc)
    assert!(result.context_builder_code.contains("Deno.env.toObject()"));
}

#[test]
fn test_kv_adapter_when_enabled() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", true);

    let result = generate(&routes, &config).unwrap();

    // Verify KV adapter is generated
    assert!(result.kv_adapter_code.is_some());

    let kv_code = result.kv_adapter_code.unwrap();

    // Verify KV methods match Deno KV API (per spec.mdc)
    assert!(kv_code.contains("async get(key: string)"));
    assert!(kv_code.contains("async getJSON"));
    assert!(kv_code.contains("async put"));
    assert!(kv_code.contains("async delete"));
    assert!(kv_code.contains("kv.get([key])")); // Deno KV uses array keys
    assert!(kv_code.contains("kv.set([key]"));
}

#[test]
fn test_deno_json_format() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config).unwrap();

    // Find deno.json in config files
    let deno_json = result
        .config_files
        .iter()
        .find(|(name, _)| name == "deno.json")
        .map(|(_, content)| content);

    assert!(deno_json.is_some());

    let content = deno_json.unwrap();

    // Verify required fields per spec.mdc
    assert!(content.contains("\"tasks\""));
    assert!(content.contains("\"imports\""));
}

#[test]
fn test_error_handling_present() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config).unwrap();

    // Verify error handling is present
    assert!(result.router_code.contains("try"));
    assert!(result.router_code.contains("catch"));
    assert!(result.router_code.contains("console.error"));
}

#[test]
fn test_spec_reference_in_comments() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config).unwrap();

    // Verify spec compliance is documented
    assert!(result.router_code.contains("spec.mdc"));
    assert!(result.context_builder_code.contains("spec.mdc"));
}

#[test]
fn test_cache_api_wrapper() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config).unwrap();

    // Verify Cache API wrapper (per spec.mdc - Web Platform APIs)
    assert!(result.context_builder_code.contains("cache:"));
    assert!(result.context_builder_code.contains("caches.open"));
}

#[test]
fn test_wait_until_polyfill() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config).unwrap();

    // Verify waitUntil polyfill (Deno Deploy doesn't have native waitUntil)
    assert!(result.context_builder_code.contains("waitUntil:"));
    assert!(result.context_builder_code.contains("promise.catch"));
}
