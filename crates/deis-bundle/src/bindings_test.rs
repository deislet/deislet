//! Tests for the `bindings` block.
//!
//! The ones that matter are the refusals: a binding declaration that parses
//! and is wrong is a namespace an application writes into without knowing, or
//! an application calling one it was never meant to reach.

use super::*;

fn parse(json: &str) -> Result<DeclaredBindings, serde_json::Error> {
    serde_json::from_str(json)
}

#[test]
fn a_declaration_round_trips_through_the_manifest() {
    let json =
        r#"{"COLD":{"kind":"kv"},"HOT":{"kind":"kv"},"AUTH":{"kind":"service","app":"auth-api"}}"#;
    let declared = parse(json).expect("parses");
    assert_eq!(declared.len(), 3);
    assert_eq!(declared.names_of(BindingKind::Kv), vec!["COLD", "HOT"]);
    assert_eq!(
        declared.get("AUTH").map(|b| b.service_environment()),
        Some(ServiceEnvironment::Same)
    );
    // Serialised back in name order, so two builds of one project produce the
    // same bytes whatever order the author wrote the entries in.
    assert_eq!(
        serde_json::to_string(&declared).unwrap(),
        r#"{"AUTH":{"kind":"service","app":"auth-api"},"COLD":{"kind":"kv"},"HOT":{"kind":"kv"}}"#
    );
}

