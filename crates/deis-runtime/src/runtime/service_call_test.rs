//! What a service binding resolves to, and what each refusal says.
//!
//! The end-to-end behaviour - two applications on one node actually calling
//! each other, the cycle refusal, the consent check, the subrequest charge -
//! is tested against a real pool in `super::pool`'s test module, because those
//! are properties of the dispatch and not of these types. What is here is the
//! part that has to be right before any of that runs: which `(application,
//! environment)` a binding means, and whether a caller can tell four different
//! failures apart.

use super::*;

fn declared(json: &str) -> deis_bundle::DeclaredBindings {
    serde_json::from_str(json).expect("a binding table")
}

#[test]
fn a_binding_that_names_no_environment_follows_the_caller() {
    // The default, and the one that keeps a preview stack self-contained.
    let bindings = ServiceBindings::resolve(
        &declared(r#"{"AUTH":{"kind":"service","app":"auth-api"}}"#),
        "pr-42",
    );
    let target = bindings.get("AUTH").expect("declared");
    assert_eq!(target.app, "auth-api");
    assert_eq!(target.environment, "pr-42");
    assert_eq!(target.tenant(), "auth-api.pr-42");
}

#[test]
fn a_binding_that_names_an_environment_means_that_one_wherever_it_runs() {
    // The escape hatch, and it is explicit on purpose: reaching production
    // from a preview is a decision, so it is written down in the manifest and
    // visible to whoever reads it.
    let bindings = ServiceBindings::resolve(
        &declared(r#"{"AUTH":{"kind":"service","app":"auth-api","environment":"production"}}"#),
        "pr-42",
    );
    assert_eq!(
        bindings.get("AUTH").expect("declared").tenant(),
        "auth-api.production"
    );
}

#[test]
fn only_service_bindings_are_in_the_table() {
    // A storage binding is a namespace of this tenant and is reached through
    // its own client; putting one here would give it a `fetch` that called
    // nothing.
    let bindings = ServiceBindings::resolve(
        &declared(
            r#"{"HOT":{"kind":"kv"},"DOCS":{"kind":"blob"},"AUTH":{"kind":"service","app":"a"}}"#,
        ),
        "production",
    );
    assert_eq!(bindings.names(), vec!["AUTH"]);
    assert!(bindings.get("HOT").is_none());
}

#[test]
fn the_table_reads_as_the_pairs_it_resolves_to() {
    let bindings = ServiceBindings::resolve(
        &declared(
            r#"{"AUTH":{"kind":"service","app":"auth"},"BILL":{"kind":"service","app":"bill","environment":"production"}}"#,
        ),
        "pr-1",
    );
    assert_eq!(
        bindings.describe(),
        "AUTH -> auth.pr-1, BILL -> bill.production"
    );
    assert_eq!(ServiceBindings::default().describe(), "nothing");
}

#[test]
fn the_caller_is_the_last_frame_of_the_chain() {
    let call = ServiceCall {
        path: vec!["front.pr-1".to_string(), "mid.pr-1".to_string()],
        binding: "AUTH".to_string(),
        url: "http://localhost/x".to_string(),
        method: "GET".to_string(),
        headers: Default::default(),
        body: None,
    };
    assert_eq!(call.caller_app(), "mid");
    assert_eq!(call.caller_tenant(), "mid.pr-1");
    assert_eq!(call.describe_path(), "front.pr-1 -> mid.pr-1");
}

#[test]
fn the_four_failures_a_caller_must_tell_apart_read_differently() {
    // The reason this is an enum. "not here", "will not take the call",
    // "failed inside it" and "you ran out of time waiting" have four different
    // fixes, and the last two in particular are two different budgets.
    let not_here = ServiceCallError::NotDeployedHere {
        binding: "AUTH".to_string(),
        callee: "auth.pr-1".to_string(),
    };
    let refused = ServiceCallError::NotConsented {
        binding: "AUTH".to_string(),
        callee: "auth.pr-1".to_string(),
        caller: "front".to_string(),
    };
    let theirs = ServiceCallError::CalleeFailed {
        binding: "AUTH".to_string(),
        callee: "auth.pr-1".to_string(),
        reason: "execution timeout".to_string(),
    };
    let mine = ServiceCallError::CallerBudgetSpent {
        binding: "AUTH".to_string(),
        callee: "auth.pr-1".to_string(),
    };

    assert!(not_here.to_string().contains("not deployed on this node"));
    assert!(refused.to_string().contains("callableBy"));
    assert!(theirs.to_string().contains("failed inside auth.pr-1"));
    assert!(
        mine.to_string().contains("this invocation's own"),
        "the caller's own budget must be named as the caller's: {mine}"
    );

    // And they are distinguishable without reading English.
    let kinds: Vec<&str> = [&not_here, &refused, &theirs, &mine]
        .iter()
        .map(|e| e.kind())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "not-deployed-here",
            "not-consented",
            "callee-failed",
            "caller-budget-spent"
        ]
    );

    // Whether the callee ran decides whether a retry repeats work.
    assert!(!not_here.reached_the_callee());
    assert!(!refused.reached_the_callee());
    assert!(theirs.reached_the_callee());
    assert!(mine.reached_the_callee());
}

#[test]
fn a_cycle_refusal_prints_the_chain_it_would_have_closed() {
    let refusal = ServiceCallError::Cycle {
        path: "a.production -> b.production".to_string(),
        callee: "a.production".to_string(),
    };
    let message = refusal.to_string();
    assert!(
        message.contains("a.production -> b.production"),
        "{message}"
    );
    assert!(
        message.contains("cannot be completed at all"),
        "a cycle is not a slow call and the message must not read like one: {message}"
    );
}

#[test]
fn a_depth_refusal_names_the_ceiling_and_the_chain() {
    let refusal = ServiceCallError::TooDeep {
        path: "a.p -> b.p -> c.p -> d.p".to_string(),
        ceiling: MAX_SERVICE_CALL_DEPTH,
    };
    let message = refusal.to_string();
    assert!(message.contains("a.p -> b.p -> c.p -> d.p"), "{message}");
    assert!(
        message.contains(&MAX_SERVICE_CALL_DEPTH.to_string()),
        "{message}"
    );
}

#[tokio::test]
async fn a_body_already_in_memory_is_handed_over_and_an_oversized_one_is_refused() {
    let bytes = collect_body(ResponseBody::Bytes(b"hello".to_vec()), 16)
        .await
        .expect("under the ceiling");
    assert_eq!(bytes, b"hello");

    assert_eq!(
        collect_body(ResponseBody::None, 16).await.expect("no body"),
        Vec::<u8>::new()
    );

    let refusal = collect_body(ResponseBody::Bytes(vec![0u8; 32]), 16)
        .await
        .expect_err("over the ceiling");
    assert!(refusal.contains("32 bytes"), "{refusal}");
}

#[test]
fn the_callers_allowance_is_what_is_left_of_its_own_budget() {
    assert_eq!(
        caller_allowance(250.0),
        Some(std::time::Duration::from_millis(250))
    );
    // An isolate with no deadline of its own waits on the callee's, which the
    // callee's own watchdog enforces.
    assert_eq!(caller_allowance(-1.0), None);
    // A budget already gone is zero and not "no deadline": the call is cut off
    // at once rather than waiting for ever.
    assert_eq!(
        caller_allowance(0.0),
        Some(std::time::Duration::from_millis(0))
    );
}
