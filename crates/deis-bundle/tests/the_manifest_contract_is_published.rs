//! Every field of a manifest has to appear in the document that publishes the
//! bundle format.
//!
//! `deis-bundle` is the only definition of what a manifest means, and
//! `docs/implementation-plan.zh.md` section 2 is where somebody building
//! against it reads that definition - the README sends them there, calling it
//! "the contracts to build against". Nothing in the compiler ties the two
//! together, so a field added here reaches the format without reaching the
//! contract, and the author of the next bundle builds against a manifest that
//! is missing whatever landed last.
//!
//! That has already happened three times over. `bindings`, `callable_by` and
//! `compatibility` were in this struct, honoured by the build, the control
//! plane, the node and the isolate, and named nowhere in that document: an
//! author reading it would conclude that an application gets one KV, one
//! database and one object store, cannot be called by another application,
//! and has no way to pin the day's behaviour. It is the same shape as the
//! object store's own entry in that section, which described a service that
//! could only put, get and delete long after it had grown listing, `Head`,
//! ranged reads and conditional writes - and an author who believed it built
//! the key index they did not need.
//!
//! So this is a check rather than a convention. It asserts the *names*, not
//! the prose: a name present says somebody had to write a sentence about the
//! field, and what that sentence says is a reviewer's job.

use std::path::PathBuf;

use deis_bundle::{
    Asset, Binding, BindingKind, DeclaredBindings, DeclaredCompatibility, DeclaredLimits, Manifest,
    NotFound, QueueConsumer, Route, SiteConfig,
};

/// Where the published contract lives, relative to this crate.
const CONTRACT_DOC: &str = "../../docs/implementation-plan.zh.md";

/// The section the bundle format is written under, and the one that ends it.
///
/// Matched on the numbered heading marker alone. The headings themselves are
/// written in Chinese, and a code file in this repository holds no CJK - so
/// the marker is what a check on this side can name without keeping a second,
/// unreadable copy of a title that is free to be reworded.
const SECTION_START: &str = "\n## 2. ";
const SECTION_END: &str = "\n## 3. ";

/// A manifest with every optional field filled in.
///
/// Filled rather than defaulted because five of the fields carry
/// `skip_serializing_if`: a default manifest serialises without them, and a
/// check reading those keys off the JSON would then never see the three
/// fields that were actually missing from the document.
fn a_manifest_holding_every_field() -> Manifest {
    let mut bindings = DeclaredBindings::default();
    bindings.insert("HOT", Binding::storage(BindingKind::Kv));

    Manifest {
        schema: 1,
        app: "demo".to_string(),
        entry: "worker/index.js".to_string(),
        runtime: Some("standard-v1".to_string()),
        services: [("kv".to_string(), true)].into_iter().collect(),
        bindings,
        callable_by: vec!["storefront".to_string()],
        env: [("LOG_LEVEL".to_string(), "info".to_string())]
            .into_iter()
            .collect(),
        secrets: vec!["API_KEY".to_string()],
        environment: Some(deis_wire::EnvironmentDeclarations::new(
            "edge-canon.next@60eacb13cda28ecc2ecb5508a2eff95ef56dff66",
            Vec::new(),
        )),
        routes: vec![Route {
            pattern: "/api/users/:id".to_string(),
            methods: vec!["GET".to_string()],
        }],
        cron: vec!["0 */6 * * *".to_string()],
        queues: vec![QueueConsumer {
            name: "jobs".to_string(),
            batch_size: Some(10),
            max_concurrency: Some(2),
        }],
        assets: vec![Asset {
            path: "index.html".to_string(),
            sha256: Some("0".repeat(64)),
            content_type: Some("text/html".to_string()),
            size: Some(512),
        }],
        site: SiteConfig {
            not_found: NotFound::Page,
            ..SiteConfig::default()
        },
        limits: Some(DeclaredLimits::default()),
        compatibility: Some(DeclaredCompatibility::default()),
    }
}

#[test]
fn every_manifest_field_is_named_in_the_document_that_publishes_the_format() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CONTRACT_DOC);
    // A missing document fails rather than skips. A skipped check reads
    // exactly like a passing one, which is how a contract goes unpublished
    // for three fields at a time.
    let doc = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read the published contract at {}: {e}",
            path.display()
        )
    });

    let start = doc.find(SECTION_START).unwrap_or_else(|| {
        panic!(
            "{} has no section 2; that is where the bundle format is published",
            path.display()
        )
    });
    let rest = &doc[start..];
    // The end is required, not `unwrap_or(len)`. Falling back to the rest of
    // the document would leave this check passing on a field named anywhere
    // at all - in the wave record, in a section about something else - which
    // is exactly the reassurance it exists to withhold.
    let end = rest.find(SECTION_END).unwrap_or_else(|| {
        panic!(
            "{} has a section 2 but no section 3 after it, so the bundle format's \
             section has no end to read up to",
            path.display()
        )
    });
    let section = &rest[..end];

    let serde_json::Value::Object(fields) =
        serde_json::to_value(a_manifest_holding_every_field()).expect("a manifest serialises")
    else {
        panic!("a manifest serialises as a JSON object");
    };

    let undocumented: Vec<&String> = fields
        .keys()
        .filter(|name| !section.contains(name.as_str()))
        .collect();

    assert!(
        undocumented.is_empty(),
        "{undocumented:?} are manifest fields that section 2 of {} never names. \
         A field the format carries and the contract does not mention is a field the next \
         author builds without. Write it into that section - what it holds, what absent \
         means, and who reads it - rather than deleting it from this list.",
        path.display()
    );
}
