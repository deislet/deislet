//! The exact EC-STREAM provider fixture must compile through every first-class
//! target, and the self-hosted target must execute the same runtime subset.

use std::future::poll_fn;
use std::io::Read;
use std::path::Path;

use deis_build::{Compiler, CompilerOptions};
use deis_runtime::runtime::isolate::AppRuntime;
use deis_runtime::runtime::{IsolateSpec, ResponseBody, ServiceRouter, ServiceWiring};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

const FIXTURE: &str =
    include_str!("../../../standards/fixtures/edge-canon-stream-provider-runtime.mjs");
const PROJECT_CONFIG: &str =
    include_str!("../../../standards/fixtures/edge-canon-stream-provider-runtime.config.json");
const STANDARD_LOCK: &str = include_str!("../../../standards/edge-canon.lock.json");
const FIXTURE_SHA256: &str = "cccf284de7fbefff0d27240cd3f8ddeadb97fb4105f408c0d0ca01eca690a241";
const VENDORS: [&str; 4] = ["cloudflare", "tencent", "deno", "deislet"];

fn stage(project: &Path) {
    std::fs::create_dir_all(project.join("functions")).expect("function directory");
    std::fs::write(project.join("functions/[[path]].mjs"), FIXTURE).expect("standard fixture");
    std::fs::write(project.join(".config.json"), PROJECT_CONFIG).expect("configuration");
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

async fn fetch_json(runtime: &mut AppRuntime, url: &str) -> serde_json::Value {
    let (tx, rx) = oneshot::channel();
    runtime
        .execute_fetch(url.to_string(), "GET".to_string(), Vec::new(), None, tx)
        .await
        .expect("execute standard fixture");
    let (status, _, body) = rx.await.unwrap().unwrap();
    let body = collect_body(runtime, body).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).expect("fixture response is JSON")
}

fn assert_probe(document: &serde_json::Value, label: &str, canary: u8) {
    assert_eq!(document["schemaVersion"], 1);
    assert_eq!(document["suiteId"], "EC-STREAM");
    assert_eq!(document["label"], label);
    let cases = &document["caseData"];
    assert_eq!(
        cases["EC-STREAM-T001"]["chunks"],
        serde_json::json!([
            [101, 100, 103, 101],
            [45, 99, 97, 110, 111, 110],
            [45, 115, 116, 114, 101, 97, 109]
        ])
    );
    assert_eq!(
        cases["EC-STREAM-T001"]["locksAfterRelease"],
        serde_json::json!({"readable": false, "writable": false})
    );
    for operation in ["secondReader", "secondWriter"] {
        assert_eq!(cases["EC-STREAM-T002"][operation]["settlement"], "throw");
        assert_eq!(cases["EC-STREAM-T002"][operation]["name"], "TypeError");
    }
    for operation in ["oldReaderRead", "oldWriterWrite"] {
        assert_eq!(cases["EC-STREAM-T002"][operation]["settlement"], "reject");
        assert_eq!(cases["EC-STREAM-T002"][operation]["name"], "TypeError");
    }
    assert_eq!(
        cases["EC-STREAM-T003"]["chunks"],
        serde_json::json!([[1], [2], [3]])
    );
    assert_eq!(
        cases["EC-STREAM-T003"]["pipeResult"]["settlement"],
        "fulfill"
    );
    assert_eq!(
        cases["EC-STREAM-T004"]["left"],
        serde_json::json!([[7], [8], [9]])
    );
    assert_eq!(
        cases["EC-STREAM-T004"]["right"],
        cases["EC-STREAM-T004"]["left"]
    );
    assert_eq!(
        cases["EC-STREAM-T009"]["body"],
        serde_json::json!([[canary]])
    );
    assert_eq!(cases["EC-STREAM-T009"]["waitUntilPresent"], true);
    assert_eq!(cases["EC-STREAM-T009"]["waitUntilReturnedUndefined"], true);
    assert_eq!(
        cases["EC-STREAM-T011"]["chunkFailures"],
        serde_json::json!([
          {"variant":"string","settlement":"reject","name":"TypeError","code":"EC_STREAM_CHUNK_TYPE"},
          {"variant":"object","settlement":"reject","name":"TypeError","code":"EC_STREAM_CHUNK_TYPE"}
        ])
    );
    assert_eq!(
        cases["EC-STREAM-T011"]["configuredTransform"],
        serde_json::json!({"settlement":"throw","name":"TypeError","code":"EC_STREAM_TRANSFORMER_NONPORTABLE"})
    );
    assert_eq!(
        cases["EC-STREAM-T011"]["providerGlobals"],
        serde_json::json!([
          {"name":"WebSocket","type":"undefined","owned":true,"writable":false,"enumerable":false,"configurable":false},
          {"name":"WebSocketPair","type":"undefined","owned":true,"writable":false,"enumerable":false,"configurable":false},
          {"name":"WebSocketServer","type":"undefined","owned":true,"writable":false,"enumerable":false,"configurable":false}
        ])
    );
    assert_eq!(
        cases["EC-STREAM-T011"]["transformDescriptor"],
        serde_json::json!({"writable":false,"enumerable":false,"configurable":false})
    );
    assert_eq!(cases["EC-STREAM-T011"]["constructorClosed"], true);
    assert_eq!(cases["EC-STREAM-T011"]["functionPrototypeClosed"], true);
}

