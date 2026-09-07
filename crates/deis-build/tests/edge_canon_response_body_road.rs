//! Which road a compiled application's answer takes out of the node.
//!
//! The handler boundary the compiler generates has to close the invocation's
//! lifecycle when the response is finished, so that a `waitUntil` captured out
//! of the invocation fails with `EC_WAIT_UNTIL_CLOSED` afterwards. For a body
//! the application is still producing, the only way to observe that ending is
//! to read the body to its end, and the boundary does that by wrapping the body
//! in a stream of its own.
//!
//! Applied to every answer, that wrapping has a price the standard never asked
//! for: a plain string response, which the node would send in one piece with a
//! Content-Length, becomes a `ReadableStream` and leaves chunked instead. Every
//! client of every application built here loses the length, length-based
//! proxies and progress reporting stop working, and "this answer was ready" and
//! "this answer is still being produced" become indistinguishable from outside.
//!
//! So the node tells the generated entry which responses it will send whole,
//! and the boundary leaves those alone. This test holds both halves of that at
//! once - the ordinary answer keeps the buffered road, the produced body keeps
//! the streaming one - and the lifecycle rule keeps holding on both.

use std::future::poll_fn;
use std::io::Read;
use std::path::Path;

use deis_build::{Compiler, CompilerOptions};
use deis_runtime::runtime::isolate::AppRuntime;
use deis_runtime::runtime::{IsolateSpec, ResponseBody, ServiceRouter, ServiceWiring};
use tokio::sync::oneshot;

const HANDLER: &str = r#"
let capturedWaitUntil;

const encoder = new TextEncoder();

export default function handler(context) {
  const pathname = new URL(context.request.url).pathname;

  if (pathname === "/whole") {
    // Captured on the ordinary road, so that /late below asks the question of
    // an invocation whose response was never wrapped in a stream.
    capturedWaitUntil = context.waitUntil;
    return new Response("whole bytes", {
      headers: { "content-type": "text/plain; charset=utf-8" },
    });
  }

  if (pathname === "/late") {
    try {
      capturedWaitUntil(Promise.resolve("must-not-register"));
      return new Response("registered");
    } catch (error) {
      return new Response(error && error.code ? error.code : "no-code");
    }
  }

  if (pathname === "/produced") {
    const identity = new TransformStream();
    const writer = identity.writable.getWriter();
    void (async () => {
      await writer.write(encoder.encode("first"));
      await writer.write(encoder.encode("-second"));
      await writer.close();
    })();
    return new Response(identity.readable, {
      headers: { "content-type": "text/event-stream" },
    });
  }

  return new Response(null, { status: 404 });
}
"#;

fn stage(project: &Path) {
    std::fs::create_dir_all(project.join("functions")).expect("function directory");
    std::fs::write(project.join("functions/[[path]].js"), HANDLER).expect("handler");
    std::fs::write(
        project.join(".config.json"),
        r#"{
          "version": "1.0.0",
          "name": "edge-canon-response-body-road",
          "runtime": "standard-v1",
          "language": "javascript",
          "functionRoot": "./functions",
          "compatibilityDate": "2026-09-01",
          "vendors": { "deislet": { "enabled": true } },
          "build": { "outDir": "./dist" }
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

async fn drain(runtime: &mut AppRuntime, body: ResponseBody) -> Vec<u8> {
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
            assert_eq!(failure, None, "the compiled response stream failed");
            bytes
        }
    }
}

async fn answer(runtime: &mut AppRuntime, pathname: &str) -> (u16, ResponseBody) {
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
        .expect("execute the compiled worker");
    let (status, _, body) = rx.await.unwrap().unwrap();
    (status, body)
}

#[tokio::test]
async fn a_whole_answer_keeps_the_buffered_road_through_the_generated_entry() {
    let project = tempfile::tempdir().expect("temporary project");
    stage(project.path());
    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: project.path().to_string_lossy().to_string(),
        ..Default::default()
    });
    let result = compiler
        .compile(project.path(), vec!["deislet".to_string()], true)
        .await
        .expect("build the response-body fixture");
    let build = &result.builds["deislet"];
    let worker = worker_from_bundle(&Path::new(&build.output_path).join("bundle.zip"));

    let mut runtime = AppRuntime::new(
        "edge-canon-response-body-road.production".to_string(),
        worker,
        None,
        IsolateSpec::default(),
        &ServiceWiring::default(),
        ServiceRouter::detached(),
    )
    .expect("construct runtime");
    runtime.init_module().await.expect("load compiled worker");

    // The claim: a handler that answers with bytes is answered with bytes. The
    // node frames such a body with a Content-Length, so this is also what every
    // client of this application sees on the wire.
    let (status, body) = answer(&mut runtime, "/whole").await;
    assert_eq!(status, 200);
    assert!(
        matches!(body, ResponseBody::Bytes(_)),
        "an ordinary answer left the isolate on the streaming road, which costs \
         every client of every application built here its Content-Length"
    );
    assert_eq!(drain(&mut runtime, body).await, b"whole bytes");
    assert!(
        !runtime.has_open_response_streams(),
        "an ordinary answer opened a response stream"
    );

    // And it is still an ending: the invocation's lifecycle closed with it, so
    // the `waitUntil` captured above is refused by name.
    let (status, body) = answer(&mut runtime, "/late").await;
    assert_eq!(status, 200);
    assert_eq!(drain(&mut runtime, body).await, b"EC_WAIT_UNTIL_CLOSED");

    // The other half, which is what the boundary's wrapper is for: a body the
    // application is still producing goes out chunk by chunk, and the head does
    // not wait for it.
    let (status, body) = answer(&mut runtime, "/produced").await;
    assert_eq!(status, 200);
    assert!(
        matches!(body, ResponseBody::Stream(_)),
        "a body the application was still producing was collected into a buffer"
    );
    assert_eq!(drain(&mut runtime, body).await, b"first-second");
}
