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

use deis_bundle::{
    BundleError, MANIFEST_ENTRY, SUPPORTED_SCHEMA, WORKER_ENTRY, read_manifest, unknown_fields,
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