#[tokio::test]
async fn exact_provider_fixture_builds_for_every_target_and_runs_on_deislet() {
    let lock: serde_json::Value = serde_json::from_str(STANDARD_LOCK).unwrap();
    assert_eq!(
        lock["source"]["streamsProviderRuntimeFixtureSha256"],
        FIXTURE_SHA256
    );
    assert_eq!(
        lock["source"]["streamsProviderRuntimeFixturePath"],
        "conformance/harness/streams-websockets-background-work/provider-runtime-fixture.mjs"
    );
    assert_eq!(
        hex::encode(Sha256::digest(FIXTURE.as_bytes())),
        FIXTURE_SHA256
    );

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
        .expect("compile the exact EC-STREAM fixture");
    assert_eq!(result.builds.len(), VENDORS.len());

    let dist = project.path().join("dist");
    for path in [
        dist.join("cloudflare/index.js"),
        dist.join("tencent/edge-functions/index.js"),
        dist.join("deno/_deis_internal/router.ts"),
        dist.join("deno/project/functions/[[path]].mjs"),
        dist.join("deislet/bundle.zip"),
    ] {
        assert!(path.is_file(), "missing deployable {}", path.display());
    }
    let deno_import_map = std::fs::read_to_string(dist.join("deno/deno.json")).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&deno_import_map).unwrap()["imports"],
        serde_json::json!({})
    );

    let worker = worker_from_bundle(&dist.join("deislet/bundle.zip"));
    assert!(worker.contains("EC-STREAM-T010"));
    assert!(worker.contains("EC_STREAM_RUNTIME_INCOMPATIBLE"));
    let mut runtime = AppRuntime::new(
        "edge-canon-stream-runtime.production".to_string(),
        worker,
        None,
        IsolateSpec::default(),
        &ServiceWiring::default(),
        ServiceRouter::detached(),
    )
    .expect("construct runtime");
    runtime.init_module().await.expect("load compiled fixture");

    let a = fetch_json(&mut runtime, "http://localhost/?label=A").await;
    let b = fetch_json(&mut runtime, "http://localhost/?label=B").await;
    assert_probe(&a, "A", 0xaa);
    assert_probe(&b, "B", 0xbb);

    let (tx, rx) = oneshot::channel();
    runtime
        .execute_fetch(
            "http://localhost/stream".to_string(),
            "GET".to_string(),
            Vec::new(),
            None,
            tx,
        )
        .await
        .expect("execute streamed response");
    let (status, headers, body) = rx.await.unwrap().unwrap();
    assert_eq!(status, 200);
    assert!(headers.iter().any(
        |(name, value)| name.eq_ignore_ascii_case("x-edge-canon-case") && value == "EC-STREAM-T006"
    ));
    assert_eq!(collect_body(&mut runtime, body).await, vec![1, 2, 3, 4]);

    let (tx, rx) = oneshot::channel();
    runtime
        .execute_fetch(
            "http://localhost/capacity".to_string(),
            "GET".to_string(),
            Vec::new(),
            None,
            tx,
        )
        .await
        .expect("execute capacity response");
    let (status, headers, body) = rx.await.unwrap().unwrap();
    let body = collect_body(&mut runtime, body).await;
    assert_eq!(status, 200);
    assert!(headers.iter().any(|(name, value)| name
        .eq_ignore_ascii_case("x-edge-canon-total-octets")
        && value == "65536"));
    assert_eq!(body.len(), 65_536);
    assert_eq!(
        hex::encode(Sha256::digest(&body)),
        "d1c4808f4915c05b0d32202151b6c8813fbc083ebf1846f0ab0f8df0fe31006e"
    );
}
