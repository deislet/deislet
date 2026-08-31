//! The platform's globals stay the platform's, across every request an isolate
//! serves.
//!
//! An isolate is reused: every request for the same application on this node
//! runs in the one built when the version loaded. So a name an application can
//! redefine is not a mistake it makes to itself for one request -- it is a
//! change to what the *next* request finds when it reads `performance.now()`,
//! `navigator.userAgent` or `crypto.getRandomValues`.
//!
//! `core.propReadOnly` is `{ writable: false, configurable: true }`, which is
//! enough to make plain assignment throw and nothing more:
//! `Object.defineProperty` and `delete` both went through, and what one
//! request installed was still standing for the one after it. These drive both
//! doors and then look at the isolate again through a second request.
//!
//! Kept as an integration test rather than beside the global-surface
//! assertions in `src/runtime/isolate_test.rs` because what is under test is
//! two requests through one isolate, which is the platform seen from outside.

use std::collections::HashMap;

use deis_runtime::runtime::isolate::AppRuntime;
use deis_runtime::runtime::{IsolateSpec, ServiceWiring};
use tokio::sync::oneshot;

/// Run one request through an isolate and hand back the JSON its handler
/// answered with.
async fn request(runtime: &mut AppRuntime, path: &str) -> serde_json::Value {
    let (tx, rx) = oneshot::channel();
    runtime
        .execute_fetch(
            format!("http://localhost{path}"),
            "GET".to_string(),
            HashMap::new(),
            None,
            tx,
        )
        .await
        .expect("the request runs");
    let (status, _headers, body) = rx.await.unwrap().unwrap();
    let body =
        String::from_utf8(body.into_bytes().unwrap_or_default()).expect("the answer is text");
    assert_eq!(status, 200, "the handler failed: {body}");
    serde_json::from_str(&body).expect("the handler answers with JSON")
}

async fn runtime_for(app: &str, code: &str) -> AppRuntime {
    let mut runtime = AppRuntime::new(
        app.to_string(),
        code.to_string(),
        None,
        IsolateSpec::default(),
        &ServiceWiring::default(),
    )
    .expect("the isolate is built");
    runtime.init_module().await.expect("the module loads");
    runtime
}

/// The first request attacks; the second reads what it left behind.
const ATTACK_THEN_READ: &str = r#"
    function attack() {
        const out = {};

        // Plain assignment, which was already refused.
        try {
            globalThis.navigator = { userAgent: "REPLACED" };
            out.assign = "silent";
        } catch (e) {
            out.assign = "threw:" + e.name;
        }

        // Redefinition, which was not.
        for (const name of ["performance", "navigator", "crypto"]) {
            try {
                Object.defineProperty(globalThis, name, {
                    value: { now: () => 1234.5678, timeOrigin: 999, userAgent: "REPLACED" },
                    writable: true,
                    configurable: true,
                });
                out["define_" + name] = "ok";
            } catch (e) {
                out["define_" + name] = "threw:" + e.name;
            }
        }

        // Deletion, which was not either: a deleted global is a global the
        // next request finds missing.
        for (const name of ["performance", "navigator", "crypto"]) {
            try {
                out["delete_" + name] = delete globalThis[name];
            } catch (e) {
                out["delete_" + name] = "threw:" + e.name;
            }
        }

        // The internal hooks that end an application's timers and outbound
        // budgets when its request ends, and the entry point that calls them.
        // A no-op in place of any of the three is work that outlives the
        // request that started it, on a runtime thread shared with every other
        // application on it.
        for (const name of [
            "__deis_runtime_sweep_intervals",
            "__deis_runtime_sweep_fetch_budgets",
            "__deis_runtime_invoke",
            "__deis_runtime_wait_until_all",
            "__deis_runtime_set_response",
        ]) {
            try {
                Object.defineProperty(globalThis, name, {
                    value: () => 0,
                    configurable: true,
                });
                out["define_" + name] = "ok";
            } catch (e) {
                out["define_" + name] = "threw:" + e.name;
            }
        }

        return out;
    }

    function read() {
        return {
            now: performance.now(),
            nowFraction: performance.now() % 1,
            timeOrigin: performance.timeOrigin,
            userAgent: navigator.userAgent,
            navigatorFrozen: Object.isFrozen(navigator),
            hasGetRandomValues: typeof crypto.getRandomValues === "function",
            sweepIsPlatforms:
                typeof globalThis.__deis_runtime_sweep_intervals === "function" &&
                globalThis.__deis_runtime_sweep_intervals.name === "sweepIntervals",
            // Proof that both requests ran in one isolate: an ordinary global
            // an application writes does survive, and is meant to.
            stash: globalThis.__stash ?? "none",
        };
    }

    export default async function handler(context) {
        const path = new URL(context.request.url).pathname;
        const out = path === "/attack" ? attack() : read();
        globalThis.__stash = "left-by-" + path;
        return new Response(JSON.stringify(out));
    }
