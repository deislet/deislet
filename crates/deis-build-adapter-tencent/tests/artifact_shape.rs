/**
 * EdgeOne target artifact shape tests (not conformance tests)
 *
 * These tests preserve output assumptions recorded in the unpinned spec.mdc
 * research snapshot. They are not Edge Canon conformance or support evidence.
 */
use deis_build_adapter_tencent::generate;
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

    let result = generate(&routes, &config, &[]).unwrap();

    // Verify onRequest entry point (per spec.mdc line 847)
    assert!(result
        .router_code
        .contains("export async function onRequest"));
    assert!(result.router_code.contains("onRequest(context)"));

    // Should NOT use export default { fetch() } format
    assert!(!result.router_code.contains("export default"));
}

#[test]
fn test_context_parameter() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Verify context parameter usage (per spec.mdc line 819-822)
    assert!(result.router_code.contains("context"));
    assert!(result.router_code.contains("context.request"));
}

#[test]
fn test_no_modern_syntax() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Verify NO Object.fromEntries (not in spec.mdc Web APIs list)
    // Match the call form so the explanatory comment in the generated
    // code does not trip the assertion.
    assert!(!result.context_builder_code.contains("Object.fromEntries("));

    // Verify NO spread operators in critical paths
    // (Cookies getAll() may have spread, but main context should not)
}

#[test]
fn test_emitted_web_apis_match_research_snapshot() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Verify only approved Web APIs are used (per spec.mdc line 899-928)

    // ✅ Allowed: URL
    assert!(result.context_builder_code.contains("new URL"));

    // ✅ Allowed: console.*
    assert!(result.support_files[0].1.contains("console.error"));

    // Verify forEach usage (compatible alternative to Object.fromEntries)
    assert!(result
        .context_builder_code
        .contains("forEach(function(value, key)"));
}

#[test]
fn test_context_structure() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Verify EdgeOne context structure (per spec.mdc line 819-822)
    // The builder aliases `const request = context.request` and then
    // exposes it as `request: request`; assert on both halves.
    assert!(result
        .context_builder_code
        .contains("const request = context.request"));
    assert!(result.context_builder_code.contains("request: request"));
    assert!(result.context_builder_code.contains("env: context.env"));
    assert!(result.context_builder_code.contains("params:"));
}

#[test]
fn test_cookie_parsing_shape() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Verify cookie parsing uses indexOf/slice (not destructuring)
    assert!(result.context_builder_code.contains("indexOf('=')"));
    assert!(result.context_builder_code.contains("slice(0, eqIndex)"));

    // Verify NO rest parameters in cookie parsing
    assert!(!result.context_builder_code.contains("...rest"));
}

#[test]
fn test_error_handling() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config, &[]).unwrap();
    let runtime = &result.support_files[0].1;

    // The Edge Canon wrapper catches both synchronous throws and rejected
    // promises, validates the returned value and emits the fixed non-leaking
    // response instead of a provider-authored error page.
    assert!(result.router_code.contains("try"));
    assert!(result.router_code.contains("catch (_)"));
    assert!(runtime.contains("console.error"));
    assert!(runtime.contains("EC_HANDLER_THROWN"));
    assert!(runtime.contains("EC_HANDLER_RESULT_INVALID"));
    assert!(runtime.contains("Internal Server Error\\n"));
    assert!(!runtime.contains("error.message"));
}

#[test]
fn test_edgeone_json_format() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Find edgeone.json in config files
    let edgeone_json = result
        .config_files
        .iter()
        .find(|(name, _)| name == "edgeone.json")
        .map(|(_, content)| content);

    assert!(edgeone_json.is_some());

    let content = edgeone_json.unwrap();

    // Verify minimal configuration (per spec.mdc)
    assert!(content.contains("\"name\""));
}

#[test]
fn test_research_snapshot_reference_in_comments() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Preserve a pointer to the artifact research snapshot.
    assert!(
        result.router_code.contains("pages-llms.mdc") || result.router_code.contains("spec.mdc")
    );
    assert!(
        result.context_builder_code.contains("pages-llms.mdc")
            || result.context_builder_code.contains("spec.mdc")
    );
}

#[test]
fn test_traditional_loops_only() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Verify traditional for loops are used (maximum compatibility)
    assert!(result.context_builder_code.contains("for (let i"));
    assert!(result.context_builder_code.contains("for (const key in"));
}

#[test]
fn test_kv_adapter_shape() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", true);

    let result = generate(&routes, &config, &[]).unwrap();

    assert!(result.kv_adapter_code.is_some());

    let kv_code = result.kv_adapter_code.unwrap();

    // Verify KV adapter uses function keyword (not arrow functions)
    assert!(kv_code.contains("get: function"));
    assert!(kv_code.contains("put: function"));

    // Verify .then() chains (not async/await in object methods)
    assert!(kv_code.contains(".then(function"));
}

#[test]
fn validated_node_graph_uses_build_output_v3_node_api_shape() {
    let routes = vec![
        ("/".to_string(), "./src/index.ts".to_string()),
        ("/users/:id".to_string(), "./src/users/[id].ts".to_string()),
    ];
    let result = deis_build_adapter_tencent::generate_with_node_runtime(
        &routes,
        &test_config("node-app", false),
        &[],
        true,
    )
    .expect("Node Function output");
    assert!(result.router_code.contains("Node Function Entry Point"));
    let config = result
        .config_files
        .iter()
        .find(|(name, _)| name == "cloud-functions/api-node/config.json")
        .map(|(_, content)| content)
        .expect("Node API route config");
    let config: serde_json::Value = serde_json::from_str(config).expect("valid config JSON");
    assert_eq!(config["version"], 3);
    assert_eq!(config["routes"][0]["src"], "^/$");
    assert_eq!(config["routes"][1]["src"], "^/users/([^/]+)$");
    assert_eq!(config["routes"][1]["dest"], "/users/$1");
}
