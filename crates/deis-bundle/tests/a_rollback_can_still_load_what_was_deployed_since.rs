//! An older build must still be able to load a bundle a newer builder wrote.
//!
//! This is the direction `deis_bundle::compatibility` does not cover. That
//! module keeps an *old* application working on a *new* node; this file is
//! about the reverse, which is what a rollback is: the binaries go back one
//! release, and every version deployed since is now a bundle written by a
//! builder newer than the code reading it.
//!
//! It used to be fatal. `limits`, `compatibility` and each `bindings` entry
//! were `deny_unknown_fields`, and serde's refusal is per document: one
//! unrecognised key threw away the entire manifest, and with it the
//! application's code, assets, routes, environment and bindings. There was no
//! repair, either - the manifest sits inside the archive whose sha256 *is* the
//! version id, so re-uploading a fixed manifest produces a different version
//! rather than mending the existing one. What an operator saw was nodes
//! reporting degraded with versions they could not load, applications
//! answering 500 through the proxy, and, on a control plane one release
//! behind, every application's cron schedule quietly emptying out, because
//! reading the expressions means parsing the manifest.
//!
//! The rule now is in `deis_bundle::forward`: within one `schema` a manifest
//! is additive, an older build ignores what it does not know and says so, and
//! only a `schema` it does not know refuses a bundle whole.
//!
//! That first went in covering unknown *keys* only, and four **values** in
//! known fields went on doing exactly the same damage: a binding kind, a
//! not-found behaviour, a redirect status and a compatibility flag are each an
//! allow-list of the spellings one release happens to have, and each of them
//! refused the whole bundle for a spelling from the next one. The second half
//! of this file is that case. The rule for a value is in
//! `deis_bundle::forward` beside the rule for a key: take the narrowest
//! reading the field can already hold, say so at `warn`, and never refuse the
//! document.

use deis_bundle::{
    BundleError, MANIFEST_ENTRY, NotFound, SUPPORTED_SCHEMA, WORKER_ENTRY, read_manifest,
    unknown_fields, unreadable_values,
};
use std::io::{Cursor, Write};

fn zip_with(files: &[(&str, &str)]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
        for (name, body) in files {
            w.start_file(*name, zip::write::FileOptions::default())
                .unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.finish().unwrap();
    }
    buf
}

/// A manifest as a builder one release newer would write it: every block this
/// build knows, plus one key in each that it does not, plus a whole top-level
/// block it has never heard of.
///
/// The added keys are deliberately plausible rather than nonsense. A key that
/// breaks a rollback is never a hostile one; it is the next small feature.
const FROM_A_NEWER_BUILDER: &str = r#"{
  "schema": 1,
  "app": "demo",
  "entry": "worker/index.js",
  "env": {"LOG_LEVEL": "info"},
  "cron": ["*/5 * * * *"],
  "routes": [{"pattern": "/hello/:name", "methods": ["GET"]}],
  "limits": {"timeout_ms": 1500, "max_open_sockets": 8},
  "compatibility": {"date": "2026-09-01", "profile": "strict"},
  "bindings": {"HOT": {"kind": "kv", "consistency": "strong"}},
  "observability": {"trace_sampling": 0.1}
}"#;