"#;

#[tokio::test]
async fn a_platform_global_cannot_be_redefined_or_deleted() {
    let mut runtime = runtime_for("globals-sealed.production", ATTACK_THEN_READ).await;
    let attacked = request(&mut runtime, "/attack").await;

    assert_eq!(attacked["assign"], "threw:TypeError");
    for name in ["performance", "navigator", "crypto"] {
        assert_eq!(
            attacked[format!("define_{name}")],
            "threw:TypeError",
            "`{name}` can be redefined, so the next request through this isolate reads the \
             object this one installed"
        );
        assert_eq!(
            attacked[format!("delete_{name}")],
            "threw:TypeError",
            "`{name}` can be deleted, so the next request through this isolate finds it missing"
        );
    }
    for name in [
        "__deis_runtime_sweep_intervals",
        "__deis_runtime_sweep_fetch_budgets",
        "__deis_runtime_invoke",
        "__deis_runtime_wait_until_all",
        "__deis_runtime_set_response",
    ] {
        assert_eq!(
            attacked[format!("define_{name}")],
            "threw:TypeError",
            "`{name}` can be replaced, and a no-op in its place leaves an application's \
             timers running past the request that created them"
        );
    }
}

#[tokio::test]
async fn a_second_request_sees_the_platforms_objects_and_not_the_previous_requests() {
    let mut runtime = runtime_for("globals-across-requests.production", ATTACK_THEN_READ).await;
    request(&mut runtime, "/attack").await;
    let seen = request(&mut runtime, "/read").await;

    assert_eq!(
        seen["stash"], "left-by-/attack",
        "these two requests did not run in the same isolate, so this proves nothing"
    );

    // The clock is the platform's: whole milliseconds and a zero origin. A
    // fractional `now()` or an invented `timeOrigin` would be the
    // sub-millisecond clock the coarse one exists to withhold.
    assert_eq!(
        seen["nowFraction"], 0.0,
        "performance.now() has a fractional part, so it is not the platform's clock"
    );
    assert_eq!(seen["timeOrigin"], 0);
    assert!(
        seen["now"].as_f64().unwrap_or_default() > 1.0e12,
        "performance.now() is not answering with the wall clock"
    );

    assert_eq!(
        seen["userAgent"], "Deislet",
        "navigator is the object the previous request installed"
    );
    assert_eq!(seen["navigatorFrozen"], true);
    assert_eq!(seen["hasGetRandomValues"], true);
    assert_eq!(
        seen["sweepIsPlatforms"], true,
        "the interval sweep is not the platform's function any more"
    );
}

/// One `EventTarget` for the isolate.
///
/// `js/ws_client.js` extends deno_web's, and `globalThis.EventTarget` used to
/// be a small hand-written class in `js/runtime.js` instead -- so listeners
/// worked while `socket instanceof EventTarget`, the guard a library runs
/// before attaching one, answered false.
#[tokio::test]
async fn every_socket_is_an_instance_of_the_eventtarget_an_application_can_name() {
    let code = r#"
        export default async function handler() {
            const pair = new WebSocketPair();
            return new Response(JSON.stringify({
                outboundClass: WebSocket.prototype instanceof EventTarget,
                inboundClass: WebSocketServer.prototype instanceof EventTarget,
                inboundInstance: pair[1] instanceof EventTarget,
                clientInstance: pair[0] instanceof EventTarget,
                // An application's own bus still works, and its events carry
                // the target the specification puts on them.
                busTarget: (() => {
                    const bus = new EventTarget();
                    let target = null;
                    bus.addEventListener("ping", (e) => { target = e.target === bus; });
                    bus.dispatchEvent(new Event("ping"));
                    return target;
                })(),
            }));
        }
    "#;
    let mut runtime = runtime_for("one-eventtarget.production", code).await;
    let seen = request(&mut runtime, "/").await;

    assert_eq!(
        seen["outboundClass"], true,
        "an outbound WebSocket is not an instance of the EventTarget an application can name"
    );
    assert_eq!(seen["inboundClass"], true);
    assert_eq!(seen["inboundInstance"], true);
    assert_eq!(seen["clientInstance"], true);
    assert_eq!(seen["busTarget"], true);
}
