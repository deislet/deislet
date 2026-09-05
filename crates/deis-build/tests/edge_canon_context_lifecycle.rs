//! The generated HTTP bridge exposes one provider-independent context and one
//! lifecycle contract. Build it for every first-class target and execute the
//! observable boundary on the self-hosted runtime.

use std::collections::HashMap;
use std::future::poll_fn;
use std::io::Read;
use std::path::Path;

use deis_build::{Compiler, CompilerOptions};
use deis_runtime::runtime::isolate::AppRuntime;
use deis_runtime::runtime::{IsolateSpec, ResponseBody, ServiceRouter, ServiceWiring};
use tokio::sync::oneshot;

const VENDORS: [&str; 4] = ["cloudflare", "tencent", "deno", "deislet"];

const HANDLER: &str = r#"
const seen = new WeakSet();
let capturedWaitUntil;
let backgroundMarkers = [];

function json(value) {
  return new Response(JSON.stringify(value), {
    headers: { "content-type": "application/json" },
  });
}

export default function handler(context) {
  const pathname = new URL(context.request.url).pathname;
  if (pathname === "/context") {
    const unique = !seen.has(context);
    seen.add(context);
    return json({
      keys: Object.keys(context).sort(),
      unique,
      environment: context.env.TEST_VALUE,
      parameter: context.params.path,
    });
  }
  if (pathname === "/non-promise") {
    try {
      context.waitUntil(42);
      return json({ name: null, code: null });
    } catch (error) {
      return json({ name: error.name, code: error.code });
    }
  }
  if (pathname === "/capture") {
    capturedWaitUntil = context.waitUntil;
    return new Response("captured");
  }
  if (pathname === "/late") {
    try {
      capturedWaitUntil(Promise.resolve("must-not-register"));
      return json({ name: null, code: null });
    } catch (error) {
      return json({ name: error.name, code: error.code });
    }
  }
  if (pathname === "/background") {
    backgroundMarkers = [];
    context.waitUntil(Promise.resolve().then(() => backgroundMarkers.push("first")));
    context.waitUntil(Promise.reject(new Error("EC_BACKGROUND_SECRET")));
    context.waitUntil(new Promise((resolve) => setTimeout(() => {
      backgroundMarkers.push("third");
      resolve();
    }, 10)));
    return new Response("background-accepted");
  }
  if (pathname === "/background-markers") return json(backgroundMarkers);
  if (pathname === "/poison-runtime-state") {
    backgroundMarkers = [];
    context.waitUntil(new Promise((resolve) => setTimeout(() => {
      backgroundMarkers.push("private-wait-until-state");
      resolve();
    }, 5)));
    globalThis.__pending_wait_until = [{ stretch: -1, promise: Promise.resolve("forged") }];
    Array.isArray = () => false;
    return new Response("runtime-state-is-private");
  }
  if (pathname === "/poison-globals") {
    const response = new Response("captured-intrinsics-survived");
    globalThis.Response = function ApplicationResponse() {};
    Promise.resolve = () => { throw new Error("application replaced Promise.resolve"); };
    Object.assign = () => { throw new Error("application replaced Object.assign"); };
    return response;
  }
  return new Response("not found", { status: 404 });
}
"#;

fn stage(project: &Path) {
    std::fs::create_dir_all(project.join("functions")).expect("function directory");
    std::fs::write(project.join("functions/[[path]].js"), HANDLER).expect("handler");
    std::fs::write(
        project.join(".config.json"),
        r#"{
          "version":"1.0.0","name":"edge-canon-context-lifecycle",
          "runtime":"standard-v1","language":"javascript",
          "functionRoot":"./functions","compatibilityDate":"2026-09-01",
          "vendors":{
            "cloudflare":{"enabled":true},"tencent":{"enabled":true},
            "deno":{"enabled":true},"deislet":{"enabled":true}
          },
          "build":{"outDir":"./dist"}
        }"#,
    )
    .expect("configuration");
}

fn worker_from_bundle(path: &Path) -> String {
    let file = std::fs::File::open(path).expect("open bundle");
    let mut archive = zip::ZipArchive::new(file).expect("bundle is ZIP");
    let mut worker = String::new();
    archive
        .by_name("worker/index.js")
        .expect("worker entry")
        .read_to_string(&mut worker)
        .expect("worker is UTF-8");
    worker
}

