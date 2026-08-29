#[cfg(test)]
mod tests {
    use crate::config::ExecutionLimits;
    use crate::runtime::bindings::{ServiceDeclaration, ServiceEndpoints, ServiceWiring};
    use crate::runtime::isolate::{AppRuntime, IsolateSpec};
    use crate::runtime::metrics::{
        recorder, EXECUTION_MS_COUNT, REQUESTS_TOTAL, REQUEST_ERRORS_TOTAL,
    };
    use crate::runtime::queue_client::LeasedMessage;
    use std::collections::HashMap;
    use tokio::sync::oneshot;

    #[test]
    fn test_transform_esm_typescript() {
        let ts_code = r#"export default function handler(_request: Request): Response {
    return new Response("Hello from Runtime!", {
      status: 200,
      headers: {
        "Content-Type": "text/plain",
      },
    });
};"#;

        let result = AppRuntime::new(
            "test_esm".to_string(),
            ts_code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        );
        assert!(
            result.is_ok(),
            "Failed to parse TypeScript ESM: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_transform_counter_app() {
        let ts_code = r#"export default function handler(context: any): Response {
    const url = new URL(context.request.url);
    return new Response("ok");
};"#;

        let result = AppRuntime::new(
            "test_counter".to_string(),
            ts_code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        );
        assert!(
            result.is_ok(),
            "Failed to parse counter app: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_transform_with_interface() {
        let ts_code = r#"interface Todo {
  id: string;
  title: string;
}

export default function handler(context: any): Response {
    const todos: Todo[] = [];
    return new Response(JSON.stringify(todos));
};"#;

        let result = AppRuntime::new(
            "test_interface".to_string(),
            ts_code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        );
        assert!(
            result.is_ok(),
            "Failed to parse TypeScript with interface: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_transform_echo_app() {
        let ts_code = r#"export default function handler(context: any): Response {
    const request = context.request;
    const url = new URL(request.url);
    const info: any = {
      method: request.method,
      url: request.url,
      path: url.pathname,
    };
    return new Response(JSON.stringify(info));
};"#;

