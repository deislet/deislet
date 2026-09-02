//! What an unauthenticated client is told when an invocation fails, and which
//! methods never reach an isolate at all.
//!
//! Both halves were one defect: the HTTP entry let a request through that no
//! handler could be built for, the runtime threw building it, and the invoke
//! wrapper put the resulting stack in the response body. `curl -X TRACE` at
//! any application on this node therefore answered 500 with deno_fetch's
//! internals; a handler that threw answered 500 with the tenant's bundle
//! layout, this runtime's line numbers and whatever the exception message
//! carried.
//!
//! These drive the real dispatcher against a real isolate, and read the node's
//! own log through a global subscriber, because "the stack is not in the body"
//! is only half the claim: the other half is that the author can still find it.

use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::any;
use axum::Router;
use deis_runtime::app::types::ResourceLimits;
use deis_runtime::app::{AppConfig, AppKey, ApplicationManager};
use deis_runtime::config::default_max_request_body_bytes;
use deis_runtime::dispatcher::{handlers, AppState};
use tower::ServiceExt;

/// The node's log, as bytes, for the one process these tests share.
#[derive(Clone)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = Sink;

    fn make_writer(&'a self) -> Sink {
        self.clone()
    }
}

/// Install the capture once for this test binary and hand back the buffer.
///
/// A global subscriber rather than a scoped one because the isolate runs on a
/// runtime thread of its own, and `with_default` is thread-local: the line
/// under test is written on a thread this test never touches. Integration
/// tests get a process each, so a global default here disturbs nothing else.
fn node_log() -> Arc<Mutex<Vec<u8>>> {
    static LOG: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOG.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Sink(buffer.clone()))
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        // Another test in this binary may have installed it already, and a
        // second install is not a failure of the test that lost the race.
        let _ = tracing::subscriber::set_global_default(subscriber);
        buffer
    })
    .clone()
}

fn logged() -> String {
    let buffer = node_log();
    let bytes = buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    String::from_utf8_lossy(&bytes).into_owned()
}

/// One node, serving one application, reached through the real entry point.
async fn node(app_id: &str, code: &str) -> Router {
    node_log();
    let manager = Arc::new(ApplicationManager::new());
    manager
        .load_app(
            AppKey::production(app_id),
            code.to_string(),
            Default::default(),
            None,
        )
        .await
        .expect("the app loads");

    Router::new()
        .fallback(any(handlers::handle_request))
        .with_state(AppState::new(manager, default_max_request_body_bytes()))
}

