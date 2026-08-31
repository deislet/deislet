// The producer ops as an application reaches them: through `js/runtime.js`,
// inside a real isolate.
//
// Both tests are about the JavaScript half, because that is where the shape of
// `sendBatch` is decided and where the wiring between the two halves either
// exists or does not. An op that is registered and called from nowhere is the
// failure this repository keeps producing, so the second test drives the whole
// path from `ctx.services.queue.sendBatch` down to the op and asserts on the
// op's own answer.

#[cfg(test)]
mod tests {
    use crate::runtime::isolate::{AppRuntime, IsolateSpec};
    use crate::runtime::ServiceWiring;
    use std::collections::HashMap;

    /// Run one handler and hand back what it answered.
    async fn answer(code: &str) -> (u16, String) {
        let mut runtime = AppRuntime::new(
            "queue_ops_app.production".to_string(),
            code.to_string(),
            None,
            IsolateSpec::default(),
            // No queue endpoint, so no queue client is installed: the ops are
            // reached, and answer that the service is not available.
            &ServiceWiring::default(),
        )
        .expect("the isolate builds");
        runtime.init_module().await.expect("the module loads");

        let (tx, rx) = tokio::sync::oneshot::channel();
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
        let (status, _headers, body) = rx.await.expect("a response").expect("a response");
        (
            status,
            String::from_utf8(body.into_bytes().unwrap_or_default()).expect("text"),
        )
    }

    /// A bare value where a `{ body }` belongs is refused, by name.
    ///
    /// `send(x)` takes the message itself, so an array of bare values and an
    /// array of `{ body }` wrappers cannot both be accepted here without one
    /// of them silently meaning the other: code ported from Cloudflare passes
    /// the wrappers, and reading those as messages would enqueue the literal
    /// text `{"body":"..."}` with nothing reporting it. The refusal has to say
    /// what to write instead, because the two shapes look equally reasonable.
    #[tokio::test]
    async fn a_batch_entry_that_is_not_a_body_wrapper_is_refused_with_the_shape_to_use() {
        let (status, body) = answer(
            r#"
            export default async function handler(context) {
                try {
                    await context.services.queue.sendBatch(["a bare string"]);
                    return new Response("no refusal");
                } catch (e) {
                    return new Response(String(e));
                }
            }
        "#,
        )
        .await;

        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains("must be an object with a \"body\" property"),
            "the refusal must name the shape to write: {body}"
        );
        assert!(
            body.contains("Nothing was enqueued"),
            "an application holding a rejected promise has to know nothing was queued: {body}"
        );
    }

    /// The JavaScript side really calls the op.
    ///
    /// A well-formed batch reaches `op_queue_send_batch`, which answers that
    /// this application has no queue bound -- an answer only the op can give.
    /// Without this the whole feature could be registered, tested and reached
    /// from nowhere, which is the shape of defect this codebase keeps
    /// producing.
    #[tokio::test]
    async fn a_well_formed_batch_reaches_the_op_rather_than_stopping_in_javascript() {
        let (status, body) = answer(
            r#"
            export default async function handler(context) {
                try {
                    await context.services.queue.sendBatch([
                        { body: "one" },
                        { body: { two: true } },
                        { body: new Uint8Array([1, 2, 3]) },
                    ]);
                    return new Response("sent");
                } catch (e) {
                    return new Response(String(e));
                }
            }
        "#,
        )
        .await;

        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains("queue service is not available to this app"),
            "the batch must reach the op, which is the only thing that can answer this: {body}"
        );
    }

    /// An empty array is refused rather than resolving.
    ///
    /// deis-queue refuses a batch with no bodies, so resolving here would tell
    /// an application that something was enqueued when the server would have
    /// said otherwise -- and it costs a round trip to find out.
    #[tokio::test]
    async fn an_empty_batch_is_refused_without_a_round_trip() {
        let (status, body) = answer(
            r#"
            export default async function handler(context) {
                try {
                    await context.services.queue.sendBatch([]);
                    return new Response("no refusal");
                } catch (e) {
                    return new Response(String(e));
                }
            }
        "#,
        )
        .await;

        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains("the array is empty"),
            "an empty batch must say so rather than look like a successful send: {body}"
        );
    }
}