async fn collect_body(runtime: &mut AppRuntime, body: ResponseBody) -> Vec<u8> {
    match body {
        ResponseBody::None => Vec::new(),
        ResponseBody::Bytes(bytes) => bytes,
        ResponseBody::Stream(mut reader) => {
            let read = async {
                let mut bytes = Vec::new();
                while let Some(chunk) = poll_fn(|cx| reader.poll_chunk(cx)).await {
                    bytes.extend_from_slice(&chunk);
                }
                (bytes, reader.failure())
            };
            let ((bytes, failure), ()) = tokio::join!(read, runtime.pump_response_streams());
            assert_eq!(failure, None, "response stream failed");
            bytes
        }
    }
}

async fn fetch(runtime: &mut AppRuntime, pathname: &str) -> (u16, Vec<u8>) {
    let (tx, rx) = oneshot::channel();
    runtime
        .execute_fetch(
            format!("http://localhost{pathname}"),
            "GET".to_string(),
            Vec::new(),
            None,
            tx,
        )
        .await
        .expect("execute lifecycle fixture");
    let (status, _, body) = rx.await.unwrap().unwrap();
    (status, collect_body(runtime, body).await)
}

async fn fetch_json(runtime: &mut AppRuntime, pathname: &str) -> serde_json::Value {
    let (status, body) = fetch(runtime, pathname).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).expect("JSON response")
}

#[tokio::test]
async fn every_target_builds_the_exact_context_and_deislet_enforces_its_lifecycle() {
    let project = tempfile::tempdir().expect("temporary project");
    stage(project.path());
    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: project.path().to_string_lossy().to_string(),
        ..Default::default()
    });
    let result = compiler
        .compile(
            project.path(),
            VENDORS.iter().map(|vendor| (*vendor).to_string()).collect(),
            true,
        )
        .await
        .expect("build the lifecycle fixture for every target");
    assert_eq!(result.builds.len(), VENDORS.len());

    let mut env = HashMap::new();
    env.insert("TEST_VALUE".to_string(), "edge-canon-env".to_string());
    let deislet_root = Path::new(&result.builds["deislet"].output_path);
    let mut runtime = AppRuntime::new(
        "edge-canon-context-lifecycle.production".to_string(),
        worker_from_bundle(&deislet_root.join("bundle.zip")),
        None,
        IsolateSpec {
            env,
            ..Default::default()
        },
        &ServiceWiring::default(),
        ServiceRouter::detached(),
    )
    .expect("construct runtime");
    runtime.init_module().await.expect("load fixture");

    for _ in 0..2 {
        let context = fetch_json(&mut runtime, "/context").await;
        assert_eq!(
            context["keys"],
            serde_json::json!(["env", "params", "request", "waitUntil"])
        );
        assert_eq!(context["unique"], true);
        assert_eq!(context["environment"], "edge-canon-env");
        assert_eq!(context["parameter"], "context");
    }

    let non_promise = fetch_json(&mut runtime, "/non-promise").await;
    assert_eq!(non_promise["name"], "TypeError");
    assert_eq!(non_promise["code"], "EC_WAIT_UNTIL_PROMISE_REQUIRED");

    let (status, body) = fetch(&mut runtime, "/capture").await;
    assert_eq!(status, 200);
    assert_eq!(body, b"captured");
    let late = fetch_json(&mut runtime, "/late").await;
    assert_eq!(late["name"], "TypeError");
    assert_eq!(late["code"], "EC_WAIT_UNTIL_CLOSED");

    let (status, body) = fetch(&mut runtime, "/background").await;
    assert_eq!(status, 200);
    assert_eq!(body, b"background-accepted");
    assert_eq!(
        fetch_json(&mut runtime, "/background-markers").await,
        serde_json::json!(["first", "third"])
    );

    let (status, body) = fetch(&mut runtime, "/poison-runtime-state").await;
    assert_eq!(status, 200);
    assert_eq!(body, b"runtime-state-is-private");
    assert_eq!(
        fetch_json(&mut runtime, "/background-markers").await,
        serde_json::json!(["private-wait-until-state"])
    );

    let (status, body) = fetch(&mut runtime, "/poison-globals").await;
    assert_eq!(status, 200);
    assert_eq!(body, b"captured-intrinsics-survived");
}
