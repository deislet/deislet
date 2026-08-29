//! What `GetStatus` tells the control plane about this node.
//!
//! The counters are the point: they used to be hard-coded zeros next to a TODO
//! while the isolates were already recording the real numbers, so a dashboard
//! read "no traffic" for an application that was serving.
//!
//! The guard from `test_host::take` is deliberately held across the awaits in
//! each test: it exists to keep two tests from using the shared isolate host at
//! once, which is exactly the span that has to be protected.
#![allow(clippy::await_holding_lock)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use tonic::Request;

use super::RuntimeManagementService;
use crate::app::{test_host, AppConfig, AppKey, ApplicationManager};
use crate::config::RuntimeConfig;
use crate::config_store::ConfigStore;
use crate::deployment::DeploymentManager;
use crate::health::NodeHealth;
use crate::management::proto::runtime_management_server::RuntimeManagement;
use crate::management::proto::{GetStatusRequest, UnloadAppRequest, UpdateAppRequest};
use crate::runtime::metrics::{recorder, RequestOutcome};

const HANDLER: &str = r#"
    export default function handler() { return new Response("ok"); }
"#;

/// A management service in front of the shared isolate host, with `app_id`
/// loaded in the environments named and a process start time far enough back
/// that an uptime of zero would be a bug.
async fn service_with(
    dir: &std::path::Path,
    app_id: &str,
    environments: &[&str],
) -> (Arc<ApplicationManager>, RuntimeManagementService) {
    let db = dir.join("node.db");
    let db = db.to_string_lossy().to_string();

    let apps = test_host::host();
    for environment in environments {
        apps.load_app(
            AppKey::new(app_id, *environment),
            HANDLER.to_string(),
            AppConfig::default(),
            None,
        )
        .await
        .expect("the test app loads");
    }

    let mut config = RuntimeConfig {
        data_dir: dir.to_path_buf(),
        ..Default::default()
    };
    config.bundles.dir = Some(dir.join("bundles"));

    let service = RuntimeManagementService::new(
        apps.clone(),
        Arc::new(ConfigStore::new(&db).await.unwrap()),
        Arc::new(
            DeploymentManager::from_config(apps.clone(), &config)
                .await
                .unwrap(),
        ),
        NodeHealth::starting(),
        "node-under-test".to_string(),
        Instant::now()
            .checked_sub(Duration::from_secs(42))
            .expect("the clock supports a 42 second offset"),
    );
    (apps, service)
}

/// Leave the shared host as this test found it.
async fn unload_all(apps: &ApplicationManager, app_id: &str, environments: &[&str]) {
    for environment in environments {
        let _ = apps.unload_app(&AppKey::new(app_id, *environment)).await;
    }
}

#[tokio::test]
async fn status_reports_the_counters_the_isolates_recorded() {
    let _turn = test_host::take();
    let dir = tempfile::tempdir().unwrap();
    let environments = ["production", "preview-2"];
    let (apps, service) = service_with(dir.path(), "status-counted", &environments).await;

    // What an isolate records as it serves. Production takes traffic, the
    // preview takes none.
    let counters = recorder();
    let production = AppKey::production("status-counted").tenant_id();
    for outcome in [
        RequestOutcome::Ok,
        RequestOutcome::Ok,
        RequestOutcome::Error,
    ] {
        counters.record_request(&production, Duration::from_millis(3), outcome);
    }

    let response = service
        .get_status(Request::new(GetStatusRequest { include_apps: true }))
        .await
        .unwrap()
        .into_inner();

    let node = response.node_info.expect("a node always names itself");
    assert_eq!(node.node_id, "node-under-test");
    assert!(
        node.uptime_seconds >= 42,
        "uptime is measured from process start, not invented: {}",
        node.uptime_seconds
    );

    let live = response
        .apps
        .iter()
        .find(|app| app.app_id == "status-counted" && app.environment == "production")
        .expect("the production copy is missing from the status answer");
    assert_eq!(live.request_count, 3);
    assert_eq!(live.error_count, 1);

    // Two environments of one app are two entries with their own counters; a
    // preview's traffic is not added to production's.
    let preview = response
        .apps
        .iter()
        .find(|app| app.app_id == "status-counted" && app.environment == "preview-2")
        .expect("the preview copy is missing from the status answer");
    assert_eq!(preview.request_count, 0);
    assert_eq!(preview.error_count, 0);

    unload_all(&apps, "status-counted", &environments).await;
}

#[tokio::test]
async fn unmeasured_resource_usage_is_left_out_rather_than_reported_as_zero() {
    let _turn = test_host::take();
    let dir = tempfile::tempdir().unwrap();
    let environments = ["production"];
    let (apps, service) = service_with(dir.path(), "status-quiet", &environments).await;

    let response = service
        .get_status(Request::new(GetStatusRequest {
            include_apps: false,
        }))
        .await
        .unwrap()
        .into_inner();

    assert!(
        response.resource_usage.is_none(),
        "a zeroed ResourceUsage reads as an idle node rather than an unmeasured one"
    );

    unload_all(&apps, "status-quiet", &environments).await;
}

#[tokio::test]
async fn a_package_upload_is_refused_rather_than_half_applied() {
    // The RPC used to parse the zip with a second, incompatible manifest
    // definition of its own. A caller that still sends a package has to see
    // that nothing was deployed.
    let _turn = test_host::take();
    let dir = tempfile::tempdir().unwrap();
    let environments = ["production"];
    let (apps, service) = service_with(dir.path(), "status-upload", &environments).await;

    let status = service
        .update_app(Request::new(UpdateAppRequest {
            app_id: "status-upload".to_string(),
            package_data: b"PK\x03\x04 not really a bundle".to_vec(),
            config: None,
        }))
        .await
        .expect_err("an upload path that no longer exists must not answer success");

    assert_eq!(status.code(), tonic::Code::Unimplemented);
    assert!(status.message().contains("ApplyDeployment"), "{status}");

    unload_all(&apps, "status-upload", &environments).await;
}

#[tokio::test]
async fn unloading_names_the_environments_that_stopped() {
    let _turn = test_host::take();
    let dir = tempfile::tempdir().unwrap();
    let environments = ["production", "preview-2"];
    let (apps, service) = service_with(dir.path(), "status-unload", &environments).await;

    let response = service
        .unload_app(Request::new(UnloadAppRequest {
            app_id: "status-unload".to_string(),
            environment: "preview-2".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();

    assert!(response.success, "{}", response.message);
    assert_eq!(response.environments, vec!["preview-2".to_string()]);
    assert!(apps.is_loaded(&AppKey::production("status-unload")));
    assert!(!apps.is_loaded(&AppKey::new("status-unload", "preview-2")));

    // An app this node is not running is not a failure: there is nothing left
    // to serve, which is what the caller asked for.
    let response = service
        .unload_app(Request::new(UnloadAppRequest {
            app_id: "never-here".to_string(),
            environment: String::new(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(response.success, "{}", response.message);
    assert!(response.environments.is_empty());

    unload_all(&apps, "status-unload", &environments).await;
}
