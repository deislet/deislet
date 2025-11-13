use denictl_compiler::Transformer;
use denictl_core::ProjectConfig;

#[test]
fn test_filename_to_route() {
    let transformer = Transformer::new();
    let config = create_test_config();

    // Test basic transformation
    let code = r#"
export default async function handler(context) {
    return new Response('Hello');
}
"#;

    let result = transformer.transform(code, "./functions/index.ts", "cloudflare", &config);
    assert!(result.is_ok());

    let transformed = result.unwrap();
    assert!(transformed.contains("Cloudflare Workers Handler"));
    assert!(transformed.contains("Route: /"));
}

#[test]
fn test_cloudflare_transformation() {
    let transformer = Transformer::new();
    let config = create_test_config();

    let code = r#"
export default async function handler(context) {
    const kv = context.services.kv;
    return new Response('Hello from KV');
}
"#;

    let result = transformer.transform(code, "./functions/api/test.ts", "cloudflare", &config);
    assert!(result.is_ok());

    let transformed = result.unwrap();
    assert!(transformed.contains("import { createContextBuilder }"));
    assert!(transformed.contains("Route: /api/test"));
}

fn create_test_config() -> ProjectConfig {
    ProjectConfig {
        version: "1.0".to_string(),
        name: "test-app".to_string(),
        runtime: "standard-v1".to_string(),
        language: "typescript".to_string(),
        function_root: "./functions".to_string(),
        routing: None,
        environment: None,
        services: None,
        vendors: None,
        build: None,
        limits: None,
    }
}
