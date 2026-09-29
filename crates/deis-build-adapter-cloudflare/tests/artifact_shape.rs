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
fn standard_context_does_not_expose_cloudflare_objects() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-worker", false);

    let result = generate(&routes, &config, &[]).unwrap();

    assert!(!result.context_builder_code.contains("request.cf"));
    assert!(!result
        .context_builder_code
        .contains("passThroughOnException"));
    assert!(result
        .context_builder_code
        .contains("createEdgeCanonContext"));
}

#[test]
fn standard_wait_until_is_backed_by_the_native_execution_context() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("test-worker", false);

    let result = generate(&routes, &config, &[]).unwrap();

    assert!(result
        .context_builder_code
        .contains("ctx.waitUntil.bind(ctx)"));
    let runtime = &result.support_files[0].1;
    assert!(runtime.contains("EC_WAIT_UNTIL_CLOSED"));
    assert!(runtime.contains("EC_WAIT_UNTIL_PROMISE_REQUIRED"));
    assert!(runtime.contains("EC_BACKGROUND_REJECTED"));
}

#[test]
fn an_unnamed_service_is_refused_with_the_named_spelling_suggested() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = test_config("unnamed", true);

    let error =
        generate(&routes, &config, &[]).expect_err("the unnamed namespace has no name to bind");
    let error = error.to_string();
    assert!(error.contains("EC_BINDING_TARGET_UNIMPLEMENTED"), "{error}");
    assert!(error.contains("named bindings"), "{error}");
}

