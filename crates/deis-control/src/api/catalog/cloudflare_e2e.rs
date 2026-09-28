//! End to end: the Control deploys an application to Cloudflare through the
//! real provider driver process, with a recording fake `wrangler` standing in
//! for Cloudflare.
//!
//! Everything between the HTTP API and `wrangler` is the production path:
//! the canonical artifact, the version and the Cloudflare payload go in over
//! the product API; the provider deployment is begun over the API and driven
//! by the same `drive_provider_deployment_once` the background dispatcher
//! calls; the dispatcher resolves a signed release installed and verified by
//! the release registry, leases the credential from the vault, copies and
//! re-hashes the payload into a private operation workspace, and launches the
//! driver with a cleared environment. The driver process runs the Cloudflare
//! driver's own entry point (`protocol::execute_from_stdin`), finds a
//! verified toolchain tree and runs "wrangler" through "node".
//!
//! Two things are test doubles, and only these two:
//!
//! * The installed driver entrypoint is a shell script that re-executes this
//!   test binary into [`cloudflare_driver_process`], which runs the Cloudflare
//!   driver library exactly as its `main` does. A unit test cannot name the
//!   driver crate's binary, and this keeps the launch a real child process.
//! * `node` is a script that answers `--version` and otherwise runs the
//!   driver crate's recording fake wrangler
//!   (`crates/deis-provider-driver-cloudflare/tests/fixtures/fake-wrangler.sh`),
//!   and the toolchain tree it is pointed at holds a placeholder
//!   `wrangler.js` with a manifest the driver verifies like any other.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use deis_build_core::BindingKind;
use deis_provider_driver::{
    ArtifactVersionRef, CredentialMaterial, DriverOperation, DriverOrchestrationOutcome,
    DriverOrchestrator, DriverOrchestratorConfig, DriverRequest, DriverWorkspaceRoot,
    ExecutableProviderDriverConfig, ProviderNeutralPlan, ProviderTargetRef,
};
use deis_wire::{CanonicalId, OperationState};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tower::ServiceExt as _;

use super::tests::{Harness, call, harness, json};
use crate::catalog::bundle::tests::sample_bundle;
use crate::catalog::provider_driver_release::tests::Fixture;
use crate::catalog::{
    CatalogPayloadSource, OperationRequestContext, ProviderCredentialBindingSpec,
    ProviderDeploymentDriveOutcome, ProviderDriverDispatcher, ProviderDriverReleaseRegistry,
    ProviderInstallationSpec, ProviderTargetGrantMutationContext, ProviderTargetGrantSpec,
    ProviderTargetSpec,
};

/// Set by the driver entrypoint script; [`cloudflare_driver_process`] does
/// nothing without it.
const DRIVER_ENTRY_ENV: &str = "DEIS_TEST_CLOUDFLARE_DRIVER_PROCESS";
const DRIVER_ENTRY_TEST: &str = "api::catalog::cloudflare_e2e::cloudflare_driver_process";
const FAKE_WRANGLER: &str =
    include_str!("../../../../deis-provider-driver-cloudflare/tests/fixtures/fake-wrangler.sh");
const TOKEN: &str = "e2e-api-token-7c2a";
const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
const TOOLCHAIN_MANIFEST: &str = ".deis-toolchain-manifest.json";

/// The Cloudflare driver process the end-to-end test launches.
///
/// Ignored so it never runs as a test of its own; the entrypoint script runs
/// it with `--include-ignored` and [`DRIVER_ENTRY_ENV`] set. It does what the
/// driver binary's `main` does and exits before the test harness prints its
/// summary, so the only JSON on stdout is the driver's outcome.
#[test]
#[ignore = "the driver process launched by the Cloudflare end-to-end test, not a test itself"]
fn cloudflare_driver_process() {
    if std::env::var_os(DRIVER_ENTRY_ENV).is_none() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("driver runtime starts");
    let result = runtime.block_on(deis_provider_driver_cloudflare::protocol::execute_from_stdin());
    let _ = std::io::stdout().flush();
    std::process::exit(i32::from(result.is_err()));
}

