//! What an older build does with a manifest a newer one wrote.
//!
//! [`crate::compatibility`] answers one direction: a *new* node running an
//! *old* bundle keeps the behaviour that bundle was written against. This
//! module answers the other one, which had no answer at all: an *old* node,
//! or an old control plane, reading a bundle a newer builder wrote.
//!
//! # The rule
//!
//! **Within one [`crate::SUPPORTED_SCHEMA`], a manifest is additive only, and
//! a build ignores every key it does not know.** A key added without a schema
//! bump must therefore be *safe to ignore*: ignoring it may only narrow, or
//! leave unchanged, what the application can do. A key that must be honoured
//! to be correct - one whose absence would widen a limit, drop a check, or
//! change what a field already in the format means - is not additive, and it
//! takes a new `schema` number. An older build already refuses a schema it
//! does not know, whole and by name, which is the loud failure such a change
//! deserves.
//!
//! So the manifest has exactly one forward-compatibility gate, and it is the
//! `schema` field. Nothing else in a manifest may refuse a bundle for being
//! newer than the build reading it.
//!
//! # Why an unknown key used to be fatal, and why that was the wrong trade
//!
//! `limits`, `compatibility` and each `bindings` entry were `deny_unknown_fields`.
//! The reasoning written next to each was sound on its own - a misspelled
//! ceiling that parsed and was dropped leaves an author believing in a limit
//! that was never in force - but the cost was paid somewhere else entirely.
//!
//! Serde's refusal is per *document*, not per key: one key an older build has
//! never heard of rejects the whole manifest, and with it the application's
//! code, assets, routes, environment and bindings. And the manifest lives
//! inside the archive whose sha256 **is** the version id, so no re-upload can
//! repair a version that already exists. Rolling a fleet back by one commit
//! turned every version deployed since into a version that build cannot load:
//! nodes degraded, applications answering 500 through the proxy, and a control
//! plane one commit old dropping the cron schedule of every application,
//! because [`crate::Manifest::parse`] is what reads the expressions out.
//!
//! The typo is still caught, and closer to the author than a node ever was:
//! `deis_build_core::config::LimitsConfig` keeps `deny_unknown_fields`, so a
//! misspelled ceiling is refused where it was typed, in the project file, with
//! the author's own source on screen. What is left for a hand-written manifest
//! is [`report`], which names every key this build did not recognise at
//! `warn`, once per bundle read - at the build, at the upload, and on each
//! node that installs the version.
//!
//! # Keeping these lists honest
//!
//! The four tables below are the field names of four structs, and a name that
//! drifted would make a key this build *does* read look unrecognised. Nothing
//! in the compiler ties a `&'static str` to a struct field, so
//! `every_table_matches_the_struct_it_describes` in the tests below serialises
//! a fully populated instance of each and compares the key sets. Adding a
//! field without adding it here fails that test.

use serde_json::Value;
use tracing::warn;

/// Every key [`crate::Manifest`] itself reads.
pub const MANIFEST_FIELDS: &[&str] = &[
    "schema",
    "app",
    "entry",
    "runtime",
    "services",
    "bindings",
    "callable_by",
    "env",
    "secrets",
    "routes",
    "cron",
    "queues",
    "assets",
    "site",
    "limits",
    "compatibility",
];

/// Every key [`crate::DeclaredLimits`] reads.
pub const LIMITS_FIELDS: &[&str] = &[
    "memory_mb",
    "timeout_ms",
    "rate_limit_ops_per_sec",
    "cpu_time_limit_ms",
    "resource_table_max",
    "max_subrequests",
];

/// Every key [`crate::DeclaredCompatibility`] reads.
pub const COMPATIBILITY_FIELDS: &[&str] = &["date", "flags"];

/// Every key one [`crate::Binding`] reads.
pub const BINDING_FIELDS: &[&str] = &["kind", "app", "environment"];

