use deis_build_adapter_tencent::generate;
use deis_build_core::ProjectConfig;

fn config() -> ProjectConfig {
    serde_json::from_str(
        r#"{"name":"routes","version":"1.0.0","runtime":"standard-v1","language":"typescript"}"#,
    )
    .unwrap()
}

#[test]
fn generated_router_preserves_catch_all_names_and_encodes_js_literals() {
    let artifacts = generate(
        &[
            ("/*".to_string(), "../src/[[tail]].ts".to_string()),
            ("/quo'te".to_string(), "../src/quo'te.ts".to_string()),
        ],
        &config(),
        &[],
    )
    .unwrap();

    assert!(artifacts
        .router_code
        .contains("buildEnhancedContext(context, \"/*\", \"tail\")"));
    assert!(artifacts
        .router_code
        .contains("import handler1 from \"../src/quo'te.ts\";"));
    assert!(artifacts
        .router_code
        .contains("matchRoute(pathname, \"/quo'te\")"));
    let runtime = &artifacts.support_files[0].1;
    assert!(runtime.contains("if (part === '*')"));
    assert!(runtime.contains("params['*'] = rest"));
    assert!(runtime.contains("params[wildcardName] = rest"));
}

#[test]
fn generated_handler_boundary_awaits_and_returns_the_fixed_non_leaking_error() {
    let artifacts = generate(
        &[("/".to_string(), "../src/index.ts".to_string())],
        &config(),
        &[],
    )
    .unwrap();
    let runtime = &artifacts.support_files[0].1;

    assert!(artifacts
        .router_code
        .contains("export async function onRequest"));
    assert!(artifacts
        .router_code
        .contains("return await invokeEdgeCanonHandler"));
    assert!(runtime.contains("const result = await invoke()"));
    assert!(runtime.contains("EC_HANDLER_THROWN"));
    assert!(runtime.contains("EC_HANDLER_RESULT_INVALID"));
    assert!(runtime.contains("'content-type': 'text/plain; charset=utf-8'"));
    assert!(runtime.contains("'cache-control': 'no-store'"));
    assert!(runtime.contains("Internal Server Error\\n"));
    assert!(!runtime.contains("error.message"));
}