async fn ask(router: &Router, method: &str, path: &str, app_id: &str) -> (StatusCode, String) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("x-deis-app-id", app_id)
                .body(Body::empty())
                .expect("the request is buildable"),
        )
        .await
        .expect("the node answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("the body is readable");
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// A handler that throws answers 500 without handing the client the platform's
/// or the tenant's insides -- and the author can still read the stack.
#[tokio::test]
async fn a_handler_that_throws_does_not_hand_its_stack_to_the_client() {
    let router = node(
        "throwing-app",
        r#"
            export default async function handler() {
                throw new Error("secret-in-message: hunter2");
            }
        "#,
    )
    .await;

    let (status, body) = ask(&router, "GET", "/boom", "throwing-app").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

    // The exception message. A throw from inside a credential check carries
    // the credential, and the client is not the party that may read it.
    assert!(
        !body.contains("hunter2"),
        "the exception message reached the client: {body:?}"
    );
    // The tenant's bundle layout, this runtime's own frames, and the vendored
    // deno version's internals -- the three shapes a stack from this isolate
    // has.
    for leak in ["deis:///", "<runtime>", "ext:"] {
        assert!(
            !body.contains(leak),
            "the answer named {leak:?}, which is the platform's or the tenant's inside: {body:?}"
        );
    }
    assert!(
        !body.contains("Error:"),
        "the answer carried an error's own text: {body:?}"
    );

    // What the client does get: a handle it can quote to the operator.
    let incident = body
        .split("incident ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .map(str::to_string)
        .unwrap_or_default();
    assert!(
        incident.len() == 16 && incident.chars().all(|c| c.is_ascii_hexdigit()),
        "the answer has to carry an incident id an operator can search on: {body:?}"
    );

    // And the other half of the claim: the author still sees the stack,
    // through the log stream `deis logs -a <app>` reads, attributed to this
    // application and to this invocation.
    let log = logged();
    let line = log
        .lines()
        .find(|line| line.contains(&incident))
        .unwrap_or_else(|| panic!("the node logged nothing for incident {incident}"));
    assert!(
        line.contains("hunter2"),
        "the operator's copy has to carry the message the client did not get: {line:?}"
    );
    assert!(
        line.contains(r#"app_id="throwing-app.production""#),
        "the line has to be attributable to the application, or `deis logs -a` cannot find it: \
         {line:?}"
    );
    let trace = line
        .split(r#"trace_id=""#)
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_default();
    assert!(
        trace.len() == 32 && trace.chars().all(|c| c.is_ascii_hexdigit()),
        "the line has to carry this invocation's trace id for `deis trace --trace-id`: {line:?}"
    );
}

/// The sibling of the wrapper's catch, one layer out: an invocation that never
/// comes back with a response at all.
///
/// A cut-off, a bundle whose module will not evaluate on a fresh isolate, a
/// runtime thread that dropped the call -- none of them reach the invoke
/// wrapper, so the dispatcher composed the 500 out of the error's own text. A
/// module error carries the application's stack and a cut-off names the budget
/// the application was held to; neither is the client's to read.
#[tokio::test]
async fn an_invocation_that_never_answers_does_not_hand_its_reason_to_the_client() {
    let manager = Arc::new(ApplicationManager::new());
    node_log();
    manager
        .load_app(
            AppKey::production("cut-off-app"),
            r#"
                export default async function handler() {
                    for (;;) {}
                }
            "#
            .to_string(),
            AppConfig {
                limits: Some(ResourceLimits {
                    timeout_ms: Some(200),
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("the app loads");
    let router = Router::new()
        .fallback(any(handlers::handle_request))
        .with_state(AppState::new(manager, default_max_request_body_bytes()));

    let (status, body) = ask(&router, "GET", "/", "cut-off-app").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    // "app cut-off-app.production was cut off: it ran for ..." is the node's
    // account of one tenant's configuration, and the client is not that
    // tenant.
    for leak in ["cut off", "cut-off-app", "budget", "ms", "deis:///"] {
        assert!(
            !body.contains(leak),
            "the answer named {leak:?}, which is the platform's or the tenant's inside: {body:?}"
        );
    }

    let incident = body
        .split("incident ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .map(str::to_string)
        .unwrap_or_default();
    assert!(
        !incident.is_empty() && incident.chars().all(|c| c.is_ascii_hexdigit()),
        "the answer has to carry an incident id an operator can search on: {body:?}"
    );

    let log = logged();
    let line = log
        .lines()
        .find(|line| line.contains(&incident))
        .unwrap_or_else(|| panic!("the node logged nothing for incident {incident}"));
    assert!(
        line.contains("cut off"),
        "the operator's copy has to carry what the client did not get: {line:?}"
    );
    // This one is the node's own log rather than the application's stream --
    // see the arm in `dispatcher/handlers.rs` for why -- so what is asserted is
    // that an operator reading the node's log can attribute it.
    assert!(
        line.contains("app_id=cut-off-app") && line.contains("environment=production"),
        "the line has to be attributable to the application: {line:?}"
    );
}

/// The app used by the method witnesses: it counts every request that reaches
/// it, and will say so.
const COUNTING_APP: &str = r#"
    let reached = 0;
    export default async function handler(context) {
        const url = new URL(context.request.url);
        if (url.pathname === "/count") return new Response(String(reached));
        reached++;
        return new Response("reached:" + context.request.method);
    }
"#;

/// The Fetch standard's forbidden methods are refused at the door.
///
/// 405, and the application is never called: the counter is the evidence,
/// because a refusal that still spent a runtime thread would be a different
/// fix wearing the same status code.
#[tokio::test]
async fn a_forbidden_method_is_refused_before_the_application_runs() {
    let router = node("forbidden-methods", COUNTING_APP).await;

    let (status, count) = ask(&router, "GET", "/count", "forbidden-methods").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        count, "0",
        "the counter starts where the test thinks it does"
    );

    for method in ["TRACE", "CONNECT", "TRACK", "trace"] {
        let (status, body) = ask(&router, method, "/anything", "forbidden-methods").await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} was not refused; it answered {body:?}"
        );
        for leak in ["deis:///", "<runtime>", "ext:", "Method is forbidden"] {
            assert!(
                !body.contains(leak),
                "the refusal of {method} named {leak:?}: {body:?}"
            );
        }
    }

    let (_, count) = ask(&router, "GET", "/count", "forbidden-methods").await;
    assert_eq!(
        count, "0",
        "a forbidden method reached the application: it must be refused before the isolate"
    );
}

/// And every other method this node does not recognise still reaches the
/// handler, because applications answer them today.
#[tokio::test]
async fn an_unrecognised_method_still_reaches_the_application() {
    let router = node("passed-through-methods", COUNTING_APP).await;

    for method in ["PROPFIND", "REPORT", "PATCH"] {
        let (status, body) = ask(&router, method, "/anything", "passed-through-methods").await;
        assert_eq!(status, StatusCode::OK, "{method} was refused: {body:?}");
        assert_eq!(body, format!("reached:{method}"));
    }

    let (_, count) = ask(&router, "GET", "/count", "passed-through-methods").await;
    assert_eq!(
        count, "3",
        "every one of them has to have reached the handler"
    );
}

/// A request naming an application this node does not run is a 404, not a 500
/// with an incident id, and it puts no ERROR line in the node's log.
///
/// On the data port -- which the proxy does not sit in front of for a
/// node-to-node or a demo call -- an unknown app id used to come back through
/// the invoke error path as `500 Internal Server Error (incident <id>)` with
/// an ERROR naming the app. Anyone who could reach the port minted incident
/// ids and ERROR lines by naming apps that do not exist, and an operator
/// counting incidents could not tell a crash from a typo. An unknown app is
/// not a failure of this node.
#[tokio::test]
async fn an_unknown_application_is_a_404_without_an_incident() {
    // One app is loaded; the request below names a different one, which this
    // node has never been told to run.
    let router = node("known-probe-app", COUNTING_APP).await;

    let (status, body) = ask(&router, "GET", "/verify", "ghost-probe-app").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an app this node does not run has to be a 404, not a 500: {body:?}"
    );
    assert!(
        !body.contains("incident") && !body.contains("Internal Server Error"),
        "a 404 for an unknown app must not carry an incident id or the 500 body: {body:?}"
    );

    let log = logged();
    assert!(
        !log.contains("ghost-probe-app.production execution failed")
            && !log.contains("Application ghost-probe-app"),
        "an unknown app must not mint an ERROR line in the node log:\n{log}"
    );
}
