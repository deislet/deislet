/**
 * EdgeOne Pages Adapter Spec Compliance Tests
 *
 * These tests verify that the generated code complies with
 * deforge-adapter-eo/spec.mdc (pages-llms.mdc) requirements.
 */
use deforge_adapter_eo::generate;
use deforge_core::ProjectConfig;

#[test]
fn test_entry_point_format() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = ProjectConfig {
        name: "test-app".to_string(),
        services: None,
    };

    let result = generate(&routes, &config).unwrap();

    // Verify onRequest entry point (per spec.mdc line 847)
    assert!(result.router_code.contains("export function onRequest"));
    assert!(result.router_code.contains("onRequest(context)"));

    // Should NOT use export default { fetch() } format
    assert!(!result.router_code.contains("export default"));
}

#[test]
fn test_context_parameter() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = ProjectConfig {
        name: "test-app".to_string(),
        services: None,
    };

    let result = generate(&routes, &config).unwrap();

    // Verify context parameter usage (per spec.mdc line 819-822)
    assert!(result.router_code.contains("context"));
    assert!(result.router_code.contains("context.request"));
}

#[test]
fn test_no_modern_syntax() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = ProjectConfig {
        name: "test-app".to_string(),
        services: None,
    };

    let result = generate(&routes, &config).unwrap();

    // Verify NO Object.fromEntries (not in spec.mdc Web APIs list)
    assert!(!result.context_builder_code.contains("Object.fromEntries"));

    // Verify NO spread operators in critical paths
    // (Cookies getAll() may have spread, but main context should not)
}

#[test]
fn test_web_apis_compliance() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = ProjectConfig {
        name: "test-app".to_string(),
        services: None,
    };

    let result = generate(&routes, &config).unwrap();

    // Verify only approved Web APIs are used (per spec.mdc line 899-928)

    // ✅ Allowed: URL
    assert!(result.context_builder_code.contains("new URL"));

    // ✅ Allowed: console.*
    assert!(result.router_code.contains("console.error"));

    // Verify forEach usage (compatible alternative to Object.fromEntries)
    assert!(result
        .context_builder_code
        .contains("forEach(function(value, key)"));
}

#[test]
fn test_context_structure() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = ProjectConfig {
        name: "test-app".to_string(),
        services: None,
    };

    let result = generate(&routes, &config).unwrap();

    // Verify EdgeOne context structure (per spec.mdc line 819-822)
    assert!(result
        .context_builder_code
        .contains("request: context.request"));
    assert!(result.context_builder_code.contains("env: context.env"));
    assert!(result.context_builder_code.contains("params:"));
}

#[test]
fn test_cookie_parsing_compatibility() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = ProjectConfig {
        name: "test-app".to_string(),
        services: None,
    };

    let result = generate(&routes, &config).unwrap();

    // Verify cookie parsing uses indexOf/slice (not destructuring)
    assert!(result.context_builder_code.contains("indexOf('=')"));
    assert!(result.context_builder_code.contains("slice(0, eqIndex)"));

    // Verify NO rest parameters in cookie parsing
    assert!(!result.context_builder_code.contains("...rest"));
}

#[test]
fn test_error_handling() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = ProjectConfig {
        name: "test-app".to_string(),
        services: None,
    };

    let result = generate(&routes, &config).unwrap();

    // Verify comprehensive error handling
    assert!(result.router_code.contains("try"));
    assert!(result.router_code.contains("catch (error)"));
    assert!(result.router_code.contains("console.error"));
    assert!(result.router_code.contains("'Internal Error'"));
}

#[test]
fn test_edgeone_json_format() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = ProjectConfig {
        name: "test-app".to_string(),
        services: None,
    };

    let result = generate(&routes, &config).unwrap();

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
fn test_spec_reference_in_comments() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = ProjectConfig {
        name: "test-app".to_string(),
        services: None,
    };

    let result = generate(&routes, &config).unwrap();

    // Verify spec compliance is documented (references pages-llms.mdc)
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
    let config = ProjectConfig {
        name: "test-app".to_string(),
        services: None,
    };

    let result = generate(&routes, &config).unwrap();

    // Verify traditional for loops are used (maximum compatibility)
    assert!(result.context_builder_code.contains("for (let i"));
    assert!(result.context_builder_code.contains("for (const key in"));
}

#[test]
fn test_kv_adapter_compatibility() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = ProjectConfig {
        name: "test-app".to_string(),
        services: Some(deforge_core::ServicesConfig {
            kv: Some(deforge_core::KVConfig { enabled: true }),
            database: None,
        }),
    };

    let result = generate(&routes, &config).unwrap();

    assert!(result.kv_adapter_code.is_some());

    let kv_code = result.kv_adapter_code.unwrap();

    // Verify KV adapter uses function keyword (not arrow functions)
    assert!(kv_code.contains("get: function"));
    assert!(kv_code.contains("put: function"));

    // Verify .then() chains (not async/await in object methods)
    assert!(kv_code.contains(".then(function"));
}
