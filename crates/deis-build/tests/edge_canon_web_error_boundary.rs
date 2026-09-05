//! The exact EC-WEB fixture must receive one error boundary on every target,
//! and the self-hosted target must execute its fixed non-leaking response.

use std::future::poll_fn;
use std::io::Read;
use std::path::Path;

use deis_build::{Compiler, CompilerOptions};
use deis_runtime::runtime::isolate::AppRuntime;
use deis_runtime::runtime::{IsolateSpec, ResponseBody, ServiceRouter, ServiceWiring};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

const FIXTURE: &str = include_str!("../../../standards/fixtures/edge-canon-web-fetch-events.mjs");
const CPU_WORKLOAD: &str =
    include_str!("../../../standards/fixtures/edge-canon-web-fetch-events-cpu-workload.mjs");
const PROJECT_CONFIG: &str =
    include_str!("../../../standards/fixtures/edge-canon-web-fetch-events.config.json");
const STANDARD_LOCK: &str = include_str!("../../../standards/edge-canon.lock.json");
const FIXTURE_SHA256: &str = "1d58a3a6320952f70d476198ff95cd109c93c2c02925620a7d725918ca290523";
const CPU_WORKLOAD_SHA256: &str =
    "6a1c34d55f87bab948b632ff9eb9b8eea5a5e10b67c132964b7b18567cef2e5b";
const VENDORS: [&str; 4] = ["cloudflare", "tencent", "deno", "deislet"];

fn stage(project: &Path) {
    std::fs::create_dir_all(project.join("functions")).expect("function directory");
    std::fs::create_dir_all(project.join("canonical")).expect("canonical module directory");
    std::fs::write(
        project.join("functions/[[path]].ts"),
        "export { default } from '../canonical/fixture.mjs';\n",
    )
    .expect("route module");
    std::fs::write(project.join("canonical/fixture.mjs"), FIXTURE).expect("standard fixture");
    std::fs::write(project.join("canonical/cpu-workload.mjs"), CPU_WORKLOAD)
        .expect("standard CPU workload");
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

async fn fetch(runtime: &mut AppRuntime, pathname: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
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
        .expect("execute EC-WEB fixture");
    let (status, headers, body) = rx.await.unwrap().unwrap();
    let body = collect_body(runtime, body).await;
    (status, headers, body)
}

fn header<'a>(headers: &'a [(String, String)], wanted: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
        .map(|(_, value)| value.as_str())
}

#[tokio::test]
async fn exact_web_fixture_builds_for_every_target_and_errors_are_canonical_on_deislet() {
    let lock: serde_json::Value = serde_json::from_str(STANDARD_LOCK).unwrap();
    assert_eq!(
        lock["source"]["webFetchEventsFixtureSha256"],
        FIXTURE_SHA256
    );
    assert_eq!(
        lock["source"]["webFetchEventsCpuWorkloadSha256"],
        CPU_WORKLOAD_SHA256
    );
    assert_eq!(
        hex::encode(Sha256::digest(FIXTURE.as_bytes())),
        FIXTURE_SHA256
    );
    assert_eq!(
        hex::encode(Sha256::digest(CPU_WORKLOAD.as_bytes())),
        CPU_WORKLOAD_SHA256
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
        .expect("compile the exact EC-WEB fixture");
    assert_eq!(result.builds.len(), VENDORS.len());

    let root = |vendor: &str| Path::new(&result.builds[vendor].output_path).to_path_buf();
    let cloudflare = root("cloudflare");
    let tencent = root("tencent");
    let deno = root("deno");
    let deislet = root("deislet");
    for path in [
        cloudflare.join("index.js"),
        tencent.join("edge-functions/index.js"),
        deno.join("_deis_internal/router.ts"),
        deno.join("_deis_internal/edge-canon-runtime.js"),
        deislet.join("bundle.zip"),
    ] {
        assert!(path.is_file(), "missing deployable {}", path.display());
    }

    for (entry, support) in [
        (cloudflare.join("index.js"), None),
        (tencent.join("edge-functions/index.js"), None),
        (
            deno.join("_deis_internal/router.ts"),
            Some(deno.join("_deis_internal/edge-canon-runtime.js")),
        ),
    ] {
        let mut code = std::fs::read_to_string(&entry).expect("provider entry is UTF-8");
        if let Some(support) = support {
            code.push_str(&std::fs::read_to_string(support).expect("support module is UTF-8"));
        }
        assert!(code.contains("EC_HANDLER_THROWN"), "{}", entry.display());
        assert!(
            code.contains("EC_HANDLER_RESULT_INVALID"),
            "{}",
            entry.display()
        );
        assert!(!code.contains("error.message"), "{}", entry.display());
    }

    let worker = worker_from_bundle(&deislet.join("bundle.zip"));
    assert!(worker.contains("EC_WEB_SECRET_MUST_NOT_LEAK"));
    assert!(worker.contains("EC_HANDLER_THROWN"));
    assert!(worker.contains("EC_HANDLER_RESULT_INVALID"));
    let mut runtime = AppRuntime::new(
        "edge-canon-web-errors.production".to_string(),
        worker,
        None,
        IsolateSpec::default(),
        &ServiceWiring::default(),
        ServiceRouter::detached(),
    )
    .expect("construct runtime");
    runtime.init_module().await.expect("load compiled fixture");

    let (status, _, body) = fetch(&mut runtime, "/sync").await;
    assert_eq!(status, 200);
    assert_eq!(body, b"edge-canon-sync");

    for pathname in [
        "/throw-sync",
        "/throw-async",
        "/invalid-undefined",
        "/invalid-string",
        "/invalid-object",
    ] {
        let (status, headers, body) = fetch(&mut runtime, pathname).await;
        assert_eq!(status, 500, "{pathname}");
        assert_eq!(
            header(&headers, "content-type"),
            Some("text/plain; charset=utf-8"),
            "{pathname}"
        );
        assert_eq!(
            header(&headers, "cache-control"),
            Some("no-store"),
            "{pathname}"
        );
        assert_eq!(body, b"Internal Server Error\n", "{pathname}");
        assert!(
            !headers
                .iter()
                .any(|(_, value)| value.contains("EC_WEB_SECRET_MUST_NOT_LEAK"))
                && !String::from_utf8_lossy(&body).contains("EC_WEB_SECRET_MUST_NOT_LEAK"),
            "{pathname} leaked the fixture sentinel"
        );
    }
}
