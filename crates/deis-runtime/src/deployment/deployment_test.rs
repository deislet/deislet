use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use super::*;
use crate::app::ApplicationManager;
use crate::config::RuntimeConfig;

/// A handler that answers with everything the test wants to observe from
/// inside the isolate.
const ECHO_HANDLER: &str = r#"
export default function handler(context) {
  return new Response(JSON.stringify({ env: context.env }), {
    headers: { "content-type": "application/json" },
  });
}
"#;

/// Code that throws while the module is being evaluated, so the isolate
/// refuses to load it at all.
const BROKEN_HANDLER: &str = "throw new Error('this module never loads');";

struct TestBundle {
    bytes: Vec<u8>,
    sha256: String,
}

/// A handler that reports what happened when it reached for KV, which is the
/// only way to tell "no client was ever built" from "the store is down".
const KV_PROBE_HANDLER: &str = r#"
export default async function handler(context) {
  try {
    await context.services.kv.get("probe");
    return new Response(JSON.stringify({ outcome: "ok", env: context.env }));
  } catch (e) {
    return new Response(JSON.stringify({ outcome: String(e), env: context.env }));
  }
}
"#;

/// Build a bundle the way `deis build --vendor deislet` lays one out.
fn bundle(app: &str, code: &str, assets: &[(&str, &str)], env: &[(&str, &str)]) -> TestBundle {
    bundle_with_services(app, code, assets, env, &[])
}

/// The same, for an app whose manifest declares platform services.
fn bundle_with_services(
    app: &str,
    code: &str,
    assets: &[(&str, &str)],
    env: &[(&str, &str)],
    services: &[&str],
) -> TestBundle {
    let asset_entries: Vec<serde_json::Value> = assets
        .iter()
        .map(|(path, body)| {
            serde_json::json!({
                "path": path,
                "sha256": sha256_hex(body.as_bytes()),
                "size": body.len(),
            })
        })
        .collect();
    let env_map: serde_json::Map<String, serde_json::Value> = env
        .iter()
        .map(|(k, v)| ((*k).to_string(), serde_json::Value::from(*v)))
        .collect();

    let service_map: serde_json::Map<String, serde_json::Value> = services
        .iter()
        .map(|name| ((*name).to_string(), serde_json::Value::from(true)))
        .collect();

    let manifest = serde_json::json!({
        "schema": 1,
        "app": app,
        "entry": "worker/index.js",
        "runtime": "standard-v1",
        "env": env_map,
        "services": service_map,
        "routes": [{ "pattern": "/api/users/:id", "methods": [] }],
        "assets": asset_entries,
    });

    let mut buf = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::FileOptions::default();
        writer
            .start_file(deis_bundle::MANIFEST_ENTRY, options)
            .unwrap();
        writer.write_all(manifest.to_string().as_bytes()).unwrap();
        writer.start_file("worker/index.js", options).unwrap();
        writer.write_all(code.as_bytes()).unwrap();
        for (path, body) in assets {
            writer
                .start_file(format!("assets/{path}"), options)
                .unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
    }

    let sha256 = sha256_hex(&buf);
    TestBundle { bytes: buf, sha256 }
}

/// A stand-in control plane serving bundles at `/bundles/{name}`.
struct ControlPlane {
    addr: SocketAddr,
    _task: tokio::task::JoinHandle<()>,
}