#[test]
fn a_bundle_a_newer_builder_wrote_still_loads_here() {
    let bundle = zip_with(&[
        (MANIFEST_ENTRY, FROM_A_NEWER_BUILDER),
        (WORKER_ENTRY, "export default () => new Response('hi')"),
    ]);

    let parsed = read_manifest(&bundle).expect(
        "a manifest carrying keys this build does not read must still load: refusing it costs \
         the application's code, routes, environment and bindings, and no re-upload can repair \
         the version, because the manifest is inside the archive whose digest is the version id",
    );

    // Everything this build does read is intact. That is the whole claim: the
    // cost of being one release behind is the four declarations below, not
    // the application.
    let manifest = &parsed.manifest;
    assert_eq!(manifest.app, "demo");
    assert_eq!(manifest.entry, WORKER_ENTRY);
    assert_eq!(
        manifest.env.get("LOG_LEVEL").map(String::as_str),
        Some("info")
    );
    assert_eq!(manifest.cron, vec!["*/5 * * * *"]);
    assert_eq!(manifest.route_patterns(), vec!["/hello/:name"]);
    assert_eq!(
        manifest.limits.as_ref().and_then(|l| l.timeout_ms),
        Some(1500),
        "a limit this build knows must survive a limit it does not"
    );
    assert_eq!(
        manifest
            .compatibility_profile()
            .expect("this build knows that date")
            .date()
            .map(|d| d.to_string())
            .as_deref(),
        Some("2026-09-01"),
        "the compatibility block must still resolve"
    );
    assert!(manifest.bindings.get("HOT").is_some());
    assert_eq!(
        parsed.entry_module,
        "export default () => new Response('hi')"
    );

    // Ignored is not unsaid: `read_manifest` names every one of these at
    // `warn`, once, on the node that installs the version.
    assert_eq!(
        unknown_fields(&parsed.json),
        vec![
            "observability".to_string(),
            "limits.max_open_sockets".to_string(),
            "compatibility.profile".to_string(),
            "bindings.HOT.consistency".to_string(),
        ]
    );

    // And the text itself is untouched, so the keys this build dropped are
    // still carried to every component and are still there after the upgrade.
    assert!(parsed.json.contains("max_open_sockets"), "{}", parsed.json);
}

#[test]
fn a_schema_this_build_does_not_know_is_still_refused_whole() {
    // The one gate that remains, and the reason the rule above is safe: a
    // change an older build could not have ignored safely takes a new schema
    // number, and then the refusal is total, immediate and by name rather
    // than an application running on half a manifest.
    let future = FROM_A_NEWER_BUILDER.replace("\"schema\": 1", "\"schema\": 2");
    let bundle = zip_with(&[(MANIFEST_ENTRY, &future), (WORKER_ENTRY, "export {}")]);

    let err = read_manifest(&bundle).expect_err("a schema this build does not know is refused");
    assert_eq!(
        err,
        BundleError::UnsupportedSchema {
            found: 2,
            supported: SUPPORTED_SCHEMA
        }
    );
    let message = err.to_string();
    assert!(message.contains('2'), "{message}");
    assert!(
        message.contains(&SUPPORTED_SCHEMA.to_string()),
        "the refusal has to name both numbers: {message}"
    );
}

/// The same bundle, from a builder several releases newer: every value this
/// build can read is still readable, and beside each one sits a value from an
/// allow-list that has grown since - a vector namespace, a maintenance page, a
/// `410 Gone` rule, a flag for a behaviour that did not exist yet.
const VALUES_FROM_A_NEWER_BUILDER: &str = r#"{
  "schema": 1,
  "app": "demo",
  "entry": "worker/index.js",
  "env": {"LOG_LEVEL": "info"},
  "cron": ["*/5 * * * *"],
  "routes": [{"pattern": "/hello/:name", "methods": ["GET"]}],
  "bindings": {
    "HOT": {"kind": "kv"},
    "SEARCH": {"kind": "vector"}
  },
  "site": {
    "not_found": "maintenance-page",
    "redirects": [
      {"from": "/gone", "to": "/gone.html", "status": 410},
      {"from": "/old", "to": "/new", "status": 301}
    ]
  },
  "compatibility": {
    "date": "2026-09-01",
    "flags": ["streaming_uploads", "single_set_cookie_header"]
  }
}"#;

