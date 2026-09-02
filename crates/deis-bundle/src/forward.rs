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
//! # The rule for a value
//!
//! The rule above is about keys, and for a while it was the whole rule. It was
//! not enough: four *values* in fields this build does know went on refusing a
//! bundle for being newer than the build reading it, which is the same
//! rollback failure one level down. Each of those four is an **allow-list of
//! the spellings this release happens to have**, and every release adds to
//! one - a new binding kind, a new not-found behaviour, a new redirect status,
//! a new behaviour flag - so each was a bundle an older node would throw away
//! whole, unrepairably, for a value that was correct when it was written.
//!
//! **Within one [`crate::SUPPORTED_SCHEMA`], a value in a known field must be
//! safe to read narrowly.** A build that meets a value it does not know takes
//! the narrowest reading the field can already hold, and says at `warn` which
//! value it did that to. It never guesses at what the value meant, and it
//! never refuses the document. "Narrowest" is decided by one question - what
//! can the application do afterwards? - and the only acceptable answers are
//! *less* and *the same*.
//!
//! That direction is the whole reason this is safe, and it is why the harder
//! half of the problem does not arise. A value an old node **misreads** is
//! worse than one it refuses: a redirect answered with a status this build
//! substituted, or a binding wired to the nearest kind it did know, is wrong
//! in a way that reaches a visitor's browser cache or another tenant's
//! namespace and stays wrong long after the rollback that caused it. So a
//! narrow reading is never a nearby value; it is the absence of the thing.
//!
//! * `bindings.<name>.kind` - the entry is not published. `env.<name>` is
//!   `undefined`, so the one handler that reaches for it fails on that one
//!   call, instead of the whole application failing to load. The *name* is
//!   still held to every rule a name is held to, because a name that collides
//!   with a variable, a secret or another binding is ambiguous whatever kind
//!   turns out to be behind it. See [`crate::DeclaredBindings`].
//! * `site.not_found` - `"none"`, which is not merely the default but what
//!   every application had before the field existed: the request reaches the
//!   isolate and the application answers its own 404. See [`crate::NotFound`].
//! * `site.redirects[].status` - the rule is dropped. Assets are looked up
//!   before this table, so a dropped rule cannot uncover a file it was hiding;
//!   what answers instead is another of the author's own rules, the
//!   application, or the not-found behaviour. See
//!   [`crate::RedirectTable::compile`].
//! * `compatibility.flags` - the name is ignored. A flag this build does not
//!   know names a behaviour this build does not have, so doing nothing is
//!   already what both the `enable` and the `disable` spelling of it would
//!   have produced. See [`crate::CompatibilityProfile::resolve`].
//!
//! The flag is the one worth arguing with, because half a fleet honouring it
//! and half not looks like exactly the split this mechanism exists to prevent.
//! It is not. A node that does not have the behaviour cannot *change* its
//! answer on it - it has only ever had one, and it is the one the application
//! gets there today and got there yesterday. And the manifest gives no way to
//! tell an `enable` name from a `disable` name for a flag nobody knows, so
//! refusing punished the safest declaration there is: an application holding a
//! behaviour *back* was refused by the very builds that were already giving it
//! what it asked for. Against that, refusing costs the whole deployment on the
//! older half of a fleet, unrecoverably if that version is the one being
//! rolled back to.
//!
//! # The one value that is still refused
//!
//! A `compatibility.date` later than
//! [`crate::PLATFORM_KNOWN_THROUGH`], and only that.
//!
//! It is the one value in the manifest with no narrow reading to take. Every
//! other value above has an "absent" that means *less*; a date means "give me
//! the answers of this day", and the older answers are not less of that, they
//! are a different one - which is the silent behaviour change the date exists
//! to prevent, delivered by the date itself. So it refuses, and it is allowed
//! to because of *where* it refuses: not in [`crate::Manifest::parse`] or
//! [`crate::Manifest::validate`], which would make the manifest unreadable,
//! but in [`crate::Manifest::compatibility_profile`], which every component
//! calls for itself. The manifest still parses, everything else in it is still
//! readable, the refusal names both days, and the very same bundle returns
//! `Ok` on a node one release newer - which is the mixed fleet working as
//! intended rather than a bundle being refused.
//!
//! # Applying this to a field that is not one of the four
//!
//! The test is which way the list moves between releases.
//!
//! An **allow-list** - the spellings a field accepts - grows, so a newer
//! builder can write a value an older reader has never seen. Every such list in
//! a manifest is one of these hazards, and needs a narrow reading, a row in
//! [`VALUE_PATHS`] and a line above.
//!
//! A **deny-list** - the spellings a field refuses - also grows, but that
//! moves the other way: the newer build refuses *more*, so an older reader
//! accepts everything a newer one would and no bundle is ever refused for
//! being new. [`crate::headers::HeaderRule`] and its refused header names are
//! the example, and they need nothing.
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

