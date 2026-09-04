//! The Deislet Node target is not complete when it merely packs an archive:
//! the exact worker bytes emitted by the compiler must load and execute in the
//! self-hosted runtime.

use std::future::poll_fn;
use std::io::Read;
use std::path::Path;

use deis_build::{Compiler, CompilerOptions};
use deis_runtime::runtime::isolate::AppRuntime;
use deis_runtime::runtime::{IsolateSpec, ResponseBody, ServiceRouter, ServiceWiring};
use tokio::sync::oneshot;

fn stage(project: &Path) {
    std::fs::create_dir_all(project.join("functions")).unwrap();
    std::fs::write(
        project.join("functions/index.ts"),
        r#"
          import { Buffer } from "node:buffer";
          import { createHash } from "node:crypto";
          import { platform, version, versions } from "node:process";

          export default async function handler() {
            const digest = createHash("sha256").update(Buffer.from("edge-canon")).digest("hex");
            const providerGlobals = ["Web" + "Socket", ["WebSocket", "Pair"].join(""), ["WebSocket", "Server"].join("")]
              .map(name => {
                const descriptor = Object.getOwnPropertyDescriptor(globalThis, name);
                return {
                  name,
                  type: typeof globalThis[name],
                  owned: descriptor !== undefined,
                  valueIsUndefined: descriptor?.value === undefined,
                  writable: descriptor?.writable,
                  enumerable: descriptor?.enumerable,
                  configurable: descriptor?.configurable,
                };
              });
            const transformDescriptor = Object.getOwnPropertyDescriptor(globalThis, "TransformStream");
            const identity = new TransformStream();
            const reader = identity.readable.getReader();
            const writer = identity.writable.getWriter();
            const [writeResult, readResult] = await Promise.all([
              writer.write(new Uint8Array([1, 2, 3])),
              reader.read(),
            ]);
            writer.releaseLock();
            reader.releaseLock();
            const invalid = new TransformStream();
            const invalidWriter = invalid.writable.getWriter();
            let invalidChunk;
            try {
              await invalidWriter.write("not-bytes");
              invalidChunk = { settlement: "fulfill", name: null, code: null };
            } catch (error) {
              invalidChunk = { settlement: "reject", name: error.name, code: error.code };
            }
            invalidWriter.releaseLock();
            let configuredTransform;
            try {
              Reflect.construct(TransformStream, [{ transform() {} }]);
              configuredTransform = { settlement: "return", name: null, code: null };
            } catch (error) {
              configuredTransform = { settlement: "throw", name: error.name, code: error.code };
            }
            return new Response(JSON.stringify({
              digest, platform, version, node: versions.node, providerGlobals,
              transform: {
                descriptor: {
                  writable: transformDescriptor?.writable,
                  enumerable: transformDescriptor?.enumerable,
                  configurable: transformDescriptor?.configurable,
                },
                writeResult: writeResult ?? null,
                chunk: [...readResult.value],
                invalidChunk,
                configuredTransform,
                constructorClosed: TransformStream.prototype.constructor === TransformStream,
                functionPrototypeClosed: Object.getPrototypeOf(TransformStream) === Function.prototype,
              },
            }));
          }
        "#,
    )
    .unwrap();
    std::fs::write(
        project.join(".config.json"),
        r#"{
          "version": "1.0.0",
          "name": "deislet-node-handoff",
          "runtime": "standard-v1",
          "language": "typescript",
          "functionRoot": "./functions",
          "compatibilityDate": "2026-09-01",
          "vendors": { "deislet": { "enabled": true } },
          "build": { "outDir": "./dist" }
        }"#,
    )
    .unwrap();
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
            assert_eq!(failure, None, "compiled response stream failed");
            bytes
        }
    }
}

#[tokio::test]
async fn compiler_output_runs_on_the_deislet_node_compatibility_layer() {
    let project = tempfile::tempdir().unwrap();
    stage(project.path());
    let compiler = Compiler::with_options(CompilerOptions {
        project_dir: project.path().to_string_lossy().to_string(),
        ..Default::default()
    });
    let result = compiler
        .compile(project.path(), vec!["deislet".to_string()], true)
        .await
        .expect("compile selected Node imports");
    let build = &result.builds["deislet"];
    let worker = worker_from_bundle(&Path::new(&build.output_path).join("bundle.zip"));
    assert!(worker.contains("node:crypto"));
    assert!(!worker.contains("node:module"));

    let mut runtime = AppRuntime::new(
        "deislet-node-handoff.production".to_string(),
        worker,
        None,
        IsolateSpec::default(),
        &ServiceWiring::default(),
        ServiceRouter::detached(),
    )
    .expect("construct runtime");
    runtime.init_module().await.expect("load compiled worker");

    let (tx, rx) = oneshot::channel();
    runtime
        .execute_fetch(
            "http://localhost/".to_string(),
            "GET".to_string(),
            Vec::new(),
            None,
            tx,
        )
        .await
        .expect("execute compiled worker");
    let (status, _, body) = rx.await.unwrap().unwrap();
    let body = String::from_utf8(collect_body(&mut runtime, body).await).unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({
          "digest": "175811799a9da49a1cd8fc30f26d1e1d6b3d677108d76108309b5900dbf44ab0",
          "platform": "linux",
          "version": "v24.20.0",
          "node": "24.20.0",
          "providerGlobals": [
            {"name":"WebSocket","type":"undefined","owned":true,"valueIsUndefined":true,"writable":false,"enumerable":false,"configurable":false},
            {"name":"WebSocketPair","type":"undefined","owned":true,"valueIsUndefined":true,"writable":false,"enumerable":false,"configurable":false},
            {"name":"WebSocketServer","type":"undefined","owned":true,"valueIsUndefined":true,"writable":false,"enumerable":false,"configurable":false}
          ],
          "transform": {
            "descriptor": {"writable":false,"enumerable":false,"configurable":false},
            "writeResult": null,
            "chunk": [1,2,3],
            "invalidChunk": {"settlement":"reject","name":"TypeError","code":"EC_STREAM_CHUNK_TYPE"},
            "configuredTransform": {"settlement":"throw","name":"TypeError","code":"EC_STREAM_TRANSFORMER_NONPORTABLE"},
            "constructorClosed": true,
            "functionPrototypeClosed": true
          }
        })
    );
}
