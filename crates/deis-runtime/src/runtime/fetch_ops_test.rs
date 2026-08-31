#[cfg(test)]
mod tests {
    use crate::runtime::fetch_ops::*;
    use std::time::Duration;

    /// The margin is what buys the handler its `catch` block. Too small and
    /// the abort and the isolate's own termination race; too large and an
    /// application with a short timeout loses most of it to a reserve it
    /// never asked for.
    #[test]
    fn a_call_gets_its_budget_less_a_margin_to_answer_in() {
        // The platform default. A tenth would be three seconds, which is far
        // more than a `catch` and a `return` need, so the cap applies.
        assert_eq!(
            allowance(Duration::from_secs(30)),
            Duration::from_millis(29_500)
        );

        // In the middle: a tenth, because it is between the floor and the cap.
        assert_eq!(
            allowance(Duration::from_secs(2)),
            Duration::from_millis(1_800)
        );

        // Short enough that a tenth would be 30ms, which is not long enough to
        // be worth calling a margin, so the floor applies.
        assert_eq!(
            allowance(Duration::from_millis(300)),
            Duration::from_millis(250)
        );
    }

    /// A budget already spent must not wrap around into an enormous one.
    #[test]
    fn a_budget_smaller_than_the_margin_leaves_nothing() {
        assert_eq!(allowance(Duration::from_millis(40)), Duration::ZERO);
        assert_eq!(allowance(Duration::ZERO), Duration::ZERO);
    }

    /// The margin never grows without bound: whatever the operator sets, the
    /// reserve stays a slice and the call keeps the rest.
    #[test]
    fn the_margin_stays_a_slice_of_even_a_very_long_budget() {
        let remaining = Duration::from_secs(600);
        assert_eq!(remaining - allowance(remaining), MAX_REPLY_MARGIN);
    }

    /// The ceiling fires inside a real isolate, and the refusal names it.
    ///
    /// Driven through an `AppRuntime` rather than against the op directly,
    /// because the thing that has to be true is that `globalThis.fetch` -- the
    /// wrapper in `js/bootstrap.js`, which is the only `fetch` an application
    /// can reach -- goes through the guard on every call, and that the guard's
    /// refusal arrives as a catchable `TypeError` the way a failed `fetch`
    /// does.
    ///
    /// The destination is the discard port on loopback, allowlisted for this
    /// isolate: the policy lets it through (which is what makes the charge
    /// stick) and nothing is listening, so the first call fails on the
    /// connection without needing a server or a network. What is asserted is
    /// the *second* call, which never gets that far.
    #[tokio::test]
    async fn a_second_outbound_call_is_refused_when_the_ceiling_is_one() {
        use crate::config::ExecutionLimits;
        use crate::runtime::fetch_policy::FetchPolicy;
        use crate::runtime::isolate::{AppRuntime, IsolateSpec};
        use crate::runtime::ServiceWiring;
        use std::collections::HashMap;

        let code = r#"
            export default async function handler() {
                const answers = [];
                for (let i = 0; i < 2; i++) {
                    try {
                        await fetch("http://127.0.0.1:9/");
                        answers.push("reached");
                    } catch (e) {
                        answers.push(String(e));
                    }
                }
                return new Response(JSON.stringify(answers));
            }
        "#;

        let mut runtime = AppRuntime::new(
            "subrequest_app.production".to_string(),
            code.to_string(),
            None,
            IsolateSpec {
                limits: ExecutionLimits {
                    max_subrequests: 1,
                    ..ExecutionLimits::default()
                },
                fetch_policy: FetchPolicy::new(["127.0.0.1:9"], Vec::<String>::new())
                    .expect("a host:port entry"),
                ..IsolateSpec::default()
            },
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
        let body = String::from_utf8(body.into_bytes().unwrap_or_default()).expect("text");
        assert_eq!(status, 200, "{body}");

        let answers: Vec<String> = serde_json::from_str(&body).expect("a JSON array");
        assert_eq!(answers.len(), 2, "{body}");
        // The first call was allowed through the guard and spent the whole
        // allowance; how it then failed on the wire is not this test's
        // subject, only that it was not refused by the ceiling.
        assert!(
            !answers[0].contains("outbound call(s)"),
            "the first call of an invocation must not meet the ceiling: {}",
            answers[0]
        );
        assert!(
            answers[1].contains("already made 1 outbound call(s)"),
            "the refusal has to name the ceiling and the number reached: {}",
            answers[1]
        );
        assert!(
            answers[1].contains("Each redirect hop counts as one"),
            "an author whose code contains one fetch has to be told why the count is higher: {}",
            answers[1]
        );
    }
}