/// The sample application: every resource kind Cloudflare maps, a consumed
/// queue, a cron trigger and a secret.
fn project() -> deis_build_core::ProjectConfig {
    serde_json::from_value(serde_json::json!({
        "version": "1.0",
        "name": "shop",
        "runtime": "standard-v1",
        "language": "typescript",
        "compatibilityDate": "2026-09-01",
        "bindings": {
            "HOT": {"kind": "kv"},
            "DB": {"kind": "database"},
            "FILES": {"kind": "blob"},
            "ORDERS": {"kind": "queue"}
        },
        "queues": [{"name": "ORDERS"}],
        "cron": ["*/5 * * * *"],
        "environment": {"secrets": ["API_KEY"]}
    }))
    .expect("sample project parses")
}

/// A fake Cloudflare account, the fake `node` that fronts it, and the
/// driver entrypoint script.
struct FakeCloudflare {
    root: tempfile::TempDir,
}

impl FakeCloudflare {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let account = root.path().join("account");
        fs::create_dir_all(account.join("fail")).unwrap();
        let wrangler = root.path().join("fake-wrangler.sh");
        write_executable(
            &wrangler,
            &FAKE_WRANGLER.replace("__STATE__", account.to_str().unwrap()),
        );
        write_executable(
            &root.path().join("node"),
            &format!(
                "#!/bin/sh\n\
                 # Answers the driver's Node version gate; every other call is\n\
                 # `node <wrangler.js> <args>` and goes to the fake wrangler.\n\
                 if [ \"$1\" = --version ]; then echo v22.12.0; exit 0; fi\n\
                 shift\n\
                 exec /bin/bash '{}' \"$@\"\n",
                wrangler.display()
            ),
        );
        Self { root }
    }

    fn account(&self) -> PathBuf {
        self.root.path().join("account")
    }

    fn node(&self) -> PathBuf {
        self.root.path().join("node")
    }

    /// The installed driver entrypoint: runs this test binary's
    /// [`cloudflare_driver_process`] with the launcher's stdin, environment
    /// and working directory, and passes only the outcome through (the test
    /// harness prints its own header, and the test name on the outcome's
    /// line).
    fn driver_entrypoint(&self) -> Vec<u8> {
        let test_binary = std::env::current_exe().unwrap();
        format!(
            "#!/bin/sh\n\
             export {DRIVER_ENTRY_ENV}=1\n\
             export DEIS_PROVIDER_NODE='{node}'\n\
             '{binary}' --exact {DRIVER_ENTRY_TEST} --include-ignored --nocapture --test-threads=1 \
             | grep -o '{{.*'\n",
            node = self.node().display(),
            binary = test_binary.display(),
        )
        .into_bytes()
    }

    /// Put a verified toolchain tree where the driver looks for it,
    /// `$HOME/.deis/tools`, with `$HOME` the launcher's shared driver home.
    fn install_toolchain(&self, driver_home: &Path) {
        let lock = deis_provider_driver_cloudflare::toolchain::EMBEDDED_TOOLCHAIN_LOCK;
        let lock_sha256 = hex::encode(Sha256::digest(lock.as_bytes()));
        let tree = driver_home.join(".deis").join("tools").join(format!(
            "wrangler-{}-{}",
            deis_provider_driver_cloudflare::EXPECTED_WRANGLER_VERSION,
            &lock_sha256[..16]
        ));
        let entry = "node_modules/wrangler/bin/wrangler.js";
        let contents = b"// stands in for wrangler; the fake node never runs it\n";
        fs::create_dir_all(tree.join("node_modules/wrangler/bin")).unwrap();
        fs::write(tree.join(entry), contents).unwrap();
        fs::set_permissions(tree.join(entry), fs::Permissions::from_mode(0o644)).unwrap();
        let manifest = serde_json::json!({
            "lock_sha256": lock_sha256,
            "files": {entry: hex::encode(Sha256::digest(contents))},
        });
        fs::write(
            tree.join(TOOLCHAIN_MANIFEST),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    fn argv(&self) -> Vec<String> {
        fs::read_to_string(self.account().join("argv.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn calls(&self, prefix: &str) -> Vec<String> {
        self.argv()
            .into_iter()
            .filter(|line| line.starts_with(prefix))
            .collect()
    }

    fn names(&self, kind: &str) -> Vec<String> {
        let mut names = fs::read_dir(self.account().join(kind))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn resource_id(&self, kind: &str, name: &str) -> String {
        fs::read_to_string(self.account().join(kind).join(name)).unwrap()
    }

    /// The `wrangler.toml` wrangler saw on its `index`-th call (1-based).
    fn toml_seen_by_call(&self, index: usize) -> String {
        fs::read_to_string(self.account().join(format!("cwd.{index}"))).unwrap_or_else(|_| {
            panic!("call {index} ran without a wrangler.toml");
        })
    }

    fn call_index(&self, prefix: &str, occurrence: usize) -> usize {
        self.argv()
            .iter()
            .enumerate()
            .filter(|(_, line)| line.starts_with(prefix))
            .nth(occurrence)
            .unwrap_or_else(|| panic!("no call #{occurrence} starting with {prefix:?}"))
            .0
            + 1
    }
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn operation_context(key: &str, route: &str) -> OperationRequestContext {
    OperationRequestContext::new(
        key,
        route,
        "account:cloudflare-e2e".to_owned(),
        CanonicalId::new_v7(),
        CanonicalId::new_v7(),
    )
    .unwrap()
}

async fn post_with_key(
    harness: &Harness,
    uri: &str,
    key: &str,
    body: Value,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("Idempotency-Key", key)
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = harness.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn envelope(derivation: &Path, payload: &Path, names: &[String]) -> Vec<u8> {
    let mut envelope = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o644);
    envelope
        .start_file(".deislet/provider-derivation.json", options)
        .unwrap();
    envelope.write_all(&fs::read(derivation).unwrap()).unwrap();
    for name in names {
        envelope
            .start_file(format!("payload/{name}"), options)
            .unwrap();
        envelope
            .write_all(&fs::read(payload.join(name)).unwrap())
            .unwrap();
    }
    envelope.finish().unwrap().into_inner()
}

/// What `deis deploy --target cloudflare` puts on the Control before it
/// begins a provider deployment: the canonical artifact, the version that
/// names it, and the Cloudflare payload derived from it.
async fn upload_application(harness: &Harness) -> ArtifactVersionRef {
    let router = &harness.router;
    let (status, _) = json(
        router,
        "POST",
        "/api/apps",
        serde_json::json!({"id": "shop"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, environment) = json(
        router,
        "POST",
        "/api/apps/shop/environments",
        serde_json::json!({"name": "production"}),
    )
    .await;
    assert!(status.is_success(), "{environment}");

    let source = tempfile::tempdir().unwrap();
    let canonical = crate::catalog::artifact::tests::source_artifact(source.path());
    let (status, artifact) = call(
        router,
        "POST",
        &format!(
            "/api/apps/shop/artifacts?sha256={}",
            canonical.artifact_sha256
        ),
        Body::from(crate::catalog::artifact::tests::transport(&canonical.root)),
        "application/zip",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{artifact}");
    let artifact_id = artifact["artifact"]["artifactId"]
        .as_str()
        .unwrap()
        .to_owned();

    // The Deislet version, which is what a provider deployment names.
    let bundle = sample_bundle("shop");
    let bundle_dir = tempfile::tempdir().unwrap();
    fs::write(bundle_dir.path().join("bundle.zip"), &bundle).unwrap();
    let store = tempfile::tempdir().unwrap();
    let published =
        deis_artifact::provider_derivation::publish_payload(store.path(), bundle_dir.path())
            .unwrap();
    let records = tempfile::tempdir().unwrap();
    let derivation = deis_artifact::provider_derivation::publish(
        records.path(),
        &published.root,
        "deislet",
        deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
        &canonical.artifact_sha256,
        &canonical.content_root_sha256,
    )
    .unwrap();
    let (status, version) = call(
        router,
        "POST",
        &format!(
            "/api/apps/shop/artifacts/{artifact_id}/versions?sha256={}",
            hex::encode(Sha256::digest(&bundle))
        ),
        Body::from(envelope(
            &derivation.manifest_path,
            bundle_dir.path(),
            &["bundle.zip".to_owned()],
        )),
        "application/vnd.deislet.provider-envelope.v1+zip",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{version}");
    let version_id: CanonicalId = version["version"]["canonical_id"]
        .as_str()
        .unwrap_or_else(|| panic!("version without canonical id: {version}"))
        .parse()
        .unwrap();

    // The Cloudflare payload, as the real build adapter writes it:
    // wrangler.toml with `deis:unresolved/...` placeholders, the Worker and
    // binding-plan.json.
    let generated = deis_build_adapter_cloudflare::generate(
        &[("/".to_owned(), "./handlers/index.ts".to_owned())],
        &project(),
        &[],
    )
    .unwrap();
    let payload = tempfile::tempdir().unwrap();
    let mut names = Vec::new();
    for (name, contents) in &generated.config_files {
        fs::write(payload.path().join(name), contents).unwrap();
        names.push(name.clone());
    }
    fs::write(payload.path().join("index.js"), &generated.router_code).unwrap();
    names.push("index.js".to_owned());
    assert!(
        fs::read_to_string(payload.path().join("wrangler.toml"))
            .unwrap()
            .contains("deis:unresolved/"),
        "the adapter's wrangler.toml carries placeholders for the driver to fill"
    );
    let payload_store = tempfile::tempdir().unwrap();
    let published =
        deis_artifact::provider_derivation::publish_payload(payload_store.path(), payload.path())
            .unwrap();
    let records = tempfile::tempdir().unwrap();
    let derivation = deis_artifact::provider_derivation::publish(
        records.path(),
        &published.root,
        "cloudflare",
        deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
        &canonical.artifact_sha256,
        &canonical.content_root_sha256,
    )
    .unwrap();
    let (status, stored) = call(
        router,
        "POST",
        &format!("/api/apps/shop/artifacts/{artifact_id}/provider-payloads"),
        Body::from(envelope(&derivation.manifest_path, payload.path(), &names)),
        "application/vnd.deislet.provider-envelope.v1+zip",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{stored}");

    ArtifactVersionRef {
        artifact_version_id: version_id,
        canonical_artifact_sha256: canonical.artifact_sha256.parse().unwrap(),
        provider_payload_sha256: published.payload_root_sha256.parse().unwrap(),
    }
}

struct ProviderSetup {
    dispatcher: ProviderDriverDispatcher,
    target: ProviderTargetRef,
    application_id: CanonicalId,
    environment_id: CanonicalId,
    _fixture: Fixture,
}

/// What an operator sets up once: the signed driver release, the Cloudflare
/// credential, an installation and target, and a grant of the target to
/// `shop/production`.
async fn register_provider(
    harness: &Harness,
    fake: &FakeCloudflare,
    work_root: &Path,
) -> ProviderSetup {
    let store = harness.state.store.as_ref().clone();
    let fixture = Fixture::with_driver_executable(fake.driver_entrypoint());
    let registry = ProviderDriverReleaseRegistry::new(
        store.clone(),
        &fixture.install_root,
        fixture.trust_root.clone(),
        None,
    )
    .await
    .unwrap();
    registry
        .install_and_register(
            fixture.release.clone(),
            fixture.manifest.clone(),
            fixture.authorization.clone(),
            fixture.transparency.clone(),
            &fixture.archive,
        )
        .await
        .unwrap();

    let vault = Arc::clone(&harness.state.provider_credentials);
    let credential = vault
        .provision(
            ProviderCredentialBindingSpec {
                backend_id: "cloudflare-workers-pages".to_owned(),
                name: "cloudflare-e2e".to_owned(),
                account_id: None,
            },
            CredentialMaterial::new(
                format!(r#"{{"api_token":"{TOKEN}","account_id":"{ACCOUNT}"}}"#).into_bytes(),
            )
            .unwrap(),
            operation_context("cloudflare-credential", "POST\n/provider-credentials\nv1"),
        )
        .await
        .unwrap();
    let installation = store
        .register_provider_installation(
            ProviderInstallationSpec {
                backend_id: "cloudflare-workers-pages".to_owned(),
                account_reference: ACCOUNT.to_owned(),
                region: None,
                credential_binding_id: credential.binding_id,
            },
            None,
        )
        .await
        .unwrap();
    let target = store
        .register_provider_target(
            ProviderTargetSpec {
                installation_id: installation.installation_id,
                name: "cloudflare".to_owned(),
                standard_version: deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION.to_owned(),
                target_config_digest: "b".repeat(64).parse().unwrap(),
                driver_manifest: fixture.manifest.clone(),
            },
            None,
        )
        .await
        .unwrap();
    let app = store.get_app("shop").await.unwrap();
    let environment = store.get_environment("shop", "production").await.unwrap();
    store
        .grant_provider_target(
            ProviderTargetGrantSpec {
                application_id: app.canonical_id,
                environment_id: environment.canonical_id,
                provider_target_id: target.target_id,
                note: Some("cloudflare end-to-end".to_owned()),
            },
            ProviderTargetGrantMutationContext {
                actor_subject: "account:cloudflare-e2e".to_owned(),
                idempotency_record: None,
            },
        )
        .await
        .unwrap();

    let workspaces = DriverWorkspaceRoot::new(
        work_root,
        Arc::new(CatalogPayloadSource::new(Arc::clone(
            &harness.state.provider_payloads,
        ))),
    )
    .unwrap();
    fake.install_toolchain(&workspaces.home());
    let dispatcher = ProviderDriverDispatcher::new(
        registry,
        vault,
        workspaces,
        ExecutableProviderDriverConfig::default(),
    )
    .unwrap();
    ProviderSetup {
        dispatcher,
        target: ProviderTargetRef {
            provider_target_id: target.target_id,
            provider_installation_id: installation.installation_id,
            backend_id: "cloudflare-workers-pages".to_owned(),
            target_config_digest: "b".repeat(64).parse().unwrap(),
        },
        application_id: app.canonical_id,
        environment_id: environment.canonical_id,
        _fixture: fixture,
    }
}

/// Begin a provider deployment over the API and drive it the way the
/// background dispatcher does, until it settles.
async fn deploy(
    harness: &Harness,
    setup: &ProviderSetup,
    artifact: &ArtifactVersionRef,
    key: &str,
) -> OperationState {
    let (status, accepted) = post_with_key(
        harness,
        "/api/apps/shop/environments/production/provider-deployments",
        key,
        serde_json::json!({
            "providerTargetId": setup.target.provider_target_id,
            "artifact": artifact,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let operation_id: CanonicalId = accepted["operationId"].as_str().unwrap().parse().unwrap();
    for _ in 0..40 {
        let outcome = harness
            .state
            .store
            .drive_provider_deployment_once(
                operation_id,
                CanonicalId::new_v7(),
                &setup.dispatcher,
                DriverOrchestratorConfig::default(),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        match outcome {
            ProviderDeploymentDriveOutcome::Succeeded(_)
            | ProviderDeploymentDriveOutcome::Failed(_)
            | ProviderDeploymentDriveOutcome::Cancelled(_) => break,
            ProviderDeploymentDriveOutcome::Waiting(workflow) => {
                let child = workflow.steps[workflow.next_step_index].child_operation_id;
                let record = harness
                    .state
                    .store
                    .provider_driver_operation(child)
                    .await
                    .unwrap();
                panic!(
                    "deployment is waiting on the provider at step {}: {:#?}",
                    workflow.next_step_index,
                    record.map(|record| (record.driver_error, record.contract_failure))
                )
            }
            ProviderDeploymentDriveOutcome::Busy(_)
            | ProviderDeploymentDriveOutcome::Advanced(_) => {}
        }
    }
    // Read back the way `deis deploy` waits for it.
    let (status, settled) = json(
        &harness.router,
        "GET",
        &format!("/api/apps/shop/environments/production/provider-deployments/{operation_id}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{settled}");
    let state: OperationState =
        serde_json::from_value(settled["operation"]["state"].clone()).unwrap();
    assert_eq!(state, OperationState::Succeeded, "{settled:#}");
    state
}

#[tokio::test]
async fn deis_deploys_to_cloudflare_through_the_driver_process_and_deletes_the_worker() {
    let harness = harness(true).await;
    let fake = FakeCloudflare::new();
    let work = tempfile::tempdir().unwrap();
    let artifact = upload_application(&harness).await;
    let setup = register_provider(&harness, &fake, &work.path().join("provider-work")).await;
    let scope = deis_provider_driver_cloudflare::naming::DeploymentScope::new(
        &setup.application_id,
        &setup.environment_id,
    );
    let worker = scope.worker_name();

    // ── first deployment ────────────────────────────────────────────────
    deploy(&harness, &setup, &artifact, "cloudflare-e2e-deploy-1").await;

    // Every resource was created under this application/environment's
    // namespace, never under its logical name.
    let kv = scope.resource_name(BindingKind::Kv, "HOT");
    let d1 = scope.resource_name(BindingKind::Database, "DB");
    let r2 = scope.resource_name(BindingKind::Blob, "FILES");
    let queue = scope.resource_name(BindingKind::Queue, "ORDERS");
    assert!(kv.starts_with(&format!("{worker}-")), "{kv}");
    assert_eq!(fake.names("kv"), vec![kv.clone()]);
    assert_eq!(fake.names("d1"), vec![d1.clone()]);
    assert_eq!(fake.names("r2"), vec![r2.clone()]);
    assert_eq!(fake.names("queues"), vec![queue.clone()]);
    for created in [
        format!("kv namespace create {kv}"),
        format!("d1 create {d1}"),
        format!("r2 bucket create {r2}"),
        format!("queues create {queue}"),
    ] {
        assert_eq!(
            fake.calls(&created).len(),
            1,
            "{created}: {:?}",
            fake.argv()
        );
    }

    // The upload carried a fully resolved, namespaced wrangler.toml.
    let upload = fake.call_index("versions upload", 0);
    let toml = fake.toml_seen_by_call(upload);
    assert!(!toml.contains("deis:unresolved"), "{toml}");
    assert!(toml.contains(&format!("name = \"{worker}\"")), "{toml}");
    assert!(
        toml.contains(&format!("id = \"{}\"", fake.resource_id("kv", &kv))),
        "{toml}"
    );
    assert!(
        toml.contains(&format!(
            "database_id = \"{}\"",
            fake.resource_id("d1", &d1)
        )),
        "{toml}"
    );
    assert!(toml.contains(&format!("bucket_name = \"{r2}\"")), "{toml}");
    assert!(toml.contains(&format!("queue = \"{queue}\"")), "{toml}");
    assert!(toml.contains("crons = [\"*/5 * * * *\"]"), "{toml}");
    // The Worker did not exist, and wrangler uploads versions only for one
    // that does: a placeholder created it first.
    assert_eq!(
        fake.calls("deploy --message deis:placeholder").len(),
        1,
        "{:?}",
        fake.argv()
    );
    assert!(fake.call_index("deploy ", 0) < upload);
    // Activated: the Worker serves the uploaded version, and its cron
    // schedule, queue consumer and workers.dev route were applied after it.
    assert_eq!(fake.calls("versions deploy").len(), 1, "{:?}", fake.argv());
    assert!(fake.account().join("workers").join(&worker).is_file());
    assert!(fake.call_index("triggers deploy", 0) > fake.call_index("versions deploy", 0));
    assert_eq!(
        fs::read_to_string(fake.account().join("schedules").join(&worker)).unwrap(),
        "[\"*/5 * * * *\"]\n"
    );
    assert_eq!(
        fs::read_to_string(fake.account().join("consumers").join(&worker)).unwrap(),
        format!("{queue}\n")
    );
    assert!(fake.account().join("subdomain").join(&worker).is_file());

    // The token reached wrangler only through its environment, never argv,
    // and the process environment held nothing else of the Control's.
    let env_log = fs::read_to_string(fake.account().join("env.log")).unwrap();
    assert!(
        env_log
            .lines()
            .all(|line| line == format!("{TOKEN}|{ACCOUNT}|home")),
        "{env_log}"
    );
    assert!(fake.argv().iter().all(|line| !line.contains(TOKEN)));
    // The operation workspace is gone once each process has exited.
    let operations = work.path().join("provider-work").join("operations");
    assert_eq!(fs::read_dir(&operations).unwrap().count(), 0);

    // ── second deployment: the resources are found, not created again ──
    deploy(&harness, &setup, &artifact, "cloudflare-e2e-deploy-2").await;
    assert_eq!(fake.names("kv"), vec![kv.clone()]);
    assert_eq!(fake.names("d1"), vec![d1.clone()]);
    assert_eq!(fake.names("r2"), vec![r2.clone()]);
    assert_eq!(fake.names("queues"), vec![queue.clone()]);
    for kind in [
        "kv namespace create",
        "d1 create",
        "r2 bucket create",
        "queues create",
    ] {
        assert_eq!(fake.calls(kind).len(), 1, "{kind}: {:?}", fake.argv());
    }
    assert_eq!(fake.calls("versions upload").len(), 2);
    assert_eq!(fake.calls("versions deploy").len(), 2);
    assert_eq!(fake.calls("triggers deploy").len(), 2);
    assert_eq!(fake.calls("deploy ").len(), 1, "the Worker is created once");
    let second = fake.toml_seen_by_call(fake.call_index("versions upload", 1));
    assert_eq!(second, toml, "the re-deploy binds the same resources");

    // ── delete: the Worker goes, the data stays without purge consent ──
    let delete = DriverRequest {
        schema_version: 1,
        operation_id: CanonicalId::new_v7(),
        operation: DriverOperation::Delete,
        desired_state_digest: "c".repeat(64).parse().unwrap(),
        application_id: setup.application_id,
        environment_id: setup.environment_id,
        target: setup.target.clone(),
        artifact: None,
        plan: ProviderNeutralPlan::default(),
        rollback_target: None,
        reconciles: None,
    };
    let operation_id = delete.operation_id;
    harness
        .state
        .store
        .begin_provider_driver_operation(
            delete,
            operation_context("cloudflare-e2e-delete", "POST\n/provider-operations\nv1"),
        )
        .await
        .unwrap();
    let driver = setup
        .dispatcher
        .driver_for_target(setup.target.provider_target_id)
        .await
        .unwrap();
    let outcome = DriverOrchestrator::new(
        harness.state.store.as_ref().clone(),
        DriverOrchestratorConfig::default(),
    )
    .unwrap()
    .execute(operation_id, CanonicalId::new_v7(), &driver)
    .await
    .unwrap();
    assert!(
        matches!(outcome, DriverOrchestrationOutcome::Succeeded(_)),
        "{outcome:?}"
    );
    assert_eq!(
        fake.calls(&format!("delete {worker}")),
        vec![format!("delete {worker} --force")]
    );
    assert!(!fake.account().join("workers").join(&worker).exists());
    assert!(!fake.account().join("schedules").join(&worker).exists());
    assert_eq!(fake.names("kv"), vec![kv]);
    assert_eq!(fake.names("d1"), vec![d1]);
    assert_eq!(fake.names("r2"), vec![r2]);
    assert_eq!(fake.names("queues"), vec![queue]);
}

#[test]
fn the_control_outlasts_the_cloudflare_driver_and_its_lease_outlasts_the_control() {
    let config = crate::catalog::provider_driver_release::driver_process_config()
        .validate()
        .expect("the production launcher bounds are valid");
    // The driver reports its own timeout before the launcher kills it.
    assert!(deis_provider_driver_cloudflare::protocol::PROCESS_TIMEOUT < config.process_timeout);
    // The vault grants the whole lease the launcher asks for, and the
    // launcher needs it to outlast the process by at least a second.
    assert!(
        config.credential_lease_duration
            <= crate::catalog::provider_credential::MAX_CREDENTIAL_LEASE
    );
    assert!(config.process_timeout + Duration::from_secs(1) < config.credential_lease_duration);
}
