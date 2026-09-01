#[cfg(test)]
mod tests {
    use crate::config::ExecutionLimits;
    use crate::runtime::bindings::{ServiceDeclaration, ServiceEndpoints, ServiceWiring};
    use crate::runtime::isolate::{AppRuntime, IsolateSpec};
    use crate::runtime::metrics::{
        recorder, CPU_CUTOFFS_TOTAL, CPU_MS_SUM, EXECUTION_MS_COUNT, REQUESTS_TOTAL,
        REQUEST_ERRORS_TOTAL, TIMEOUT_CUTOFFS_TOTAL,
    };
    use crate::runtime::queue_client::LeasedMessage;
    use crate::runtime::{ResponseBody, ServiceRouter};
    use std::collections::HashMap;
    use tokio::sync::oneshot;

    /// A response body as text.
    ///
    /// The body crosses as bytes now, because a response is not always text.
    /// Most handlers in this file answer with a string, and this is the one
    /// place that says so; a body that is not UTF-8 here is a bug in the test,
    /// not in the runtime.
    fn text(body: ResponseBody) -> Option<String> {
        body.into_bytes()
            .map(|bytes| String::from_utf8(bytes).expect("this test's handler answers with text"))
    }

    /// A request body written as a string, for the tests whose payload is one.
    fn sent(body: &str) -> Option<Vec<u8>> {
        Some(body.as_bytes().to_vec())
    }

    /// Read a streaming body to its end, driving the isolate as
    /// `runtime/pool.rs` does.
    ///
    /// The two halves have to run together: the producer only advances while
    /// the isolate's event loop is being driven, and the pump only finishes
    /// when the reader has taken everything. Awaiting either one alone hangs,
    /// which is exactly the arrangement the runtime thread makes.
    async fn drain_stream(
        runtime: &mut AppRuntime,
        body: ResponseBody,
    ) -> (Vec<u8>, Option<String>) {
        let ResponseBody::Stream(mut reader) = body else {
            panic!("this test expected a body that is still being produced");
        };
        let read = async {
            let mut bytes = Vec::new();
            while let Some(chunk) = std::future::poll_fn(|cx| reader.poll_chunk(cx)).await {
                bytes.extend_from_slice(&chunk);
            }
            (bytes, reader.failure())
        };
        let (collected, ()) = tokio::join!(read, runtime.pump_response_streams());
        collected
    }

    /// Ask for one path and hand back whatever the handler answered.
    async fn answer_to(
        runtime: &mut AppRuntime,
        path: &str,
    ) -> (u16, crate::runtime::HeaderPairs, ResponseBody) {
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
        rx.await.expect("a response").expect("a response")
    }

    #[tokio::test]
    async fn test_transform_esm_typescript() {
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        );
        assert!(
            result.is_ok(),
            "Failed to parse TypeScript ESM: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_transform_counter_app() {
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        );
        assert!(
            result.is_ok(),
            "Failed to parse counter app: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_transform_with_interface() {
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        );
        assert!(
            result.is_ok(),
            "Failed to parse TypeScript with interface: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_transform_echo_app() {
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        );
        assert!(
            result.is_ok(),
            "Failed to parse echo app: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_create_runtime_with_valid_code() {
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        );
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_create_runtime_with_invalid_code() {
        let invalid_code = "this is not valid javascript {{{";
        let result = AppRuntime::new(
            "test_app".to_string(),
            invalid_code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        );
        assert!(result.is_ok());

        let runtime = result.unwrap();
        assert_eq!(runtime.app_id(), "test_app");
    }

    // --- queue consumption -------------------------------------------------