impl ControlPlane {
    async fn serving(bundles: HashMap<String, Vec<u8>>) -> Self {
        let state = Arc::new(bundles);
        let router = Router::new()
            .route(
                "/bundles/:name",
                get(
                    |State(state): State<Arc<HashMap<String, Vec<u8>>>>,
                     AxumPath(name): AxumPath<String>| async move {
                        match state.get(&name) {
                            Some(bytes) => Response::builder()
                                .status(StatusCode::OK)
                                .body(Body::from(bytes.clone()))
                                .unwrap()
                                .into_response(),
                            None => StatusCode::NOT_FOUND.into_response(),
                        }
                    },
                ),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { addr, _task: task }
    }

    fn url(&self, name: &str) -> String {
        format!("http://{}/bundles/{}", self.addr, name)
    }
}

fn config_in(dir: &std::path::Path) -> RuntimeConfig {
    let mut config = RuntimeConfig {
        data_dir: dir.to_path_buf(),
        ..Default::default()
    };
    config.bundles.dir = Some(dir.join("bundles"));
    config
}

async fn deployer_in(
    dir: &std::path::Path,
    app_manager: Arc<ApplicationManager>,
) -> DeploymentManager {
    DeploymentManager::from_config(app_manager, &config_in(dir))
        .await
        .unwrap()
}

fn order(
    app: &str,
    version: &str,
    url: String,
    sha: &str,
    env: &[(&str, &str)],
) -> DeploymentOrder {
    DeploymentOrder {
        app_id: app.to_string(),
        version_id: version.to_string(),
        environment: "production".to_string(),
        bundle_url: url,
        bundle_sha: sha.to_string(),
        env: env
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
    }
}

#[tokio::test]
async fn installs_a_version_and_indexes_its_assets() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = bundle(
        "demo",
        ECHO_HANDLER,
        &[("index.html", "<h1>hi</h1>"), ("css/app.css", "body{}")],
        &[],
    );
    let control =
        ControlPlane::serving(HashMap::from([("v1".to_string(), bundle.bytes.clone())])).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer_in(dir.path(), apps.clone()).await;

    let applied = deployer
        .apply(order("demo", "v1", control.url("v1"), &bundle.sha256, &[]))
        .await
        .unwrap();

    assert_eq!(
        applied,
        Applied::Installed {
            reused_bundle: false
        }
    );
    assert_eq!(
        apps.installed_version("demo").unwrap().version_id,
        "v1".to_string()
    );

    let assets = apps.assets("demo").unwrap();
    assert_eq!(assets.len(), 2);
    assert_eq!(assets.resolve("/").unwrap().path, "index.html");
    assert_eq!(assets.resolve("/css/app.css").unwrap().path, "css/app.css");
    assert!(assets.resolve("/api/users/7").is_none());
}

#[tokio::test]
async fn a_digest_that_does_not_match_leaves_the_running_version_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let good = bundle("demo", ECHO_HANDLER, &[("index.html", "v1")], &[]);
    let next = bundle("demo", ECHO_HANDLER, &[("index.html", "v2")], &[]);
    let control = ControlPlane::serving(HashMap::from([
        ("v1".to_string(), good.bytes.clone()),
        ("v2".to_string(), next.bytes.clone()),
    ]))
    .await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer_in(dir.path(), apps.clone()).await;
    deployer
        .apply(order("demo", "v1", control.url("v1"), &good.sha256, &[]))
        .await
        .unwrap();

    // The control plane promises a digest the bytes do not have. Nothing about
    // the bundle may be trusted, so it must never be opened.
    let wrong_sha = sha256_hex(b"a digest for something else");
    let err = deployer
        .apply(order("demo", "v2", control.url("v2"), &wrong_sha, &[]))
        .await
        .unwrap_err();

    assert!(
        matches!(err, DeployError::Fetch(FetchError::DigestMismatch { .. })),
        "{err}"
    );
    assert_eq!(apps.installed_version("demo").unwrap().version_id, "v1");
    let serving = apps.assets("demo").unwrap();
    assert_eq!(
        &serving.resolve("/index.html").unwrap().bytes[..],
        b"v1",
        "the working version stopped serving its own assets"
    );
    assert!(
        !dir.path().join("bundles/apps/demo/v2.zip").exists(),
        "a bundle that failed verification was kept"
    );
}