        let result = AppRuntime::new(
            "test_echo".to_string(),
            ts_code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        );
        assert!(
            result.is_ok(),
            "Failed to parse echo app: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_create_runtime_with_valid_code() {
        let code = r#"
            export default function handler() {
                    return new Response("Hello");
            }
        "#;

        let result = AppRuntime::new(
            "test_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_runtime_with_invalid_code() {
        let invalid_code = "this is not valid javascript {{{";
        let result = AppRuntime::new(
            "test_app".to_string(),
            invalid_code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        );
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_create_runtime() {
        let code = r#"
            export default function handler() {
                    return new Response("test");
            }
        "#;

        let result = AppRuntime::new(
            "test_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        );
        assert!(result.is_ok());

        let runtime = result.unwrap();
        assert_eq!(runtime.app_id(), "test_app");
    }

    // --- queue consumption -------------------------------------------------

    fn leased(id: &str, body: &str, attempts: i32) -> LeasedMessage {
        LeasedMessage {
            id: id.to_string(),
            body: body.to_string(),
            attempts,
            lease_expires_at: 0,
            created_at: 1_700_000_000,
            // Never handed to the application; the isolate is given the batch,
            // not the lease.
            lease_token: format!("lease-of-{id}"),
        }
    }

    #[tokio::test]
    async fn a_queue_handler_sees_the_batch_and_can_fail_single_messages() {
        // Half a batch failing must be expressible: acknowledging everything
        // would lose the failure, failing everything would redo work that
        // already succeeded.
        // The signature is the canon's `queue(batch, env, context)`
        // (SPECIFICATION_EXT.md section 3.2): the bindings table arrives in
        // the second position and the platform context in the third. A
        // handler written to the canon must not have to guess.
        let code = r#"
            export default function handler() { return new Response("unused"); }
            export async function queue(batch, env, context) {
                if (env.QUEUE_GREETING !== "hello") {
                    throw new Error("env is not in the env position: " + JSON.stringify(env));
                }
                if (typeof context.services.kv !== "object") {
                    throw new Error("context is not in the context position");
                }
                if (typeof context.log.info !== "function") {
                    throw new Error("context.log is missing on a queue invocation");
                }
                globalThis.seen = batch.messages.map((m) => m.id + ":" + m.body + ":" + m.attempts);
                globalThis.queueName = batch.queue;
                for (const message of batch.messages) {
                    if (message.body.includes("poison")) message.retry();
                }
            }
        "#;

        let spec = IsolateSpec {
            env: HashMap::from([("QUEUE_GREETING".to_string(), "hello".to_string())]),
            ..IsolateSpec::default()
        };

        let mut runtime = AppRuntime::new(
            "queue_ok_app".to_string(),
            code.to_string(),
            None,
            spec,
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let outcome = runtime
            .execute_queue_batch(
                "jobs".to_string(),
                vec![
                    leased("m1", "fine", 1),
                    leased("m2", "poison pill", 3),
                    leased("m3", "fine too", 1),
                ],
            )
            .await
            .expect("the handler ran");

        assert_eq!(outcome.failed, vec!["m2".to_string()]);
    }

    #[tokio::test]
    async fn a_queue_handler_that_throws_fails_the_whole_batch() {
        let code = r#"
            export default function handler() { return new Response("unused"); }
            export async function queue() { throw new Error("no database today"); }
        "#;

        let mut runtime = AppRuntime::new(
            "queue_throw_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let outcome = runtime
            .execute_queue_batch(
                "jobs".to_string(),
                vec![leased("m1", "a", 1), leased("m2", "b", 1)],
            )
            .await
            .expect("a throwing handler is reported, not propagated");

        assert_eq!(outcome.failed, vec!["m1".to_string(), "m2".to_string()]);
    }

    #[tokio::test]
    async fn an_app_with_no_queue_handler_hands_the_batch_back() {
        // Dropping the messages would be silent data loss; handing them back
        // means they are retried and eventually dead lettered, which is
        // visible.
        let code =
            r#"export default function handler() { return new Response("no queue export"); }"#;

        let mut runtime = AppRuntime::new(
            "queue_missing_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let outcome = runtime
            .execute_queue_batch("jobs".to_string(), vec![leased("m1", "a", 1)])
            .await
            .expect("the absence of a handler is an outcome, not an error");

        assert_eq!(outcome.failed, vec!["m1".to_string()]);
    }

    #[tokio::test]
    async fn a_queue_handler_that_acknowledges_everything_fails_nothing() {
        let code = r#"
            export default function handler() { return new Response("unused"); }
            export async function queue(batch) {
                for (const message of batch.messages) { message.retry(); message.ack(); }
            }
        "#;

        let mut runtime = AppRuntime::new(
            "queue_ack_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let outcome = runtime
            .execute_queue_batch("jobs".to_string(), vec![leased("m1", "a", 1)])
            .await
            .unwrap();
        assert!(outcome.failed.is_empty());
    }

    #[tokio::test]
    async fn a_scheduled_handler_runs_and_sees_the_expression_that_was_due() {
        // The control plane owns the schedule and calls one node per tick;
        // the handler has to be told which expression fired, because an app
        // may declare several and do different work for each.
        // Signature per the canon: `scheduled(event, env, context)`.
        let code = r#"
            export default function handler() { return new Response("unused"); }
            export async function scheduled(event, env, context) {
                if (event.cron !== "0 */6 * * *") throw new Error("wrong expression: " + event.cron);
                if (event.scheduledTime !== 1700000000000) throw new Error("wrong time");
                if (env.CRON_GREETING !== "hello") {
                    throw new Error("env is not in the env position: " + JSON.stringify(env));
                }
                if (typeof context.services.kv !== "object") throw new Error("no services");
                if (typeof context.log.info !== "function") throw new Error("no context.log");
            }
        "#;

        let spec = IsolateSpec {
            env: HashMap::from([("CRON_GREETING".to_string(), "hello".to_string())]),
            ..IsolateSpec::default()
        };

        let mut runtime = AppRuntime::new(
            "cron_app".to_string(),
            code.to_string(),
            None,
            spec,
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        runtime
            .execute_scheduled("0 */6 * * *".to_string(), 1_700_000_000_000)
            .await
            .expect("the scheduled handler ran");
    }

    #[tokio::test]
    async fn a_scheduled_handler_that_throws_reports_the_tick_as_failed() {
        // A tick that silently "succeeds" turns a dead cron into no signal at
        // all; the control plane needs the failure to try another node.
        let code = r#"
            export default function handler() { return new Response("unused"); }
            export async function scheduled() { throw new Error("nightly job broke"); }
        "#;

        let mut runtime = AppRuntime::new(
            "cron_broken_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let error = runtime
            .execute_scheduled("* * * * *".to_string(), 0)
            .await
            .expect_err("a throwing handler is a failed tick");
        assert!(error.to_string().contains("nightly job broke"), "{error}");
    }

    #[tokio::test]
    async fn an_app_with_no_scheduled_handler_says_so_instead_of_reporting_success() {
        let code = r#"
            export default function handler() { return new Response("unused"); }
        "#;

        let mut runtime = AppRuntime::new(
            "cron_missing_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let error = runtime
            .execute_scheduled("* * * * *".to_string(), 0)
            .await
            .expect_err("nothing ran, so the tick did not succeed");
        assert!(
            error.to_string().contains("no scheduled handler"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_handler_can_read_the_request_body() {
        // Without Request.text() every handler that reads a POST body failed
        // with "text is not a function", which named the runtime's polyfill
        // rather than anything the app did wrong.
        let code = r#"
            export default async function handler(context) {
                const text = await context.request.text();
                return new Response(JSON.stringify({ text, used: context.request.bodyUsed }));
            }
        "#;

        let mut runtime = AppRuntime::new(
            "body_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "POST".to_string(),
                HashMap::new(),
                Some("hello body".to_string()),
                tx,
            )
            .await
            .unwrap();
        let (status, _headers, body) = rx.await.unwrap().unwrap();

        assert_eq!(status, 200);
        let seen: serde_json::Value = serde_json::from_str(&body.unwrap()).unwrap();
        assert_eq!(seen["text"], "hello body");
        assert_eq!(seen["used"], true);
    }

    #[tokio::test]
    async fn a_handler_can_read_a_json_body() {
        let code = r#"
            export default async function handler(context) {
                const parsed = await context.request.json();
                return new Response(JSON.stringify({ got: parsed.key }));
            }
        "#;

        let mut runtime = AppRuntime::new(
            "json_body_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "POST".to_string(),
                HashMap::new(),
                Some(r#"{"key":"value"}"#.to_string()),
                tx,
            )
            .await
            .unwrap();
        let (status, _headers, body) = rx.await.unwrap().unwrap();

        assert_eq!(status, 200);
        let seen: serde_json::Value = serde_json::from_str(&body.unwrap()).unwrap();
        assert_eq!(seen["got"], "value");
    }

    #[tokio::test]
    async fn reading_a_request_body_twice_is_refused_rather_than_answered() {
        let code = r#"
            export default async function handler(context) {
                await context.request.text();
                try {
                    await context.request.text();
                    return new Response("read twice");
                } catch (e) {
                    return new Response("refused: " + e.message);
                }
            }
        "#;

        let mut runtime = AppRuntime::new(
            "double_read_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "POST".to_string(),
                HashMap::new(),
                Some("once".to_string()),
                tx,
            )
            .await
            .unwrap();
        let (_, _, body) = rx.await.unwrap().unwrap();
        assert!(body.unwrap().starts_with("refused:"));
    }

    #[tokio::test]
    async fn an_app_can_name_the_queue_it_sends_to() {
        // A consumer polls a named queue, so a producer has to be able to
        // name one; without this everything an app sends lands in "default"
        // and nothing consuming "jobs" would ever see it. An empty name is
        // refused rather than silently turned into the default queue.
        let code = r#"
            export default async function handler(ctx) {
                const named = ctx.services.queue.to("jobs");
                let outcome = named && named.name === "jobs" ? "named jobs" : "wrong queue";
                try {
                    ctx.services.queue.to("");
                    outcome += ", an empty name was accepted";
                } catch (e) {
                    outcome += ", empty name rejected";
                }
                return new Response(outcome);
            }
        "#;

        let mut runtime = AppRuntime::new(
            "queue_named_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let (status, _, body) = rx.await.unwrap().unwrap();

        assert_eq!(status, 200);
        assert_eq!(body, Some("named jobs, empty name rejected".to_string()));
    }

    #[tokio::test]
    async fn a_send_with_no_queue_bound_is_refused_instead_of_answered_with_an_id() {
        // The failure this replaces: the op used to push the message into a
        // vector inside this isolate and answer "mem-id", so the app saw a
        // resolved promise and a plausible id while the message went nowhere
        // and vanished with the isolate. The producer looked healthy and the
        // consumer never received anything.
        let code = r#"
            export default async function handler(ctx) {
                try {
                    await ctx.services.queue.send({ work: 1 });
                    return new Response("accepted");
                } catch (e) {
                    return new Response("refused: " + String(e.message || e));
                }
            }
        "#;

        let mut runtime = AppRuntime::new(
            "queue_unbound_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let (status, _, body) = rx.await.unwrap().unwrap();

        assert_eq!(status, 200);
        let body = body.unwrap_or_default();
        assert!(
            body.starts_with("refused:"),
            "an app with no queue must not be told its message was accepted: {body}"
        );
        assert!(
            body.contains("not available") && body.contains("not enqueued"),
            "the rejection has to say the message was not queued anywhere: {body}"
        );
    }

    #[tokio::test]
    async fn a_request_after_a_cut_off_one_still_runs() {
        // A cut-off ends an execution by terminating the isolate, and V8's
        // terminate flag is not spent when that execution returns: deno_core
        // clears it only long enough to build the error and then sets it
        // again. Left set, this isolate is finished for good -- every later
        // request 500s, every queue batch is handed back until its delivery
        // attempts run out and the messages are dead lettered, and every cron
        // tick fails -- until the app is redeployed or the node restarted. One
        // slow request would take the application off this node.
        //
        // The flag is set here directly rather than through one of the two
        // real triggers, because neither is usable in a test: the timeout is a
        // spawned timer that only lands if the handler happens to be inside
        // JS at that moment, and the rate limiter would go on firing during
        // the request that is meant to prove the isolate recovered. The state
        // it leaves behind is identical either way -- the same call on the
        // same handle.
        let code = r#"
            export default function handler(ctx) {
                return new Response("served " + new URL(ctx.request.url).pathname);
            }
        "#;

        let mut runtime = AppRuntime::new(
            "cut_off_recovery_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        runtime.terminate_like_a_cut_off();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/after".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .expect("the isolate must still be usable after a cut-off");
        let (status, _, body) = rx.await.unwrap().unwrap();

        assert_eq!(status, 200);
        assert_eq!(body, Some("served /after".to_string()));
    }

    #[tokio::test]
    async fn a_queue_batch_after_a_cut_off_still_runs() {
        // The same flag, and the consequence that made it worth fixing: a
        // batch handed to a terminated isolate never reaches the handler, so
        // every message is nacked, and once their attempts run out they are
        // dead lettered -- messages nothing was ever wrong with.
        let code = r#"
            export async function queue(batch) {
                return { failed: [] };
            }
            export default function handler() { return new Response("ok"); }
        "#;

        let mut runtime = AppRuntime::new(
            "cut_off_recovery_queue_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        runtime.terminate_like_a_cut_off();

        let outcome = runtime
            .execute_queue_batch("jobs".to_string(), vec![leased("m1", "{}", 1)])
            .await
            .expect("a batch after a cut-off must still reach the handler");
        assert!(
            outcome.failed.is_empty(),
            "the handler acknowledged the batch; nothing should be handed back: {:?}",
            outcome.failed
        );
    }

    // --- metrics -----------------------------------------------------------

    #[tokio::test]
    async fn a_served_request_is_counted_with_its_execution_time() {
        // The app id is unique per test because the registry is process-wide
        // and the test binary runs its tests in parallel.
        let app_id = "metrics_ok_app";
        let code = r#"export default function handler() { return new Response("ok"); }"#;

        let mut runtime = AppRuntime::new(
            app_id.to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let _ = rx.await.unwrap().unwrap();

        assert_eq!(recorder().value(REQUESTS_TOTAL, app_id), 1.0);
        assert_eq!(recorder().value(REQUEST_ERRORS_TOTAL, app_id), 0.0);
        assert_eq!(recorder().value(EXECUTION_MS_COUNT, app_id), 1.0);
    }

    #[tokio::test]
    async fn an_app_answering_5xx_is_counted_as_an_error() {
        // This is what an operator means by "error rate": the app said it
        // failed, even though the isolate did its job.
        let app_id = "metrics_5xx_app";
        let code = r#"
            export default function handler() {
                return new Response("broken", { status: 503 });
            }
        "#;

        let mut runtime = AppRuntime::new(
            app_id.to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let (status, _, _) = rx.await.unwrap().unwrap();

        assert_eq!(status, 503);
        assert_eq!(recorder().value(REQUESTS_TOTAL, app_id), 1.0);
        assert_eq!(recorder().value(REQUEST_ERRORS_TOTAL, app_id), 1.0);
    }

    // --- bindings ----------------------------------------------------------

    #[tokio::test]
    async fn a_service_the_app_did_not_declare_answers_that_it_is_unavailable() {
        // The failure has to reach the app as a catchable error, not as a
        // dead isolate.
        let code = r#"
            export default async function handler(ctx) {
                try {
                    await ctx.services.kv.get("anything");
                    return new Response("unexpectedly reached the store");
                } catch (e) {
                    return new Response(String(e.message || e));
                }
            }
        "#;

        let mut runtime = AppRuntime::new(
            "no_services_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let (status, _, body) = rx.await.unwrap().unwrap();

        assert_eq!(status, 200);
        let body = body.unwrap_or_default();
        assert!(
            body.contains("not available"),
            "an undeclared service must say so rather than hang or crash: {body}"
        );
    }

    #[tokio::test]
    async fn a_declared_service_is_bound_and_its_failure_stays_inside_the_request() {
        // Nothing listens on port 1: the client exists, the call fails, and
        // the isolate survives to serve the next request.
        let wiring = ServiceWiring {
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints {
                store: Some("http://127.0.0.1:1".to_string()),
                queue: None,
            },
            consumers: Vec::new(),
        };

        let code = r#"
            export default async function handler(ctx) {
                try {
                    await ctx.services.kv.get("anything");
                    return new Response("unexpectedly reached the store");
                } catch (e) {
                    return new Response("call failed: " + String(e.message || e));
                }
            }
        "#;

        let mut runtime = AppRuntime::new(
            "bound_kv_app.production".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &wiring,
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        for _ in 0..2 {
            let (tx, rx) = oneshot::channel();
            runtime
                .execute_fetch(
                    "http://localhost/".to_string(),
                    "GET".to_string(),
                    HashMap::new(),
                    None,
                    tx,
                )
                .await
                .unwrap();
            let (status, _, body) = rx.await.unwrap().unwrap();
            let body = body.unwrap_or_default();
            assert_eq!(status, 200);
            assert!(body.starts_with("call failed:"), "{body}");
            assert!(
                !body.contains("not available"),
                "the binding exists; the failure must be the transport error, not a missing \
                 client: {body}"
            );
        }
    }

    /// The store harness lives in `runtime::test_store` because more than one
    /// module here needs it: an application's own JavaScript reaching a real
    /// store (below) and the SQL client's own round trip (`db_client`) prove
    /// different seams of the same chain - manifest, address, op, client,
    /// wire - and neither is provable without the real server.
    use crate::runtime::test_store::TestStore;

    /// Run one request against an app with the given wiring and hand back the
    /// response body.
    async fn body_of_one_request(app_id: &str, code: &str, wiring: &ServiceWiring) -> String {
        let mut runtime = AppRuntime::new(
            app_id.to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            wiring,
        )
        .expect("the isolate is built");
        runtime.init_module().await.expect("the module loads");

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .expect("the request runs");
        let (status, _, body) = rx.await.expect("a response").expect("a response");
        let body = body.unwrap_or_default();
        assert_eq!(status, 200, "{body}");
        body
    }

    #[tokio::test]
    async fn an_app_reads_back_the_metadata_it_wrote() {
        // getWithMetadata did not exist until deis-store's GetResponse could
        // carry the field: an app could attach metadata to a write and never
        // see it again. The object goes out and the same object comes back,
        // because put() JSON-encodes and this decodes.
        let store = TestStore::start().await;
        let code = r#"
            export default async function handler(ctx) {
                await ctx.services.kv.put("avatar", "png-bytes", {
                    metadata: { contentType: "image/png", width: 64 },
                });
                await ctx.services.kv.put("plain", "no-metadata-here");
                return new Response(JSON.stringify({
                    withMetadata: await ctx.services.kv.getWithMetadata("avatar"),
                    without: await ctx.services.kv.getWithMetadata("plain"),
                    missing: await ctx.services.kv.getWithMetadata("never-written"),
                }));
            }
        "#;

        let body =
            body_of_one_request("kv_metadata_app.production", code, &store.kv_wiring()).await;
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");

        assert_eq!(parsed["withMetadata"]["value"], "png-bytes");
        assert_eq!(
            parsed["withMetadata"]["metadata"]["contentType"],
            "image/png"
        );
        assert_eq!(parsed["withMetadata"]["metadata"]["width"], 64);

        // A key with no metadata is a record whose metadata is null; a key
        // that does not exist is null itself. An app branches on those
        // differently.
        assert_eq!(parsed["without"]["value"], "no-metadata-here");
        assert!(parsed["without"]["metadata"].is_null(), "{body}");
        assert!(parsed["missing"].is_null(), "{body}");
    }

    #[tokio::test]
    async fn a_lifetime_comes_back_as_the_moment_the_entry_lapses() {
        // The app asked for a lifetime and cannot otherwise tell how much of
        // it is left.
        let store = TestStore::start().await;
        let code = r#"
            export default async function handler(ctx) {
                await ctx.services.kv.put("session", "abc", { expirationTtl: 3600 });
                await ctx.services.kv.put("forever", "abc");
                const leased = await ctx.services.kv.getWithMetadata("session");
                const permanent = await ctx.services.kv.getWithMetadata("forever");
                return new Response(JSON.stringify({
                    expiration: leased.expiration,
                    permanent: permanent.expiration,
                    now: Math.floor(Date.now() / 1000),
                }));
            }
        "#;

        let body = body_of_one_request("kv_expiry_app.production", code, &store.kv_wiring()).await;
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");

        let expiration = parsed["expiration"].as_i64().expect("an expiry");
        let now = parsed["now"].as_i64().expect("the app's clock");
        assert!(
            expiration >= now + 3595 && expiration <= now + 3605,
            "an hour's lifetime should lapse about an hour from now: {body}"
        );
        assert!(
            parsed["permanent"].is_null(),
            "an entry written without a lifetime never lapses: {body}"
        );
    }

    #[tokio::test]
    async fn an_app_reaches_a_real_store_through_its_binding() {
        // The whole chain in one test: the manifest declares kv, the control
        // plane's address reaches the isolate, the op finds a client, and the
        // write carries the lifetime and metadata the app asked for. Every
        // link of that was missing before wave B.
        let store = TestStore::start().await;

        let wiring = ServiceWiring {
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints {
                store: Some(store.endpoint.clone()),
                queue: None,
            },
            consumers: Vec::new(),
        };

        let code = r#"
            export default async function handler(ctx) {
                await ctx.services.kv.put("greeting", "hello", {
                    expirationTtl: 3600,
                    metadata: { source: "test" },
                });
                const value = await ctx.services.kv.get("greeting");
                const missing = await ctx.services.kv.get("nothing-here");
                return new Response(JSON.stringify({ value, missing }));
            }
        "#;

        let mut runtime = AppRuntime::new(
            "e2e_kv_app.production".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &wiring,
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx2) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let (status, _, body) = rx2.await.unwrap().unwrap();
        drop(store);

        assert_eq!(status, 200, "body: {body:?}");
        let body = body.unwrap_or_default();
        assert!(body.contains("\"value\":\"hello\""), "{body}");
        assert!(
            body.contains("\"missing\":null") || !body.contains("\"missing\":\""),
            "a key that was never written reads back as absent: {body}"
        );
    }

    #[tokio::test]
    async fn a_lifetime_that_is_not_a_lifetime_is_refused_before_the_store_sees_it() {
        // A zero or negative expirationTtl would delete the value the instant
        // it was written, which reads as the store losing data.
        let code = r#"
            export default async function handler(ctx) {
                try {
                    await ctx.services.kv.put("k", "v", { expirationTtl: 0 });
                    return new Response("accepted");
                } catch (e) {
                    return new Response(String(e.message || e));
                }
            }
        "#;

        let mut runtime = AppRuntime::new(
            "kv_bad_ttl_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let (_, _, body) = rx.await.unwrap().unwrap();
        let body = body.unwrap_or_default();
        assert!(
            body.contains("expirationTtl"),
            "the app must be told which option it got wrong: {body}"
        );
    }

    #[tokio::test]
    async fn test_execute_fetch() {
        let code = r#"
            export default function handler() {
                    return new Response("Hello from test", {
                        status: 200,
                        headers: { "content-type": "text/plain" }
                    });
            }
        "#;

        let mut runtime = AppRuntime::new(
            "test_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();

        runtime.init_module().await.unwrap();

        let mut headers = HashMap::new();
        headers.insert("host".to_string(), "localhost".to_string());

        let (tx, rx) = oneshot::channel();
        let result = runtime
            .execute_fetch(
                "http://localhost/test".to_string(),
                "GET".to_string(),
                headers,
                None,
                tx,
            )
            .await;

        assert!(result.is_ok());

        let result_data = rx.await.unwrap().unwrap();
        let (status, _headers, body) = result_data;
        assert_eq!(status, 200);
        assert_eq!(body, Some("Hello from test".to_string()));
    }

    #[tokio::test]
    async fn test_execute_fetch_error_handling() {
        let code = r#"
            export default function handler() {
                    throw new Error("Test error");
            }
        "#;

        let mut runtime = AppRuntime::new(
            "test_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();

        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        let result = runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await;

        assert!(result.is_ok());

        let result_data = rx.await.unwrap().unwrap();
        let (status, _headers, body) = result_data;
        assert_eq!(status, 500);
        assert!(body.unwrap().contains("Test error"));
    }

    #[tokio::test]
    async fn test_web_api_console_url_timers() {
        let code = r#"
            export default async function handler(ctx) {
                    console.log("web api ok");
                    const url = new URL(ctx.request.url);
                    await new Promise((resolve) => setTimeout(resolve, 1));
                    return new Response(url.pathname, {
                        status: 200,
                        headers: { "content-type": "text/plain" }
                    });
            }
        "#;

        let mut runtime = AppRuntime::new(
            "test_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();

        runtime.init_module().await.unwrap();

        let mut headers = HashMap::new();
        headers.insert("host".to_string(), "localhost".to_string());

        let (tx, rx) = oneshot::channel();
        let result = runtime
            .execute_fetch(
                "http://localhost/test".to_string(),
                "GET".to_string(),
                headers,
                None,
                tx,
            )
            .await;

        assert!(result.is_ok());

        let result_data = rx.await.unwrap().unwrap();
        let (status, _headers, body) = result_data;
        assert_eq!(status, 200);
        assert_eq!(body, Some("/test".to_string()));
    }

    #[tokio::test]
    async fn test_execute_fetch_populates_route_params() {
        let code = r#"
            export default function handler(context) {
                return new Response(JSON.stringify(context.params), {
                    status: 200,
                    headers: { "content-type": "application/json" }
                });
            }
        "#;

        let mut runtime = AppRuntime::new(
            "test_params_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec {
                route_patterns: vec!["/users/:id".to_string()],
                ..Default::default()
            },
            &ServiceWiring::default(),
        )
        .unwrap();

        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        let result = runtime
            .execute_fetch(
                "http://localhost/users/123".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await;

        assert!(result.is_ok());

        let result_data = rx.await.unwrap().unwrap();
        let (status, _headers, body) = result_data;
        assert_eq!(status, 200);
        assert_eq!(body, Some(r#"{"id":"123"}"#.to_string()));
    }

    #[tokio::test]
    async fn test_execute_fetch_without_matching_route_gives_empty_params() {
        let code = r#"
            export default function handler(context) {
                return new Response(JSON.stringify(context.params));
            }
        "#;

        let mut runtime = AppRuntime::new(
            "test_no_params_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec {
                route_patterns: vec!["/users/:id".to_string()],
                ..Default::default()
            },
            &ServiceWiring::default(),
        )
        .unwrap();

        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        let result = runtime
            .execute_fetch(
                "http://localhost/posts/1".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await;

        assert!(result.is_ok());

        let result_data = rx.await.unwrap().unwrap();
        let (_status, _headers, body) = result_data;
        assert_eq!(body, Some("{}".to_string()));
    }

    #[test]
    fn test_new_stores_configured_timeout_and_rate_limit_overrides() {
        let code = r#"
            export default function handler() {
                return new Response("ok");
            }
        "#;

        let runtime = AppRuntime::new(
            "test_limits_override_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec {
                limits: ExecutionLimits {
                    timeout_ms: 5_000,
                    rate_limit_ops_per_sec: 42.5,
                },
                ..Default::default()
            },
            &ServiceWiring::default(),
        )
        .unwrap();

        // Both values differ from the platform defaults (30_000ms /
        // 1000.0 ops/sec) used elsewhere in this file, so this only passes
        // if the values actually threaded through `AppRuntime::new` rather
        // than falling back to a hard-coded default.
        assert_eq!(runtime.timeout_ms(), 5_000);
        assert_eq!(runtime.rate_limit_ops_per_sec(), 42.5);
    }

    #[test]
    fn test_new_stores_platform_default_limits_when_not_overridden() {
        let code = r#"
            export default function handler() {
                return new Response("ok");
            }
        "#;

        let runtime = AppRuntime::new(
            "test_default_limits_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();

        assert_eq!(runtime.timeout_ms(), 30_000);
        assert_eq!(runtime.rate_limit_ops_per_sec(), 1000.0);
    }

    /// A `console.log` from an application must reach deis-telemetry tagged
    /// with that application. Untagged, the entry is stored but no per-app
    /// query can ever surface it, so `deis tail my-app` stays empty while the
    /// shipper reports success.
    #[tokio::test]
    async fn console_log_is_attributed_to_the_application() {
        use crate::logging::{LogBuffer, RuntimeLayer};
        use std::sync::Arc;
        use tracing_subscriber::prelude::*;

        let code = r#"
            export default function handler() {
                console.log("hello from the app");
                return new Response("ok");
            }
        "#;

        let buffer = Arc::new(LogBuffer::new(16));
        let subscriber = tracing_subscriber::registry()
            .with(RuntimeLayer::new(buffer.clone(), "node-a".to_string()));
        // `set_default` rather than `with_default`: the guard has to stay alive
        // across the awaits below, which a closure-scoped default cannot do.
        let _guard = tracing::subscriber::set_default(subscriber);

        let mut runtime = AppRuntime::new(
            "log_attribution_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        rx.await.unwrap().unwrap();

        let entries = buffer.drain(16);
        let logged = entries
            .iter()
            .find(|e| e.message.contains("hello from the app"))
            .expect("the app's console.log should have been buffered");
        assert_eq!(
            logged.app_id.as_deref(),
            Some("log_attribution_app"),
            "an app log with no app_id is unqueryable in deis-telemetry"
        );
    }

    /// The console bridge has to preserve severity and render non-string
    /// arguments; a log that arrives at the wrong level defeats the `min_level`
    /// filter every telemetry query uses, and one that arrives as
    /// "[object Object]" carries no information at all.
    #[tokio::test]
    async fn console_levels_and_objects_survive_the_bridge() {
        use crate::logging::{LogBuffer, LogLevel, RuntimeLayer};
        use std::sync::Arc;
        use tracing_subscriber::prelude::*;

        let code = r#"
            export default function handler() {
                console.error("boom", { code: 7 });
                console.warn("careful");
                return new Response("ok");
            }
        "#;

        let buffer = Arc::new(LogBuffer::new(16));
        let subscriber = tracing_subscriber::registry()
            .with(RuntimeLayer::new(buffer.clone(), "node-a".to_string()));
        let _guard = tracing::subscriber::set_default(subscriber);

        let mut runtime = AppRuntime::new(
            "console_levels_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        rx.await.unwrap().unwrap();

        let entries = buffer.drain(16);
        let error = entries
            .iter()
            .find(|e| e.message.starts_with("boom"))
            .expect("console.error should have been buffered");
        assert_eq!(error.level, LogLevel::Error);
        assert_eq!(error.message, r#"boom {"code":7}"#);

        let warning = entries
            .iter()
            .find(|e| e.message == "careful")
            .expect("console.warn should have been buffered");
        assert_eq!(warning.level, LogLevel::Warn);
    }
    // --- the global surface ---------------------------------------------
    //
    // This runtime boots the bare `deno_core` engine, which brings only
    // `console` and `queueMicrotask`. Everything else is put on the global
    // object by hand: most of it in `runtime/js/bootstrap.js`, which binds the
    // classes deno_web, deno_fetch and deno_crypto define but never expose
    // (Deno does that in a `99_main.js` this runtime does not use), and the
    // rest in `runtime/js/runtime.js`. That makes the inventory easy to change
    // by accident in either direction - a global quietly added, or one lost in
    // a refactor - and until this test existed nothing noticed.
    //
    // Both halves are asserted on purpose. The present list is what the
    // platform promises; the absent list is what `crates/deis-runtime/README.md`
    // and edge-canon's `PLATFORM_MATRIX.md` say a handler cannot use. Adding a
    // global therefore fails this test until those documents move with it,
    // which is the only thing keeping them honest.

    /// Names SPECIFICATION_BASIC.md section 8.1 requires that this platform
    /// does provide.
    ///
    /// All of them, as of the change that bound the deno extensions' classes
    /// onto the global object. `fetch` is not the raw one: it is the wrapper
    /// in `js/bootstrap.js` that puts every destination past
    /// `runtime/fetch_policy.rs` first.
    const CANON_GLOBALS_PRESENT: &[&str] = &[
        "Request",
        "Response",
        "Headers",
        "URL",
        "URLSearchParams",
        "TextEncoder",
        "TextDecoder",
        "btoa",
        "atob",
        "setTimeout",
        "clearTimeout",
        "fetch",
        "crypto",
        "ReadableStream",
        "WritableStream",
        "TransformStream",
        "setInterval",
        "clearInterval",
    ];

    /// Names section 8.1 requires that this platform does NOT provide.
    ///
    /// Empty, and meant to stay that way: section 8.1 is met in full. A name
    /// that has to be given up again belongs here with the reason, because
    /// this list is what the documents are checked against.
    const CANON_GLOBALS_ABSENT: &[&str] = &[];

    /// Beyond the canon's list: platform extras this runtime does provide.
    const EXTRA_GLOBALS_PRESENT: &[&str] = &[
        "console",
        "queueMicrotask",
        "EventTarget",
        "caches",
        "WebSocketPair",
        "WebSocketServer",
        // Not in section 8.1, but section 8.1's own surface is not usable
        // without them: `AbortController`/`AbortSignal` are the only way to
        // cancel an outbound `fetch`, so without them a slow upstream burns
        // the isolate's whole execution budget, and `DOMException` is the type
        // `crypto.subtle`, the streams and an aborted fetch throw - a handler
        // that cannot name it cannot tell those errors apart.
        "AbortController",
        "AbortSignal",
        "DOMException",
    ];

    /// Beyond the canon's list: things a handler ported from another platform
    /// is likely to reach for and will not find here.
    ///
    /// Several of these are loaded and working inside the isolate - deno_fetch
    /// needs `Blob` and `FormData`, deno_web needs `Event` - and are simply
    /// not bound to a global name. That is the deliberate line: section 8.1
    /// plus what section 8.1 needs, and nothing further, so the surface this
    /// platform promises stays one an operator can reason about.
    const EXTRA_GLOBALS_ABSENT: &[&str] = &[
        "Deno",
        "Blob",
        "File",
        "FormData",
        "URLPattern",
        "WebSocket",
        "BroadcastChannel",
        "structuredClone",
        "Event",
        "performance",
        "navigator",
        "addEventListener",
    ];

    #[tokio::test]
    async fn the_isolate_global_surface_is_the_one_the_documents_describe() {
        let names: Vec<&str> = CANON_GLOBALS_PRESENT
            .iter()
            .chain(CANON_GLOBALS_ABSENT)
            .chain(EXTRA_GLOBALS_PRESENT)
            .chain(EXTRA_GLOBALS_ABSENT)
            .copied()
            .collect();
        let list = names
            .iter()
            .map(|n| format!("\"{n}\""))
            .collect::<Vec<_>>()
            .join(",");

        let code = format!(
            "export default function handler() {{ const names = [{list}]; const out = {{}}; \
             for (const n of names) out[n] = typeof globalThis[n] !== \"undefined\"; \
             return new Response(JSON.stringify(out)); }}"
        );

        let mut runtime = AppRuntime::new(
            "globals_app".to_string(),
            code,
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let (_status, _headers, body) = rx.await.unwrap().unwrap();
        let seen: serde_json::Value = serde_json::from_str(&body.unwrap()).unwrap();

        for name in CANON_GLOBALS_PRESENT.iter().chain(EXTRA_GLOBALS_PRESENT) {
            assert_eq!(
                seen[*name], true,
                "`{name}` is documented as available and is not defined in the isolate"
            );
        }
        for name in CANON_GLOBALS_ABSENT.iter().chain(EXTRA_GLOBALS_ABSENT) {
            assert_eq!(
                seen[*name], false,
                "`{name}` is now defined in the isolate; add it to the present list here and \
                 to crates/deis-runtime/README.md and edge-canon's PLATFORM_MATRIX.md"
            );
        }
    }

    // --- outbound fetch ----------------------------------------------------
    //
    // The policy itself is unit-tested in `runtime/fetch_policy_test.rs`.
    // These drive it the way an application does: through the `fetch` binding
    // in `js/bootstrap.js`, so they also assert that a refusal reaches the
    // handler as a catchable error and that an allowed call really opens a
    // socket.

    /// Run one handler and hand back its body.
    async fn body_of(app: &str, code: &str, spec: IsolateSpec, wiring: &ServiceWiring) -> String {
        let mut runtime =
            AppRuntime::new(app.to_string(), code.to_string(), None, spec, wiring).unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let (status, _headers, body) = rx.await.unwrap().unwrap();
        let body = body.unwrap_or_default();
        assert_eq!(status, 200, "the handler failed: {body}");
        body
    }

    /// A handler that reports what `fetch(url)` did, without letting the
    /// failure escape: a refusal has to be something an application can catch.
    const FETCH_PROBE: &str = r#"
        export default async function handler(ctx) {
            const url = ctx.request.headers.get("x-probe-url") ?? ctx.env.PROBE_URL;
            try {
                const response = await fetch(url);
                return new Response("reached:" + response.status + ":" + (await response.text()));
            } catch (e) {
                return new Response("refused:" + e.constructor.name + ":" + e.message);
            }
        }
    "#;

    fn probe_spec(url: &str, policy: crate::runtime::FetchPolicy) -> IsolateSpec {
        IsolateSpec {
            env: HashMap::from([("PROBE_URL".to_string(), url.to_string())]),
            fetch_policy: policy,
            ..IsolateSpec::default()
        }
    }

    #[tokio::test]
    async fn an_outbound_fetch_to_loopback_is_refused() {
        // deis-store listens on loopback and believes the tenant header it is
        // handed, so this exact call is how one application would read every
        // other application's data.
        let body = body_of(
            "fetch-loopback.production",
            FETCH_PROBE,
            probe_spec(
                "http://127.0.0.1:8090/kv/get",
                crate::runtime::FetchPolicy::default(),
            ),
            &ServiceWiring::default(),
        )
        .await;

        assert!(body.starts_with("refused:TypeError:"), "{body}");
        assert!(body.contains("127.0.0.1"), "{body}");
        assert!(body.contains("loopback"), "{body}");
    }

    #[tokio::test]
    async fn an_outbound_fetch_to_a_name_that_resolves_to_loopback_is_refused() {
        // The case a policy that only read the hostname string would wave
        // through: `localhost` is a name, and a name an attacker controls can
        // be pointed at 127.0.0.1 just as easily.
        let body = body_of(
            "fetch-localhost.production",
            FETCH_PROBE,
            probe_spec(
                "http://localhost:8090/kv/get",
                crate::runtime::FetchPolicy::default(),
            ),
            &ServiceWiring::default(),
        )
        .await;

        assert!(body.starts_with("refused:TypeError:"), "{body}");
        assert!(body.contains("loopback"), "{body}");
    }

    #[tokio::test]
    async fn an_outbound_fetch_into_a_private_range_is_refused() {
        let body = body_of(
            "fetch-private.production",
            FETCH_PROBE,
            probe_spec(
                "http://10.0.0.5:9200/_search",
                crate::runtime::FetchPolicy::default(),
            ),
            &ServiceWiring::default(),
        )
        .await;

        assert!(body.starts_with("refused:TypeError:"), "{body}");
        assert!(body.contains("private range"), "{body}");
    }

    #[tokio::test]
    async fn a_platform_service_endpoint_is_refused_even_when_the_operator_allowlisted_it() {
        // The operator has opened up the whole of 127.0.0.1 - which is already
        // a mistake - and the application still may not reach the store this
        // deployment is wired to. Widening the policy must not be able to
        // widen it into deis-store by accident.
        let wiring = ServiceWiring {
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints {
                store: Some("http://127.0.0.1:8090".to_string()),
                queue: None,
            },
            consumers: Vec::new(),
        };
        let policy = crate::runtime::FetchPolicy::new(["127.0.0.1"], Vec::<String>::new()).unwrap();

        let body = body_of(
            "fetch-platform.production",
            FETCH_PROBE,
            probe_spec("http://127.0.0.1:8090/kv/get", policy),
            &wiring,
        )
        .await;

        assert!(body.starts_with("refused:TypeError:"), "{body}");
        assert!(body.contains("platform"), "{body}");
        assert!(body.contains("ctx.services"), "{body}");
    }

    /// Serve one fixed body on loopback and hand back the address.
    async fn a_server_answering(body: &'static str) -> std::net::SocketAddr {
        use axum::routing::get;
        let router = axum::Router::new().route("/", get(move || async move { body }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        addr
    }

    #[tokio::test]
    async fn an_allowlisted_destination_is_actually_reachable() {
        // The other half of the policy: a refusal that refused everything
        // would pass every test above and be useless. This one opens a real
        // socket to a real server and reads the body back, so the whole path -
        // the guard op, deno_fetch, the response body - has to work.
        let addr = a_server_answering("hello from upstream").await;
        let url = format!("http://{addr}/");
        let policy =
            crate::runtime::FetchPolicy::new([addr.to_string()], Vec::<String>::new()).unwrap();

        let body = body_of(
            "fetch-allowed.production",
            FETCH_PROBE,
            probe_spec(&url, policy),
            &ServiceWiring::default(),
        )
        .await;

        assert_eq!(body, "reached:200:hello from upstream", "{body}");
    }

    #[tokio::test]
    async fn an_allowlisted_name_survives_the_resolver_that_backs_the_policy() {
        // An allowlisted *name* takes the other path: hyper asks
        // `PolicyResolver` for its addresses, and that resolver would drop
        // every loopback answer if it did not consult the allowlist too. If it
        // did, an operator's allowlist would appear to work and then fail at
        // connect time with an unexplained network error.
        let addr = a_server_answering("hello by name").await;
        let url = format!("http://localhost:{}/", addr.port());
        let policy = crate::runtime::FetchPolicy::new(
            [format!("localhost:{}", addr.port())],
            Vec::<String>::new(),
        )
        .unwrap();

        let body = body_of(
            "fetch-allowed-name.production",
            FETCH_PROBE,
            probe_spec(&url, policy),
            &ServiceWiring::default(),
        )
        .await;

        assert_eq!(body, "reached:200:hello by name", "{body}");
    }

    #[tokio::test]
    async fn a_redirect_into_loopback_is_refused_even_though_the_handler_never_named_it() {
        // The destination that matters here is one the application never
        // wrote down: it asks for an allowed address, and that address answers
        // 302 to deis-store. `fetch` follows redirects, and the pre-flight
        // guard runs once, in front of the call, so this hop reaches deno's
        // own permission layer and nothing else. That layer carries the
        // blocked ranges as a deny list precisely for this.
        use axum::response::{IntoResponse, Redirect};
        use axum::routing::get;

        let router = axum::Router::new().route(
            "/",
            get(|| async { Redirect::temporary("http://127.0.0.1:8090/kv/get").into_response() }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        // The redirector is allowlisted by *name*, which is how an operator
        // opens an internal service and is also what keeps this test honest:
        // an allowlist entry that is a literal address is punched out of the
        // range covering it, and allowlisting `127.0.0.1` would punch out the
        // very address the second hop goes to.
        let entry = format!("localhost:{}", addr.port());
        let policy = crate::runtime::FetchPolicy::new([entry], Vec::<String>::new()).unwrap();
        let body = body_of(
            "redirect-chaser.production",
            FETCH_PROBE,
            probe_spec(&format!("http://localhost:{}/", addr.port()), policy),
            &ServiceWiring::default(),
        )
        .await;

        assert!(
            body.starts_with("refused:"),
            "the redirect reached loopback: {body}"
        );
        assert!(body.contains("127.0.0.1"), "{body}");
    }

    #[tokio::test]
    async fn a_handler_can_return_the_response_it_fetched() {
        // `return fetch(upstream)` is the shortest reverse proxy anyone
        // writes, and it is the shape that catches two things at once: the
        // Response `fetch` hands back is a *native* one, so it has to satisfy
        // `instanceof Response` in the dispatcher glue, and it still carries
        // the upstream's own framing headers, which no longer describe the
        // body this node will send.
        let addr = a_server_answering("proxied body").await;
        let url = format!("http://{addr}/");
        let policy =
            crate::runtime::FetchPolicy::new([addr.to_string()], Vec::<String>::new()).unwrap();

        let code = r#"
            export default async function handler(ctx) {
                return await fetch(ctx.env.PROBE_URL);
            }
        "#;

        let mut runtime = AppRuntime::new(
            "proxy.production".to_string(),
            code.to_string(),
            None,
            probe_spec(&url, policy),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let (status, headers, body) = rx.await.unwrap().unwrap();

        assert_eq!(status, 200);
        assert_eq!(body.as_deref(), Some("proxied body"));
        // The upstream sent one; forwarding it would describe a body this node
        // is not sending, and a client would truncate or hang on the
        // difference.
        assert!(
            !headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("content-length")),
            "the upstream's framing was forwarded: {headers:?}"
        );
        // Everything that does describe the body still comes through.
        assert!(
            headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v.contains("text/plain")),
            "{headers:?}"
        );
    }

    // --- the rest of section 8.1 -------------------------------------------

    #[tokio::test]
    async fn web_crypto_is_available_to_a_handler() {
        let code = r#"
            export default async function handler() {
                const bytes = new Uint8Array(16);
                crypto.getRandomValues(bytes);

                const digest = await crypto.subtle.digest(
                    "SHA-256",
                    new TextEncoder().encode("abc"),
                );
                const hex = Array.from(new Uint8Array(digest))
                    .map((b) => b.toString(16).padStart(2, "0"))
                    .join("");

                const key = await crypto.subtle.importKey(
                    "raw",
                    new TextEncoder().encode("secret"),
                    { name: "HMAC", hash: "SHA-256" },
                    false,
                    ["sign"],
                );
                const mac = await crypto.subtle.sign("HMAC", key, new TextEncoder().encode("abc"));

                return new Response(JSON.stringify({
                    // 16 zero bytes coming back would mean getRandomValues did
                    // nothing, which is the failure worth catching here.
                    filled: bytes.some((b) => b !== 0),
                    sha256OfAbc: hex,
                    uuidShape: /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/
                        .test(crypto.randomUUID()),
                    macBytes: mac.byteLength,
                }));
            }
        "#;

        let seen: serde_json::Value = serde_json::from_str(
            &body_of(
                "crypto_app",
                code,
                IsolateSpec::default(),
                &ServiceWiring::default(),
            )
            .await,
        )
        .unwrap();

        assert_eq!(seen["filled"], true);
        // The published SHA-256 of "abc"; a digest that is merely 32 bytes
        // long proves nothing.
        assert_eq!(
            seen["sha256OfAbc"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(seen["uuidShape"], true);
        assert_eq!(seen["macBytes"], 32);
    }

    #[tokio::test]
    async fn the_stream_types_are_available_to_a_handler() {
        let code = r#"
            export default async function handler() {
                const readable = new ReadableStream({
                    start(controller) {
                        controller.enqueue("one ");
                        controller.enqueue("two");
                        controller.close();
                    },
                });

                const upper = new TransformStream({
                    transform(chunk, controller) {
                        controller.enqueue(chunk.toUpperCase());
                    },
                });

                const collected = [];
                const writable = new WritableStream({
                    write(chunk) {
                        collected.push(chunk);
                    },
                });

                await readable.pipeThrough(upper).pipeTo(writable);

                // A Response reads its body off a stream, so this is also the
                // check that the streams the platform binds are the ones
                // deno_fetch is built on.
                const streamed = await new Response(
                    new ReadableStream({
                        start(controller) {
                            controller.enqueue(new TextEncoder().encode("streamed body"));
                            controller.close();
                        },
                    }),
                ).text();

                return new Response(JSON.stringify({ collected, streamed }));
            }
        "#;

        let seen: serde_json::Value = serde_json::from_str(
            &body_of(
                "streams_app",
                code,
                IsolateSpec::default(),
                &ServiceWiring::default(),
            )
            .await,
        )
        .unwrap();

        assert_eq!(seen["collected"][0], "ONE ");
        assert_eq!(seen["collected"][1], "TWO");
        assert_eq!(seen["streamed"], "streamed body");
    }

    #[tokio::test]
    async fn a_timeout_fires_and_an_interval_repeats() {
        let code = r#"
            export default async function handler() {
                const started = Date.now();
                await new Promise((resolve) => setTimeout(resolve, 20));
                const slept = Date.now() - started >= 15;

                let ticks = 0;
                await new Promise((resolve) => {
                    const id = setInterval(() => {
                        ticks++;
                        if (ticks === 3) {
                            clearInterval(id);
                            resolve();
                        }
                    }, 1);
                });

                return new Response(JSON.stringify({ slept, ticks }));
            }
        "#;

        let seen: serde_json::Value = serde_json::from_str(
            &body_of(
                "timers_app",
                code,
                IsolateSpec::default(),
                &ServiceWiring::default(),
            )
            .await,
        )
        .unwrap();

        // The polyfill these replaced ignored the delay and ran the callback
        // on the microtask queue, so `setTimeout(f, 20)` returned immediately.
        assert_eq!(seen["slept"], true);
        assert_eq!(seen["ticks"], 3);
    }

    #[tokio::test]
    async fn an_interval_does_not_outlive_the_request_that_created_it() {
        // An isolate is reused across requests. An interval left running would
        // keep firing inside every later request - burning CPU that the
        // request being served is charged for, and spending that isolate's
        // rate-limit budget - for the life of the application. See the timers
        // section of js/bootstrap.js.
        //
        // Two requests: the first starts an interval, the second starts none.
        // If the first one's interval survived, the second would see its
        // counter move while it was doing nothing but sleeping.
        let code = r#"
            globalThis.__ticks = 0;
            export default async function handler(ctx) {
                const before = globalThis.__ticks;
                if (ctx.request.headers.get("x-start-interval") === "yes") {
                    setInterval(() => { globalThis.__ticks++; }, 1);
                }
                await new Promise((resolve) => setTimeout(resolve, 30));
                return new Response(JSON.stringify({ before, after: globalThis.__ticks }));
            }
        "#;

        let mut runtime = AppRuntime::new(
            "interval-leak.production".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let mut seen = Vec::new();
        for start_interval in [true, false] {
            let mut headers = HashMap::new();
            if start_interval {
                headers.insert("x-start-interval".to_string(), "yes".to_string());
            }
            let (tx, rx) = oneshot::channel();
            runtime
                .execute_fetch(
                    "http://localhost/".to_string(),
                    "GET".to_string(),
                    headers,
                    None,
                    tx,
                )
                .await
                .unwrap();
            let (status, _headers, body) = rx.await.unwrap().unwrap();
            let body = body.unwrap_or_default();
            assert_eq!(status, 200, "{body}");
            seen.push(serde_json::from_str::<serde_json::Value>(&body).unwrap());
        }

        // The interval did run while the request that created it was running:
        // the sweep is the end of an invocation, not a ban on intervals.
        assert!(
            seen[0]["after"].as_i64().unwrap() > seen[0]["before"].as_i64().unwrap(),
            "the interval never fired at all: {:?}",
            seen[0]
        );
        // ... and nothing ticked during the second request, which started no
        // interval of its own. A survivor would have fired around a dozen
        // times in the 30ms that request spent asleep.
        assert_eq!(
            seen[1]["after"], seen[1]["before"],
            "an interval from the first request was still firing during the second: {seen:?}"
        );
    }

    #[tokio::test]
    async fn utf8_round_trips_through_the_encoding_globals() {
        // Not a formality: `request.arrayBuffer()`, `blob.put` with a string,
        // `blob.get(...).text()` and binary WebSocket frames all call these,
        // and every one of them threw ReferenceError while they were missing.
        let code = r#"
            export default function handler() {
                const enc = new TextEncoder();
                const dec = new TextDecoder();
                const text = "canon é €€ 🚀";

                const bytes = enc.encode(text);
                const out = {
                    encoding: enc.encoding,
                    isUint8Array: bytes instanceof Uint8Array,
                    // The euro sign is three bytes and the rocket four, so a
                    // per-character count would be wrong here and this would
                    // say so.
                    byteLength: bytes.length,
                    roundTrip: dec.decode(bytes) === text,
                    decodesArrayBuffer: dec.decode(bytes.buffer) === text,
                    emptyIsEmptyString: dec.decode(new Uint8Array([])) === "",
                    // A byte order mark is metadata, not the first character.
                    bomStripped: dec.decode(new Uint8Array([0xef, 0xbb, 0xbf, 0x68, 0x69])) === "hi",
                    // A truncated sequence becomes U+FFFD rather than throwing
                    // or silently producing a wrong character.
                    invalidReplaced: dec.decode(new Uint8Array([0x68, 0xff, 0x69])) === "h�i",
                    base64: btoa("Hello World"),
                    base64RoundTrip: atob(btoa("Hello World")) === "Hello World",
                };

                try {
                    new TextDecoder("iso-8859-1");
                    out.rejectsOtherEncodings = false;
                } catch (e) {
                    out.rejectsOtherEncodings = e instanceof RangeError;
                }

                try {
                    btoa("é€");
                    out.btoaRejectsNonLatin1 = false;
                } catch (e) {
                    out.btoaRejectsNonLatin1 = e instanceof TypeError;
                }

                try {
                    atob("not base64!");
                    out.atobRejectsGarbage = false;
                } catch (e) {
                    out.atobRejectsGarbage = e instanceof TypeError;
                }

                return new Response(JSON.stringify(out));
            }
        "#;

        let mut runtime = AppRuntime::new(
            "encoding_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let (status, _headers, body) = rx.await.unwrap().unwrap();
        assert_eq!(status, 200);
        let seen: serde_json::Value = serde_json::from_str(&body.unwrap()).unwrap();

        assert_eq!(seen["encoding"], "utf-8");
        assert_eq!(seen["isUint8Array"], true);
        // "canon " 6 + "é" 2 + " " 1 + two euro signs 6 + " " 1 + rocket 4 = 20.
        assert_eq!(seen["byteLength"], 20);
        assert_eq!(seen["roundTrip"], true, "{seen}");
        assert_eq!(seen["decodesArrayBuffer"], true);
        assert_eq!(seen["emptyIsEmptyString"], true);
        assert_eq!(seen["bomStripped"], true);
        assert_eq!(seen["invalidReplaced"], true);
        assert_eq!(seen["base64"], "SGVsbG8gV29ybGQ=");
        assert_eq!(seen["base64RoundTrip"], true);
        assert_eq!(seen["rejectsOtherEncodings"], true);
        assert_eq!(seen["btoaRejectsNonLatin1"], true);
        assert_eq!(seen["atobRejectsGarbage"], true);
    }

    #[tokio::test]
    async fn a_request_body_can_be_read_as_bytes() {
        // `arrayBuffer()` is how a handler gets at a body that is not text.
        // It went through TextEncoder, so it threw ReferenceError on every
        // call until the encoders above existed - and no test covered it.
        let code = r#"
            export default async function handler(context) {
                const buf = await context.request.arrayBuffer();
                const bytes = new Uint8Array(buf);
                return new Response(JSON.stringify({
                    byteLength: buf.byteLength,
                    first: bytes[0],
                    text: new TextDecoder().decode(bytes),
                }));
            }
        "#;

        let mut runtime = AppRuntime::new(
            "array_buffer_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "POST".to_string(),
                HashMap::new(),
                Some("héllo".to_string()),
                tx,
            )
            .await
            .unwrap();
        let (status, _headers, body) = rx.await.unwrap().unwrap();
        assert_eq!(status, 200);
        let seen: serde_json::Value = serde_json::from_str(&body.unwrap()).unwrap();
        assert_eq!(seen["byteLength"], 6);
        assert_eq!(seen["first"], 104);
        assert_eq!(seen["text"], "héllo");
    }

    #[tokio::test]
    async fn an_app_stores_and_reads_bytes_through_its_blob_binding() {
        // Object storage had two independent breaks and no test on the path,
        // so the platform matrix called it working for a long time while a
        // single `blob.put(key, "text")` threw. One break was in the op - the
        // bytes went through `#[serde]`, which rejects a Uint8Array outright -
        // and the other was in the JS, which reached for TextEncoder and
        // TextDecoder that this runtime did not define.
        let store = TestStore::start().await;

        let wiring = ServiceWiring {
            declaration: ServiceDeclaration::from_names(["blob"]),
            endpoints: ServiceEndpoints {
                store: Some(store.endpoint.clone()),
                queue: None,
            },
            consumers: Vec::new(),
        };

        let code = r#"
            export default async function handler(ctx) {
                const blob = ctx.services.blob;

                // A string, which has to be encoded on the way in.
                await blob.put("greeting.txt", "hello é€🚀");
                const text = await blob.get("greeting.txt");

                // Raw bytes, which must survive unchanged.
                const raw = new Uint8Array([0, 1, 2, 253, 254, 255]);
                await blob.put("raw.bin", raw);
                const back = await blob.get("raw.bin");
                const bytes = new Uint8Array(await back.arrayBuffer());

                const missing = await blob.get("never-written");

                await blob.delete("greeting.txt");
                const deleted = await blob.get("greeting.txt");

                return new Response(JSON.stringify({
                    text: await text.text(),
                    roundTripBytes: Array.from(bytes),
                    missingIsNull: missing === null,
                    deletedIsNull: deleted === null,
                }));
            }
        "#;

        let mut runtime = AppRuntime::new(
            "e2e_blob_app.production".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &wiring,
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let (status, _, body) = rx.await.unwrap().unwrap();
        drop(store);

        assert_eq!(status, 200, "body: {body:?}");
        let seen: serde_json::Value = serde_json::from_str(&body.unwrap()).unwrap();
        assert_eq!(seen["text"], "hello é€🚀");
        assert_eq!(
            seen["roundTripBytes"],
            serde_json::json!([0, 1, 2, 253, 254, 255])
        );
        assert_eq!(seen["missingIsNull"], true);
        assert_eq!(seen["deletedIsNull"], true);
    }
}
