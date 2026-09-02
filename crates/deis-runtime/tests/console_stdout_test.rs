//! What an application's `console.log` can do to this process's stdout.
//!
//! `console.*` in an isolate is bridged twice: to `op_log`, which tags the
//! line and hands it to `tracing` (escaped at the producer, and again by the
//! `LineSafe` writer every service installs), and to deno's own console, which
//! writes to this process's stdout with nothing in between. In a deployment
//! that stdout *is* the node's log file - the demo tees it there and fifteen
//! of its checks read it by line - so a newline in that second copy wrote a
//! whole forged line into the file while the first copy of the very same call
//! sat safely escaped beside it. `js/runtime.js` now escapes the native copy
//! with the same rule.
//!
//! The copy under test never passes through Rust, so no in-process capture can
//! see it: this test runs itself again as a child process, serves one forging
//! request there, and reads the child's real stdout and stderr.

use std::process::{Command, Stdio};
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use axum::routing::any;
use axum::Router;
use deis_runtime::app::{AppKey, ApplicationManager};
use deis_runtime::config::default_max_request_body_bytes;
use deis_runtime::dispatcher::{handlers, AppState};
use tower::ServiceExt;

/// The line an application would like to see standing on its own.
const FORGED_STAMP: &str = "2026-09-03T00:00:00Z  INFO forged app_id=other";

/// Set on the child so the same test function knows which half it is.
const CHILD_FLAG: &str = "DEIS_CONSOLE_STDOUT_TEST_CHILD";

const TEST_NAME: &str = "an_applications_console_line_cannot_start_a_second_line_on_stdout";

#[tokio::test]
async fn an_applications_console_line_cannot_start_a_second_line_on_stdout() {
    if std::env::var_os(CHILD_FLAG).is_some() {
        serve_one_forging_request().await;
        return;
    }

    let child = Command::new(std::env::current_exe().expect("this test binary has a path"))
        .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
        .env(CHILD_FLAG, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("the child test process runs");

    let stdout = String::from_utf8_lossy(&child.stdout);
    let stderr = String::from_utf8_lossy(&child.stderr);
    assert!(
        child.status.success(),
        "the child did not serve its request cleanly\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    let everything = format!("{stdout}\n{stderr}");

    // The native copy is still made - the message reached the process's
    // output, not just `tracing`.
    assert!(
        everything.contains("secret-in-message: hunter2"),
        "the native console copy was not written at all:\n{everything}"
    );

    // No line of the child's output starts where the application wanted a
    // line to start.
    let forged: Vec<&str> = everything
        .lines()
        .filter(|line| line.trim_start().starts_with("2026-09-03T00:00:00Z"))
        .collect();
    assert!(
        forged.is_empty(),
        "an application's newline started a line of its own on stdout: {forged:?}\n{everything}"
    );

    // And it is the escaped spelling that got there, on the same line as the
    // rest of the message.
    assert!(
        everything.contains(&format!("hunter2\\n{FORGED_STAMP}")),
        "the newline was not spelled out on the line it belongs to:\n{everything}"
    );
}

/// The child's half: one application, one request, one `console.log` that
/// carries a newline and a line composed to look like the node's own.
async fn serve_one_forging_request() {
    let code = format!(
        r#"
        export default function handler() {{
            console.log("secret-in-message: hunter2\n{FORGED_STAMP}");
            return new Response("ok");
        }}
        "#
    );
    let manager = Arc::new(ApplicationManager::new());
    manager
        .load_app(AppKey::production("forger"), code, Default::default(), None)
        .await
        .expect("the app loads");
    let router = Router::new()
        .fallback(any(handlers::handle_request))
        .with_state(AppState::new(manager, default_max_request_body_bytes()));

    let response = router
        .oneshot(
            Request::builder()
                .uri("/")
                .header("x-deis-app-id", "forger")
                .body(Body::empty())
                .expect("the request is buildable"),
        )
        .await
        .expect("the node answers");
    assert_eq!(
        response.status(),
        200,
        "the forging handler itself answers 200"
    );
}
