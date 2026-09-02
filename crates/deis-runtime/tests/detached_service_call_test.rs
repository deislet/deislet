//! What a service call the handler did not wait for can and cannot do once
//! the invocation that made it has ended, and which methods a service call
//! carries at all.
//!
//! Two applications on one pool: a caller and a callee the caller reaches
//! through a service binding. Everything here goes through
//! `RuntimePool::execute_request`, which is the path the dispatcher takes, so
//! what is asserted is what a node does and not what one isolate does on its
//! own.
//!
//! The node's own log is captured for the whole process, the way
//! `error_disclosure_test.rs` captures it: a callee's invoke error is written
//! on a runtime thread this test never touches, so a thread-local subscriber
//! would not see the line whose absence is the point.

use std::sync::{Arc, Mutex, OnceLock};

use deis_runtime::runtime::{IsolateSpec, RuntimePool, ServiceWiring};

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

fn node_log() -> Arc<Mutex<Vec<u8>>> {
    static LOG: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOG.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Sink(buffer.clone()))
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
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

fn wiring(bindings: &[(&str, &str)], callable_by: &[&str]) -> ServiceWiring {
    let mut wiring = ServiceWiring::default();
    for (name, target) in bindings {
        wiring.bindings.insert(
            *name,
            deis_bundle::Binding {
                kind: deis_bundle::BindingKind::Service,
                app: Some(target.to_string()),
                environment: None,
            },
        );
    }
    wiring.callable_by = callable_by.iter().map(|name| name.to_string()).collect();
    wiring
}

async fn load(pool: &RuntimePool, tenant: &str, code: &str, wiring: ServiceWiring) {
    load_with_timeout(pool, tenant, code, wiring, None).await;
}

async fn load_with_timeout(
    pool: &RuntimePool,
    tenant: &str,
    code: &str,
    wiring: ServiceWiring,
    timeout_ms: Option<u64>,
) {
    let mut spec = IsolateSpec::default();
    wiring.write_to_env(&mut spec.env);
    if let Some(timeout_ms) = timeout_ms {
        spec.limits.timeout_ms = timeout_ms;
    }
    pool.load_app(tenant.to_string(), code.to_string(), spec)
        .await
        .unwrap_or_else(|e| panic!("{tenant} loads: {e:#}"));
}

