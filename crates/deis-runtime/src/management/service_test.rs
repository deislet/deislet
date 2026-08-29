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
use crate::deployment::DeploymentManager;
use crate::health::NodeHealth;
use crate::management::proto::runtime_management_server::RuntimeManagement;
use crate::management::proto::{
    ConfigType, ExecuteCommandRequest, GetStatusRequest, PushConfigRequest, StartInspectRequest,
    UnloadAppRequest, UpdateAppRequest,
};
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
async fn an_update_app_call_is_refused_rather_than_half_applied() {
    // The RPC used to take a whole zip in one message and parse it with a
    // second, incompatible manifest definition of its own. The archive field
    // is now `reserved` in management.proto - hence the absence of a field
    // for it here, which is the point of the test as much as the refusal is:
    // if `package_data` ever comes back, this stops compiling. A caller that
    // still asks has to see that nothing was deployed.
    let _turn = test_host::take();
    let dir = tempfile::tempdir().unwrap();
    let environments = ["production"];
    let (apps, service) = service_with(dir.path(), "status-upload", &environments).await;

    let status = service
        .update_app(Request::new(UpdateAppRequest {
            app_id: "status-upload".to_string(),
            config: None,
        }))
        .await
        .expect_err("an install path that no longer exists must not answer success");

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

#[tokio::test]
async fn an_inspect_session_attaches_to_the_environment_the_request_names() {
    // A node runs every environment of an application at once, so attaching a
    // debugger to "the app" is ambiguous. The RPC carries the environment;
    // pinning it to production here would open a session against the wrong
    // isolate - and report success while doing it.
    let _turn = test_host::take();
    let dir = tempfile::tempdir().unwrap();
    let environments = ["production", "preview-3"];
    let (apps, service) = service_with(dir.path(), "status-inspect", &environments).await;

    let response = service
        .start_inspect(Request::new(StartInspectRequest {
            app_id: "status-inspect".to_string(),
            ttl_seconds: 60,
            environment: "preview-3".to_string(),
        }))
        .await
        .unwrap()
        .into_inner();

    let preview = AppKey::new("status-inspect", "preview-3");
    assert!(
        apps.validate_inspect_token(&preview, &response.token),
        "the token belongs to the environment that was asked for"
    );
    assert!(
        !apps.validate_inspect_token(&AppKey::production("status-inspect"), &response.token),
        "production must not have been opened instead"
    );

    // An empty environment still means the default one, so callers written
    // before the field existed keep working.
    let response = service
        .start_inspect(Request::new(StartInspectRequest {
            app_id: "status-inspect".to_string(),
            ttl_seconds: 60,
            environment: String::new(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(apps.validate_inspect_token(&AppKey::production("status-inspect"), &response.token));

    // An environment this node does not run is refused, not silently
    // redirected to one it does.
    let status = service
        .start_inspect(Request::new(StartInspectRequest {
            app_id: "status-inspect".to_string(),
            ttl_seconds: 60,
            environment: "no-such-environment".to_string(),
        }))
        .await
        .expect_err("an environment that is not loaded has nothing to inspect");
    assert_eq!(status.code(), tonic::Code::Internal);

    unload_all(&apps, "status-inspect", &environments).await;
}

#[tokio::test]
async fn a_config_push_is_refused_rather_than_written_somewhere_nobody_reads() {
    // The previous implementation filed the payload in a local table that no
    // part of this node ever read back, then answered "Config applied
    // successfully". A caller had no way to tell that from a configuration
    // that had actually taken effect.
    let _turn = test_host::take();
    let dir = tempfile::tempdir().unwrap();
    let environments = ["production"];
    let (apps, service) = service_with(dir.path(), "status-config", &environments).await;

    let status = service
        .push_config(Request::new(PushConfigRequest {
            version: 7,
            config_json: "{\"anything\":true}".to_string(),
            config_type: ConfigType::Global as i32,
            force: true,
        }))
        .await
        .expect_err("nothing applies a pushed config, so it must not report success");
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    assert!(status.message().contains("PushConfig"), "{status}");

    unload_all(&apps, "status-config", &environments).await;
}

#[tokio::test]
async fn a_command_is_refused_rather_than_reported_as_a_failure() {
    // `success: false` reads as "the command ran and lost", which invites a
    // retry of something that was never going to run. There is no command
    // surface on this node at all.
    let _turn = test_host::take();
    let dir = tempfile::tempdir().unwrap();
    let environments = ["production"];
    let (apps, service) = service_with(dir.path(), "status-command", &environments).await;

    let status = service
        .execute_command(Request::new(ExecuteCommandRequest {
            command: "restart".to_string(),
            args: Vec::new(),
            options: std::collections::HashMap::new(),
        }))
        .await
        .expect_err("a node that runs no commands must say so");
    assert_eq!(status.code(), tonic::Code::Unimplemented);
    assert!(status.message().contains("ExecuteCommand"), "{status}");

    unload_all(&apps, "status-command", &environments).await;
}