/// How many key names one log line quotes before it stops and counts the
/// rest.
///
/// A manifest is untrusted input and its keys are attacker-chosen strings, so
/// a line naming all of them is a line an uploader can make arbitrarily long
/// in every node's log. The count that follows is what keeps the line honest
/// about how much it left out.
const MAX_NAMED: usize = 12;

/// The keys in `json` that this build does not read, as dotted paths -
/// `"observability"`, `"limits.max_open_sockets"`, `"bindings.HOT.consistency"`.
///
/// # What it looks at, and what it does not
///
/// The manifest's own top level - which is where a whole new block appears -
/// and the three blocks that used to refuse a bundle outright. It does not
/// descend into a route, a queue consumer, an asset entry or a `site` rule.
/// Those have always ignored a key they did not know, silently and without
/// costing anybody a deployment, so nothing there is owed the apology this
/// function exists to make. Extending the walk to one of them is a table
/// above, a branch below, and a line in
/// `every_table_matches_the_struct_it_describes` - nothing more.
///
/// Sorted and stable: `serde_json` keeps an object's keys in a `BTreeMap`, so
/// two builds reading one manifest report the same list in the same order.
///
/// It parses the text a second time, which is why [`crate::read_manifest`]
/// calls it once per archive rather than anything calling it per request. The
/// second parse is bounded by [`crate::MAX_MANIFEST_BYTES`], the same ceiling
/// the first one was read under.
///
/// An empty result is also what a manifest that does not parse as an object
/// gets. That is not this function's failure to report - [`crate::Manifest::parse`]
/// has already refused such a document with the parse error, which says far
/// more than "no unknown keys" would.
pub fn unknown_fields(json: &str) -> Vec<String> {
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect(&root, MANIFEST_FIELDS, "", &mut out);
    if let Some(Value::Object(limits)) = root.get("limits") {
        collect(limits, LIMITS_FIELDS, "limits.", &mut out);
    }
    if let Some(Value::Object(compatibility)) = root.get("compatibility") {
        collect(
            compatibility,
            COMPATIBILITY_FIELDS,
            "compatibility.",
            &mut out,
        );
    }
    if let Some(Value::Object(bindings)) = root.get("bindings") {
        for (name, binding) in bindings {
            if let Value::Object(binding) = binding {
                collect(
                    binding,
                    BINDING_FIELDS,
                    &format!("bindings.{name}."),
                    &mut out,
                );
            }
        }
    }
    out
}

fn collect(
    object: &serde_json::Map<String, Value>,
    known: &[&str],
    prefix: &str,
    out: &mut Vec<String>,
) {
    for key in object.keys() {
        if !known.contains(&key.as_str()) {
            out.push(format!("{prefix}{key}"));
        }
    }
}

