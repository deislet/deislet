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
    assert!(artifacts.router_code.contains("if (part === '*')"));
    assert!(artifacts
        .context_builder_code
        .contains("params['*'] = rest"));
    assert!(artifacts
        .context_builder_code
        .contains("params[wildcardName] = rest"));
}