#[test]
fn a_misspelled_key_is_still_refused_once_the_binding_is_judged() {
    // An author who wrote "App" instead of "app" gets a service binding with
    // nothing to call. This used to be a serde refusal, from
    // `deny_unknown_fields`; it is now `validate`'s, because a parser that
    // refuses an unrecognised key refuses the whole manifest and therefore
    // every bundle a newer builder wrote (see `crate::forward`). The refusal
    // survived the move and says more than it did: serde could only name the
    // key it did not want, while this names what is missing and what to add.
    let declared = parse(r#"{"AUTH":{"kind":"service","App":"auth-api"}}"#)
        .expect("an unrecognised key must not cost the manifest that carries it");
    let err = declared
        .validate("demo")
        .expect_err("must still be refused");
    assert!(err.contains("names no application to call"), "{err}");

    // And the key itself is named, so the author sees the spelling too.
    assert_eq!(
        crate::forward::unknown_fields(
            r#"{"schema":1,"app":"d","entry":"e.js","bindings":{"AUTH":{"kind":"service","App":"a"}}}"#
        ),
        vec!["bindings.AUTH.App".to_string()]
    );
}

#[test]
fn two_names_that_differ_only_in_case_are_refused() {
    // On a case-insensitive filesystem these are one directory in deis-store
    // and one database file, so the two namespaces would be one namespace on
    // some machines and two on others.
    let declared = parse(r#"{"HOT":{"kind":"kv"},"hot":{"kind":"blob"}}"#).unwrap();
    let err = declared.validate("demo").expect_err("must be refused");
    assert!(err.contains("differ only in case"), "{err}");
}

#[test]
fn a_hostile_binding_name_never_becomes_a_namespace() {
    for hostile in ["a@b", "a.b", "..", ".", "a/b", "a b", "", &"x".repeat(65)] {
        let mut declared = DeclaredBindings::new();
        declared.insert(hostile, Binding::storage(BindingKind::Kv));
        assert!(
            declared.validate("demo").is_err(),
            "binding {} must be refused",
            hostile.escape_debug()
        );
    }
}

#[test]
fn the_platforms_own_binding_name_cannot_be_taken() {
    for taken in ["ASSETS", "assets", "DEIS_STORE_ENDPOINT", "deis_anything"] {
        let mut declared = DeclaredBindings::new();
        declared.insert(taken, Binding::storage(BindingKind::Kv));
        let err = declared
            .validate("demo")
            .expect_err("a reserved name must be refused");
        assert!(err.contains("reserved"), "{taken}: {err}");
    }
}

#[test]
fn a_storage_binding_cannot_name_another_application() {
    // The sharpest one. A storage binding is a namespace inside this tenant;
    // if `app` were accepted and ignored, an author would read their own
    // manifest as pointing somewhere it never pointed.
    let declared = parse(r#"{"HOT":{"kind":"kv","app":"someone-else"}}"#).unwrap();
    let err = declared.validate("demo").expect_err("must be refused");
    assert!(err.contains("can never name another application"), "{err}");

    let declared = parse(r#"{"HOT":{"kind":"kv","environment":"production"}}"#).unwrap();
    let err = declared.validate("demo").expect_err("must be refused");
    assert!(err.contains("always in the environment"), "{err}");
}

#[test]
fn a_service_binding_needs_something_to_call() {
    let declared = parse(r#"{"AUTH":{"kind":"service"}}"#).unwrap();
    let err = declared.validate("demo").expect_err("must be refused");
    assert!(err.contains("names no application"), "{err}");

    let declared = parse(r#"{"AUTH":{"kind":"service","app":"not a name"}}"#).unwrap();
    assert!(declared.validate("demo").is_err());

    let declared =
        parse(r#"{"AUTH":{"kind":"service","app":"auth","environment":"not a name"}}"#).unwrap();
    assert!(declared.validate("demo").is_err());
}

#[test]
fn an_application_cannot_bind_itself_in_its_own_environment() {
    // A guaranteed cycle: the caller's isolate would be waiting on its own
    // runtime thread. Refused where it is written rather than on the first
    // request that takes it.
    let declared = parse(r#"{"SELF":{"kind":"service","app":"demo"}}"#).unwrap();
    let err = declared.validate("demo").expect_err("must be refused");
    assert!(err.contains("calling itself"), "{err}");

    // Naming another environment of the same application is legitimate and
    // explicit, so it stands.
    let declared =
        parse(r#"{"PROD":{"kind":"service","app":"demo","environment":"production"}}"#).unwrap();
    assert!(declared.validate("demo").is_ok());
}

#[test]
fn absent_environment_means_the_callers_own() {
    let declared = parse(r#"{"AUTH":{"kind":"service","app":"auth-api"}}"#).unwrap();
    let binding = declared.get("AUTH").unwrap();
    assert_eq!(binding.service_environment().resolve("pr-42"), "pr-42");

    let declared =
        parse(r#"{"AUTH":{"kind":"service","app":"auth-api","environment":"production"}}"#)
            .unwrap();
    let binding = declared.get("AUTH").unwrap();
    assert_eq!(binding.service_environment().resolve("pr-42"), "production");
}

#[test]
fn describe_names_the_kind_and_the_target() {
    let declared = parse(
        r#"{"HOT":{"kind":"kv"},"AUTH":{"kind":"service","app":"auth-api","environment":"production"}}"#,
    )
    .unwrap();
    assert_eq!(
        declared.describe(),
        "AUTH (calls auth-api in environment production), HOT (a kv namespace of this tenant)"
    );
    assert_eq!(DeclaredBindings::new().describe(), "nothing");
}

#[test]
fn consent_is_read_from_the_list_the_callee_wrote() {
    assert!(callable_by_allows(&["frontend".into()], "frontend"));
    assert!(!callable_by_allows(&["frontend".into()], "someone-else"));
    assert!(callable_by_allows(&["*".into()], "anybody"));
    // An empty list is the default and consents to nobody, which is what an
    // application that never thought about being called should mean.
    assert!(!callable_by_allows(&[], "frontend"));
    // Not a prefix match: `front` must not reach a list naming `frontend`.
    assert!(!callable_by_allows(&["frontend".into()], "front"));
}

#[test]
fn a_callable_by_entry_that_is_not_an_application_is_refused() {
    assert!(validate_callable_by(&["frontend".into(), "*".into()]).is_ok());
    let err = validate_callable_by(&["front*".into()]).expect_err("must be refused");
    assert!(err.contains("not an application id"), "{err}");
    assert!(validate_callable_by(&["a.b".into()]).is_err());
}