    fn leased(id: &str, body: &str, attempts: i32) -> LeasedMessage {
        LeasedMessage {
            id: id.to_string(),
            body: body.as_bytes().to_vec(),
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
    async fn a_request_body_of_raw_bytes_reaches_the_handler_unchanged() {
        // The request used to be serialised into the *source text* of the
        // invocation script, so a body that was not valid UTF-8 could not
        // survive the trip at all: an upload -- an image, a protobuf, a zip --
        // reached the handler mangled. The bytes travel on their own channel
        // now; this is the test that would notice if they went back into JSON.
        let code = r#"
            export default async function handler(context) {
                const bytes = new Uint8Array(await context.request.arrayBuffer());
                // Echoed back as bytes, so the return leg is under test too.
                return new Response(bytes);
            }
        "#;

        let mut runtime = AppRuntime::new(
            "raw_body_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();

        // Every byte value once, so a lossy step anywhere shows up: 0x00 ends
        // a C string, 0x80..=0xFF are not valid UTF-8 on their own, and the
        // quote and backslash are what a JSON round trip would escape.
        let payload: Vec<u8> = (0u8..=255).collect();
        assert!(
            String::from_utf8(payload.clone()).is_err(),
            "the fixture has to be bytes a string could not hold"
        );

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/upload".to_string(),
                "POST".to_string(),
                HashMap::new(),
                Some(payload.clone()),
                tx,
            )
            .await
            .unwrap();
        let (status, _headers, body) = rx.await.unwrap().unwrap();

        assert_eq!(status, 200);
        assert_eq!(
            body.into_bytes().as_deref(),
            Some(&payload[..]),
            "the body did not survive the trip through the isolate byte for byte"
        );
    }

    #[tokio::test]
    async fn a_response_that_carries_no_body_is_not_given_an_empty_one() {
        // A 204 and a 204 with a zero-length body are different answers, and
        // an isolate is reused: the body of the previous request must not be
        // handed to the next one either.
        let code = r#"
            export default async function handler(context) {
                if (context.request.url.endsWith("/empty")) {
                    return new Response(null, { status: 204 });
                }
                return new Response("first");
            }
        "#;

        let mut runtime = AppRuntime::new(
            "no_body_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/first".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let (status, _, body) = rx.await.unwrap().unwrap();
        assert_eq!(status, 200);
        assert_eq!(text(body).as_deref(), Some("first"));

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/empty".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .unwrap();
        let (status, _, body) = rx.await.unwrap().unwrap();
        assert_eq!(status, 204);
        assert!(
            matches!(body, ResponseBody::None),
            "a body-less answer must stay body-less, and must not inherit the last one"
        );
    }

    #[tokio::test]
    async fn a_request_body_is_handed_to_the_isolate_once_and_not_again() {
        // The bytes are *taken* out of the isolate's state rather than copied
        // from it. Leaving them there would let the next request -- an isolate
        // serves many -- read a body belonging to one already answered, which
        // is one application's payload delivered to another caller.
        let code = r#"
            export default async function handler(context) {
                return new Response(JSON.stringify({
                    body: await context.request.text()
                }));
            }
        "#;

        let mut runtime = AppRuntime::new(
            "once_body_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "POST".to_string(),
                HashMap::new(),
                sent("only once"),
                tx,
            )
            .await
            .unwrap();
        let (status, _, body) = rx.await.unwrap().unwrap();
        assert_eq!(status, 200);
        let seen: serde_json::Value = serde_json::from_str(&text(body).unwrap()).unwrap();
        assert_eq!(seen["body"], "only once");

        // The next request carries nothing: the previous body must be gone,
        // not still waiting in the isolate.
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
        let seen: serde_json::Value = serde_json::from_str(&text(body).unwrap()).unwrap();
        assert_eq!(
            seen["body"], "",
            "the next request read the previous request's body"
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "POST".to_string(),
                HashMap::new(),
                sent("hello body"),
                tx,
            )
            .await
            .unwrap();
        let (status, _headers, body) = rx.await.unwrap().unwrap();
        let body = text(body);

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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "POST".to_string(),
                HashMap::new(),
                sent(r#"{"key":"value"}"#),
                tx,
            )
            .await
            .unwrap();
        let (status, _headers, body) = rx.await.unwrap().unwrap();
        let body = text(body);

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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "POST".to_string(),
                HashMap::new(),
                sent("once"),
                tx,
            )
            .await
            .unwrap();
        let (_, _, body) = rx.await.unwrap().unwrap();
        let body = text(body);
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        let body = text(body);

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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        let body = text(body);

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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        let body = text(body);

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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        let body = text(body);

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
            storage_token: None,
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints {
                store: Some("http://127.0.0.1:1".to_string()),
                queue: None,
            },
            consumers: Vec::new(),
            ..ServiceWiring::default()
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
            let body = text(body);
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        let body = text(body);
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
    async fn a_kv_value_that_is_not_valid_utf8_round_trips_through_an_app() {
        // KV values were typed as `string` all the way down, so a value that
        // is not text could not be stored at all: the write encoded it as
        // UTF-8 and the read decoded it again, and every byte outside ASCII
        // came back as something else. An app that caches a rendered image or
        // a compressed payload in KV is the ordinary case for this.
        let store = TestStore::start().await;
        let code = r#"
            export default async function handler(ctx) {
                // Every byte value once, including 0x00 and the 0x80..0xFF
                // range that is not valid UTF-8 on its own.
                const written = new Uint8Array(256);
                for (let i = 0; i < 256; i++) written[i] = i;

                await ctx.services.kv.put("blob", written);
                const read = new Uint8Array(
                    await ctx.services.kv.get("blob", { type: "arrayBuffer" })
                );

                let identical = read.length === written.length;
                for (let i = 0; identical && i < written.length; i++) {
                    if (read[i] !== written[i]) identical = false;
                }
                return new Response(JSON.stringify({
                    identical,
                    length: read.length,
                    first: read[0],
                    last: read[255],
                }));
            }
        "#;

        let body = body_of_one_request("kv_bytes_app.production", code, &store.kv_wiring()).await;
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(parsed["length"], 256, "{body}");
        assert_eq!(parsed["first"], 0, "{body}");
        assert_eq!(parsed["last"], 255, "{body}");
        assert_eq!(
            parsed["identical"], true,
            "the bytes did not survive the round trip through KV: {body}"
        );
    }

    #[tokio::test]
    async fn kv_reads_answer_in_the_shape_the_caller_asked_for() {
        // The types applications are already written against: text by
        // default, and json / arrayBuffer on request. A read that ignored the
        // option would hand a handler a string where it expected an object.
        let store = TestStore::start().await;
        let code = r#"
            export default async function handler(ctx) {
                await ctx.services.kv.put("doc", JSON.stringify({ n: 7 }));

                const plain = await ctx.services.kv.get("doc");
                const asText = await ctx.services.kv.get("doc", { type: "text" });
                const asJson = await ctx.services.kv.get("doc", { type: "json" });
                const asBuffer = await ctx.services.kv.get("doc", { type: "arrayBuffer" });

                return new Response(JSON.stringify({
                    plainIsString: typeof plain === "string",
                    plain,
                    textMatches: asText === plain,
                    jsonIsObject: typeof asJson === "object" && asJson !== null,
                    n: asJson.n,
                    bufferIsArrayBuffer: asBuffer instanceof ArrayBuffer,
                    bufferLength: asBuffer.byteLength,
                    missing: await ctx.services.kv.get("absent", { type: "json" }),
                }));
            }
        "#;

        let body = body_of_one_request("kv_types_app.production", code, &store.kv_wiring()).await;
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        assert_eq!(parsed["plainIsString"], true, "{body}");
        assert_eq!(parsed["plain"], r#"{"n":7}"#, "{body}");
        assert_eq!(parsed["textMatches"], true, "{body}");
        assert_eq!(parsed["jsonIsObject"], true, "{body}");
        assert_eq!(parsed["n"], 7, "{body}");
        assert_eq!(parsed["bufferIsArrayBuffer"], true, "{body}");
        assert_eq!(parsed["bufferLength"], 7, "{body}");
        assert!(
            parsed["missing"].is_null(),
            "a key that is not there is null whatever type was asked for: {body}"
        );
    }

    #[tokio::test]
    async fn a_kv_read_type_this_platform_does_not_implement_is_refused() {
        // "stream" is a name in the same set as the three that work, and
        // there is no streaming read behind it here: op_kv_get answers with
        // the whole value. Ignoring the option and handing back text would
        // give the handler something that is not a ReadableStream and fail on
        // the first method it called, far from the call that was wrong.
        let store = TestStore::start().await;
        let code = r#"
            export default async function handler(ctx) {
                await ctx.services.kv.put("k", "v");
                const attempts = {};
                for (const type of ["stream", "nonsense"]) {
                    try {
                        await ctx.services.kv.get("k", { type });
                        attempts[type] = "accepted";
                    } catch (e) {
                        attempts[type] = String(e.message || e);
                    }
                }
                return new Response(JSON.stringify(attempts));
            }
        "#;

        let body =
            body_of_one_request("kv_bad_type_app.production", code, &store.kv_wiring()).await;
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        let refusal = parsed["stream"].as_str().expect("a message");
        assert!(
            refusal.contains("stream") && refusal.contains("not available"),
            "the refusal has to name the type and say it is not implemented: {body}"
        );
        assert!(
            parsed["nonsense"]
                .as_str()
                .expect("a message")
                .contains("unknown type"),
            "a type that is not in the set at all is refused too: {body}"
        );
    }

    #[tokio::test]
    async fn kv_put_takes_a_string_a_buffer_or_a_view() {
        // The three shapes an application has to hand: a string, the
        // ArrayBuffer a `fetch` response or a file read produces, and the
        // typed-array view everything else works in. Anything else is a
        // mistake worth naming rather than storing "[object Object]".
        let store = TestStore::start().await;
        let code = r#"
            export default async function handler(ctx) {
                const bytes = new Uint8Array([104, 105]); // "hi"

                await ctx.services.kv.put("as-string", "hi");
                await ctx.services.kv.put("as-buffer", bytes.buffer);
                await ctx.services.kv.put("as-view", bytes);
                // A view over part of a larger buffer must store its own
                // window, not the whole allocation behind it.
                const wide = new Uint8Array([0, 104, 105, 0]);
                await ctx.services.kv.put("as-slice", wide.subarray(1, 3));

                let refused = "accepted";
                try {
                    await ctx.services.kv.put("as-object", { not: "bytes" });
                } catch (e) {
                    refused = String(e.message || e);
                }

                return new Response(JSON.stringify({
                    string: await ctx.services.kv.get("as-string"),
                    buffer: await ctx.services.kv.get("as-buffer"),
                    view: await ctx.services.kv.get("as-view"),
                    slice: await ctx.services.kv.get("as-slice"),
                    refused,
                }));
            }
        "#;

        let body =
            body_of_one_request("kv_put_shapes_app.production", code, &store.kv_wiring()).await;
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        for shape in ["string", "buffer", "view", "slice"] {
            assert_eq!(parsed[shape], "hi", "{shape} did not round trip: {body}");
        }
        assert!(
            parsed["refused"]
                .as_str()
                .expect("a message")
                .contains("ArrayBuffer"),
            "a value that is none of the three shapes has to say what is accepted: {body}"
        );
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
            storage_token: None,
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints {
                store: Some(store.endpoint.clone()),
                queue: None,
            },
            consumers: Vec::new(),
            ..ServiceWiring::default()
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        let body = text(body);
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        let body = text(body);
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        let body = text(body);
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        let body = text(body);
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        let body = text(body);
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        let body = text(body);
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        let body = text(body);
        assert_eq!(body, Some("{}".to_string()));
    }

    #[tokio::test]
    async fn test_new_stores_configured_timeout_and_rate_limit_overrides() {
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
                    wait_until_timeout_ms: 7_500,
                    stream_timeout_ms: 60_000,
                    rate_limit_ops_per_sec: 42.5,
                    cpu_time_limit_ms: 1_250,
                    max_subrequests: 50,
                },
                ..Default::default()
            },
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();

        // Every value differs from the platform defaults (30_000ms / 30_000ms
        // / 1000.0 ops/sec) used elsewhere in this file, so this only passes
        // if the values actually threaded through `AppRuntime::new` rather
        // than falling back to a hard-coded default. The tail budget is the
        // one that is easy to get wrong: it is configured separately from the
        // request budget, so an isolate built with the request's value in that
        // slot would still look correct until a tail ran.
        assert_eq!(runtime.timeout_ms(), 5_000);
        assert_eq!(runtime.wait_until_timeout_ms(), 7_500);
        assert_eq!(runtime.rate_limit_ops_per_sec(), 42.5);
        assert_eq!(runtime.cpu_time_limit_ms(), 1_250);
    }

    #[tokio::test]
    async fn test_new_stores_platform_default_limits_when_not_overridden() {
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();

        assert_eq!(runtime.timeout_ms(), 30_000);
        assert_eq!(runtime.wait_until_timeout_ms(), 30_000);
        assert_eq!(runtime.rate_limit_ops_per_sec(), 1000.0);
        assert_eq!(runtime.cpu_time_limit_ms(), 5_000);
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        // The outbound socket, and policed: not deno_websocket's client, which
        // dials with a connector of its own that never reaches
        // `runtime/fetch_policy.rs`. This one is `runtime/ws_client_ops.rs`,
        // which puts every destination through the same decision point an
        // outbound `fetch` goes through and then connects to the address that
        // was judged, so a name resolving to loopback, to a private range or
        // to one of this platform's own services is refused here too. Its
        // sockets are bounded by the invocation's execution budget and closed
        // when the invocation ends.
        "WebSocket",
        // Not in section 8.1, but section 8.1's own surface is not usable
        // without them: `AbortController`/`AbortSignal` are the only way to
        // cancel an outbound `fetch`, so without them a slow upstream burns
        // the isolate's whole execution budget, and `DOMException` is the type
        // `crypto.subtle`, the streams and an aborted fetch throw - a handler
        // that cannot name it cannot tell those errors apart.
        "AbortController",
        "AbortSignal",
        "DOMException",
        // `request.formData()` hands back a `FormData` whose file entries are
        // `File`s, so a multipart upload is unreadable without these three
        // names: `entry instanceof File` is the check every upload handler
        // makes, and it was a ReferenceError. There was also no way to build
        // a multipart body to send one on. `Blob` is the type the other two
        // are defined in terms of, and what `response.blob()` had been
        // returning all along without an application being able to name it.
        "Blob",
        "File",
        "FormData",
        // What a router matches a path against, and routing is the first
        // thing a request handler does - so this one is missed on line one of
        // a ported application rather than in some corner of it.
        "URLPattern",
        // Deep copy. The fallback without it is
        // `JSON.parse(JSON.stringify(x))`, which drops `undefined`, functions,
        // `Map` and `Set`, turns a `Date` into a string and throws on a cycle.
        "structuredClone",
        // `EventTarget` above is furniture without an event to dispatch
        // through it, and `CustomEvent` comes with it because carrying a
        // payload is the only reason an application fires one of its own.
        "Event",
        "CustomEvent",
        // Deliberately coarse: `now()` is clamped to whole milliseconds and
        // `timeOrigin` is a fixed 0, because a sub-millisecond clock is the
        // standard building block of a cross-isolate timing side channel and
        // this node runs several tenants' isolates on one OS thread. Browsers
        // and the edge platform this one is modelled on coarsen it the same
        // way. See the section in `js/bootstrap.js`.
        "performance",
        // This runtime's own frozen object, carrying `userAgent` and nothing
        // else, so a library choosing between a browser, Node and an edge
        // runtime can tell where it is. Every other member of the browser's
        // `navigator` would either describe the node - the operator's
        // business, not the tenant's - or be a promise this platform cannot
        // keep.
        "navigator",
    ];

    /// Beyond the canon's list: things a handler ported from another platform
    /// is likely to reach for and will not find here.
    ///
    /// Short, and every entry carries its own reason. Three of them are
    /// permanent decisions and one is a genuine gap. Nothing is on this list
    /// merely
    /// because it was loaded and never bound - that was the state this list
    /// used to describe, and it is no longer a reason to withhold a name.
    const EXTRA_GLOBALS_ABSENT: &[&str] = &[
        "Deno",
        // `deno_core` publishes its op table on both `globalThis.Deno.core`
        // and `globalThis.__bootstrap.core`. A full Deno boot deletes the
        // latter in `99_main.js`; this runtime does not run that, so
        // `runtime/js/runtime.js` deletes it by hand next to `Deno`. Either
        // one left standing hands an application `core.ops.op_net_connect_tcp`
        // and every other op, which is a total bypass of the outbound fetch
        // policy. See `an_application_cannot_reach_the_raw_op_table`.
        "__bootstrap",
        // The service-worker authoring style - `addEventListener("fetch", e =>
        // e.respondWith(...))` - is not what this platform runs. Edge Canon is
        // module syntax: `export default function handler(context)`, and
        // `js/runtime.js` calls that export and nothing else. A global that
        // accepted a "fetch" listener and then never fired it would be worse
        // than no global at all: the application would boot, register its
        // handler, answer nothing, and the failure would look like a platform
        // fault rather than a porting mistake. A ReferenceError on the
        // registration line says where the problem actually is. `EventTarget`,
        // `Event` and `CustomEvent` are all present, so an application that
        // wants an event bus of its own still has one - what is refused is the
        // *global* one, which implies a dispatcher this runtime does not have.
        "addEventListener",
        // Genuinely not implemented, and not a binding away. It means a
        // message bus between isolates and there is none: an isolate is one
        // (application, environment) on one node, two of them share no memory,
        // and nothing on this node carries a message from one to another.
        // deno_web's implementation is loaded and would bind, which is exactly
        // the trap - `runtime/runtime_init.rs` builds a fresh
        // `InMemoryBroadcastChannel` per isolate, so a bound
        // `BroadcastChannel` would post messages that no receiver anywhere can
        // ever observe. A name that accepts every send and delivers none is a
        // silent data-loss bug in an application that trusted it. Use a queue
        // (`context.services.queue`), which is durable and crosses nodes.
        "BroadcastChannel",
    ];

    /// The budget has to hold against code that never lets the runtime thread
    /// go, which is the only version an attacker writes.
    ///
    /// This is the case a `tokio::spawn`ed timer cannot cover, and it was not
    /// covered for a long time: each isolate runs on a `current_thread`
    /// runtime, and a handler that never awaits never lets that runtime poll
    /// the timer that was supposed to stop it. Measured at 75 seconds against
    /// a 300 ms budget. A test that spun on `await` instead would have passed
    /// the whole time, which is why the loop below is deliberately
    /// synchronous, and why the same handler serves a second, ordinary path:
    /// a cut-off that took the isolate with it would be no better than one
    /// that never fired.
    #[tokio::test]
    async fn a_handler_that_never_yields_is_still_cut_off() {
        let code = r#"
            export default function handler(context) {
                if (new URL(context.request.url).pathname === "/spin") {
                    while (true) {}
                }
                return new Response("ok");
            }
        "#;
        let mut runtime = AppRuntime::new(
            "spinning_app".to_string(),
            code.to_string(),
            None,
            IsolateSpec {
                limits: ExecutionLimits {
                    timeout_ms: 300,
                    ..ExecutionLimits::default()
                },
                ..IsolateSpec::default()
            },
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();

        let started = std::time::Instant::now();
        let (tx, rx) = oneshot::channel();
        let outcome = runtime
            .execute_fetch(
                "http://localhost/spin".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await;
        let elapsed = started.elapsed();

        assert!(
            outcome.is_err(),
            "a handler that was cut off must fail the invocation, not answer it"
        );
        // Generous against a loaded machine, and still two orders of magnitude
        // below the failure it guards: unbounded meant minutes, not
        // milliseconds.
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the 300ms budget did not stop a synchronous loop; it ran for {elapsed:?}"
        );
        assert!(
            rx.await.is_err(),
            "the response channel must be closed rather than left open forever"
        );

        // The same isolate goes back to serving. A cut-off that left the
        // termination flag set would be spent on the next request instead,
        // which is how one handler's loop becomes every request's problem.
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
            .expect("the isolate serves again after a cut-off");
        let (status, _, body) = rx.await.unwrap().unwrap();
        assert_eq!(status, 200);
        assert_eq!(body.into_bytes().as_deref(), Some(&b"ok"[..]));
    }

    /// The whole point of a CPU ceiling: two handlers that a wall clock cannot
    /// tell apart, and which cost the node amounts that differ by orders of
    /// magnitude.
    ///
    /// Both run with a 10-second duration budget, so neither can be stopped by
    /// it inside this test. One spins, the other waits 1.5 seconds on a timer
    /// -- five times the 300ms CPU ceiling in wall clock, and next to nothing
    /// in CPU. The first has to be cut off and the second has to be served,
    /// and if the ceiling were measuring duration the results would be the
    /// other way round.
    #[tokio::test]
    async fn the_cpu_ceiling_stops_a_spinning_handler_and_not_a_waiting_one() {
        const CPU_CEILING_MS: u64 = 300;
        const WALL_BUDGET_MS: u64 = 10_000;
        const SLEEP_MS: u64 = 1_500;

        let code = format!(
            r#"
            export default async function handler(context) {{
                if (new URL(context.request.url).pathname === "/spin") {{
                    while (true) {{}}
                }}
                await new Promise((resolve) => setTimeout(resolve, {SLEEP_MS}));
                return new Response("waited");
            }}
        "#
        );

        let limits = ExecutionLimits {
            timeout_ms: WALL_BUDGET_MS,
            cpu_time_limit_ms: CPU_CEILING_MS,
            ..ExecutionLimits::default()
        };

        // Two isolates rather than two requests to one, so each application's
        // counters are its own and the assertions below cannot be satisfied by
        // the other one's numbers.
        let waiting_app = "cpu_ceiling_waiting_app";
        let mut waiting = AppRuntime::new(
            waiting_app.to_string(),
            code.clone(),
            None,
            IsolateSpec {
                limits: limits.clone(),
                ..IsolateSpec::default()
            },
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();

        let started = std::time::Instant::now();
        let (tx, rx) = oneshot::channel();
        waiting
            .execute_fetch(
                "http://localhost/wait".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .expect("a handler that waits five times its CPU ceiling must still be served");
        let waited = started.elapsed();
        let (status, _, body) = rx.await.unwrap().unwrap();
        assert_eq!(status, 200);
        assert_eq!(text(body).as_deref(), Some("waited"));
        assert!(
            waited >= std::time::Duration::from_millis(SLEEP_MS - 100),
            "the waiting handler returned in {waited:?}, so it never actually outlived the \
             {CPU_CEILING_MS}ms ceiling and the test proves nothing"
        );
        assert_eq!(
            recorder().value(CPU_CUTOFFS_TOTAL, waiting_app),
            0.0,
            "waiting on a timer is not spending CPU, and must not be charged as if it were"
        );
        assert_eq!(
            recorder().value(TIMEOUT_CUTOFFS_TOTAL, waiting_app),
            0.0,
            "the duration budget was {WALL_BUDGET_MS}ms and the handler took {waited:?}"
        );

        let spinning_app = "cpu_ceiling_spinning_app";
        let mut spinning = AppRuntime::new(
            spinning_app.to_string(),
            code,
            None,
            IsolateSpec {
                limits,
                ..IsolateSpec::default()
            },
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();

        let started = std::time::Instant::now();
        let (tx, rx) = oneshot::channel();
        let outcome = spinning
            .execute_fetch(
                "http://localhost/spin".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await;
        let spun = started.elapsed();
        assert!(rx.await.is_err(), "a cut-off request answers nobody");

        let error = outcome
            .expect_err("a spinning handler must be cut off")
            .to_string();
        // The distinction the ceiling exists to make, in the words an operator
        // reads: a message that said "timed out" here would send them looking
        // for a slow upstream that does not exist.
        assert!(
            error.contains("CPU"),
            "the cut-off must name CPU time, not duration; it said: {error}"
        );
        assert!(
            error.contains("wall clock"),
            "the cut-off must give both numbers so the two faults can be told apart; it said: \
             {error}"
        );

        // The wall clock is what proves which ceiling fired. The duration
        // budget was 10 seconds and could not have been reached; anything near
        // the CPU ceiling can only have come from the CPU ceiling. Generous
        // upwards against a loaded machine, and still far below the budget it
        // is being distinguished from.
        assert!(
            spun < std::time::Duration::from_millis(WALL_BUDGET_MS / 2),
            "a {CPU_CEILING_MS}ms CPU ceiling did not stop a spin; it ran {spun:?} against a \
             {WALL_BUDGET_MS}ms duration budget"
        );
        assert_eq!(
            recorder().value(TIMEOUT_CUTOFFS_TOTAL, spinning_app),
            0.0,
            "the spin was stopped by the CPU ceiling, so nothing may be counted as a timeout"
        );
        assert_eq!(
            recorder().value(CPU_CUTOFFS_TOTAL, spinning_app),
            1.0,
            "one CPU cut-off has to be countable per application, or \"which application is \
             burning this node\" has no answer"
        );

        // The counter that answers the question when nobody is being cut off:
        // the spin spent at least its ceiling, the wait spent almost nothing,
        // and the two are orders of magnitude apart.
        let spinning_cpu_ms = recorder().value(CPU_MS_SUM, spinning_app);
        let waiting_cpu_ms = recorder().value(CPU_MS_SUM, waiting_app);
        assert!(
            spinning_cpu_ms >= CPU_CEILING_MS as f64,
            "the spinning app was charged {spinning_cpu_ms}ms of CPU, less than the \
             {CPU_CEILING_MS}ms it had to burn to be cut off"
        );
        assert!(
            waiting_cpu_ms < spinning_cpu_ms,
            "waiting {SLEEP_MS}ms was charged {waiting_cpu_ms}ms of CPU and spinning was charged \
             {spinning_cpu_ms}ms; if a wait costs as much as a spin, this counter is measuring \
             duration again"
        );

        // A CPU cut-off costs one request, not the isolate: the termination
        // flag has to be cleared the same way a duration cut-off's is.
        let (tx, rx) = oneshot::channel();
        spinning
            .execute_fetch(
                "http://localhost/wait".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .expect("the isolate serves again after a CPU cut-off");
        let (status, _, body) = rx.await.unwrap().unwrap();
        assert_eq!(status, 200);
        assert_eq!(text(body).as_deref(), Some("waited"));
    }

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
            "globals_app.production".to_string(),
            code,
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        let body = text(body);
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

    /// An application must not be able to reach the raw op table.
    ///
    /// The whole outbound fetch policy rests on the `fetch` wrapper in
    /// `js/bootstrap.js` being the only way an application can open a socket.
    /// `deno_core` exposes its op table on the global object twice --
    /// `globalThis.Deno.core.ops` and `globalThis.__bootstrap.core.ops` -- and
    /// either one hands an application `op_fetch` and, worse,
    /// `op_net_connect_tcp`: a raw TCP connect the policy's pre-flight guard,
    /// its DNS resolver and its redirect loop never see. `runtime/js/runtime.js`
    /// deletes both globals before any application code runs. This drives the
    /// same two doors an attacker would and asserts each is shut, down to the
    /// op an application would actually call to reach a loopback service.
    #[tokio::test]
    async fn an_application_cannot_reach_the_raw_op_table() {
        let code = r#"
            export default function handler() {
                const g = globalThis;
                const bootstrapOps =
                    !!(g.__bootstrap && g.__bootstrap.core && g.__bootstrap.core.ops);
                const denoOps = !!(g.Deno && g.Deno.core && g.Deno.core.ops);
                let connectReachable = false;
                try {
                    const ops =
                        (g.__bootstrap && g.__bootstrap.core && g.__bootstrap.core.ops) ||
                        (g.Deno && g.Deno.core && g.Deno.core.ops);
                    connectReachable =
                        !!(ops && typeof ops.op_net_connect_tcp === "function");
                } catch (_) {}
                return new Response(
                    JSON.stringify({ bootstrapOps, denoOps, connectReachable }),
                );
            }
        "#;
        let body = body_of(
            "op_table_app",
            code,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .await;
        let seen: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            seen["bootstrapOps"], false,
            "globalThis.__bootstrap.core.ops is reachable from an application, which hands it \
             every op directly and bypasses the outbound fetch policy"
        );
        assert_eq!(
            seen["denoOps"], false,
            "globalThis.Deno.core.ops is reachable from an application"
        );
        assert_eq!(
            seen["connectReachable"], false,
            "op_net_connect_tcp is reachable from an application; it opens a raw socket the \
             fetch policy never sees"
        );
    }

    // --- the globals bound beyond section 8.1 ------------------------------
    //
    // `the_isolate_global_surface_is_the_one_the_documents_describe` asserts
    // that a name is defined. That is the cheap half. A name that is bound and
    // broken is the failure this repo keeps hitting, and it reads exactly like
    // a working platform until an application calls the thing, so each of the
    // globals below is also driven the way an application drives it.

    /// A multipart upload has to arrive as something a handler can read.
    ///
    /// `request.formData()` already parsed one before these were bound - the
    /// parser is deno_fetch's and it was never the problem. What an
    /// application could not do was *read the result*: a file entry is a
    /// `File`, `entry instanceof File` was a ReferenceError, and there was no
    /// constructor to build a multipart body to send one on. So this drives
    /// both directions in one handler: the multipart body below is parsed,
    /// and then the same upload is rebuilt out of `FormData`/`File`/`Blob` and
    /// parsed back through a second `Request`.
    #[tokio::test]
    async fn a_multipart_upload_round_trips_through_request_formdata() {
        let boundary = "----deislet-test-boundary";
        let payload = format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"title\"\r\n\
             \r\n\
             a picture of a cat\r\n\
             --{boundary}\r\n\
             Content-Disposition: form-data; name=\"upload\"; filename=\"cat.txt\"\r\n\
             Content-Type: text/plain\r\n\
             \r\n\
             meow\r\n\
             --{boundary}--\r\n"
        );

        let code = r#"
            export default async function handler(context) {
                const form = await context.request.formData();
                const title = form.get("title");
                const upload = form.get("upload");

                // The outbound direction: build the same upload out of the
                // constructors and parse it back, because a handler that can
                // read an upload but not forward one is only half ported.
                const rebuilt = new FormData();
                rebuilt.append("title", title);
                rebuilt.append(
                    "upload",
                    new File([await upload.text()], upload.name, { type: upload.type }),
                );
                const reparsed = await new Request("http://localhost/echo", {
                    method: "POST",
                    body: rebuilt,
                }).formData();

                const blob = new Blob(["meow"], { type: "text/plain" });

                return new Response(JSON.stringify({
                    isFormData: form instanceof FormData,
                    title,
                    uploadIsFile: upload instanceof File,
                    uploadIsBlob: upload instanceof Blob,
                    name: upload.name,
                    type: upload.type,
                    text: await upload.text(),
                    rebuiltTitle: reparsed.get("title"),
                    rebuiltName: reparsed.get("upload").name,
                    rebuiltText: await reparsed.get("upload").text(),
                    blobSize: blob.size,
                    blobText: await blob.text(),
                }));
            }
        "#;

        let mut headers = HashMap::new();
        headers.insert(
            "content-type".to_string(),
            format!("multipart/form-data; boundary={boundary}"),
        );

        let mut runtime = AppRuntime::new(
            "formdata_app.production".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/upload".to_string(),
                "POST".to_string(),
                headers,
                Some(payload.into_bytes()),
                tx,
            )
            .await
            .unwrap();
        let (status, _headers, body) = rx.await.unwrap().unwrap();
        let body = text(body).unwrap_or_default();
        assert_eq!(status, 200, "the handler failed: {body}");
        let seen: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(seen["isFormData"], true);
        assert_eq!(seen["title"], "a picture of a cat");
        assert_eq!(
            seen["uploadIsFile"], true,
            "a file entry has to be a `File` an application can name; this is the check every \
             upload handler makes"
        );
        assert_eq!(seen["uploadIsBlob"], true, "`File` extends `Blob`");
        assert_eq!(seen["name"], "cat.txt");
        assert_eq!(seen["type"], "text/plain");
        assert_eq!(seen["text"], "meow");
        assert_eq!(seen["rebuiltTitle"], "a picture of a cat");
        assert_eq!(
            seen["rebuiltName"], "cat.txt",
            "a `FormData` built by the handler has to survive being sent as a request body and \
             parsed again, or an upload can be received but not forwarded"
        );
        assert_eq!(seen["rebuiltText"], "meow");
        assert_eq!(seen["blobSize"], 4);
        assert_eq!(seen["blobText"], "meow");
    }

    /// Routing, which is the first thing a request handler does.
    ///
    /// `URLPattern` is bound because a router reaches for it; a binding that
    /// existed but did not match would move the failure from the import to
    /// the route table, which is worse. So this matches, extracts a named
    /// group, refuses a path it should refuse, and takes a wildcard.
    #[tokio::test]
    async fn a_router_matches_a_path_with_urlpattern() {
        let code = r#"
            export default function handler() {
                const books = new URLPattern({ pathname: "/books/:id" });
                const hit = books.exec("http://localhost/books/9780134685991");
                const assets = new URLPattern({ pathname: "/assets/*" });
                return new Response(JSON.stringify({
                    matches: books.test("http://localhost/books/42"),
                    id: hit.pathname.groups.id,
                    miss: books.exec("http://localhost/authors/1"),
                    wildcard: assets.test("http://localhost/assets/img/logo.png"),
                    wildcardMiss: assets.test("http://localhost/books/42"),
                }));
            }
        "#;
        let body = body_of(
            "urlpattern_app.production",
            code,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .await;
        let seen: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(seen["matches"], true);
        assert_eq!(
            seen["id"], "9780134685991",
            "a `:named` group has to come back"
        );
        assert!(
            seen["miss"].is_null(),
            "a pattern that matched a path it should not have is worse than no pattern at all"
        );
        assert_eq!(seen["wildcard"], true);
        assert_eq!(seen["wildcardMiss"], false);
    }

    /// The deep copy, against the thing it replaces.
    ///
    /// Without `structuredClone` an application copies with
    /// `JSON.parse(JSON.stringify(x))`, which drops `undefined` keys and
    /// functions, flattens `Map` and `Set` to `{}`, turns a `Date` into a
    /// string and throws outright on a cycle. Each of those is asserted here
    /// against a value that carries all of them at once, plus the point of a
    /// deep copy: writing through the copy must not reach the original.
    #[tokio::test]
    async fn structuredclone_deep_copies_what_a_json_round_trip_loses() {
        let code = r#"
            export default function handler() {
                const original = {
                    when: new Date("2026-08-30T00:00:00.000Z"),
                    tags: new Set(["a", "b"]),
                    index: new Map([["k", { deep: [1, 2, 3] }]]),
                    bytes: new Uint8Array([1, 2, 3]),
                    missing: undefined,
                    nested: { list: [{ n: 1 }] },
                };
                original.self = original;

                let jsonThrows = false;
                try {
                    JSON.stringify(original);
                } catch (_) {
                    jsonThrows = true;
                }

                const copy = structuredClone(original);
                copy.nested.list[0].n = 99;
                copy.index.get("k").deep.push(4);

                // The transfer list. It is accepted and it does nothing
                // here - see the assertions below.
                const buffer = new Uint8Array([9, 9, 9]).buffer;
                const moved = structuredClone(buffer, { transfer: [buffer] });

                return new Response(JSON.stringify({
                    movedLength: moved.byteLength,
                    sourceDetached: buffer.byteLength === 0,
                    jsonThrows,
                    separate: copy !== original,
                    originalUntouched: original.nested.list[0].n,
                    copyWritten: copy.nested.list[0].n,
                    originalMapDepth: original.index.get("k").deep.length,
                    copyMapDepth: copy.index.get("k").deep.length,
                    dateSurvives:
                        copy.when instanceof Date &&
                        copy.when.getTime() === original.when.getTime(),
                    setSurvives: copy.tags instanceof Set && copy.tags.has("b"),
                    mapSurvives: copy.index instanceof Map,
                    bytesSurvive: copy.bytes instanceof Uint8Array && copy.bytes[2] === 3,
                    undefinedKeyKept: "missing" in copy,
                    cycleSurvives: copy.self === copy,
                }));
            }
        "#;
        let body = body_of(
            "structured_clone_app.production",
            code,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .await;
        let seen: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            seen["jsonThrows"], true,
            "the value under test has to be one the JSON fallback cannot copy, or this test is \
             not testing anything"
        );
        assert_eq!(seen["separate"], true);
        assert_eq!(
            seen["originalUntouched"], 1,
            "a write through the copy reached the original, which makes this a shallow copy"
        );
        assert_eq!(seen["copyWritten"], 99);
        assert_eq!(seen["originalMapDepth"], 3);
        assert_eq!(seen["copyMapDepth"], 4);
        assert_eq!(seen["dateSurvives"], true);
        assert_eq!(seen["setSurvives"], true);
        assert_eq!(seen["mapSurvives"], true);
        assert_eq!(seen["bytesSurvive"], true);
        assert_eq!(seen["undefinedKeyKept"], true);
        assert_eq!(seen["cycleSurvives"], true);
        assert_eq!(seen["movedLength"], 3);
        // Documented, not aspirational. The bound `structuredClone` takes a
        // `{ transfer: [...] }` list because it is the one from
        // `13_message_port.js`, but deno_core only moves a buffer when the
        // runtime carries a shared ArrayBuffer store, and this one does not:
        // the buffer is copied and the source stays usable. The clone is
        // still correct; the move is what is missing. Asserting the current
        // answer means turning that store on becomes a visible test failure
        // instead of a silent change of contract - and it keeps
        // `js/bootstrap.js` honest about it in the meantime.
        assert_eq!(
            seen["sourceDetached"], false,
            "a transferred buffer is now detached; structuredClone's transfer list has started \
             moving buffers, so js/bootstrap.js, crates/deis-runtime/README.md and edge-canon's \
             PLATFORM_MATRIX.md all say something that is no longer true"
        );
    }

    /// The clock is coarse on purpose, and that is a property worth a test.
    ///
    /// deno_web's `performance.now()` reads a nanosecond clock, and a
    /// sub-millisecond clock an application can read is the standard building
    /// block of a cross-isolate timing side channel - this node runs several
    /// tenants' isolates on one OS thread. `js/bootstrap.js` binds its own
    /// object instead, clamped to whole milliseconds with `timeOrigin` fixed
    /// at 0. A later change that swapped deno_web's object back in would keep
    /// the surface test green and quietly hand that clock back, so the
    /// clamping itself is asserted: two thousand reads, none of them
    /// fractional.
    #[tokio::test]
    async fn the_clock_an_application_reads_is_clamped_to_whole_milliseconds() {
        let code = r#"
            export default function handler() {
                const samples = [];
                for (let i = 0; i < 2000; i++) samples.push(performance.now());
                const before = Date.now();
                const reading = performance.now();
                const after = Date.now();
                return new Response(JSON.stringify({
                    timeOrigin: performance.timeOrigin,
                    fractional: samples.filter((n) => !Number.isInteger(n)).length,
                    neverGoesBackwards: samples[samples.length - 1] >= samples[0],
                    // timeOrigin is 0, so timeOrigin + now() has to read as a
                    // plain Unix timestamp or the two halves disagree.
                    agreesWithTheWallClock: reading >= before && reading <= after,
                    frozen: Object.isFrozen(performance),
                    mark: typeof performance.mark,
                    measure: typeof performance.measure,
                }));
            }
        "#;
        let body = body_of(
            "performance_app.production",
            code,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .await;
        let seen: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(seen["timeOrigin"], 0);
        assert_eq!(
            seen["fractional"], 0,
            "performance.now() returned a sub-millisecond reading; that is the timer a \
             cross-isolate timing side channel is built on"
        );
        assert_eq!(seen["neverGoesBackwards"], true);
        assert_eq!(seen["agreesWithTheWallClock"], true);
        assert_eq!(seen["frozen"], true);
        assert_eq!(
            seen["mark"], "undefined",
            "the profiler entry list is deliberately absent; nothing on this node collects it"
        );
        assert_eq!(seen["measure"], "undefined");
    }

    /// `navigator` says what this platform is and nothing else.
    ///
    /// The name exists so a library that branches on the runtime it is in can
    /// find one - without it such a library takes its browser branch and
    /// reaches for `document`. Everything else a browser hangs off `navigator`
    /// would either describe the node, which is the operator's business rather
    /// than the tenant's, or answer a plausible lie. It is frozen because an
    /// isolate is reused: a property written onto a platform global by one
    /// request would still be there, looking like platform surface, during the
    /// next.
    #[tokio::test]
    async fn navigator_names_this_platform_and_carries_nothing_else() {
        let code = r#"
            export default function handler() {
                let refused = false;
                try {
                    navigator.hardwareConcurrency = 64;
                } catch (_) {
                    refused = true;
                }
                return new Response(JSON.stringify({
                    userAgent: navigator.userAgent,
                    keys: Object.keys(navigator),
                    frozen: Object.isFrozen(navigator),
                    refused,
                    afterwards: typeof navigator.hardwareConcurrency,
                }));
            }
        "#;
        let body = body_of(
            "navigator_app.production",
            code,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .await;
        let seen: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(seen["userAgent"], "Deislet");
        assert_eq!(
            seen["keys"],
            serde_json::json!(["userAgent"]),
            "navigator grew a member; every one of them is a promise to applications"
        );
        assert_eq!(seen["frozen"], true);
        assert_eq!(
            seen["refused"], true,
            "handler code is a module, so it is strict"
        );
        assert_eq!(seen["afterwards"], "undefined");
    }

    /// `EventTarget` was bound with nothing to fire through it.
    ///
    /// `dispatchEvent` has to be handed an object, and every handler written
    /// for another runtime hands it an `Event` or a `CustomEvent` - so until
    /// those two were bound the event bus this platform advertised could only
    /// be driven with a hand-rolled `{ type }` literal. This dispatches both
    /// through the `EventTarget` an application actually gets (the small one
    /// in `js/runtime.js`) and reads the payload back off the listener.
    #[tokio::test]
    async fn an_event_dispatched_through_eventtarget_carries_its_payload() {
        let code = r#"
            export default function handler() {
                const bus = new EventTarget();
                const seen = [];
                bus.addEventListener("ping", (e) => {
                    seen.push({ type: e.type, detail: e.detail ?? null });
                });
                bus.dispatchEvent(new Event("ping"));
                bus.dispatchEvent(new CustomEvent("ping", { detail: { n: 7 } }));
                bus.dispatchEvent(new CustomEvent("pong", { detail: { n: 8 } }));
                return new Response(JSON.stringify({
                    delivered: seen.length,
                    plainDetail: seen[0].detail,
                    customDetail: seen[1].detail,
                    type: seen[1].type,
                    customIsAnEvent: new CustomEvent("x") instanceof Event,
                }));
            }
        "#;
        let body = body_of(
            "event_app.production",
            code,
            IsolateSpec::default(),
            &ServiceWiring::default(),
        )
        .await;
        let seen: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            seen["delivered"], 2,
            "a listener registered for one type must not be given another type's event"
        );
        assert!(
            seen["plainDetail"].is_null(),
            "a plain Event carries no detail"
        );
        assert_eq!(seen["customDetail"], serde_json::json!({ "n": 7 }));
        assert_eq!(seen["type"], "ping");
        assert_eq!(seen["customIsAnEvent"], true);
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
        let mut runtime = AppRuntime::new(
            app.to_string(),
            code.to_string(),
            None,
            spec,
            wiring,
            ServiceRouter::detached(),
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
        let body = text(body);
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
                return new Response("refused:" + e.constructor.name + ":" + e.message + " CAUSE=" + (e.cause && (e.cause.message || String(e.cause))));
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
            storage_token: None,
            declaration: ServiceDeclaration::from_names(["kv"]),
            endpoints: ServiceEndpoints {
                store: Some("http://127.0.0.1:8090".to_string()),
                queue: None,
            },
            consumers: Vec::new(),
            ..ServiceWiring::default()
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

    // ---------------------------------------------------------------
    // Response bodies that are still being produced
    // ---------------------------------------------------------------

    /// A handler that streams six numbered chunks, and a second route that
    /// reports how many of them its producer has managed so far.
    ///
    /// The counter is what makes "incrementally" checkable without a clock: a
    /// producer that had already finished would report six.
    const STREAMING_APP: &str = r#"
        let produced = 0;
        export default async function handler(context) {
          const path = new URL(context.request.url).pathname;
          if (path === "/progress") {
            return new Response("produced=" + produced);
          }
          let n = 0;
          const stream = new ReadableStream({
            pull(controller) {
              if (n >= 6) { controller.close(); return; }
              n += 1;
              produced = n;
              controller.enqueue("chunk" + n + "\n");
            },
          });
          return new Response(stream, {
            headers: { "content-type": "text/event-stream" },
          });
        }
    "#;

    async fn streaming_runtime(app_id: &str, code: &str) -> AppRuntime {
        let mut runtime = AppRuntime::new(
            app_id.to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();
        runtime
    }

    #[tokio::test]
    async fn a_streaming_body_is_answered_before_a_byte_of_it_exists() {
        // The head has to leave while the body is still being made. Before
        // this, a handler returning a stream had its whole body collected
        // first, so an events feed -- whose body never ends -- simply hit the
        // execution deadline and the client got nothing at all.
        let mut runtime = streaming_runtime("stream.production", STREAMING_APP).await;

        let (status, headers, body) = answer_to(&mut runtime, "/feed").await;
        assert_eq!(status, 200);
        assert_eq!(
            crate::runtime::response_headers::first_value(&headers, "content-type"),
            Some("text/event-stream")
        );
        assert!(
            matches!(body, ResponseBody::Stream(_)),
            "the handler's stream was collected into a buffer instead"
        );

        let (bytes, failure) = drain_stream(&mut runtime, body).await;
        assert_eq!(failure, None, "the producer finished, so nothing failed");
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "chunk1\nchunk2\nchunk3\nchunk4\nchunk5\nchunk6\n",
            "every chunk has to arrive, in order and exactly once"
        );
    }

    #[tokio::test]
    async fn another_request_is_served_while_a_stream_is_still_open() {
        // The constraint that shapes the whole design: one isolate, one
        // thread, and a stream that may run for minutes. A stream that held
        // the isolate until its client finished reading would let one slow
        // reader stop every other request for that application.
        let mut runtime = streaming_runtime("interleave.production", STREAMING_APP).await;

        let (_, _, body) = answer_to(&mut runtime, "/feed").await;
        let ResponseBody::Stream(mut reader) = body else {
            panic!("the handler answered with a stream");
        };

        // One chunk, and no more: the reader stops there, which is what a slow
        // client looks like from inside the node.
        let first = tokio::select! {
            chunk = std::future::poll_fn(|cx| reader.poll_chunk(cx)) => chunk,
            () = runtime.pump_response_streams() => panic!("the stream ended before its first chunk"),
        };
        assert_eq!(first.as_deref(), Some(&b"chunk1\n"[..]));

        // The isolate now has an open stream whose reader is not reading. A
        // second request must still be answered.
        let (status, _, progress) = answer_to(&mut runtime, "/progress").await;
        assert_eq!(
            status, 200,
            "a request arriving while a stream was open was not served"
        );
        let progress = text(progress).unwrap();
        let produced: u32 = progress.trim_start_matches("produced=").parse().unwrap();
        assert!(
            (1..6).contains(&produced),
            "the producer had made {produced} chunk(s) when the second request was answered; \
             below 1 means nothing was produced, 6 means the whole body was collected after all"
        );

        // And the stream still finishes afterwards.
        let read = async {
            let mut rest = Vec::new();
            while let Some(chunk) = std::future::poll_fn(|cx| reader.poll_chunk(cx)).await {
                rest.extend_from_slice(&chunk);
            }
            (rest, reader.failure())
        };
        let ((rest, failure), ()) = tokio::join!(read, runtime.pump_response_streams());
        assert_eq!(failure, None);
        assert_eq!(
            String::from_utf8(rest).unwrap(),
            "chunk2\nchunk3\nchunk4\nchunk5\nchunk6\n"
        );
    }

    #[tokio::test]
    async fn a_client_that_hangs_up_stops_the_producer() {
        // Otherwise the producer goes on making chunks nobody will ever read,
        // holding the isolate's attention until the stream's budget runs out --
        // which is minutes of a runtime thread spent on a socket that is
        // already closed.
        let code = r#"
            let cancelled = "no";
            let made = 0;
            export default async function handler(context) {
              const path = new URL(context.request.url).pathname;
              if (path === "/after") {
                return new Response(cancelled + ":" + made);
              }
              const stream = new ReadableStream({
                pull(controller) {
                  made += 1;
                  controller.enqueue("x".repeat(8));
                },
                cancel(reason) {
                  cancelled = "yes";
                },
              });
              return new Response(stream);
            }
        "#;
        let mut runtime = streaming_runtime("hangup.production", code).await;

        let (_, _, body) = answer_to(&mut runtime, "/endless").await;
        let ResponseBody::Stream(mut reader) = body else {
            panic!("the handler answered with a stream");
        };
        let first = tokio::select! {
            chunk = std::future::poll_fn(|cx| reader.poll_chunk(cx)) => chunk,
            () = runtime.pump_response_streams() => panic!("an endless stream ended on its own"),
        };
        assert_eq!(first.as_deref(), Some(&b"xxxxxxxx"[..]));

        // The client goes away. Nothing else changes.
        drop(reader);

        // The pump returns, which it can only do once no stream is open: the
        // producer was cancelled rather than left running.
        runtime.pump_response_streams().await;
        assert!(
            !runtime.has_open_response_streams(),
            "the stream stayed open after its client went away"
        );

        let (_, _, after) = answer_to(&mut runtime, "/after").await;
        let after = text(after).unwrap();
        let (cancelled, made) = after.split_once(':').unwrap();
        assert_eq!(
            cancelled, "yes",
            "the producer was never told; it stopped only because nothing polled it"
        );
        let made: u32 = made.parse().unwrap();
        assert!(
            made <= 8,
            "the producer made {made} chunks for a client that took one; the queue is not \
             bounding it"
        );
    }

    #[tokio::test]
    async fn a_producer_that_throws_leaves_the_client_a_failed_transfer() {
        // The 200 went out with the head, so there is no status left to
        // change. What the client must not get is a body that ends cleanly:
        // that would be a truncated answer it believes is complete.
        let code = r#"
            export default async function handler(context) {
              let n = 0;
              const stream = new ReadableStream({
                pull(controller) {
                  n += 1;
                  if (n === 3) throw new Error("the third page could not be built");
                  controller.enqueue("part" + n + " ");
                },
              });
              return new Response(stream);
            }
        "#;
        let mut runtime = streaming_runtime("throws.production", code).await;

        let (status, _, body) = answer_to(&mut runtime, "/half").await;
        assert_eq!(status, 200, "the head had already been decided");
        let (bytes, failure) = drain_stream(&mut runtime, body).await;

        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "part1 part2 ",
            "everything produced before the throw still belongs to the client"
        );
        let failure = failure.expect(
            "a stream that stopped part-way must be distinguishable from one that finished, or \
             the client reads a truncated body as a whole one",
        );
        assert!(
            failure.contains("the third page could not be built"),
            "the reason has to name what threw, for the node's log: {failure}"
        );
    }

    #[tokio::test]
    async fn a_stream_that_runs_past_its_budget_is_closed_by_the_node() {
        // An unbounded stream is a queue, a socket and a producer this node
        // holds for as long as the application feels like it. The budget is
        // enforced by closing the stream rather than by terminating the
        // isolate, so the application's other requests -- and its other
        // streams -- are untouched.
        let code = r#"
            export default async function handler(context) {
              if (new URL(context.request.url).pathname === "/plain") {
                return new Response("still serving");
              }
              const stream = new ReadableStream({
                pull(controller) { controller.enqueue("tick "); },
              });
              return new Response(stream);
            }
        "#;
        let mut runtime = AppRuntime::new(
            "budget.production".to_string(),
            code.to_string(),
            None,
            IsolateSpec {
                limits: ExecutionLimits {
                    // Long enough that the first chunks are produced normally,
                    // short enough that the test is not a wait.
                    stream_timeout_ms: 300,
                    // This producer has no pacing of its own, so it writes as
                    // fast as the queue drains -- thousands of chunks a second,
                    // one op dispatch each. At the platform default of 1000 the
                    // op rate limiter would terminate the isolate first and the
                    // test would be measuring that ceiling instead of this one.
                    // A real producer writes far larger chunks far less often;
                    // see `pump_response_streams` for what an application that
                    // does not is told.
                    rate_limit_ops_per_sec: 1_000_000.0,
                    ..ExecutionLimits::default()
                },
                ..IsolateSpec::default()
            },
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let started = std::time::Instant::now();
        let (_, _, body) = answer_to(&mut runtime, "/endless").await;
        let (bytes, failure) = drain_stream(&mut runtime, body).await;
        let elapsed = started.elapsed();

        assert!(
            !bytes.is_empty(),
            "the client got nothing at all; the budget cut the stream before it started"
        );
        let failure = failure.expect("a stream cut off at its budget is not a stream that ended");
        assert!(
            failure.contains("stream_timeout_ms") && failure.contains("300"),
            "the reason has to name the ceiling and its value: {failure}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "the stream ran {elapsed:?} against a 300ms budget"
        );

        // The isolate is still healthy: the budget closed one stream, it did
        // not kill the application.
        let (status, _, plain) = answer_to(&mut runtime, "/plain").await;
        assert_eq!(status, 200);
        assert_eq!(text(plain).as_deref(), Some("still serving"));
    }

    #[tokio::test]
    async fn a_producer_past_the_op_ceiling_is_told_which_ceiling_it_crossed() {
        // One chunk costs one op dispatch, so a producer writing tiny chunks
        // as fast as it can meets `rate_limit_ops_per_sec` long before it
        // meets any byte rate. The limiter terminates the isolate from inside
        // the dispatch, which surfaces as V8 unwinding mid-call -- a message
        // that names neither the application nor the ceiling. Left as it
        // arrives, an operator reading it would have nothing to act on.
        let code = r#"
            export default async function handler(context) {
              const stream = new ReadableStream({
                pull(controller) { controller.enqueue("x"); },
              });
              return new Response(stream);
            }
        "#;
        let mut runtime = AppRuntime::new(
            "opceiling.production".to_string(),
            code.to_string(),
            None,
            IsolateSpec {
                limits: ExecutionLimits {
                    rate_limit_ops_per_sec: 50.0,
                    stream_timeout_ms: 30_000,
                    ..ExecutionLimits::default()
                },
                ..IsolateSpec::default()
            },
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (status, _, body) = answer_to(&mut runtime, "/flood").await;
        assert_eq!(status, 200);
        let (_, failure) = drain_stream(&mut runtime, body).await;

        let failure = failure.expect("a stream cut off by the op ceiling did not end cleanly");
        assert!(
            failure.contains("op dispatches per second") && failure.contains("50"),
            "the reason has to name the ceiling and its value so an operator can act on it: \
             {failure}"
        );
    }

    #[tokio::test]
    async fn a_plain_string_answer_still_takes_the_simple_road() {
        // Most responses are a string, and none of the machinery above may get
        // in their way: no queue, no deadline of its own, no producer. The
        // body arrives whole, in the answer itself.
        let mut runtime = streaming_runtime(
            "plain.production",
            r#"export default async function handler() { return new Response("just bytes"); }"#,
        )
        .await;

        let (status, _, body) = answer_to(&mut runtime, "/").await;
        assert_eq!(status, 200);
        assert!(
            !matches!(body, ResponseBody::Stream(_)),
            "an ordinary answer was put on the streaming path"
        );
        assert_eq!(text(body).as_deref(), Some("just bytes"));
        assert!(!runtime.has_open_response_streams());
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        // Streamed, not collected: the Response `fetch` hands back is one this
        // runtime did not build, so it is an upstream body being passed
        // through, and reading it into this isolate's heap first is what used
        // to kill an application for proxying a file bigger than its heap.
        assert!(
            matches!(body, ResponseBody::Stream(_)),
            "the upstream's body was collected before the handler could answer"
        );
        let (bytes, failure) = drain_stream(&mut runtime, body).await;
        assert_eq!(failure, None, "the proxied body did not finish");
        assert_eq!(String::from_utf8(bytes).unwrap(), "proxied body");
        // The upstream sent one; forwarding it would describe a body this node
        // is not sending, and a client would truncate or hang on the
        // difference.
        assert!(
            !headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("content-length")),
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
            let body = text(body);
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
    async fn an_interval_does_not_outlive_a_request_that_was_cut_off() {
        // The sweep the test above relies on lives at the end of
        // `__deis_runtime_wait_until_all`, and that is reached only by an
        // invocation that settles. A request that overruns its budget returns
        // before it -- and overrunning is the one thing an application decides
        // for itself, so the guarantee was off on exactly the path an
        // application controls. Measured before this was fixed: 138 interval
        // callbacks of the cut-off request firing inside the next one, on the
        // next request's budget.
        let code = r#"
            globalThis.__ticks = 0;
            export default async function handler(ctx) {
                if (new URL(ctx.request.url).pathname === "/arm") {
                    setInterval(() => { globalThis.__ticks++; }, 1);
                    // Never settles: this invocation ends in its budget.
                    await new Promise(() => {});
                    return new Response("unreachable");
                }
                const before = globalThis.__ticks;
                await new Promise((resolve) => setTimeout(resolve, 100));
                return new Response(JSON.stringify({ before, after: globalThis.__ticks }));
            }
        "#;

        let spec = IsolateSpec {
            limits: ExecutionLimits {
                timeout_ms: 500,
                ..ExecutionLimits::default()
            },
            ..IsolateSpec::default()
        };
        let mut runtime = AppRuntime::new(
            "interval-cutoff.production".to_string(),
            code.to_string(),
            None,
            spec,
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        let armed = runtime
            .execute_fetch(
                "http://localhost/arm".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await;
        assert!(
            armed.is_err(),
            "the invocation was expected to be cut off by its execution budget"
        );
        drop(rx);

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/check".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            )
            .await
            .expect("the isolate must serve again after a request was cut off");
        let (status, _headers, body) = rx.await.unwrap().unwrap();
        let body = text(body).unwrap_or_default();
        assert_eq!(status, 200, "{body}");

        let seen: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            seen["after"], seen["before"],
            "an interval armed by a request that was cut off was still firing inside the next \
             request: {seen}"
        );
    }

    /// Two requests through one isolate, where the first one tries to arm a
    /// timer the end-of-invocation sweep will not find.
    ///
    /// Returns what the second request saw: `before`/`after` counters that must
    /// be equal, plus whatever the first request reported about the doors it
    /// found open.
    async fn ticks_after_a_second_request(app: &str, code: &str) -> serde_json::Value {
        let mut runtime = AppRuntime::new(
            app.to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let mut seen = Vec::new();
        for path in ["/arm", "/check"] {
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
                .unwrap();
            let (status, _headers, body) = rx.await.unwrap().unwrap();
            let body = text(body).unwrap_or_default();
            assert_eq!(status, 200, "{body}");
            seen.push(serde_json::from_str::<serde_json::Value>(&body).unwrap());
        }
        serde_json::json!({ "armed": seen[0], "checked": seen[1] })
    }

    #[tokio::test]
    async fn an_application_cannot_choose_what_owns_its_timers() {
        // The end-of-invocation sweep clears the sets the runtime files timers
        // in, and which set that is used to be decided by an object on
        // `globalThis`: `__deis_runtime_stream_scope`, sealed, present and
        // callable. A handler that called `enter({ intervals: new Set() })`,
        // armed an interval and called `leave()` filed that interval in a set
        // no sweep looks in -- measured at 4, 9, 14 and 23 ticks across
        // separate later requests, with nothing in the log. `adopt()` was the
        // same hole without even an invented object.
        //
        // Two things are asserted, because either one alone would leave the
        // door ajar: the scope is not reachable from application code at all
        // (`js/runtime.js` claims it once while the isolate is built), and an
        // owner that is not one this runtime minted is refused even if it were.
        let code = r#"
            globalThis.__ticks = 0;

            // Every door an application has to the ownership of a timer.
            function reach() {
                const out = {};
                out.scope = typeof globalThis.__deis_runtime_stream_scope;
                try {
                    out.claimed = globalThis.__deis_runtime_claim_stream_scope();
                    out.claim = out.claimed === null ? "null" : typeof out.claimed;
                } catch (e) {
                    out.claim = "threw:" + e.name;
                }
                delete out.claimed;
                return out;
            }

            export default async function handler(ctx) {
                const out = reach();
                out.before = globalThis.__ticks;
                if (new URL(ctx.request.url).pathname === "/arm") {
                    const scope = globalThis.__deis_runtime_stream_scope;
                    for (const [name, make] of [
                        ["invented", () => ({
                            guards: new Set(), intervals: new Set(), timeouts: new Set(),
                        })],
                        ["adopted", () => scope.adopt()],
                    ]) {
                        try {
                            scope.enter(make());
                            setInterval(() => { globalThis.__ticks++; }, 1);
                            scope.leave();
                            out[name] = "entered";
                        } catch (e) {
                            out[name] = "threw:" + e.name;
                        }
                    }
                }
                await new Promise((resolve) => setTimeout(resolve, 40));
                out.after = globalThis.__ticks;
                return new Response(JSON.stringify(out));
            }
        "#;

        let seen = ticks_after_a_second_request("timer-owner.production", code).await;

        // The object itself is gone from the global surface, and the claim that
        // replaced it has already been spent by `js/runtime.js`.
        assert_eq!(
            seen["armed"]["scope"], "undefined",
            "the stream scope is still on globalThis, where an application can call it: {seen}"
        );
        assert_eq!(
            seen["armed"]["claim"], "null",
            "the stream scope can still be claimed after the isolate was built: {seen}"
        );
        // Which is why both attempts above got no further than a TypeError on
        // an object that is not there.
        assert_eq!(seen["armed"]["invented"], "threw:TypeError", "{seen}");
        assert_eq!(seen["armed"]["adopted"], "threw:TypeError", "{seen}");
        // And nothing ticked during the second request, which armed nothing.
        assert_eq!(
            seen["checked"]["after"], seen["checked"]["before"],
            "an interval the first request filed against an owner of its own choosing was still \
             firing inside the second request: {seen}"
        );
    }

    #[tokio::test]
    async fn a_response_stream_owns_its_timer_and_gives_it_back_when_it_ends() {
        // The other half of the guarantee, and the reason "sweep everything at
        // the end of the invocation" is not the answer either. A response stream
        // outlives the invocation that created it on purpose -- its head has
        // gone to the client and its body is still being written -- and the code
        // writing it legitimately paces itself with a timer. So the stream takes
        // ownership of what the handler left armed, keeps it for as long as it
        // is producing, and gives it back when it ends.
        let code = r#"
            globalThis.__ticks = 0;
            export default async function handler(context) {
                if (new URL(context.request.url).pathname === "/check") {
                    const before = globalThis.__ticks;
                    await new Promise((resolve) => setTimeout(resolve, 60));
                    return new Response(JSON.stringify({ before, after: globalThis.__ticks }));
                }

                // Armed by the handler, so it is the adoption that carries it
                // over rather than the producer arming it inside a step.
                setInterval(() => { globalThis.__ticks++; }, 1);

                let sent = 0;
                const stream = new ReadableStream({
                    async pull(controller) {
                        if (sent === 3) { controller.close(); return; }
                        sent++;
                        await new Promise((resolve) => setTimeout(resolve, 20));
                        controller.enqueue("tick " + globalThis.__ticks + " ");
                    },
                });
                return new Response(stream);
            }
        "#;

        let mut runtime = AppRuntime::new(
            "stream-owns-timer.production".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (_, _, body) = answer_to(&mut runtime, "/stream").await;
        let (bytes, failure) = drain_stream(&mut runtime, body).await;
        assert!(failure.is_none(), "the stream did not finish: {failure:?}");
        let sent = String::from_utf8(bytes).unwrap();

        // The interval was still firing while the body was being produced,
        // which is the whole point: the counter it prints has moved by the last
        // chunk. Sweeping it with the invocation would have cut the producer's
        // own pacing out from under it.
        let counts: Vec<i64> = sent
            .split_whitespace()
            .filter(|word| *word != "tick")
            .map(|word| word.parse().expect("the producer prints the tick count"))
            .collect();
        assert_eq!(counts.len(), 3, "the producer sent {sent:?}");
        assert!(
            counts[2] > counts[0],
            "the stream's own interval stopped firing while it was still producing: {sent:?}"
        );

        // ... and it ended with the stream, not with some later request.
        let (status, _, body) = answer_to(&mut runtime, "/check").await;
        let body = text(body).unwrap_or_default();
        assert_eq!(status, 200, "{body}");
        let seen: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            seen["after"], seen["before"],
            "the interval a finished response stream owned was still firing in a later \
             request: {seen}"
        );
    }

    #[tokio::test]
    async fn a_stream_the_node_had_to_terminate_gives_its_timer_back_too() {
        // The same leak as the two above, reached through the one owner the
        // end-of-invocation sweep is not allowed to touch, and with no internal
        // call at all: answer with a streaming body whose producer stops
        // yielding.
        //
        // The node can only end that by terminating the isolate, which leaves
        // the producer inside an `await` that never resumes -- so
        // `js/runtime.js` never learns its stream is over. The entry stayed in
        // `openStreams` holding the stream's owner, and the interval pacing a
        // body nobody can read any more went on firing inside every later
        // request this application served. The node's half of the body was
        // always ended; `runtime/isolate.rs::release_stream_producers` is the
        // isolate's half.
        //
        // The two timings this depends on, so that a later reader can see the
        // test is not a race. `QUEUED_CHUNKS` is 2, so the invocation that
        // answers produces three chunks and then parks with the node's queue
        // full: the pulls that spin are reached only once the client below
        // starts draining, which is what keeps the spin out of the request's own
        // `waitUntil` tail. And the spin lasts far longer than
        // `stream_timeout_ms` plus the stream watchdog's grace, so the ending is
        // always the cut-off and never the node reaping a stream that overran.
        let code = r#"
            globalThis.__ticks = 0;
            export default async function handler(context) {
                if (new URL(context.request.url).pathname === "/check") {
                    const before = globalThis.__ticks;
                    await new Promise((resolve) => setTimeout(resolve, 100));
                    return new Response(JSON.stringify({ before, after: globalThis.__ticks }));
                }

                // Armed by the handler, so the stream adopts it and it is the
                // stream, not the invocation, that has to give it back.
                setInterval(() => { globalThis.__ticks++; }, 1);

                let pulls = 0;
                const stream = new ReadableStream({
                    pull(controller) {
                        controller.enqueue("x");
                        if (pulls++ < 5) return;
                        // Computes rather than awaits, so nothing short of
                        // terminating the isolate stops it.
                        const until = Date.now() + 10_000;
                        while (Date.now() < until) {}
                    },
                });
                return new Response(stream);
            }
        "#;

        let mut runtime = AppRuntime::new(
            "stream-cutoff-timer.production".to_string(),
            code.to_string(),
            None,
            IsolateSpec {
                limits: ExecutionLimits {
                    stream_timeout_ms: 300,
                    ..ExecutionLimits::default()
                },
                ..IsolateSpec::default()
            },
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (_, _, body) = answer_to(&mut runtime, "/endless").await;
        let (_, failure) = drain_stream(&mut runtime, body).await;
        let failure = failure.expect("a producer that will not yield does not end cleanly");
        // The test only means anything if the stream ended on the path that
        // terminates the isolate. The other ending -- the node reaping a stream
        // that ran past `stream_timeout_ms` -- cancels the producer, which
        // reaches `retireStream` through `js/runtime.js` and released the owner
        // even before this was fixed.
        assert!(
            failure.contains("was cut off"),
            "the stream was reaped rather than terminated, so this proves nothing: {failure}"
        );

        let (status, _, body) = answer_to(&mut runtime, "/check").await;
        let body = text(body).unwrap_or_default();
        assert_eq!(status, 200, "{body}");
        let seen: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            seen["after"], seen["before"],
            "the interval owned by a response stream the node had to terminate was still firing \
             inside a later request: {seen}"
        );
    }

    #[tokio::test]
    async fn what_the_node_says_about_a_swept_timer_is_not_the_applications_to_silence() {
        // The sweep's warning is the only account an operator gets of an
        // application that leaves work running past its requests, and it went
        // out through `globalThis.console` -- a name this platform lets an
        // application replace, as a browser and Deno do, and one that survives
        // into every later request because the isolate is reused. One line of
        // `globalThis.console = { warn() {} }` and the timers were still swept
        // and nobody was told, which is half the guarantee gone.
        //
        // `js/runtime.js` logs through a console of its own now, held in the
        // closure and not reachable by any name.
        use tracing_subscriber::prelude::*;
        let buffer = std::sync::Arc::new(crate::logging::LogBuffer::new(64));
        let subscriber = tracing_subscriber::registry().with(crate::logging::RuntimeLayer::new(
            buffer.clone(),
            "node-under-test".to_string(),
        ));
        // A thread-local dispatcher, and `#[tokio::test]` polls this future on
        // the thread that set it.
        let _collecting = tracing::subscriber::set_default(subscriber);

        let code = r#"
            export default async function handler(ctx) {
                // Ordinary instrumentation, and this platform allows it: what
                // the application logs from here on is the application's
                // business.
                globalThis.console = {
                    log() {}, info() {}, debug() {}, warn() {}, error() {}, trace() {},
                };
                setInterval(() => {}, 1000);
                return new Response("quiet");
            }
        "#;

        let mut runtime = AppRuntime::new(
            "quiet-sweep.production".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (status, _, body) = answer_to(&mut runtime, "/").await;
        assert_eq!(status, 200, "{:?}", text(body));

        let lines: Vec<String> = buffer
            .drain(64)
            .into_iter()
            .map(|entry| entry.message)
            .collect();
        assert!(
            lines.iter().any(
                |line| line.contains("were still armed when this invocation ended")
                    && line.contains("1 interval(s)")
            ),
            "the node said nothing about the interval it cleared, because the application had \
             replaced the console the warning went through: {lines:?}"
        );
    }

    #[tokio::test]
    async fn a_self_rescheduling_timeout_does_not_outlive_the_request_that_started_it() {
        // `setTimeout` was bound straight to the raw timer and tracked nowhere,
        // on the reasoning that "a setTimeout fires once and is gone". A
        // callback that arms the next one before it returns is never gone: it
        // needs no internal API, produces no warning, and keeps firing inside
        // every later request this isolate serves, exactly as an interval does.
        let code = r#"
            globalThis.__ticks = 0;
            export default async function handler(ctx) {
                const before = globalThis.__ticks;
                if (new URL(ctx.request.url).pathname === "/arm") {
                    const again = () => { globalThis.__ticks++; setTimeout(again, 1); };
                    setTimeout(again, 1);
                }
                await new Promise((resolve) => setTimeout(resolve, 40));
                return new Response(JSON.stringify({ before, after: globalThis.__ticks }));
            }
        "#;

        let seen = ticks_after_a_second_request("timeout-chain.production", code).await;
        // The chain did run while its own request was running: the sweep is the
        // end of an invocation, not a ban on timers.
        assert!(
            seen["armed"]["after"].as_i64().unwrap() > seen["armed"]["before"].as_i64().unwrap(),
            "the timeout chain never fired at all: {seen}"
        );
        assert_eq!(
            seen["checked"]["after"], seen["checked"]["before"],
            "a self-rescheduling setTimeout from the first request was still firing inside the \
             second: {seen}"
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        let body = text(body);
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "POST".to_string(),
                HashMap::new(),
                sent("héllo"),
                tx,
            )
            .await
            .unwrap();
        let (status, _headers, body) = rx.await.unwrap().unwrap();
        let body = text(body);
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
            storage_token: None,
            declaration: ServiceDeclaration::from_names(["blob"]),
            endpoints: ServiceEndpoints {
                store: Some(store.endpoint.clone()),
                queue: None,
            },
            consumers: Vec::new(),
            ..ServiceWiring::default()
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
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        let body = text(body);
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

    #[tokio::test]
    async fn an_app_lists_heads_ranges_and_conditionally_writes_through_its_blob_binding() {
        // The other four object-store verbs, exercised the only way that
        // proves an application can reach them: through `ctx.services.blob` in
        // a real isolate, against a real deis-store. Each of them exists on
        // the wire and in the client, and each of them would still be
        // unreachable if `js/runtime.js` did not spell the call.
        let store = TestStore::start().await;

        let wiring = ServiceWiring {
            storage_token: None,
            declaration: ServiceDeclaration::from_names(["blob"]),
            endpoints: ServiceEndpoints {
                store: Some(store.endpoint.clone()),
                queue: None,
            },
            consumers: Vec::new(),
            ..ServiceWiring::default()
        };

        let code = r#"
            export default async function handler(ctx) {
                const blob = ctx.services.blob;

                await blob.put("img/a.png", "aaaa", { contentType: "image/png" });
                await blob.put("img/b.png", "bbbb");
                await blob.put("index.html", "0123456789");

                // What an object is, without moving it.
                const head = await blob.head("img/a.png");

                // One level of the key space, folders rolled up.
                const top = await blob.list({ delimiter: "/" });

                // A page smaller than the objects under a prefix, walked to
                // the end the way the docs say to walk it.
                const keys = [];
                let page = { list_complete: false, cursor: undefined };
                let pages = 0;
                while (!page.list_complete && pages < 10) {
                    page = await blob.list({ prefix: "img/", limit: 1, cursor: page.cursor });
                    for (const object of page.objects) keys.push(object.key);
                    pages++;
                }

                // A Range header, forwarded exactly as an application would
                // forward the one it was handed.
                const slice = await blob.get("index.html", { range: "bytes=2-5" });
                const suffix = await blob.get("index.html", { range: { suffix: 3 } });

                // Unsatisfiable: an application owes 416 here, not 404, so it
                // has to arrive as something other than null.
                let unsatisfiable = "no exception";
                try {
                    await blob.get("index.html", { range: { offset: 99 } });
                } catch (error) {
                    unsatisfiable = "threw";
                }

                // Create-once, then the same create again: the second must be
                // told it lost rather than overwrite.
                const created = await blob.put("lock", "mine", { onlyIf: { ifNoneMatch: "*" } });
                const lost = await blob.put("lock", "yours", { onlyIf: { ifNoneMatch: "*" } });

                // Compare-and-set on the etag an earlier write answered with.
                const updated = await blob.put("lock", "second", {
                    onlyIf: { ifMatch: created.etag }
                });
                const stale = await blob.put("lock", "third", {
                    onlyIf: { ifMatch: created.etag }
                });

                const deletedStale = await blob.delete("lock", {
                    onlyIf: { ifMatch: created.etag }
                });
                const deletedCurrent = await blob.delete("lock", {
                    onlyIf: { ifMatch: updated.etag }
                });

                return new Response(JSON.stringify({
                    headSize: head.size,
                    headType: head.contentType,
                    headEtagIsSha: /^[0-9a-f]{64}$/.test(head.etag),
                    topKeys: top.objects.map((o) => o.key),
                    topPrefixes: top.prefixes,
                    pagedKeys: keys,
                    sliceText: await slice.text(),
                    sliceRange: slice.range,
                    sliceTotal: slice.size,
                    suffixText: await suffix.text(),
                    unsatisfiable,
                    createdWon: created !== null,
                    lostIsNull: lost === null,
                    updatedWon: updated !== null,
                    staleIsNull: stale === null,
                    deletedStale,
                    deletedCurrent,
                    goneAfterDelete: (await blob.head("lock")) === null,
                }));
            }
        "#;

        let mut runtime = AppRuntime::new(
            "e2e_blob_verbs.production".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            &wiring,
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        let body = text(body);
        drop(store);

        assert_eq!(status, 200, "body: {body:?}");
        let seen: serde_json::Value = serde_json::from_str(&body.unwrap()).unwrap();

        assert_eq!(seen["headSize"], 4);
        assert_eq!(seen["headType"], "image/png");
        assert_eq!(
            seen["headEtagIsSha"], true,
            "the etag an application conditions a write on is the body's sha256"
        );

        assert_eq!(
            seen["topKeys"],
            serde_json::json!(["index.html"]),
            "with a delimiter, only the keys with no separator left belong at this level"
        );
        assert_eq!(seen["topPrefixes"], serde_json::json!(["img/"]));
        assert_eq!(
            seen["pagedKeys"],
            serde_json::json!(["img/a.png", "img/b.png"]),
            "paging one object at a time must visit every key exactly once"
        );

        assert_eq!(seen["sliceText"], "2345");
        assert_eq!(
            seen["sliceRange"],
            serde_json::json!({"offset": 2, "length": 4})
        );
        assert_eq!(
            seen["sliceTotal"], 10,
            "a ranged read still reports the whole object's size, which is what a Content-Range's \
             total is"
        );
        assert_eq!(seen["suffixText"], "789");
        assert_eq!(
            seen["unsatisfiable"], "threw",
            "a range naming no byte must not read as a missing object"
        );

        assert_eq!(seen["createdWon"], true);
        assert_eq!(
            seen["lostIsNull"], true,
            "the second create-if-absent must be told it lost, not silently overwrite"
        );
        assert_eq!(seen["updatedWon"], true);
        assert_eq!(
            seen["staleIsNull"], true,
            "a write quoting an etag that has been superseded must not land"
        );
        assert_eq!(
            seen["deletedStale"], false,
            "a conditional delete against a stale etag must refuse"
        );
        assert_eq!(seen["deletedCurrent"], true);
        assert_eq!(seen["goneAfterDelete"], true);
    }

    // --- redirects ---------------------------------------------------------
    //
    // A redirect is the destination an application never named: whoever
    // answered the first hop chose it. `js/bootstrap.js` follows the chain
    // itself, with `redirect: "manual"`, so that every hop goes through the
    // same policy check the first request did.
    //
    // These tests redirect for real -- a server on loopback answering 3xx with
    // a `Location:` header -- rather than asserting anything about how the
    // following is implemented. Two of them cover destinations that were
    // reachable before the wrapper followed redirects itself, because deno's
    // net deny list, which was the only thing looking at a hop, cannot spell
    // an IPv6 prefix and cannot make a per-port exception.

    /// Start a router on loopback and hand back the address it answers on.
    async fn serve(router: axum::Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        addr
    }

    /// A handler that reports everything about one outbound call that a
    /// redirect can change: whether it arrived, where it ended up, whether it
    /// was redirected at all, and -- when it failed -- what the application was
    /// told and by what kind of error.
    const REDIRECT_PROBE: &str = r#"
        export default async function handler(ctx) {
            const init = ctx.env.FETCH_INIT
                ? JSON.parse(ctx.env.FETCH_INIT)
                : undefined;
            try {
                const response = await fetch(ctx.env.PROBE_URL, init);
                return new Response(JSON.stringify({
                    ok: true,
                    status: response.status,
                    url: response.url,
                    redirected: response.redirected,
                    location: response.headers.get("location"),
                    body: await response.text(),
                }));
            } catch (e) {
                return new Response(JSON.stringify({
                    ok: false,
                    name: e.name,
                    kind: e.constructor.name,
                    message: String((e && e.message) || e),
                }));
            }
        }
    "#;

    /// Run [`REDIRECT_PROBE`] against `url` and hand back what it reported.
    async fn probe(
        app: &str,
        url: &str,
        init: Option<&str>,
        policy: crate::runtime::FetchPolicy,
    ) -> serde_json::Value {
        probe_with_limits(app, url, init, policy, ExecutionLimits::default()).await
    }

    async fn probe_with_limits(
        app: &str,
        url: &str,
        init: Option<&str>,
        policy: crate::runtime::FetchPolicy,
        limits: ExecutionLimits,
    ) -> serde_json::Value {
        let mut env = HashMap::from([("PROBE_URL".to_string(), url.to_string())]);
        if let Some(init) = init {
            env.insert("FETCH_INIT".to_string(), init.to_string());
        }
        let spec = IsolateSpec {
            env,
            fetch_policy: policy,
            limits,
            ..IsolateSpec::default()
        };
        let body = body_of(app, REDIRECT_PROBE, spec, &ServiceWiring::default()).await;
        serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("the probe did not report JSON ({e}): {body}"))
    }

    /// An allowlist entry naming one loopback port by name. A *name* is used
    /// wherever the test is not about ports: an entry that is a literal
    /// address is punched out of deno's deny list, and a test whose upstream
    /// was allowlisted that way would be proving less than it looks.
    fn allow_localhost(port: u16) -> crate::runtime::FetchPolicy {
        crate::runtime::FetchPolicy::new([format!("localhost:{port}")], Vec::<String>::new())
            .unwrap()
    }

    #[tokio::test]
    async fn a_redirect_to_an_ipv6_unique_local_address_is_refused() {
        // The first of the two holes the wrapper closes. deno's net descriptor
        // has no spelling for an IPv6 *subnet* -- it reads `fc00::/7` as a host
        // and a port and gives up -- so the deny list that used to be the only
        // thing looking at a redirect carried the IPv4 ranges and the two exact
        // addresses `::1` and `::`, and nothing that covers `fd00::1`. deno
        // logged this hop as "Granted net access".
        use axum::response::{IntoResponse, Redirect};
        use axum::routing::get;

        let addr = serve(axum::Router::new().route(
            "/",
            get(|| async { Redirect::temporary("http://[fd00::1]:8090/kv/get").into_response() }),
        ))
        .await;

        let seen = probe(
            "redirect-v6.production",
            &format!("http://localhost:{}/", addr.port()),
            None,
            allow_localhost(addr.port()),
        )
        .await;

        assert_eq!(seen["ok"], false, "the hop was followed: {seen}");
        let message = seen["message"].as_str().unwrap();
        assert!(message.contains("fd00::1"), "{message}");
        assert!(message.contains("unique-local"), "{message}");
    }

    #[tokio::test]
    async fn a_redirect_to_another_port_on_an_allowlisted_address_is_refused() {
        // The second hole. deno's deny list has no per-port exception, so an
        // allowlist entry that is a literal address has to be punched out of
        // the range that covers it -- one address wide, *every port*. On a node
        // that address is where deis-control, deis-store, deis-queue and
        // deis-telemetry all listen, and the telemetry metrics port was
        // reachable exactly this way.
        //
        // The operator here opened one port on 127.0.0.1. The redirect goes to
        // a different port on the same address, which the pre-flight check has
        // always refused for a destination an application named itself -- and
        // now refuses for one it did not.
        use axum::response::{IntoResponse, Redirect};
        use axum::routing::get;

        let secret = serve(
            axum::Router::new().route("/metrics", get(|| async { "deis_requests_total 41" })),
        )
        .await;
        let redirector = serve(axum::Router::new().route(
            "/",
            get(move || async move {
                Redirect::temporary(&format!("http://127.0.0.1:{}/metrics", secret.port()))
                    .into_response()
            }),
        ))
        .await;

        // A literal address with a port: the shape that punches the hole.
        let policy = crate::runtime::FetchPolicy::new(
            [format!("127.0.0.1:{}", redirector.port())],
            Vec::<String>::new(),
        )
        .unwrap();

        let seen = probe(
            "redirect-port.production",
            &format!("http://127.0.0.1:{}/", redirector.port()),
            None,
            policy,
        )
        .await;

        assert_eq!(
            seen["ok"], false,
            "the redirect reached another port on the node: {seen}"
        );
        let message = seen["message"].as_str().unwrap();
        assert!(message.contains(&secret.port().to_string()), "{message}");
        assert!(message.contains("loopback"), "{message}");
    }

    #[tokio::test]
    async fn a_redirect_is_followed_and_the_response_says_where_it_ended_up() {
        // The other half: a policy that refused every hop would pass both
        // tests above and be useless. `url` and `redirected` are part of what
        // `fetch` promises, and following the chain here rather than inside
        // deno_fetch means nothing else fills them in.
        use axum::response::{IntoResponse, Redirect};
        use axum::routing::get;

        let destination =
            serve(axum::Router::new().route("/final", get(|| async { "arrived" }))).await;
        let entry = serve(axum::Router::new().route(
            "/",
            get(move || async move {
                Redirect::temporary(&format!("http://localhost:{}/final", destination.port()))
                    .into_response()
            }),
        ))
        .await;

        let policy = crate::runtime::FetchPolicy::new(
            [
                format!("localhost:{}", entry.port()),
                format!("localhost:{}", destination.port()),
            ],
            Vec::<String>::new(),
        )
        .unwrap();

        let seen = probe(
            "redirect-followed.production",
            &format!("http://localhost:{}/", entry.port()),
            None,
            policy,
        )
        .await;

        assert_eq!(seen["ok"], true, "{seen}");
        assert_eq!(seen["status"], 200, "{seen}");
        assert_eq!(seen["body"], "arrived", "{seen}");
        assert_eq!(
            seen["url"],
            format!("http://localhost:{}/final", destination.port()),
            "response.url must be where the call ended, not where it started: {seen}"
        );
        assert_eq!(seen["redirected"], true, "{seen}");
    }

    #[tokio::test]
    async fn a_relative_location_is_resolved_against_the_url_that_sent_it() {
        // `Location: /landed` is the common case and it is only meaningful
        // relative to the hop that returned it. Resolving it against anything
        // else -- the original request, or nothing at all -- turns a working
        // redirect into a failure or, worse, into a different destination.
        use axum::response::{IntoResponse, Redirect};
        use axum::routing::get;

        let addr = serve(
            axum::Router::new()
                .route(
                    "/deep/start",
                    get(|| async { Redirect::temporary("../landed").into_response() }),
                )
                .route("/landed", get(|| async { "arrived" })),
        )
        .await;

        let seen = probe(
            "redirect-relative.production",
            &format!("http://localhost:{}/deep/start", addr.port()),
            None,
            allow_localhost(addr.port()),
        )
        .await;

        assert_eq!(seen["ok"], true, "{seen}");
        assert_eq!(seen["body"], "arrived", "{seen}");
        assert_eq!(
            seen["url"],
            format!("http://localhost:{}/landed", addr.port()),
            "{seen}"
        );
    }

    #[tokio::test]
    async fn a_redirect_loop_is_given_up_on_rather_than_followed_forever() {
        // Without a limit this is a handler that never returns, holding the
        // runtime thread until the execution timeout terminates the isolate.
        // With one, the application gets a `TypeError` naming the limit, which
        // is what it would get from any other `fetch`.
        use axum::response::{IntoResponse, Redirect};
        use axum::routing::get;

        let addr = serve(axum::Router::new().route(
            "/",
            get(|| async { Redirect::temporary("/").into_response() }),
        ))
        .await;

        let seen = probe(
            "redirect-loop.production",
            &format!("http://localhost:{}/", addr.port()),
            None,
            allow_localhost(addr.port()),
        )
        .await;

        assert_eq!(seen["ok"], false, "{seen}");
        assert_eq!(seen["kind"], "TypeError", "{seen}");
        let message = seen["message"].as_str().unwrap();
        assert!(
            message.contains("redirected more than 20 times"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn a_303_turns_a_post_into_a_get_and_a_307_repeats_it() {
        // The rules `fetch` is defined by, and applications rely on them: a 303
        // means "now go and GET this instead", a 307 means "do the same thing
        // again over there". Following redirects by hand means implementing
        // both, and getting either wrong sends a body somewhere it was not
        // meant to go or drops one that was.
        use axum::extract::Request;
        use axum::response::{IntoResponse, Redirect};
        use axum::routing::{any, post};

        let router = axum::Router::new()
            .route(
                "/see-other",
                post(|| async { Redirect::to("/echo").into_response() }),
            )
            .route(
                "/keep",
                post(|| async { Redirect::temporary("/echo").into_response() }),
            )
            .route(
                "/echo",
                any(|request: Request| async move {
                    let method = request.method().clone();
                    let body = axum::body::to_bytes(request.into_body(), 64 * 1024)
                        .await
                        .unwrap();
                    format!("{method}:{}", String::from_utf8_lossy(&body))
                }),
            );
        let addr = serve(router).await;

        let init = r#"{"method":"POST","body":"payload"}"#;

        let see_other = probe(
            "redirect-303.production",
            &format!("http://localhost:{}/see-other", addr.port()),
            Some(init),
            allow_localhost(addr.port()),
        )
        .await;
        assert_eq!(see_other["ok"], true, "{see_other}");
        assert_eq!(
            see_other["body"], "GET:",
            "a 303 must arrive as a GET with no body: {see_other}"
        );

        let keep = probe(
            "redirect-307.production",
            &format!("http://localhost:{}/keep", addr.port()),
            Some(init),
            allow_localhost(addr.port()),
        )
        .await;
        assert_eq!(keep["ok"], true, "{keep}");
        assert_eq!(
            keep["body"], "POST:payload",
            "a 307 must arrive as the same request: {keep}"
        );
    }

    #[tokio::test]
    async fn a_credential_is_not_carried_to_a_different_origin() {
        // A `Location:` header is written by whoever answered the previous hop.
        // Carrying an `Authorization:` across it hands that party's pick of
        // destination a credential the application only ever meant for the
        // destination it named itself.
        use axum::http::HeaderMap;
        use axum::response::{IntoResponse, Redirect};
        use axum::routing::get;

        fn echo_auth(headers: HeaderMap) -> String {
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("none")
                .to_string()
        }

        let elsewhere = serve(axum::Router::new().route(
            "/echo",
            get(|headers: HeaderMap| async move { echo_auth(headers) }),
        ))
        .await;
        let entry = serve(
            axum::Router::new()
                .route(
                    "/cross",
                    get(move || async move {
                        Redirect::temporary(&format!("http://localhost:{}/echo", elsewhere.port()))
                            .into_response()
                    }),
                )
                .route(
                    "/same",
                    get(|| async { Redirect::temporary("/echo").into_response() }),
                )
                .route(
                    "/echo",
                    get(|headers: HeaderMap| async move { echo_auth(headers) }),
                ),
        )
        .await;

        let policy = crate::runtime::FetchPolicy::new(
            [
                format!("localhost:{}", entry.port()),
                format!("localhost:{}", elsewhere.port()),
            ],
            Vec::<String>::new(),
        )
        .unwrap();
        let init = r#"{"headers":{"authorization":"Bearer s3cret"}}"#;

        let crossed = probe(
            "redirect-cross-origin.production",
            &format!("http://localhost:{}/cross", entry.port()),
            Some(init),
            policy.clone(),
        )
        .await;
        assert_eq!(crossed["ok"], true, "{crossed}");
        assert_eq!(
            crossed["body"], "none",
            "the credential travelled to another origin: {crossed}"
        );

        // ... and dropping it on a redirect that stays where it was would be
        // just as wrong: an application that authenticates to a service must
        // still be authenticated after that service moves the path.
        let stayed = probe(
            "redirect-same-origin.production",
            &format!("http://localhost:{}/same", entry.port()),
            Some(init),
            policy,
        )
        .await;
        assert_eq!(stayed["ok"], true, "{stayed}");
        assert_eq!(stayed["body"], "Bearer s3cret", "{stayed}");
    }

    #[tokio::test]
    async fn a_location_that_is_not_http_is_not_followed() {
        // `file:` is the node's own disk and `data:` never leaves the isolate;
        // neither is something an outbound HTTP call may be turned into by the
        // server that answered it.
        use axum::response::{IntoResponse, Redirect};
        use axum::routing::get;

        for location in ["file:///etc/passwd", "data:text/plain,pwned"] {
            let addr = serve(axum::Router::new().route(
                "/",
                get(move || async move { Redirect::temporary(location).into_response() }),
            ))
            .await;

            let seen = probe(
                "redirect-scheme.production",
                &format!("http://localhost:{}/", addr.port()),
                None,
                allow_localhost(addr.port()),
            )
            .await;

            assert_eq!(seen["ok"], false, "{location} was followed: {seen}");
            let message = seen["message"].as_str().unwrap();
            assert!(
                message.contains("http and https") || message.contains("scheme"),
                "{location}: {message}"
            );
        }
    }

    #[tokio::test]
    async fn redirect_manual_and_redirect_error_still_mean_what_they_mean() {
        // Following redirects here must not take the application's own say in
        // it away. `manual` hands the 3xx back -- and an application that then
        // acts on it makes another `fetch`, which comes through the same guard
        // -- and `error` fails the call.
        use axum::response::{IntoResponse, Redirect};
        use axum::routing::get;

        let addr = serve(
            axum::Router::new()
                .route(
                    "/",
                    get(|| async { Redirect::temporary("/landed").into_response() }),
                )
                .route("/landed", get(|| async { "arrived" })),
        )
        .await;
        let url = format!("http://localhost:{}/", addr.port());

        let manual = probe(
            "redirect-manual.production",
            &url,
            Some(r#"{"redirect":"manual"}"#),
            allow_localhost(addr.port()),
        )
        .await;
        assert_eq!(manual["ok"], true, "{manual}");
        assert_eq!(manual["status"], 307, "{manual}");
        assert_eq!(manual["location"], "/landed", "{manual}");
        assert_eq!(manual["redirected"], false, "{manual}");
        assert_eq!(manual["url"], url, "{manual}");

        let errored = probe(
            "redirect-error.production",
            &url,
            Some(r#"{"redirect":"error"}"#),
            allow_localhost(addr.port()),
        )
        .await;
        assert_eq!(errored["ok"], false, "{errored}");
        assert_eq!(errored["kind"], "TypeError", "{errored}");
        assert!(
            errored["message"].as_str().unwrap().contains("error"),
            "{errored}"
        );
    }

    // --- the execution budget ----------------------------------------------

    #[tokio::test]
    async fn a_fetch_that_never_answers_is_cut_off_by_the_execution_budget() {
        // The defect this closes: an execution timeout is enforced by
        // terminating V8, and terminating V8 does nothing to a Rust operation
        // already in flight. A `fetch` to an address that accepts the
        // connection and then says nothing was measured returning after 81
        // seconds against a 30-second budget -- and for all of that time it
        // held the runtime thread its application shares with every other
        // application on that thread.
        //
        // Two things are asserted, and the first is the one that would have
        // been impossible before: the *application* comes back, with an error
        // it could catch, inside its own budget. A terminated isolate produces
        // no response at all, so `body_of` would not have got one.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Accepted and then held open, unanswered, for as long as the test
            // runs. Dropping the sockets would answer with a close, which is
            // an answer.
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });

        let policy =
            crate::runtime::FetchPolicy::new([addr.to_string()], Vec::<String>::new()).unwrap();
        let limits = ExecutionLimits {
            timeout_ms: 2_000,
            ..ExecutionLimits::default()
        };

        let started = std::time::Instant::now();
        let seen = probe_with_limits(
            "budget-hang.production",
            &format!("http://{addr}/"),
            None,
            policy,
            limits,
        )
        .await;
        let elapsed = started.elapsed();

        assert_eq!(seen["ok"], false, "{seen}");
        assert_eq!(
            seen["name"], "TimeoutError",
            "the application has to be able to tell this apart from any other \
             network failure: {seen}"
        );
        assert!(
            seen["message"]
                .as_str()
                .unwrap()
                .contains("execution budget"),
            "{seen}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(4),
            "the call was not cut off by the budget: it took {elapsed:?} against a \
             2000ms timeout"
        );
    }

    #[tokio::test]
    async fn a_detached_fetch_is_aborted_when_the_invocation_ends() {
        // The other half of the budget's guarantee. The hang test above covers
        // the call the handler awaits; this covers the one it does not. A
        // `fetch` started and neither awaited nor handed to `waitUntil` is
        // still open when the handler returns. Its budget timer would not fire
        // for the rest of the budget, and the end-of-invocation sweep used to
        // only stop that timer -- so the call stayed open, its socket charged
        // to the node process, a resource shared with every other application,
        // until the far end closed it. A handler firing one such call per
        // request piled them up without bound. The sweep now aborts every call
        // still bounded by the budget, so a detached one cannot outlive its
        // request.
        //
        // Asserted where the abort is observable: the detached fetch's own
        // promise. The first request starts it against an upstream that
        // accepts and never answers, and stashes the promise on the isolate.
        // The sweep aborts the call as that request ends, rejecting the
        // promise with the `TimeoutError` the budget raises -- a rejection that
        // carries this specific name only when the budget aborted it. The
        // second request reads what the promise settled to, racing a short
        // timer so a promise still pending reports itself rather than hanging.
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Accept and hold, unanswered, so the only way the fetch settles is
            // an abort.
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1];
                    let _ = stream.read(&mut buf).await;
                });
            }
        });

        // One isolate, two requests: the first stashes the detached fetch on a
        // global, the second reads what it settled to. Both live in one module
        // that branches on whether the stash exists yet.
        let code = r#"
            export default async function handler(ctx) {
                if (!globalThis.__detached) {
                    globalThis.__detached = fetch(ctx.env.HANG_URL).then(
                        () => "resolved",
                        (e) => "rejected:" + (e && e.name ? e.name : String(e)),
                    );
                    // Deliberately neither awaited nor handed to waitUntil.
                    return new Response("started");
                }
                const pending = new Promise((r) => setTimeout(() => r("still-pending"), 200));
                return new Response(await Promise.race([globalThis.__detached, pending]));
            }
        "#;

        let policy =
            crate::runtime::FetchPolicy::new([addr.to_string()], Vec::<String>::new()).unwrap();
        let spec = IsolateSpec {
            env: HashMap::from([("HANG_URL".to_string(), format!("http://{addr}/"))]),
            fetch_policy: policy,
            limits: ExecutionLimits {
                timeout_ms: 30_000,
                ..ExecutionLimits::default()
            },
            ..IsolateSpec::default()
        };

        let mut runtime = AppRuntime::new(
            "detached-fetch.production".to_string(),
            code.to_string(),
            None,
            spec,
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
        let body = text(body);
        assert_eq!(status, 200);
        assert_eq!(body.as_deref(), Some("started"));

        let (tx2, rx2) = oneshot::channel();
        runtime
            .execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx2,
            )
            .await
            .unwrap();
        let (_status2, _, body2) = rx2.await.unwrap().unwrap();
        let outcome = text(body2).unwrap_or_default();
        assert_eq!(
            outcome, "rejected:TimeoutError",
            "a detached fetch was not aborted when its invocation ended (outcome: {outcome}); \
             its socket would outlive the request that made it"
        );
    }

    #[tokio::test]
    async fn an_isolate_serves_a_second_request_after_one_that_fetched() {
        // An isolate is reused, and the budget arms a timer per outbound call.
        // A timer left armed would be counting down inside the next request in
        // this same isolate; the sweep at the end of every invocation is what
        // stops that, and this is the test that would notice if the sweep, or
        // the arming, left the isolate unable to serve again.
        use axum::routing::get;

        let addr = serve(axum::Router::new().route("/", get(|| async { "again" }))).await;
        let url = format!("http://localhost:{}/", addr.port());
        let spec = IsolateSpec {
            env: HashMap::from([("PROBE_URL".to_string(), url)]),
            fetch_policy: allow_localhost(addr.port()),
            ..IsolateSpec::default()
        };

        let mut runtime = AppRuntime::new(
            "fetch-reuse.production".to_string(),
            REDIRECT_PROBE.to_string(),
            None,
            spec,
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        for attempt in 1..=2 {
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
                .unwrap_or_else(|e| panic!("request {attempt} did not complete: {e}"));
            let (status, _, body) = rx.await.unwrap().unwrap();
            let body = text(body);
            assert_eq!(status, 200, "request {attempt}: {body:?}");
            let seen: serde_json::Value = serde_json::from_str(&body.unwrap()).unwrap();
            assert_eq!(seen["ok"], true, "request {attempt}: {seen}");
            assert_eq!(seen["body"], "again", "request {attempt}: {seen}");
        }
    }

    #[tokio::test]
    async fn a_fetch_started_in_wait_until_still_runs_after_the_response_is_sent() {
        // The request's budget is disarmed when the handler's own execution
        // ends, which is before the `waitUntil` tail runs. Carrying what was
        // left of it into the tail would make every `waitUntil` fetch fail with
        // an expired budget -- the tail is meant to outlive the response. The
        // tail is armed with a budget of its own instead, which this call fits
        // inside comfortably; see the tail tests at the end of this file.
        use axum::routing::get;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        let addr = serve(axum::Router::new().route(
            "/",
            get(move || {
                let counted = counted.clone();
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    "counted"
                }
            }),
        ))
        .await;

        let code = r#"
            export default function handler(ctx) {
                ctx.waitUntil(fetch(ctx.env.PROBE_URL).then((r) => r.text()));
                return new Response("sent");
            }
        "#;
        let spec = IsolateSpec {
            env: HashMap::from([(
                "PROBE_URL".to_string(),
                format!("http://localhost:{}/", addr.port()),
            )]),
            fetch_policy: allow_localhost(addr.port()),
            ..IsolateSpec::default()
        };

        let body = body_of(
            "fetch-wait-until.production",
            code,
            spec,
            &ServiceWiring::default(),
        )
        .await;

        assert_eq!(body, "sent");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the waitUntil call never reached the upstream"
        );
    }

    // --- the waitUntil tail, and messages, have budgets of their own --------

    #[tokio::test]
    async fn a_wait_until_tail_that_never_finishes_is_cut_off_and_the_isolate_serves_again() {
        // The defect this closes: the clock was stopped as soon as the response
        // was sent, and `__deis_runtime_wait_until_all()` was then run with
        // nothing bounding it. Whatever the handler had handed to `waitUntil`
        // ran unbounded on the runtime thread that every other application on
        // that thread is waiting for, so one endless tail took the thread out
        // of service for good -- behind a request that had answered 200 and
        // looked entirely healthy.
        //
        // The tail here is the shape that actually happens: a background poll
        // whose exit condition never arrives. It yields between rounds, which
        // is what lets the cut-off task -- a `tokio::spawn`ed timer, the same
        // mechanism the request budget uses -- get a turn. A purely
        // synchronous loop still holds its thread; that is a property of the
        // mechanism itself and is described on `AppRuntime::timeout_ms`.
        let code = r#"
            export default function handler(ctx) {
                if (globalThis.__tail_started) {
                    return new Response("still serving");
                }
                globalThis.__tail_started = true;
                ctx.waitUntil((async () => {
                    for (;;) {
                        await new Promise((resolve) => setTimeout(resolve, 5));
                    }
                })());
                return new Response("answered");
            }
        "#;

        let spec = IsolateSpec {
            limits: ExecutionLimits {
                wait_until_timeout_ms: 500,
                ..ExecutionLimits::default()
            },
            ..IsolateSpec::default()
        };
        let mut runtime = AppRuntime::new(
            "wait-until-hang.production".to_string(),
            code.to_string(),
            None,
            spec,
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
        )
        .unwrap();
        runtime.init_module().await.unwrap();

        let (tx, rx) = oneshot::channel();
        let started = std::time::Instant::now();
        let ran = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            runtime.execute_fetch(
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
                tx,
            ),
        )
        .await
        .expect("the invocation never returned: the tail was not cut off");
        let elapsed = started.elapsed();
        ran.expect("a cut-off tail must not be reported as a failed request");

        let (status, _, body) = rx.await.unwrap().unwrap();
        let body = text(body);
        assert_eq!(status, 200, "{body:?}");
        assert_eq!(
            body.as_deref(),
            Some("answered"),
            "the response goes out before the tail runs at all"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(500),
            "the tail was cut off after {elapsed:?}, before the 500ms it was given"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the tail ran for {elapsed:?} against a 500ms budget"
        );

        // And the isolate is still usable. A cut-off leaves V8 marked for
        // termination, so a tail cut off without clearing that flag would take
        // the application off this thread just as surely as the hang did.
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
            .expect("the isolate must serve again after a tail was cut off");
        let (status, _, body) = rx.await.unwrap().unwrap();
        let body = text(body);
        assert_eq!(status, 200, "{body:?}");
        assert_eq!(body.as_deref(), Some("still serving"));
    }

    #[tokio::test]
    async fn a_tail_that_finishes_is_left_alone_even_past_the_request_budget() {
        // The other half: the tail has to be allowed to outlive the response,
        // which is the entire point of `waitUntil`. Measuring it against what
        // was left of the request's budget would cut off every tail that took
        // longer than the handler had to spare -- here, a request budget of
        // 200ms and a tail that takes rather longer than that on purpose.
        let code = r#"
            export default function handler(ctx) {
                if (globalThis.__tail_result) {
                    return new Response(globalThis.__tail_result);
                }
                ctx.waitUntil((async () => {
                    await new Promise((resolve) => setTimeout(resolve, 400));
                    globalThis.__tail_result = "the tail finished";
                })());
                return new Response("answered");
            }
        "#;

        let spec = IsolateSpec {
            limits: ExecutionLimits {
                timeout_ms: 200,
                wait_until_timeout_ms: 10_000,
                ..ExecutionLimits::default()
            },
            ..IsolateSpec::default()
        };
        let mut runtime = AppRuntime::new(
            "wait-until-slow.production".to_string(),
            code.to_string(),
            None,
            spec,
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
            .expect("the request runs");
        let (status, _, body) = rx.await.unwrap().unwrap();
        let body = text(body);
        assert_eq!(status, 200, "{body:?}");
        assert_eq!(body.as_deref(), Some("answered"));

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
            .expect("the second request runs");
        let (_, _, body) = rx.await.unwrap().unwrap();
        assert_eq!(
            text(body).as_deref(),
            Some("the tail finished"),
            "the tail was cut off although it was inside its own budget"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_websocket_message_that_never_finishes_is_cut_off() {
        // A message dispatched into an isolate runs the application's own
        // `onmessage` on the runtime thread, exactly as a request runs its
        // handler -- and it used to run with no budget at all. A socket stays
        // open as long as the client keeps it open and carries as many messages
        // as the client cares to send, so that was a way into unbounded
        // execution that needed no request at all.
        //
        // `_handleMessage` calls the handler synchronously, so the message
        // *is* the loop below: there is nothing for an async handler to hold
        // the dispatch with. That makes this the one budget test that has to
        // run on a multi-threaded runtime -- the cut-off is delivered by a
        // `tokio::spawn`ed timer, and a synchronous loop never gives a
        // single-threaded runtime the turn it would need to fire. On the node
        // each runtime thread has a `current_thread` runtime, so a loop of this
        // shape still holds its thread there; what this test pins down is that
        // the budget is armed, that the cut-off ends the message rather than
        // the isolate, and that the deadline outbound calls read is given back
        // afterwards.
        let code = r#"
            export default function handler() {
                const pair = new WebSocketPair();
                const socket = pair[1];
                socket.accept();
                socket.onmessage = () => {
                    for (;;) {}
                };
                return new Response(socket.ws_id);
            }
        "#;

        let spec = IsolateSpec {
            limits: ExecutionLimits {
                timeout_ms: 500,
                ..ExecutionLimits::default()
            },
            ..IsolateSpec::default()
        };
        let mut runtime = AppRuntime::new(
            "ws-message-hang.production".to_string(),
            code.to_string(),
            None,
            spec,
            &ServiceWiring::default(),
            // No pool behind this isolate, so a service call it made would be
            // refused as "not deployed on this node", which is what it is.
            ServiceRouter::detached(),
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
            .expect("the upgrade handler runs");
        let (status, _, body) = rx.await.unwrap().unwrap();
        let ws_id = text(body).expect("the handler answers with the socket id");
        assert_eq!(status, 200, "{ws_id}");

        let started = std::time::Instant::now();
        let dispatched = runtime
            .dispatch_ws_message(ws_id.clone(), "hello".to_string())
            .await;
        let elapsed = started.elapsed();

        assert!(
            dispatched.is_err(),
            "a message the application never finished handling must be reported as failed, \
             not acknowledged"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(500),
            "the message was cut off after {elapsed:?}, before the 500ms budget it was given"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "the message ran for {elapsed:?} against a 500ms budget"
        );

        // The application survives the message that was cut off. The socket
        // does not: `dispatcher/handlers.rs` ends the bridge for this
        // connection when a dispatch fails, which is the honest outcome for a
        // client whose message was never handled. What must not happen is the
        // isolate staying terminated, which would take the application off this
        // thread for every later request as well.
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
            .expect("the isolate must serve again after a message was cut off");
        let (status, _, body) = rx.await.unwrap().unwrap();
        assert_eq!(status, 200, "{:?}", text(body));
    }
}
