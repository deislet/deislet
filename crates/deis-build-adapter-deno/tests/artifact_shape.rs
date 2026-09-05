/**
 * Deno target artifact shape tests (not conformance tests)
 *
 * These tests preserve output assumptions recorded in the unpinned spec.mdc
 * research snapshot. They are not Edge Canon conformance or support evidence.
 */
use deis_build_adapter_deno::{generate, ASSETS_DIR};
use deis_build_core::{ProjectConfig, StaticAsset};

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

    // Verify Deno.serve entry point (per spec.mdc)
    assert!(result.router_code.contains("Deno.serve"));
    assert!(result
        .router_code
        .contains("async (request: Request): Promise<Response>"));
}

#[test]
fn unresolved_kv_binding_is_rejected() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", true);

    let error = generate(&routes, &config, &[]).expect_err("unresolved bindings must fail closed");
    assert!(error
        .to_string()
        .contains("EC_BINDING_TARGET_UNIMPLEMENTED"));
}

#[test]
fn test_env_access() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Verify Deno.env.toObject() usage (per spec.mdc)
    assert!(result.context_builder_code.contains("Deno.env.toObject()"));
}

#[test]
fn callable_resource_without_a_binding_plan_is_rejected() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", true);

    let error = generate(&routes, &config, &[]).expect_err("unresolved bindings must fail closed");
    assert!(error
        .to_string()
        .contains("EC_BINDING_TARGET_UNIMPLEMENTED"));
}

#[test]
fn test_deno_json_format() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config, &[]).unwrap();

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
    let parsed: serde_json::Value = serde_json::from_str(content).expect("deno.json parses");
    assert_eq!(parsed["imports"], serde_json::json!({}));
}

#[test]
fn test_error_handling_present() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Verify error handling is present
    assert!(result.router_code.contains("try"));
    assert!(result.router_code.contains("catch"));
    assert!(result.support_files[0].1.contains("console.error"));
}

#[test]
fn test_research_snapshot_reference_in_comments() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config, &[]).unwrap();

    // Preserve a pointer to the artifact research snapshot.
    assert!(result.router_code.contains("spec.mdc"));
    assert!(result.router_code.contains("spec.mdc"));
}

#[test]
fn context_does_not_invent_a_provider_specific_cache_field() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config, &[]).unwrap();

    assert!(!result.context_builder_code.contains("cache:"));
    assert!(!result.context_builder_code.contains("services:"));
    assert!(result
        .context_builder_code
        .contains("createEdgeCanonContext"));
}

#[test]
fn standard_wait_until_uses_the_shared_lifecycle_boundary() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-app", false);

    let result = generate(&routes, &config, &[]).unwrap();

    assert!(result.context_builder_code.contains("void promise"));
    let runtime = &result.support_files[0].1;
    assert!(runtime.contains("EC_WAIT_UNTIL_CLOSED"));
    assert!(runtime.contains("EC_WAIT_UNTIL_PROMISE_REQUIRED"));
    assert!(runtime.contains("EC_BACKGROUND_REJECTED"));
}

#[test]
fn the_router_serves_the_static_files_the_build_carried() {
    // Deno Deploy has no static layer: a file is served only if the program
    // reads it. Every Deno build used to leave the project's `public/` both
    // uncopied and unserved, and said nothing about either.
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let assets = vec![
        StaticAsset {
            path: "index.html".to_string(),
            content_type: "text/html; charset=utf-8".to_string(),
        },
        StaticAsset {
            path: "css/app.css".to_string(),
            content_type: "text/css; charset=utf-8".to_string(),
        },
    ];

    let result = generate(&routes, &test_config("site", false), &assets).unwrap();
    let router = &result.router_code;

    // The media type comes from the build, not from a second table in
    // JavaScript that could disagree with the manifest's.
    assert!(
        router.contains(r#""css/app.css": "text/css; charset=utf-8""#),
        "{router}"
    );
    assert!(
        router.contains(&format!("'../{ASSETS_DIR}/' + key")),
        "the router has to read from the directory the compiler stages into: {router}"
    );

    // Asset-first: the file is tried before the route table, which is the
    // order deis-runtime answers in.
    let asset_at = router
        .find("serveAsset(url.pathname)")
        .expect("the router serves assets");
    let route_at = router
        .find("matchRoute(url.pathname")
        .expect("the router routes");
    assert!(
        asset_at < route_at,
        "a file has to be tried before a handler"
    );
}

#[test]
fn a_project_with_no_static_files_gets_an_empty_table() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let result = generate(&routes, &test_config("bare", false), &[]).unwrap();
    assert!(
        result
            .router_code
            .contains("const ASSETS: Record<string, string> = {}"),
        "{}",
        result.router_code
    );
}
