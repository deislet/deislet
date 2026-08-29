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
        }
    }

    #[tokio::test]
    async fn a_queue_handler_sees_the_batch_and_can_fail_single_messages() {
        // Half a batch failing must be expressible: acknowledging everything
        // would lose the failure, failing everything would redo work that
        // already succeeded.
        let code = r#"
            export default function handler() { return new Response("unused"); }
            export async function queue(batch, context) {
                globalThis.seen = batch.messages.map((m) => m.id + ":" + m.body + ":" + m.attempts);
                globalThis.queueName = batch.queue;
                globalThis.hasServices = typeof context.services.kv === "object";
                for (const message of batch.messages) {
                    if (message.body.includes("poison")) message.retry();
                }
            }
        "#;

        let mut runtime = AppRuntime::new(
            "queue_ok_app".to_string(),
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
        let code = r#"
            export default function handler() { return new Response("unused"); }
            export async function scheduled(event, context) {
                if (event.cron !== "0 */6 * * *") throw new Error("wrong expression: " + event.cron);
                if (event.scheduledTime !== 1700000000000) throw new Error("wrong time");
                if (typeof context.services.kv !== "object") throw new Error("no services");
            }
        "#;

        let mut runtime = AppRuntime::new(
            "cron_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
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

    #[tokio::test]
    async fn an_app_reaches_a_real_store_through_its_binding() {
        // The whole chain in one test: the manifest declares kv, the control
        // plane's address reaches the isolate, the op finds a client, and the
        // write carries the lifetime and metadata the app asked for. Every
        // link of that was missing before wave B.
        let data = tempfile::TempDir::new().expect("temp dir");
        let config = deis_store::config::Config {
            listen: "127.0.0.1:0".to_string(),
            data_dir: data.path().to_path_buf(),
            ..deis_store::config::Config::default()
        };
        let listener = deis_store::server::bind(&config).await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let (shutdown, rx) = oneshot::channel();
        tokio::spawn(async move {
            deis_store::server::serve(config, listener, async {
                let _ = rx.await;
            })
            .await
            .expect("serve");
        });

        let wiring = ServiceWiring {
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints {
                store: Some(format!("http://{addr}")),
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
        let _ = shutdown.send(());

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
    #[ignore]
    async fn test_execute_fetch_with_kv() {
        let code = r#"
            export default async function handler(ctx) {
                    await ctx.services.kv.put("test", "value123");
                    const val = await ctx.services.kv.get("test");
                    return new Response(val);
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
        let (_status, _headers, body) = result_data;
        assert_eq!(body, Some("value123".to_string()));
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
}