#[tokio::test]
async fn a_bundle_filed_under_another_app_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = bundle("someone-else", ECHO_HANDLER, &[], &[]);
    let control =
        ControlPlane::serving(HashMap::from([("v1".to_string(), bundle.bytes.clone())])).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer_in(dir.path(), apps.clone()).await;

    let err = deployer
        .apply(order("demo", "v1", control.url("v1"), &bundle.sha256, &[]))
        .await
        .unwrap_err();

    assert!(matches!(err, DeployError::Bundle(_)), "{err}");
    assert!(apps.installed_version("demo").is_none());
}

#[tokio::test]
async fn a_missing_bundle_url_does_not_disturb_anything() {
    let dir = tempfile::tempdir().unwrap();
    let control = ControlPlane::serving(HashMap::new()).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer_in(dir.path(), apps.clone()).await;

    let err = deployer
        .apply(order("demo", "v1", control.url("v1"), "abc", &[]))
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            DeployError::Fetch(FetchError::Status { code: 404, .. })
        ),
        "{err}"
    );
    assert!(apps.installed_version("demo").is_none());
}

#[tokio::test]
async fn redelivering_the_same_version_reuses_the_bundle_already_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = bundle("demo", ECHO_HANDLER, &[], &[]);
    let control =
        ControlPlane::serving(HashMap::from([("v1".to_string(), bundle.bytes.clone())])).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer_in(dir.path(), apps.clone()).await;

    deployer
        .apply(order("demo", "v1", control.url("v1"), &bundle.sha256, &[]))
        .await
        .unwrap();

    // A secret change redelivers the same version. The bytes are already
    // here and already proven, so nothing is downloaded, but the isolate is
    // rebuilt because `context.env` is read when it is created.
    let applied = deployer
        .apply(order(
            "demo",
            "v1",
            control.url("v1"),
            &bundle.sha256,
            &[("API_KEY", "rotated")],
        ))
        .await
        .unwrap();

    assert_eq!(
        applied,
        Applied::Installed {
            reused_bundle: true
        }
    );
}

#[tokio::test]
async fn an_assignment_skips_an_app_already_on_that_version() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = bundle("demo", ECHO_HANDLER, &[], &[]);
    let control =
        ControlPlane::serving(HashMap::from([("v1".to_string(), bundle.bytes.clone())])).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer_in(dir.path(), apps.clone()).await;

    deployer
        .apply(order(
            "demo",
            "v1",
            control.url("v1"),
            &bundle.sha256,
            &[("API_KEY", "from the push")],
        ))
        .await
        .unwrap();

    // Startup must not undo a push that landed while it was still working
    // through its list - that push is newer, and it is the one that carried
    // the secrets.
    let applied = deployer
        .apply_from_assignment(order("demo", "v1", control.url("v1"), &bundle.sha256, &[]))
        .await
        .unwrap();

    assert_eq!(applied, Applied::AlreadyRunning);
}

