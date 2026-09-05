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
    ApplyDeploymentRequest, CompatibilityDeclaration, ConfigType, DeploymentAction,
    ExecuteCommandRequest, GetStatusRequest, PushConfigRequest, StartInspectRequest,
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

/// The wire carries the manifest's words, and this node answers them with its
/// own table.
///
/// The case that makes the distinction matter: a version naming a date *and* a
/// flag that holds one of that date's behaviours back. Any list of "what is
/// on" has already lost the flag, so a node that trusted such a list would
/// apply the date and turn the behaviour on again - handing the application
/// the exact answer its author wrote a line to avoid.
#[test]
fn a_flag_that_holds_a_behaviour_back_is_honoured_after_the_wire() {
    let profile = super::decode_compatibility(CompatibilityDeclaration {
        date: "2026-09-01".to_string(),
        flags: vec!["single_set_cookie_header".to_string()],
    })
    .expect("both the date and the flag are ones this build knows");

    assert_eq!(
        profile.date().map(|d| d.to_string()).as_deref(),
        Some("2026-09-01")
    );
    assert!(
        !profile.is_on(deis_bundle::Flag::MultipleSetCookieHeaders),
        "the date alone would have turned this on; the flag is what holds it back"
    );
}

/// An empty date is proto3's absent string, and here the two mean the same
/// thing: the application named no day and takes the oldest behaviour.
#[test]
fn an_empty_declaration_on_the_wire_is_the_oldest_behaviour() {
    let profile = super::decode_compatibility(CompatibilityDeclaration {
        date: String::new(),
        flags: Vec::new(),
    })
    .expect("declaring nothing is not an error");

    assert_eq!(profile.date(), None);
    assert!(!profile.is_on(deis_bundle::Flag::MultipleSetCookieHeaders));
}

