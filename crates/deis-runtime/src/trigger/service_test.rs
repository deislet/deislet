//! What a node answers when a scheduled tick does not succeed.
//!
//! The caller walks the fleet on a failure, so these two answers must not look
//! alike: one is worth another node, the other would just repeat the
//! application's side effects on every node it has.
//!
//! The guard from `test_host::take` is deliberately held across the awaits in
//! each test: it exists to keep two tests from using the shared isolate host
//! at once, which is exactly the span that has to be protected.
#![allow(clippy::await_holding_lock)]

use std::sync::Arc;

use tonic::Request;

use super::{TriggerServiceImpl, HANDLER_FAILED_PREFIX};
use crate::app::{test_host, AppConfig, AppKey, ApplicationManager};
use crate::trigger::proto::trigger_server::Trigger;
use crate::trigger::proto::ScheduledEvent;

/// Load one application on the shared isolate host and hand back a trigger
/// service in front of it.
async fn service_running(
    app_id: &str,
    code: &str,
) -> (Arc<ApplicationManager>, TriggerServiceImpl) {
    let apps = test_host::host();
    apps.load_app(
        AppKey::production(app_id),
        code.to_string(),
        AppConfig::default(),
        None,
    )
    .await
    .expect("the test app loads");
    let service = TriggerServiceImpl::new(apps.clone());
    (apps, service)
}

fn tick(app_id: &str) -> Request<ScheduledEvent> {
    tick_for(app_id, "production")
}

/// A tick for one named environment of an application, the way the control
/// plane sends it now that it keys schedules by (app, environment).
fn tick_for(app_id: &str, environment: &str) -> Request<ScheduledEvent> {
    Request::new(ScheduledEvent {
        app_id: app_id.to_string(),
        scheduled_time: 1_700_000_000_000,
        cron: "* * * * *".to_string(),
        environment: environment.to_string(),
    })
}

/// A tick that names no environment.
fn tick_without_an_environment(app_id: &str) -> Request<ScheduledEvent> {
    tick_for(app_id, "")
}

#[tokio::test]
async fn a_handler_that_throws_is_not_offered_to_another_node() {
    let _turn = test_host::take();
    // The handler sends a message and then throws. Retrying elsewhere would
    // send that message once per node in the fleet, which is why the answer
    // has to say the code already ran here.
    let code = r#"
        export default function handler() { return new Response("unused"); }
        export async function scheduled() { throw new Error("nightly job broke"); }
    "#;
    let (apps, service) = service_running("cron-thrower", code).await;

    let response = service
        .on_scheduled(tick("cron-thrower"))
        .await
        .expect("the RPC itself succeeds")
        .into_inner();

    assert!(
        !response.success,
        "a throwing handler is not a successful tick"
    );
    assert!(
        response.error.starts_with(HANDLER_FAILED_PREFIX),
        "the caller cannot tell this from a node that could not run it: {}",
        response.error
    );
    assert!(
        response.error.contains("nightly job broke"),
        "{}",
        response.error
    );

    let _ = apps.unload_app(&AppKey::production("cron-thrower")).await;
}

#[tokio::test]
async fn a_bundle_with_no_scheduled_handler_is_the_app_s_own_failure() {
    let _turn = test_host::take();
    // Otherwise every occurrence walks the whole fleet, once a minute, to
    // discover the same missing export on every node.
    let code = r#"
        export default function handler() { return new Response("unused"); }
    "#;
    let (apps, service) = service_running("cron-missing", code).await;

    let response = service
        .on_scheduled(tick("cron-missing"))
        .await
        .expect("the RPC itself succeeds")
        .into_inner();

    assert!(!response.success);
    assert!(
        response.error.starts_with(HANDLER_FAILED_PREFIX),
        "{}",
        response.error
    );

    let _ = apps.unload_app(&AppKey::production("cron-missing")).await;
}

#[tokio::test]
async fn an_app_this_node_does_not_run_is_worth_another_node() {
    let _turn = test_host::take();
    let service = TriggerServiceImpl::new(test_host::host());

    let response = service
        .on_scheduled(tick("not-here"))
        .await
        .expect("the RPC itself succeeds")
        .into_inner();

    assert!(!response.success);
    assert!(
        !response.error.starts_with(HANDLER_FAILED_PREFIX),
        "this node never entered the app's code, so the tick can still run \
         somewhere else: {}",
        response.error
    );
}

#[tokio::test]
async fn a_handler_that_succeeds_ends_the_search() {
    let _turn = test_host::take();
    let code = r#"
        export default function handler() { return new Response("unused"); }
        export async function scheduled() { return; }
    "#;
    let (apps, service) = service_running("cron-ok", code).await;

    let response = service
        .on_scheduled(tick("cron-ok"))
        .await
        .expect("the RPC itself succeeds")
        .into_inner();

    assert!(response.success, "{}", response.error);
    assert!(response.error.is_empty());

    let _ = apps.unload_app(&AppKey::production("cron-ok")).await;
}

#[tokio::test]
async fn a_preview_tick_runs_the_preview_handler_and_not_productions() {
    let _turn = test_host::take();
    // Both environments of one application are loaded here, which is the
    // normal state of a node. Before ScheduledEvent carried an environment,
    // every tick landed on production: a preview's schedule ran production's
    // code with production's secrets, and nothing said so.
    let apps = test_host::host();
    let production = AppKey::new("cron-two-envs", "production");
    let preview = AppKey::new("cron-two-envs", "preview-7");
    apps.load_app(
        production.clone(),
        r#"
            export default function handler() { return new Response("unused"); }
            export async function scheduled() { return; }
        "#
        .to_string(),
        AppConfig::default(),
        None,
    )
    .await
    .expect("production loads");
    apps.load_app(
        preview.clone(),
        r#"
            export default function handler() { return new Response("unused"); }
            export async function scheduled() { throw new Error("this is the preview"); }
        "#
        .to_string(),
        AppConfig::default(),
        None,
    )
    .await
    .expect("the preview loads");

    let service = TriggerServiceImpl::new(apps.clone());

    let response = service
        .on_scheduled(tick_for("cron-two-envs", "preview-7"))
        .await
        .expect("the RPC itself succeeds")
        .into_inner();
    assert!(!response.success);
    assert!(
        response.error.contains("this is the preview"),
        "the preview's tick ran some other environment's handler: {}",
        response.error
    );

    let response = service
        .on_scheduled(tick_for("cron-two-envs", "production"))
        .await
        .expect("the RPC itself succeeds")
        .into_inner();
    assert!(response.success, "{}", response.error);

    let _ = apps.unload_app(&preview).await;
    let _ = apps.unload_app(&production).await;
}

#[tokio::test]
async fn a_tick_with_no_environment_is_refused() {
    // A schedule belongs to one environment; a tick that names none cannot
    // be run without guessing which copy of the application it is for.
    let _turn = test_host::take();
    let code = r#"
        export default function handler() { return new Response("unused"); }
        export async function scheduled() { return; }
    "#;
    let (apps, service) = service_running("cron-no-env", code).await;

    let status = service
        .on_scheduled(tick_without_an_environment("cron-no-env"))
        .await
        .expect_err("a tick naming no environment is refused");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    let _ = apps.unload_app(&AppKey::production("cron-no-env")).await;
}