#[test]
fn the_wrangler_toml_carries_placeholders_never_empty_or_invented_identifiers() {
    // Resource identifiers are resolved by the provider driver from the
    // deployment state, never invented by the build. The old templates emitted
    // `id = ""` for the author to fill in by hand, which would deploy a Worker
    // that references nothing. The build now emits the binding plan's
    // unresolved placeholder - machine-findable, never empty, and refused
    // downstream if a prepare step did not replace it.
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let config = planned_config("planned", r#"{"HOT": {"kind": "kv"}}"#);

    let artifacts = generate(&routes, &config, &[]).expect("a planned kv binding maps");
    let content = wrangler_toml(&artifacts);

    assert!(content.contains("[[kv_namespaces]]"), "{content}");
    assert!(content.contains("binding = \"HOT\""), "{content}");
    assert!(content.contains("deis:unresolved/kv/HOT"), "{content}");
    for line in content.lines() {
        if line.starts_with("id =")
            || line.starts_with("database_id =")
            || line.starts_with("bucket_name =")
        {
            let value = line.split('=').nth(1).unwrap().trim().trim_matches('"');
            assert!(
                !value.is_empty(),
                "an empty identifier must never be emitted: {content}"
            );
            assert!(
                value.starts_with("deis:unresolved/"),
                "every identifier the build emits is the plan's placeholder: {line}"
            );
        }
    }
}

/// A project with one binding of every mapped kind, a consumer, crons and a
/// config declaration: the whole wrangler.toml surface in one snapshot.
fn planned_config(name: &str, bindings: &str) -> ProjectConfig {
    let json = serde_json::json!({
        "version": "1.0",
        "name": name,
        "runtime": "edge",
        "language": "typescript",
        "compatibilityDate": PINNED_DATE,
        "bindings": serde_json::from_str::<serde_json::Value>(bindings).expect("bindings"),
        "cron": ["*/5 * * * *"],
        "queues": [{ "name": "jobs", "batchSize": 10, "maxConcurrency": 2, "maxRetries": 7 }],
        "environment": {
            "declarations": [
                {"name": "PUBLIC_NAME", "kind": "config", "valueType": "string", "required": true},
                {"name": "TOKEN", "kind": "secret", "valueType": "string", "required": true}
            ]
        }
    });
    serde_json::from_value(json).expect("valid test project config")
}

#[test]
fn the_binding_plan_snapshot_covers_every_provider_section() {
    let routes = vec![
        ("/".to_string(), "./src/index.ts".to_string()),
        ("/_queue".to_string(), "../src/_queue.ts".to_string()),
        (
            "/_scheduled".to_string(),
            "../src/_scheduled.ts".to_string(),
        ),
    ];
    let config = planned_config(
        "full-plan",
        r#"{
            "HOT": {"kind": "kv"},
            "DB": {"kind": "database"},
            "FILES": {"kind": "blob"},
            "ORDERS": {"kind": "queue"}
        }"#,
    );

    let artifacts = generate(&routes, &config, &[]).expect("every kind maps");
    let content = wrangler_toml(&artifacts);

    // The sections, in the order the plan states them. Every identifier is
    // the placeholder the driver resolves at prepare time.
    let expected = "name = \"full-plan\"\n\
         main = \"index.js\"\n\
         compatibility_date = \"2026-09-01\"\n\
         \n[triggers]\n\
         crons = [\"*/5 * * * *\"]\n\
         \n[[d1_databases]]\n\
         binding = \"DB\"\n\
         database_name = \"DB\"\n\
         # Resolved by the deis provider driver at prepare time from the deployment state; this placeholder is replaced, never deployed.\n\
         database_id = \"deis:unresolved/database/DB\"\n\
         \n[[r2_buckets]]\n\
         binding = \"FILES\"\n\
         # The bucket name is the driver's provisioning decision; the placeholder is replaced at prepare time from the deployment state.\n\
         bucket_name = \"deis:unresolved/blob/FILES\"\n\
         \n[[kv_namespaces]]\n\
         binding = \"HOT\"\n\
         # Resolved by the deis provider driver at prepare time from the deployment state; this placeholder is replaced, never deployed.\n\
         id = \"deis:unresolved/kv/HOT\"\n\
         \n[[queues.producers]]\n\
         binding = \"ORDERS\"\n\
         # The queue is named, not identified: the driver provisions a queue with this logical name at prepare time.\n\
         queue = \"deis:unresolved/queue/ORDERS\"\n\
         \n[[queues.consumers]]\n\
         queue = \"jobs\"\n\
         max_retries = 7\n\
         max_batch_size = 10\n\
         max_concurrency = 2\n\
         \n[vars]\n\
         PUBLIC_NAME = \"deis:unresolved/config/PUBLIC_NAME\"\n";
    assert_eq!(content, expected);

    // A secret is never written into the toml, not even as a placeholder:
    // it travels to the provider through its secret command.
    assert!(!content.contains("TOKEN"), "{content}");

    // The machine-readable half the driver reads first.
    let plan_file = artifacts
        .config_files
        .iter()
        .find(|(name, _)| name == deis_build_adapter_cloudflare::BINDING_PLAN_FILE)
        .map(|(_, content)| content)
        .expect("the adapter writes binding-plan.json");
    let plan: serde_json::Value = serde_json::from_str(plan_file).expect("valid JSON");
    assert_eq!(plan["format"], "deislet.binding-plan/v1");
    assert_eq!(plan["target"], "cloudflare");
    let names: Vec<&str> = plan["bindings"]
        .as_array()
        .expect("bindings array")
        .iter()
        .map(|binding| binding["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["DB", "FILES", "HOT", "ORDERS"]);
    assert_eq!(plan["queue_consumers"][0]["max_retries"], 7);
    assert_eq!(plan["crons"][0], "*/5 * * * *");
}

#[test]
fn an_absent_max_retries_is_declared_at_the_standard_default() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let json = serde_json::json!({
        "version": "1.0",
        "name": "defaulted",
        "runtime": "edge",
        "language": "typescript",
        "compatibilityDate": PINNED_DATE,
        "queues": [{ "name": "jobs" }]
    });
    let config: ProjectConfig = serde_json::from_value(json).expect("config");

    let artifacts = generate(&routes, &config, &[]).expect("consumer plans");
    let content = wrangler_toml(&artifacts);
    assert!(content.contains("[[queues.consumers]]"), "{content}");
    assert!(content.contains("max_retries = 3"), "{content}");
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
    assert!(result.router_code.contains("spec.mdc"));
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

#[test]
fn a_root_queue_handler_is_exported_not_published_as_a_route() {
    // `_queue.ts` is the platform-called consumer. It must never appear in the
    // fetch table - that would publish /_queue as a public HTTP endpoint - and
    // it must be exported, or Workers would have nothing to deliver batches
    // to. The export bridges the native `(batch, env, ctx)` onto the canon's
    // `(batch, env, context)`.
    let routes = vec![
        ("/".to_string(), "./src/index.ts".to_string()),
        ("/_queue".to_string(), "../src/_queue.ts".to_string()),
    ];
    let result = generate(&routes, &test_config("consumer", false), &[]).unwrap();
    let router = &result.router_code;

    assert!(
        router.contains("export async function queue(batch, env, ctx)"),
        "{router}"
    );
    assert!(
        router.contains("queueHandler(batch, context.env, context)"),
        "{router}"
    );
    assert!(
        !router.contains("\"/_queue\""),
        "the queue file is not a route: {router}"
    );
    assert!(
        !router.contains("export async function scheduled"),
        "{router}"
    );
}

#[test]
fn a_root_scheduled_handler_is_exported_the_same_way() {
    let routes = vec![
        ("/".to_string(), "./src/index.ts".to_string()),
        (
            "/_scheduled".to_string(),
            "../src/_scheduled.ts".to_string(),
        ),
    ];
    let result = generate(&routes, &test_config("cronic", false), &[]).unwrap();
    let router = &result.router_code;

    assert!(
        router.contains("export async function scheduled(event, env, ctx)"),
        "{router}"
    );
    assert!(
        router.contains("scheduledHandler(event, context.env, context)"),
        "{router}"
    );
    assert!(!router.contains("export async function queue"), "{router}");
}

#[test]
fn an_app_with_no_event_handlers_exports_neither() {
    let routes = vec![("/".to_string(), "./src/index.ts".to_string())];
    let result = generate(&routes, &test_config("plain", false), &[]).unwrap();
    assert!(!result.router_code.contains("export async function queue"));
    assert!(!result
        .router_code
        .contains("export async function scheduled"));
}