/// Say out loud which declarations this build is not honouring.
///
/// Called from [`crate::read_manifest`], which is the one path an archive
/// comes in on: once at the build, once at the upload, and once on each node
/// that installs the version. Deliberately *not* called from
/// [`crate::Manifest::parse`] - the control plane's scheduler parses the
/// stored manifest of every deployed environment on every tick, and a line
/// per application per minute would bury the one that matters.
///
/// `warn` rather than `info`, because the manifest asked for something and
/// this build is not doing it. It is not an error: the bundle is loaded,
/// served and rolled back to normally, and in a fleet mid-upgrade this line
/// is the expected state of the older half.
pub fn report(json: &str, schema: u32) {
    let unknown = unknown_fields(json);
    if unknown.is_empty() {
        return;
    }
    let named: Vec<&str> = unknown.iter().take(MAX_NAMED).map(String::as_str).collect();
    warn!(
        schema,
        unknown_keys = unknown.len(),
        named = ?named,
        not_named = unknown.len().saturating_sub(named.len()),
        "this bundle's manifest carries keys this build does not read, so those declarations \
         are not in force here; everything else in the manifest is. Schema {schema} is the \
         version this build understands, and within one schema a manifest is additive, so a \
         newer builder wrote this - upgrade this component to honour those keys. A key named \
         here that nobody added on purpose is a misspelling in a hand-written manifest",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Asset, Binding, BindingKind, DeclaredBindings, DeclaredCompatibility, DeclaredLimits,
        Manifest, NotFound, QueueConsumer, Route, SiteConfig,
    };
    use std::collections::BTreeSet;

    /// The key names one value actually serialises as.
    fn keys_of<T: serde::Serialize>(value: &T) -> BTreeSet<String> {
        let serde_json::Value::Object(map) =
            serde_json::to_value(value).expect("the value serialises")
        else {
            panic!("this type does not serialise as a JSON object");
        };
        map.keys().cloned().collect()
    }

    fn table(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    /// Every optional field filled in, so that nothing is hidden by a
    /// `skip_serializing_if` and the comparison below sees the whole struct.
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
    fn every_table_matches_the_struct_it_describes() {
        // A name that drifted would make a key this build does read look
        // unrecognised, and the log line that exists to say "your builder is
        // newer than this node" would start accusing the operator's own
        // manifest of carrying keys nobody wrote. Nothing in the compiler ties
        // a `&'static str` to a struct field, so this is the tie.
        assert_eq!(
            keys_of(&a_manifest_holding_every_field()),
            table(MANIFEST_FIELDS),
            "MANIFEST_FIELDS is not what a Manifest serialises as"
        );
        assert_eq!(
            keys_of(&DeclaredLimits {
                memory_mb: Some(128),
                timeout_ms: Some(1500),
                rate_limit_ops_per_sec: Some(100.0),
                cpu_time_limit_ms: Some(250),
                resource_table_max: Some(64),
                max_subrequests: Some(4),
            }),
            table(LIMITS_FIELDS),
            "LIMITS_FIELDS is not what a DeclaredLimits serialises as"
        );
        assert_eq!(
            keys_of(&DeclaredCompatibility {
                date: Some("2026-09-01".parse().expect("a real day")),
                flags: vec!["multiple_set_cookie_headers".to_string()],
            }),
            table(COMPATIBILITY_FIELDS),
            "COMPATIBILITY_FIELDS is not what a DeclaredCompatibility serialises as"
        );
        assert_eq!(
            keys_of(&Binding {
                kind: BindingKind::Service,
                app: Some("auth-api".to_string()),
                environment: Some("production".to_string()),
            }),
            table(BINDING_FIELDS),
            "BINDING_FIELDS is not what a Binding serialises as"
        );
    }

    #[test]
    fn a_manifest_this_build_wrote_has_nothing_unrecognised_in_it() {
        // The common case, and the one that decides whether the log line
        // above is trustworthy: if a manifest this very build produced were
        // reported as carrying unknown keys, every operator would learn to
        // ignore the line.
        let json = serde_json::to_string(&a_manifest_holding_every_field()).expect("serialises");
        assert!(unknown_fields(&json).is_empty(), "{json}");
    }

    #[test]
    fn unknown_keys_are_reported_by_path_and_in_a_stable_order() {
        let json = r#"{
            "schema": 1, "app": "demo", "entry": "worker/index.js",
            "observability": {"sampling": 0.1},
            "limits": {"timeout_ms": 1500, "max_open_sockets": 8},
            "compatibility": {"date": "2026-09-01", "profile": "strict"},
            "bindings": {"HOT": {"kind": "kv", "consistency": "strong"}}
        }"#;
        assert_eq!(
            unknown_fields(json),
            vec![
                "observability".to_string(),
                "limits.max_open_sockets".to_string(),
                "compatibility.profile".to_string(),
                "bindings.HOT.consistency".to_string(),
            ]
        );
    }

    #[test]
    fn a_document_that_is_not_an_object_reports_nothing() {
        // The parse error says what is wrong with these; a list of unknown
        // keys would only be noise on top of it.
        for not_a_manifest in ["[]", "\"text\"", "7", "not json at all", ""] {
            assert!(
                unknown_fields(not_a_manifest).is_empty(),
                "{not_a_manifest}"
            );
        }
    }
}