/// One request to `tenant`, as the dispatcher would make it. `None` when the
/// invocation did not answer at all, which is what a cut-off looks like.
async fn ask(
    pool: &RuntimePool,
    tenant: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> Option<(u16, String)> {
    let headers = headers
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    let (status, _, body) = pool
        .execute_request(
            tenant,
            format!("http://localhost{path}"),
            "GET".to_string(),
            headers,
            None,
        )
        .await
        .ok()?;
    Some((
        status,
        String::from_utf8_lossy(&body.into_bytes().unwrap_or_default()).into_owned(),
    ))
}

fn answered(pool_answer: Option<(u16, String)>, what: &str) -> String {
    let (status, body) = pool_answer.unwrap_or_else(|| panic!("{what} has to answer"));
    assert_eq!(status, 200, "{what}: {body}");
    body
}

/// A callee that is slow enough for a call to it to be still in flight when
/// the caller's handler returns, and that says which method reached it.
const SLOW_CALLEE: &str = r#"
    export default async function handler(context) {
        const url = new URL(context.request.url);
        if (url.pathname === "/work") {
            const until = Date.now() + 80;
            while (Date.now() < until) {}
        }
        return new Response("callee ran " + context.request.method);
    }
"#;

/// The round-nine probe, as the judge wrote it: Alice, signed in, fires a
/// service call and caches its answer in the continuation without awaiting
/// the call; Bob, anonymous, arrives while the call is still in flight and
/// reads the cache.
///
/// Before this the call was filed in no guard set, so nothing ended it with
/// Alice's invocation: it settled inside Bob's, its continuation read Bob's
/// statement about the request being answered, the entry built from Alice's
/// cookie was judged against an anonymous request, stored, and served to
/// Bob on the first try.
#[tokio::test]
async fn a_service_call_the_handler_did_not_await_ends_with_its_invocation() {
    node_log();
    let pool = RuntimePool::new(Some(2));
    load(
        &pool,
        "detached-callee.production",
        SLOW_CALLEE,
        wiring(&[], &["detached-shop"]),
    )
    .await;
    load(
        &pool,
        "detached-shop.production",
        r#"
            let outcome = "pending";
            export default async function handler(context) {
                const url = new URL(context.request.url);
                if (url.pathname === "/report") return new Response(outcome);
                if (url.pathname === "/alice") {
                    context.bindings.SLOW.fetch("/work").then(async () => {
                        const kept = await caches.default.put(
                            "https://example.invalid/page",
                            new Response("alice-private-page for " +
                                context.request.headers.get("cookie")));
                        outcome = kept.stored ? "stored" : "refused: " + kept.reason;
                    }, (e) => {
                        outcome = "call ended: " + e.name + ": " + e.message;
                    });
                    return new Response("alice ok");
                }
                if (url.pathname === "/bob") {
                    await new Promise((resolve) => setTimeout(resolve, 150));
                    const hit = await caches.default.match("https://example.invalid/page");
                    return new Response(hit ? "HIT: " + await hit.text() : "MISS");
                }
                return new Response("no such route", { status: 404 });
            }
        "#,
        wiring(&[("SLOW", "detached-callee")], &[]),
    )
    .await;

    let alice = answered(
        ask(
            &pool,
            "detached-shop.production",
            "/alice",
            &[("cookie", "session=alice")],
        )
        .await,
        "Alice",
    );
    assert_eq!(alice, "alice ok");

    let bob = answered(
        ask(&pool, "detached-shop.production", "/bob", &[]).await,
        "Bob",
    );
    assert_eq!(
        bob, "MISS",
        "an anonymous caller was handed the page built from another caller's cookie"
    );

    let report = answered(
        ask(&pool, "detached-shop.production", "/report", &[]).await,
        "the report",
    );
    assert!(
        report.starts_with("call ended: TimeoutError:"),
        "a service call the handler never awaited has to be ended with the invocation that \
         made it, the way a detached fetch is, and the application told so: {report}"
    );
    assert!(
        report.contains("the invocation ended"),
        "the reason has to name what happened to the call: {report}"
    );

    let _ = pool.unload_app("detached-shop.production").await;
    let _ = pool.unload_app("detached-callee.production").await;
}

/// The same probe on the path where the sweep cannot deliver the abort inside
/// Alice's own stretch: her handler overruns its budget after firing the call,
/// so the sweep runs from the node between invocations and the call's
/// rejection reaches the application's code inside Bob's invocation. The
/// construction-level guarantee is asserted deterministically instead: a
/// promise Alice leaves pending and Bob resolves.
///
/// Alice's handler stashes a resolver and attaches a `put` to the promise;
/// Bob's handler calls the resolver, so the `put` runs as a microtask inside
/// Bob's invocation. It is Alice's code, written for Alice's request, running
/// under Bob's stretch of the node's work. The statement travels with the
/// code -- Alice's, stamped with a stretch that has ended -- so the `put`
/// finds it stale and is refused with a reason naming the request it was
/// really written for. Bob's own `put`, in the same stretch, is judged
/// against Bob and is not affected: the statement travels with the code, not
/// with the stretch. This is the case the runtime's own header used to
/// disclaim as one it could not tell apart; it now can.
#[tokio::test]
async fn a_continuation_that_outlives_its_invocation_cannot_store_for_the_next_one() {
    node_log();
    let pool = RuntimePool::new(Some(2));
    load(
        &pool,
        "stash-shop.production",
        r#"
            let outcome = "pending";
            let resume = null;
            export default async function handler(context) {
                const url = new URL(context.request.url);
                if (url.pathname === "/report") return new Response(outcome);
                if (url.pathname === "/alice") {
                    new Promise((resolve) => { resume = resolve; }).then(async () => {
                        const kept = await caches.default.put(
                            "https://example.invalid/page",
                            new Response("alice-private-page for " +
                                context.request.headers.get("cookie")));
                        outcome = kept.stored ? "stored" : "refused: " + kept.reason;
                    });
                    return new Response("alice ok");
                }
                if (url.pathname === "/bob") {
                    // Resolve Alice's promise, then let its `.then` run as a
                    // microtask inside this invocation before answering.
                    if (resume) resume();
                    await new Promise((r) => setTimeout(r, 10));
                    const own = await caches.default.put(
                        "https://example.invalid/bob-page",
                        new Response("bob-public-page"));
                    const alices = await caches.default.match("https://example.invalid/page");
                    const bobs = await caches.default.match("https://example.invalid/bob-page");
                    return new Response(JSON.stringify({
                        alices: alices ? await alices.text() : null,
                        own: own.stored ? "stored" : own.reason,
                        bobs: bobs ? await bobs.text() : null,
                    }));
                }
                return new Response("no such route", { status: 404 });
            }
        "#,
        wiring(&[], &[]),
    )
    .await;

    let alice = answered(
        ask(
            &pool,
            "stash-shop.production",
            "/alice",
            &[("cookie", "session=alice")],
        )
        .await,
        "Alice",
    );
    assert_eq!(alice, "alice ok");

    let bob: serde_json::Value = serde_json::from_str(&answered(
        ask(&pool, "stash-shop.production", "/bob", &[]).await,
        "Bob",
    ))
    .expect("Bob answers JSON");
    assert_eq!(
        bob["alices"],
        serde_json::Value::Null,
        "an anonymous caller was handed the page built from another caller's cookie: {bob}"
    );
    assert_eq!(
        bob["own"], "stored",
        "Bob's own put runs under Bob's statement and is not refused for what Alice's \
         continuation did in the same stretch: {bob}"
    );
    assert_eq!(bob["bobs"], "bob-public-page", "{bob}");

    let report = answered(
        ask(&pool, "stash-shop.production", "/report", &[]).await,
        "the report",
    );
    assert!(
        report.starts_with("refused: "),
        "the put made by a continuation of an ended invocation has to be refused, not \
         stored and not left unattempted: {report}"
    );
    assert!(
        report.contains("continues an invocation that has already ended")
            && report.contains("GET http://localhost/alice")
            && report.contains("stretch "),
        "the refusal has to name the stale statement -- the request the continuation was \
         written for and the stretch it was made in: {report}"
    );

    let _ = pool.unload_app("stash-shop.production").await;
}

/// The Fetch standard's forbidden methods, and anything that is not an HTTP
/// method at all, are refused by the caller's own node before the call is
/// posted: the caller gets a `TypeError` naming the method, and the callee
/// never sees the call and writes nothing about it.
///
/// Before this the raw string reached the callee's `createContext`, whose
/// `new Request` threw `Method is forbidden`: the caller got a 500 with an
/// incident id, and the callee's own log stream -- what `deis logs -a
/// <callee>` shows -- gained an ERROR line with a platform stack for a call no
/// callee code ever ran.
#[tokio::test]
async fn a_method_a_service_call_cannot_carry_is_refused_before_the_callee_is_entered() {
    node_log();
    let pool = RuntimePool::new(Some(2));
    load(
        &pool,
        "method-callee.production",
        SLOW_CALLEE,
        wiring(&[], &["method-caller"]),
    )
    .await;
    load(
        &pool,
        "method-caller.production",
        r#"
            export default async function handler(context) {
                const url = new URL(context.request.url);
                const method = url.searchParams.get("method");
                try {
                    const answer = await context.bindings.CALLEE.fetch("/hello", { method });
                    return new Response("status=" + answer.status + " body=" + await answer.text());
                } catch (e) {
                    return new Response("threw: " + e.name + ": " + e.message);
                }
            }
        "#,
        wiring(&[("CALLEE", "method-callee")], &[]),
    )
    .await;

    for method in ["TRACE", "CONNECT", "TRACK", "trace", "GET X", "G\u{e9}T"] {
        let encoded: String = url_encode(method);
        let body = answered(
            ask(
                &pool,
                "method-caller.production",
                &format!("/?method={encoded}"),
                &[],
            )
            .await,
            &format!("the caller sending {method:?}"),
        );
        assert!(
            body.starts_with("threw: TypeError:"),
            "{method:?} has to be refused in the caller with the error a failed fetch \
             throws, not answered 500 from inside the callee: {body}"
        );
        assert!(
            body.contains(&format!("{method:?}")) && body.contains("cannot carry"),
            "the refusal has to name the method as one a service call cannot carry: {body}"
        );
    }

    // A method the standard merely does not list is still the application's
    // to use.
    let put = answered(
        ask(&pool, "method-caller.production", "/?method=PUT", &[]).await,
        "the caller sending PUT",
    );
    assert_eq!(put, "status=200 body=callee ran PUT");

    let log = logged();
    assert!(
        !log.contains("Runtime invoke error")
            && !log.contains("Method is forbidden")
            && !log.contains("Method is not valid"),
        "the callee was entered for a call that could never be served, and its log stream \
         now carries an ERROR for it:\n{log}"
    );
    assert!(
        !log.contains("app_id=\"method-callee.production\" execution failed")
            && !log.contains("incident"),
        "no incident id may be minted for a refusal the caller is told about:\n{log}"
    );

    let _ = pool.unload_app("method-caller.production").await;
    let _ = pool.unload_app("method-callee.production").await;
}

fn url_encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}