#[tokio::test]
async fn secrets_reach_the_isolate_and_never_the_disk() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = bundle("demo", ECHO_HANDLER, &[], &[("LOG_LEVEL", "info")]);
    let control =
        ControlPlane::serving(HashMap::from([("v1".to_string(), bundle.bytes.clone())])).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer_in(dir.path(), apps.clone()).await;

    let secret = "s3cr3t-value-nothing-else-uses";
    deployer
        .apply(order(
            "demo",
            "v1",
            control.url("v1"),
            &bundle.sha256,
            &[("API_KEY", secret)],
        ))
        .await
        .unwrap();

    let (status, _headers, body) = apps
        .execute_app(
            "demo",
            "http://localhost/".to_string(),
            "GET".to_string(),
            HashMap::new(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(status, 200);
    let seen: serde_json::Value = serde_json::from_str(&body.unwrap()).unwrap();
    assert_eq!(seen["env"]["API_KEY"], secret, "the secret never arrived");
    assert_eq!(
        seen["env"]["LOG_LEVEL"], "info",
        "the manifest's own variables must survive the merge"
    );

    // Nothing under the node's data directory may contain the value. The
    // bundle carries only secret names, and the installed-version record is
    // written without any env at all.
    assert!(
        !tree_contains(dir.path(), secret.as_bytes()),
        "a decrypted secret was written to disk"
    );
}

/// The storage wiring only works if two halves meet at install time: the
/// control plane knows the addresses, the bundle's manifest knows which
/// services the app declared. Before they were joined here, every
/// `ctx.services.*` call answered "service not available" no matter what was
/// configured.
#[tokio::test]
async fn a_declared_service_gets_a_client_built_from_the_address_the_order_carried() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = bundle_with_services("demo", KV_PROBE_HANDLER, &[], &[], &["kv"]);
    let control =
        ControlPlane::serving(HashMap::from([("v1".to_string(), bundle.bytes.clone())])).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer_in(dir.path(), apps.clone()).await;

    deployer
        .apply(order(
            "demo",
            "v1",
            control.url("v1"),
            &bundle.sha256,
            // Nothing listens on port 1, so the call fails at the transport.
            // That is the point: a transport failure proves a client existed.
            &[(crate::runtime::STORE_ENDPOINT_KEY, "http://127.0.0.1:1")],
        ))
        .await
        .unwrap();

    let (_, _, body) = apps
        .execute_app(
            "demo",
            "http://localhost/".to_string(),
            "GET".to_string(),
            HashMap::new(),
            None,
        )
        .await
        .unwrap();
    let seen: serde_json::Value = serde_json::from_str(&body.unwrap()).unwrap();
    let outcome = seen["outcome"].as_str().unwrap();
    assert!(
        outcome.contains("failed"),
        "a KV client should have been built and the call should have failed at the \
         transport; instead: {outcome}"
    );
    assert!(
        !outcome.contains("not available to this app"),
        "no KV client was built at all: {outcome}"
    );

    // The platform's own keys are lifted out before the app sees its
    // environment, so an app cannot read - or redirect - its own wiring.
    assert!(seen["env"][crate::runtime::STORE_ENDPOINT_KEY].is_null());
    assert!(seen["env"][crate::runtime::SERVICES_KEY].is_null());
}

/// An app that declares nothing gets no client, which is both cheaper and the
/// honest answer to "what can this app reach".
#[tokio::test]
async fn an_app_that_declares_no_service_is_told_so_rather_than_given_one() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = bundle("demo", KV_PROBE_HANDLER, &[], &[]);
    let control =
        ControlPlane::serving(HashMap::from([("v1".to_string(), bundle.bytes.clone())])).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer_in(dir.path(), apps.clone()).await;

    deployer
        .apply(order(
            "demo",
            "v1",
            control.url("v1"),
            &bundle.sha256,
            &[(crate::runtime::STORE_ENDPOINT_KEY, "http://127.0.0.1:1")],
        ))
        .await
        .unwrap();

    let (_, _, body) = apps
        .execute_app(
            "demo",
            "http://localhost/".to_string(),
            "GET".to_string(),
            HashMap::new(),
            None,
        )
        .await
        .unwrap();
    let seen: serde_json::Value = serde_json::from_str(&body.unwrap()).unwrap();
    assert!(
        seen["outcome"]
            .as_str()
            .unwrap()
            .contains("not available to this app"),
        "{}",
        seen["outcome"]
    );
}

