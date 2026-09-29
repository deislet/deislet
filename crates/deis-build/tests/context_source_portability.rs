//! A standard handler cannot compile against provider or historical context
//! conveniences that the exact four-field EC-WEB context does not contain.

use std::fs;
use std::path::Path;

use deis_build::{Compiler, CompilerOptions};

const VENDORS: [&str; 4] = ["cloudflare", "tencent", "deno", "deislet"];

fn stage(project: &Path, vendor: &str) {
    fs::create_dir_all(project.join("functions")).expect("function directory");
    fs::write(
        project.join("functions/index.ts"),
        r#"
        function handler(ctx: any): Response {
          const alias = ctx;
          const { services } = alias;
          return new Response(ctx.path + services + ctx["bindings"]);
        }
        export { handler as default };
        "#,
    )
    .expect("handler");
    fs::write(
        project.join(".config.json"),
        format!(
            r#"{{
              "version":"1.0.0","name":"context-source-portability",
              "runtime":"standard-v1","language":"typescript",
              "functionRoot":"./functions","compatibilityDate":"2026-09-01",
              "vendors":{{"{vendor}":{{"enabled":true}}}},
              "build":{{"outDir":"./dist"}}
            }}"#
        ),
    )
    .expect("configuration");
}

#[tokio::test]
async fn every_target_rejects_non_standard_context_fields_before_packaging() {
    for vendor in VENDORS {
        let project = tempfile::tempdir().expect("temporary project");
        stage(project.path(), vendor);
        let error = Compiler::with_options(CompilerOptions {
            project_dir: project.path().to_string_lossy().to_string(),
            ..Default::default()
        })
        .compile(project.path(), vec![vendor.to_string()], false)
        .await
        .expect_err("a non-standard context must fail before packaging")
        .to_string();

        assert!(
            error.contains("EC_WEB_CONTEXT_FIELD_NONSTANDARD"),
            "{vendor}: {error}"
        );
        assert!(error.contains("context.path"), "{vendor}: {error}");
        assert!(error.contains("context.services"), "{vendor}: {error}");
        assert!(error.contains("context.bindings"), "{vendor}: {error}");

        let target = project.path().join("dist/providers").join(vendor);
        let deployable = match vendor {
            "cloudflare" => target.join("index.js"),
            "tencent" => target.join("edge-functions/index.js"),
            "deno" => target.join("_deis_internal/router.ts"),
            "deislet" => target.join("bundle.zip"),
            _ => unreachable!(),
        };
        assert!(
            !deployable.exists(),
            "{vendor} left deployable {} after source refusal",
            deployable.display()
        );
    }
}

/// The two platform-only fields are refused on an event handler too. Its
/// context is the third parameter, and it is not an EC-WEB request context, so
/// only `bindings` and `services` are judged there - but those two are refused
/// on every kind of handler, because a logical resource is reached as
/// `env.<NAME>` and nowhere else.
#[tokio::test]
async fn event_handlers_reject_the_platform_only_context_fields_before_packaging() {
    for (vendor, stem) in [
        ("deislet", "_queue"),
        ("deislet", "_scheduled"),
        ("cloudflare", "_queue"),
        ("cloudflare", "_scheduled"),
    ] {
        let project = tempfile::tempdir().expect("temporary project");
        stage(project.path(), vendor);
        fs::write(
            project.path().join("functions/index.ts"),
            "export default (ctx: any) => new Response(ctx.request.url);\n",
        )
        .expect("http handler");
        fs::write(
            project.path().join(format!("functions/{stem}.ts")),
            r#"
            export default async function handler(event: any, env: any, context: any) {
              const { services } = context;
              await context.bindings.KV.get(String(event) + services + env);
            }
            "#,
        )
        .expect("event handler");
        let error = Compiler::with_options(CompilerOptions {
            project_dir: project.path().to_string_lossy().to_string(),
            ..Default::default()
        })
        .compile(project.path(), vec![vendor.to_string()], false)
        .await
        .expect_err("a platform-only context field must fail before packaging")
        .to_string();

        assert!(
            error.contains("EC_WEB_CONTEXT_FIELD_NONSTANDARD"),
            "{vendor} {stem}: {error}"
        );
        assert!(
            error.contains("context.services"),
            "{vendor} {stem}: {error}"
        );
        assert!(
            error.contains("context.bindings"),
            "{vendor} {stem}: {error}"
        );
    }
}