#[test]
fn a_value_a_newer_builder_wrote_costs_that_declaration_and_nothing_else() {
    let bundle = zip_with(&[
        (MANIFEST_ENTRY, VALUES_FROM_A_NEWER_BUILDER),
        (WORKER_ENTRY, "export default () => new Response('hi')"),
    ]);

    let parsed = read_manifest(&bundle).expect(
        "a manifest holding values this build does not know must still load. Each of these four \
         fields is an allow-list that grows with the platform, so refusing one costs the whole \
         application - its code, its routes, its other bindings - on every node one release \
         behind, and no re-upload repairs it: the manifest is inside the archive whose digest is \
         the version id",
    );
    let manifest = &parsed.manifest;

    // Everything beside the four is untouched. That is the claim.
    assert_eq!(manifest.app, "demo");
    assert_eq!(manifest.cron, vec!["*/5 * * * *"]);
    assert_eq!(manifest.route_patterns(), vec!["/hello/:name"]);
    assert_eq!(
        manifest.env.get("LOG_LEVEL").map(String::as_str),
        Some("info")
    );

    // A binding kind this build does not know: the entry is simply not
    // published, so `env.SEARCH` is undefined and the one handler that reaches
    // for it fails on that call. The binding beside it is untouched.
    assert!(manifest.bindings.get("HOT").is_some());
    assert!(
        manifest.bindings.get("SEARCH").is_none(),
        "a kind this build cannot act on must not be guessed at"
    );
    assert_eq!(
        manifest.bindings.unreadable_kinds(),
        vec![("SEARCH", "vector")],
        "and it must not vanish either: an operator has to be able to see it"
    );

    // A not-found behaviour this build does not know reads as "none", which is
    // what every application had before the field existed.
    assert_eq!(manifest.site.not_found, NotFound::None);

    // A redirect status this build cannot answer with drops that one rule.
    // The rule beside it still works, which is the point of dropping rather
    // than refusing.
    let table = deis_bundle::RedirectTable::compile(&manifest.site.redirects)
        .expect("a status from a newer release must not make the table unusable");
    assert_eq!(table.len(), 1);
    assert!(
        matches!(
            table.apply(&["old".to_string()], ""),
            Some(deis_bundle::RedirectAction::Redirect { status: 301, .. })
        ),
        "the rules this build does understand still answer"
    );
    assert!(
        table.apply(&["gone".to_string()], "").is_none(),
        "and the one it does not is not answered with a status it made up"
    );

    // A compatibility flag this build does not know is ignored; the flag
    // beside it still decides what is on.
    let profile = manifest
        .compatibility_profile()
        .expect("an unknown flag name must not make the profile unresolvable");
    assert_eq!(
        profile.date().map(|d| d.to_string()).as_deref(),
        Some("2026-09-01")
    );
    assert!(
        !profile.is_on(deis_bundle::Flag::MultipleSetCookieHeaders),
        "single_set_cookie_header still holds the behaviour back"
    );

    // Narrowed is not unsaid: `read_manifest` names all four at `warn`, once,
    // on the node that installs the version.
    assert_eq!(
        unreadable_values(&parsed.json),
        vec![
            "bindings.SEARCH.kind = \"vector\"".to_string(),
            "site.not_found = \"maintenance-page\"".to_string(),
            "site.redirects[0].status = 410".to_string(),
            "compatibility.flags[0] = \"streaming_uploads\"".to_string(),
        ]
    );
    // None of it is an unknown *key*: every one of these sits in a field this
    // build reads.
    assert!(unknown_fields(&parsed.json).is_empty(), "{}", parsed.json);

    // And the text is untouched, so the values this build narrowed are still
    // carried to every component and are still there after the upgrade.
    assert!(parsed.json.contains("\"vector\""), "{}", parsed.json);
    assert!(parsed.json.contains("410"), "{}", parsed.json);
}

/// The one value that is still refused, and the shape of that refusal.
///
/// A `compatibility.date` past this build's own knowledge has no narrow
/// reading: the older answers are not less of what it asked for, they are a
/// different one. So it refuses - but only where the question is asked, not by
/// making the manifest unreadable, and the same bundle resolves on a node one
/// release newer.
#[test]
fn a_date_this_build_cannot_honour_refuses_itself_and_not_the_manifest() {
    let future = VALUES_FROM_A_NEWER_BUILDER.replace("\"2026-09-01\"", "\"2099-01-01\"");
    let bundle = zip_with(&[(MANIFEST_ENTRY, &future), (WORKER_ENTRY, "export {}")]);

    let parsed = read_manifest(&bundle)
        .expect("the manifest still parses: everything but the compatibility answer is readable");
    assert_eq!(parsed.manifest.route_patterns(), vec!["/hello/:name"]);

    let err = parsed
        .manifest
        .compatibility_profile()
        .expect_err("this build cannot give an application the semantics of a day it never saw");
    assert!(err.contains("2099-01-01"), "{err}");
    assert!(
        err.contains(&deis_bundle::PLATFORM_KNOWN_THROUGH.to_string()),
        "the refusal has to name both days: {err}"
    );
}