/// Every place a manifest holds a value from an allow-list, as a dotted path.
///
/// Not read by the code that walks a manifest - [`unreadable_values`] has one
/// branch per row, because the four live at four different depths and two of
/// them are inside arrays, so a table of paths could not drive the walk
/// without a path language of its own. It is here to be *read*: the module
/// documentation above explains what a narrow reading is, and this is the list
/// of everywhere one had to be found. `every_value_path_is_walked` below is
/// what keeps a row from being added without the branch that goes with it.
pub const VALUE_PATHS: &[&str] = &[
    "bindings.*.kind",
    "site.not_found",
    "site.redirects[].status",
    "compatibility.flags[]",
];

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

/// The values in `json` that sit in a field this build reads and are spellings
/// it does not know - `"site.not_found = \"custom-page\""`,
/// `"bindings.HOT.kind = \"vector\""`.
///
/// The other half of [`unknown_fields`]: that one names the declarations this
/// build dropped because it did not recognise the *key*, and this one names
/// the declarations it narrowed because it did not recognise the *value*. Both
/// are the same apology, and [`report`] makes it once.
///
/// Each entry names the path and quotes the value as it was written, because
/// the value is the actionable half: an operator reading `site.not_found =
/// "custom-page"` on a node knows both which application declared it and which
/// release they would have to be on to honour it.
///
/// Sorted by where it sits in the document, and stable: `serde_json` keeps an
/// object's keys in a `BTreeMap` and an array's entries in order, so two
/// builds reading one manifest report the same list in the same order.
///
/// A `compatibility.date` past [`crate::PLATFORM_KNOWN_THROUGH`] is
/// deliberately **not** here. It is refused rather than narrowed - see the
/// module documentation - and the refusal already names both days, at the one
/// component that cannot honour it.
pub fn unreadable_values(json: &str) -> Vec<String> {
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let mut out = Vec::new();

    if let Some(Value::Object(bindings)) = root.get("bindings") {
        for (name, binding) in bindings {
            if let Some(kind) = binding.get("kind").and_then(Value::as_str)
                && crate::BindingKind::from_name(kind).is_none()
            {
                out.push(format!("bindings.{name}.kind = {kind:?}"));
            }
        }
    }

    if let Some(Value::Object(site)) = root.get("site") {
        if let Some(choice) = site.get("not_found").and_then(Value::as_str)
            && !crate::NotFound::MANIFEST_SPELLINGS.contains(&choice)
        {
            out.push(format!("site.not_found = {choice:?}"));
        }
        if let Some(Value::Array(rules)) = site.get("redirects") {
            for (index, rule) in rules.iter().enumerate() {
                let Some(status) = rule.get("status").and_then(Value::as_u64) else {
                    continue;
                };
                if u16::try_from(status)
                    .map(crate::redirects::status_is_known)
                    .unwrap_or(false)
                {
                    continue;
                }
                out.push(format!("site.redirects[{index}].status = {status}"));
            }
        }
    }

    if let Some(Value::Object(compatibility)) = root.get("compatibility")
        && let Some(Value::Array(flags)) = compatibility.get("flags")
    {
        for (index, flag) in flags.iter().enumerate() {
            let Some(name) = flag.as_str() else { continue };
            if crate::FLAGS
                .iter()
                .any(|spec| spec.enable == name.trim() || spec.disable == name.trim())
            {
                continue;
            }
            out.push(format!("compatibility.flags[{index}] = {name:?}"));
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
    if !unknown.is_empty() {
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

    // A second line rather than a second clause on the first: these are read
    // by somebody acting on them, and the two lists call for different acts.
    // An unknown *key* is a declaration this build never saw; an unknown
    // *value* is a declaration it saw, narrowed, and is now serving something
    // different from - which is the one worth waking up for.
    let unreadable = unreadable_values(json);
    if unreadable.is_empty() {
        return;
    }
    let named: Vec<&str> = unreadable
        .iter()
        .take(MAX_NAMED)
        .map(String::as_str)
        .collect();
    warn!(
        schema,
        unreadable_values = unreadable.len(),
        named = ?named,
        not_named = unreadable.len().saturating_sub(named.len()),
        "this bundle's manifest holds values this build does not know, in fields it does read, \
         so each was taken in its narrowest reading instead: a binding kind is not published at \
         all, an unknown not-found behaviour is \"none\", a redirect rule with an unknown status \
         is dropped, an unknown compatibility flag is ignored. The application runs; these four \
         declarations do less here than where they were built. Within schema {schema} a value is \
         only added when that narrow reading is safe, so this means a newer builder wrote the \
         bundle - upgrade this component to honour them",
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

    /// A manifest as a builder some releases newer would write it: one value
    /// this build has never seen at every path in [`VALUE_PATHS`].
    ///
    /// Deliberately plausible rather than nonsense, like the unknown keys
    /// above. A value that breaks a rollback is never a hostile one; it is the
    /// next small feature - a vector namespace, a maintenance page, a `410
    /// Gone` rule, a behaviour flag.
    const FROM_A_NEWER_BUILDER: &str = r#"{
        "schema": 1, "app": "demo", "entry": "worker/index.js",
        "bindings": {"SEARCH": {"kind": "vector"}},
        "site": {
            "not_found": "maintenance-page",
            "redirects": [{"from": "/gone", "to": "/gone.html", "status": 410}]
        },
        "compatibility": {"date": "2026-09-01", "flags": ["streaming_uploads"]}
    }"#;

    /// The shape of a produced path: the binding name a manifest chose becomes
    /// `*` and an array index becomes `[]`, so that what comes back can be
    /// compared with the table, which cannot know either.
    fn shape_of(reported: &str) -> String {
        let path = reported.split(" = ").next().unwrap_or(reported);
        let mut out = String::with_capacity(path.len());
        for (index, segment) in path.split('.').enumerate() {
            if index > 0 {
                out.push('.');
            }
            // `bindings.<whatever the author called it>.kind`
            if index == 1 && path.starts_with("bindings.") {
                out.push('*');
                continue;
            }
            match segment.split_once('[') {
                Some((name, _)) => {
                    out.push_str(name);
                    out.push_str("[]");
                }
                None => out.push_str(segment),
            }
        }
        out
    }

    #[test]
    fn every_value_path_is_walked() {
        // The table is documentation and the walk is code, and nothing in the
        // compiler ties them together. A row added without its branch would
        // leave a field still refusing a bundle for being newer than this
        // build - which is the whole defect this rule was written for - while
        // the list above claimed it was covered.
        let reported = unreadable_values(FROM_A_NEWER_BUILDER);
        let shapes: BTreeSet<String> = reported.iter().map(|value| shape_of(value)).collect();
        assert_eq!(
            shapes,
            VALUE_PATHS.iter().map(|p| p.to_string()).collect(),
            "reported: {reported:?}"
        );
    }

    #[test]
    fn an_unknown_value_is_reported_by_path_with_the_value_it_could_not_read() {
        assert_eq!(
            unreadable_values(FROM_A_NEWER_BUILDER),
            vec![
                "bindings.SEARCH.kind = \"vector\"".to_string(),
                "site.not_found = \"maintenance-page\"".to_string(),
                "site.redirects[0].status = 410".to_string(),
                "compatibility.flags[0] = \"streaming_uploads\"".to_string(),
            ]
        );
    }

    #[test]
    fn a_manifest_this_build_wrote_has_no_unreadable_values_in_it() {
        // The same claim as for keys, and for the same reason: a line that
        // fires on an ordinary bundle is a line every operator learns to skip.
        let json = serde_json::to_string(&a_manifest_holding_every_field()).expect("serialises");
        assert!(unreadable_values(&json).is_empty(), "{json}");

        let with_rules = serde_json::to_string(&SiteConfig {
            not_found: NotFound::SinglePageApp,
            redirects: crate::redirects::ALLOWED_STATUSES
                .iter()
                .map(|status| crate::RedirectRule {
                    from: "/a".to_string(),
                    to: "/b".to_string(),
                    status: *status,
                })
                .collect(),
            ..SiteConfig::default()
        })
        .expect("serialises");
        assert!(
            unreadable_values(&format!(
                r#"{{"schema":1,"app":"d","entry":"e.js","site":{with_rules}}}"#
            ))
            .is_empty(),
            "{with_rules}"
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
            assert!(
                unreadable_values(not_a_manifest).is_empty(),
                "{not_a_manifest}"
            );
        }
    }
}