#[tokio::test]
async fn deployment_phase_identity_is_validated_before_payload_work() {
    let _turn = test_host::take();
    let dir = tempfile::tempdir().unwrap();
    let (_apps, service) = service_with(dir.path(), "phase-id", &[]).await;

    for (deployment_id, routing_generation, action, expected) in [
        (
            "not-a-canonical-id",
            "018f2518-7e38-7f80-8000-000000000012",
            DeploymentAction::Prepare,
            "lowercase UUIDv7",
        ),
        (
            "018f2518-7e38-7f80-8000-000000000011",
            "",
            DeploymentAction::Prepare,
            "present together",
        ),
        ("", "", DeploymentAction::Activate, "requires deployment_id"),
    ] {
        let response = service
            .apply_deployment(Request::new(ApplyDeploymentRequest {
                deployment_id: deployment_id.to_string(),
                routing_generation: routing_generation.to_string(),
                action: action as i32,
                // This URL must never be reached: phase identity is an
                // admission boundary before canonical environment decoding
                // or bundle I/O.
                bundle_url: "http://127.0.0.1:1/not-fetched".to_string(),
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(!response.success);
        assert!(response.message.contains(expected), "{}", response.message);
        assert_eq!(response.observed_state, "rejected");
    }

    let status = service
        .apply_deployment(Request::new(ApplyDeploymentRequest {
            action: i32::MAX,
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
}

/// The mixed-fleet refusal, at the door.
///
/// A control plane built after this node accepts a date this node has never
/// heard of and pushes it here. There is no honest way to run it: every
/// behaviour this build could give the application is one the application did
/// not ask for. So the push is refused before a byte of the bundle is
/// downloaded, the response names the version this environment keeps serving,
/// and the control plane's delivery report shows this node as failed - which
/// is what tells an operator to upgrade it.
#[tokio::test]
async fn a_deployment_naming_a_date_this_node_cannot_honour_is_refused_before_anything_is_fetched()
{
    let dir = tempfile::tempdir().unwrap();
    let guard = test_host::take();
    let (apps, service) = service_with(dir.path(), "compat-app", &["production"]).await;

    let response = service
        .apply_deployment(Request::new(ApplyDeploymentRequest {
            app_id: "compat-app".to_string(),
            version_id: "v9".to_string(),
            environment: "production".to_string(),
            // Unreachable on purpose: nothing may try to fetch it.
            bundle_url: "http://127.0.0.1:1/bundle".to_string(),
            bundle_sha: "0".repeat(64),
            env: Default::default(),
            limits: None,
            compatibility: Some(CompatibilityDeclaration {
                date: "2099-01-01".to_string(),
                flags: Vec::new(),
            }),
            canonical_application_id: String::new(),
            canonical_environment_id: String::new(),
            environment_declarations_json: String::new(),
            environment_snapshot_json: String::new(),
            environment_snapshot_id: String::new(),
            secret_values_by_revision: Default::default(),
            deployment_id: String::new(),
            routing_generation: String::new(),
            action: DeploymentAction::LegacyApply as i32,
        }))
        .await
        .expect("the call itself succeeds; the answer is the refusal")
        .into_inner();

    assert!(!response.success, "{}", response.message);
    assert!(
        response.message.contains("2099-01-01"),
        "{}",
        response.message
    );
    assert!(
        response
            .message
            .contains(&deis_bundle::PLATFORM_KNOWN_THROUGH.to_string()),
        "the refusal has to name the date this build does know: {}",
        response.message
    );

    unload_all(&apps, "compat-app", &["production"]).await;
    drop(guard);
}

/// A flag name this build does not know is **not** the same refusal, and the
/// two sit next to each other because the difference is the whole rule
/// (`deis_bundle::forward`). A date asks for every answer as of a day this
/// node has never seen and there is no honest subset of that to serve. A flag
/// names one behaviour this build does not have in either direction, so
/// ignoring it gives exactly what this node was already giving - while
/// refusing it made every version naming a flag added since undeployable here,
/// which is what a rollback runs into.
#[tokio::test]
async fn a_deployment_naming_a_flag_this_node_does_not_know_is_not_refused_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let guard = test_host::take();
    let (apps, service) = service_with(dir.path(), "flag-app", &["production"]).await;

    let response = service
        .apply_deployment(Request::new(ApplyDeploymentRequest {
            app_id: "flag-app".to_string(),
            version_id: "v9".to_string(),
            environment: "production".to_string(),
            bundle_url: "http://127.0.0.1:1/bundle".to_string(),
            bundle_sha: "0".repeat(64),
            env: Default::default(),
            limits: None,
            compatibility: Some(CompatibilityDeclaration {
                date: String::new(),
                flags: vec!["behaviour_from_a_later_build".to_string()],
            }),
            canonical_application_id: String::new(),
            canonical_environment_id: String::new(),
            environment_declarations_json: String::new(),
            environment_snapshot_json: String::new(),
            environment_snapshot_id: String::new(),
            secret_values_by_revision: Default::default(),
            deployment_id: String::new(),
            routing_generation: String::new(),
            action: DeploymentAction::LegacyApply as i32,
        }))
        .await
        .expect("the call itself succeeds; the answer is the refusal")
        .into_inner();

    // The deployment gets past the compatibility gate and fails where this
    // fixture makes it fail - the bundle URL points at a closed port. What
    // matters is that the flag is not what stopped it.
    assert!(!response.success, "{}", response.message);
    assert!(
        !response.message.contains("behaviour_from_a_later_build"),
        "a flag name from a newer build must not be what refuses a deployment: {}",
        response.message
    );
    assert!(
        response.message.contains("bundle"),
        "and the refusal that remains has to be the real one: {}",
        response.message
    );

    unload_all(&apps, "flag-app", &["production"]).await;
    drop(guard);
}

#[tokio::test]
async fn a_partial_canonical_identity_is_refused_before_the_bundle_is_fetched() {
    let dir = tempfile::tempdir().unwrap();
    let guard = test_host::take();
    let (apps, service) = service_with(dir.path(), "identity-app", &["production"]).await;

    let response = service
        .apply_deployment(Request::new(ApplyDeploymentRequest {
            app_id: "identity-app".to_string(),
            version_id: "v9".to_string(),
            environment: "production".to_string(),
            // Unreachable on purpose. Identity validation must finish first.
            bundle_url: "http://127.0.0.1:1/bundle".to_string(),
            bundle_sha: "0".repeat(64),
            env: Default::default(),
            limits: None,
            compatibility: None,
            canonical_application_id: "018f2518-7e38-7f80-8000-000000000001".to_string(),
            canonical_environment_id: String::new(),
            environment_declarations_json: String::new(),
            environment_snapshot_json: String::new(),
            environment_snapshot_id: String::new(),
            secret_values_by_revision: Default::default(),
            deployment_id: String::new(),
            routing_generation: String::new(),
            action: DeploymentAction::LegacyApply as i32,
        }))
        .await
        .unwrap()
        .into_inner();

    assert!(!response.success);
    assert!(
        response.message.contains("without an environment identity"),
        "{}",
        response.message
    );
    assert!(
        response.message.contains("canonical application identity"),
        "{}",
        response.message
    );

    unload_all(&apps, "identity-app", &["production"]).await;
    drop(guard);
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

/// The wire's zero sentinel, read back into the node's `Option`s.
///
/// The gap this closed was one step earlier: `ApplyDeploymentRequest` had no
/// limits field at all and this crate never mentioned the `ResourceLimits`
/// message, so no application could narrow anything.
mod decoding_the_limits_a_deployment_carries {
    use super::super::decode_limits;
    use crate::management::proto::ResourceLimits as Wire;

    #[test]
    fn a_message_of_zeroes_is_an_application_that_asked_for_nothing() {
        let decoded = decode_limits(Wire::default());
        assert_eq!(
            decoded.memory_mb, 0,
            "0MB is the unset sentinel, not a heap"
        );
        assert_eq!(decoded.timeout_ms, None);
        assert_eq!(decoded.cpu_time_limit_ms, None);
        assert_eq!(decoded.rate_limit_ops_per_sec, None);
        assert_eq!(decoded.resource_table_max, None);
    }

    #[test]
    fn every_field_an_application_can_narrow_survives_the_wire() {
        let decoded = decode_limits(Wire {
            memory_mb: 48,
            cpu_milli: 500,
            max_concurrency: 0,
            resource_table_max: 16,
            timeout_ms: 2_000,
            rate_limit_ops_per_sec: 40.0,
            cpu_time_limit_ms: 250,
        });
        assert_eq!(decoded.memory_mb, 48);
        assert_eq!(decoded.timeout_ms, Some(2_000));
        assert_eq!(decoded.rate_limit_ops_per_sec, Some(40.0));
        assert_eq!(decoded.cpu_time_limit_ms, Some(250));
        assert_eq!(decoded.resource_table_max, Some(16));
        assert_eq!(decoded.cpu_milli, 500, "carried, though nothing applies it");
    }

    /// proto3 signs its integers, so a negative value is representable even
    /// though nothing should ever send one. Read as unset rather than cast
    /// into an enormous unsigned ceiling.
    #[test]
    fn a_negative_value_cannot_become_an_enormous_ceiling() {
        let decoded = decode_limits(Wire {
            memory_mb: -1,
            timeout_ms: -1,
            cpu_time_limit_ms: -1,
            resource_table_max: -1,
            rate_limit_ops_per_sec: -1.0,
            ..Wire::default()
        });
        assert_eq!(decoded.memory_mb, 0);
        assert_eq!(decoded.timeout_ms, None);
        assert_eq!(decoded.cpu_time_limit_ms, None);
        assert_eq!(decoded.resource_table_max, None);
        assert_eq!(decoded.rate_limit_ops_per_sec, None);
    }
}