#[tokio::test]
async fn a_version_that_will_not_load_puts_the_previous_one_back() {
    let dir = tempfile::tempdir().unwrap();
    let working = bundle("demo", ECHO_HANDLER, &[("index.html", "v1")], &[]);
    let broken = bundle("demo", BROKEN_HANDLER, &[], &[]);
    let control = ControlPlane::serving(HashMap::from([
        ("v1".to_string(), working.bytes.clone()),
        ("v2".to_string(), broken.bytes.clone()),
    ]))
    .await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer_in(dir.path(), apps.clone()).await;
    deployer
        .apply(order("demo", "v1", control.url("v1"), &working.sha256, &[]))
        .await
        .unwrap();

    let err = deployer
        .apply(order("demo", "v2", control.url("v2"), &broken.sha256, &[]))
        .await
        .unwrap_err();

    assert!(matches!(err, DeployError::Load(_)), "{err}");
    assert_eq!(
        apps.installed_version("demo").unwrap().version_id,
        "v1",
        "a deploy that could not load took the working app down with it"
    );
    let (status, _headers, _body) = apps
        .execute_app(
            "demo",
            "http://localhost/".to_string(),
            "GET".to_string(),
            HashMap::new(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(status, 200, "the restored version is not serving");
}

#[tokio::test]
async fn recovers_its_versions_from_disk_when_there_is_no_control_plane() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = bundle("demo", ECHO_HANDLER, &[("index.html", "hi")], &[]);
    let control =
        ControlPlane::serving(HashMap::from([("v1".to_string(), bundle.bytes.clone())])).await;

    {
        let apps = Arc::new(ApplicationManager::new());
        let deployer = deployer_in(dir.path(), apps.clone()).await;
        deployer
            .apply(order("demo", "v1", control.url("v1"), &bundle.sha256, &[]))
            .await
            .unwrap();
    }

    // A restart with nobody to ask: same bundle directory, no URL usable.
    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer_in(dir.path(), apps.clone()).await;
    let recovered = deployer.recover_from_disk().await;

    assert_eq!(recovered, 1);
    assert_eq!(apps.installed_version("demo").unwrap().version_id, "v1");
    assert_eq!(apps.assets("demo").unwrap().len(), 1);
}

#[tokio::test]
async fn unloading_an_app_takes_its_bundles_and_its_record_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = bundle("demo", ECHO_HANDLER, &[], &[]);
    let control =
        ControlPlane::serving(HashMap::from([("v1".to_string(), bundle.bytes.clone())])).await;

    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer_in(dir.path(), apps.clone()).await;
    deployer
        .apply(order("demo", "v1", control.url("v1"), &bundle.sha256, &[]))
        .await
        .unwrap();

    deployer.unload("demo").await.unwrap();

    assert!(apps.installed_version("demo").is_none());
    assert!(apps.assets("demo").is_none());
    assert!(deployer.store().records().await.is_empty());
    // Otherwise a restart with no control plane would bring the app back.
    assert!(!dir.path().join("bundles/apps/demo").exists());
}

#[tokio::test]
async fn an_app_id_that_would_escape_the_bundle_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let apps = Arc::new(ApplicationManager::new());
    let deployer = deployer_in(dir.path(), apps.clone()).await;

    let err = deployer
        .apply(order(
            "../../etc",
            "v1",
            "http://127.0.0.1:1/never".to_string(),
            "abc",
            &[],
        ))
        .await
        .unwrap_err();

    assert!(matches!(err, DeployError::Identifier(_)), "{err}");
}

#[test]
fn a_deployment_order_never_prints_a_secret_value() {
    let order = order(
        "demo",
        "v1",
        "http://control/bundle".to_string(),
        "abc",
        &[("API_KEY", "s3cr3t")],
    );
    let printed = format!("{order:?}");
    assert!(!printed.contains("s3cr3t"), "{printed}");
    assert!(printed.contains("API_KEY"), "{printed}");
    assert!(printed.contains("redacted"), "{printed}");
}

/// Whether any file under `root` contains `needle`.
fn tree_contains(root: &std::path::Path, needle: &[u8]) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if tree_contains(&path, needle) {
                return true;
            }
        } else if let Ok(bytes) = std::fs::read(&path) {
            if bytes.windows(needle.len()).any(|window| window == needle) {
                return true;
            }
        }
    }
    false
}
