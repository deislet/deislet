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
//! So the manifest's *shape* has exactly one forward-compatibility gate, and
//! it is the `schema` field. Its *values* have a short list of deliberate
//! gates, every one of them named in [`GATES`] with the reason it has no
//! narrow reading. Nothing outside that list may refuse a bundle for being
//! newer than the build reading it, and the tests at the bottom of this file
//! are what hold the code to that sentence.
//!
//! # The rule for a value
//!
//! The rule above is about keys, and for a while it was the whole rule. It was
//! not enough: *values* in fields this build does know went on refusing a
//! bundle for being newer than the build reading it, which is the same
//! rollback failure one level down. Each of them is an **allow-list of the
//! spellings this release happens to have**, and every release adds to one -
//! a new binding kind, a new not-found behaviour, a new redirect status, a new
//! pattern syntax, a new way to name a caller, a new behaviour flag - so each
//! was a bundle an older node would throw away whole, unrepairably, for a
//! value that was correct when it was written.
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
//!   turns out to be behind it. See [`crate::DeclaredBindings`]. That is a
//!   node's reading; the control plane's entitlement gate does something
//!   else with the same value, and [`GATES`] says what and why.
//! * `callable_by[]` - the entry consents to nobody. It is left out of the
//!   parsed list, which is the answer the node's exact comparison would have
//!   given it anyway, so the callee is reachable by fewer applications here
//!   than where it was built and never by more. The build's own list never
//!   goes through that reader, so an author's typo is still refused by name
//!   with the project file on screen. See `bindings::readable_callable_by`
//!   and [`crate::validate_callable_by`].
//! * `site.not_found` - `"none"`, which is not merely the default but what
//!   every application had before the field existed: the request reaches the
//!   isolate and the application answers its own 404. See [`crate::NotFound`].
//! * `site.redirects[].status` - the rule is dropped. Assets are looked up
//!   before this table, so a dropped rule cannot uncover a file it was hiding;
//!   what answers instead is another of the author's own rules, the
//!   application, or the not-found behaviour. See
//!   [`crate::RedirectTable::compile`].
//! * `site.redirects[].from` - the rule is dropped, for the same reason and
//!   with the same consequence. The pattern grammar is [`crate::routing`]'s
//!   and it grows; a `/*.js` matched as literal text would be a rule firing on
//!   a path nobody asked for, which is a nearby value and not the absence of
//!   one. What a dropped rule can uncover is a function route or the
//!   not-found page a `404` rule stood in front of, never an asset - and
//!   `_redirects` is not an access control. See
//!   [`crate::redirects::source_is_readable`].
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
//! # The values that are still refused
//!
//! [`GATES`] is the list, and it is short on purpose. A row goes in only when
//! there is no reading of the value that leaves the application able to do
//! *less or the same*; each row says where it refuses, because *where* is
//! most of what makes a gate tolerable.
//!
//! **`schema`** refuses in [`crate::Manifest::validate`], whole. It is the
//! gate the rule above is built on, and the only one that is about the
//! document rather than a value in it.
//!
//! **`compatibility.date`** later than [`crate::PLATFORM_KNOWN_THROUGH`] has
//! no narrow reading to take: every narrowed value above has an "absent" that
//! means *less*; a date means "give me the answers of this day", and the older
//! answers are not less of that, they are a different one - which is the
//! silent behaviour change the date exists to prevent, delivered by the date
//! itself. So it refuses, and it is allowed to because of *where* it refuses:
//! not in [`crate::Manifest::parse`] or [`crate::Manifest::validate`], which
//! would make the manifest unreadable, but in
//! [`crate::Manifest::compatibility_profile`], which every component calls
//! for itself. The manifest still parses, everything else in it is still
//! readable, the refusal names both days, and the very same bundle returns
//! `Ok` on a node one release newer.
//!
//! **`site.headers[].pattern`** this build cannot compile refuses in
//! [`crate::HeaderTable::compile`], and because [`crate::SiteConfig::validate`]
//! runs it, that is the whole manifest - at upload and on the node. It is the
//! one `site` rule with no narrow reading. A `_headers` block is where
//! `Content-Security-Policy`, `Strict-Transport-Security` and
//! `X-Frame-Options` are set, so dropping a block this build cannot place lets
//! a visitor's browser do *more*; a block that only removes headers does not
//! escape that, because `! Access-Control-Allow-Origin` on one directory is
//! also a narrowing an author wrote down; and applying the block to every
//! path would be a nearby value with its own damage. This is the rollback
//! failure the rest of this module forbids, accepted for one value because
//! the alternative is a site whose security headers hold on half a fleet. It
//! is a cost, and the refusal says which release ends it. Round 7 claimed
//! there was exactly one gate; this row is why that sentence was replaced.
//!
//! **`bindings.<name>.kind`, at the control plane only.** A node reads it
//! narrowly, as above. The control plane's entitlement gate
//! (`deis_control::api::bindings::check_service_bindings`) is the only place
//! that ever asks whether the *uploader* may wire this application to another
//! one, and it refuses the upload when the manifest carries a kind it cannot
//! read - naming the binding and the kind, and saying the control plane must
//! be upgraded to judge it. It has to, because "not published on an older
//! node" is not "authorised": a newer node reads the same bundle out of the
//! archive, publishes the binding, and would honour a cross-application route
//! that no authority check ever ran on. A refusal at upload is not the
//! rollback failure: the author is present, the failure is loud, and nothing
//! is in the catalog yet. It does not reopen that failure later either - a
//! version already in the catalog was judged by the control plane that
//! accepted it, and neither `deployments::deploy` nor `deployments::rollback`
//! runs the gate again (they build a delivery from the stored version and
//! record a pointer; `check_service_bindings` has exactly one caller, the
//! upload handler), so rolling a control plane back does not make an accepted
//! version undeployable. Today no such kind exists; the gate is here so that
//! the day one does, the older control plane fails closed instead of open.
//!
//! # Applying this to a field that is not one of the six
//!
//! The test is which way the list moves between releases.
//!
//! An **allow-list** - the spellings a field accepts - grows, so a newer
//! builder can write a value an older reader has never seen. Every such list in
//! a manifest is one of these hazards, and needs either a narrow reading, a
//! row in [`VALUE_PATHS`] and a line above, or - when no reading leaves the
//! application able to do less or the same - a row in [`GATES`] with the
//! reason, and a refusal that names the release it stops on.
//!
//! A **deny-list** - the spellings a field refuses - also grows, but that
//! moves the other way: the newer build refuses *more*, so an older reader
//! accepts everything a newer one would and no bundle is ever refused for
//! being new. [`crate::headers::HeaderRule`] and its refused header names are
//! the example, and they need nothing. So are the shapes a redirect rule may
//! never take - a host a visitor could choose, a rewrite to another origin -
//! which [`crate::RedirectTable::compile`] still refuses whole, because a
//! builder never writes one and a bundle carrying one came from somewhere
//! else.
//!
//! **Identifier grammar** is neither. A binding name, an application id and an
//! environment name are held to `deis_wire::conventions`, which every
//! component shares and which the control plane admits tenants under. It is
//! not a manifest vocabulary that a builder can get ahead of: an id this
//! build cannot spell is an id no tenant on this build can have, and a change
//! to that grammar is a platform change, which is what a schema bump is for.
//! `callable_by` is the exception only because it has a vocabulary of its own
//! beside the ids - `*` today - and that vocabulary is what grows.
//!
//! **Structural refusals** are not gates in this sense and stay: a document
//! that does not parse, a required key that is missing, an archive or a member
//! past a ceiling in [`crate::archive`], a binding entry with no `kind` or an
//! `app` that is not a string. Those are shapes the format has never had, and
//! a builder that produced one would be producing a new format, not a newer
//! value in this one.
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
//! The four field tables below are the field names of four structs, and a
//! name that drifted would make a key this build *does* read look
//! unrecognised. Nothing in the compiler ties a `&'static str` to a struct
//! field, so `every_table_matches_the_struct_it_describes` in the tests below
//! serialises a fully populated instance of each and compares the key sets.
//! Adding a field without adding it here fails that test.
//!
//! [`VALUE_PATHS`] and [`GATES`] are held the same way, from two sides.
//! `every_value_path_is_walked` and `every_gate_refuses_where_it_says` feed a
//! manifest from a newer builder through the code and check that each row
//! does what it claims. `every_reader_of_an_allow_list_is_classified_here`
//! reads this crate's own sources and counts the call sites of every
//! allow-list predicate against a table that classifies each one as narrow,
//! gate or build-only: a new reader of one of those predicates - which is
//! what a new refusal looks like - fails the test until somebody has written
//! down which of the three it is.

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
    "environment",
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
    "max_outbound_connections",
];

/// Every key [`crate::DeclaredCompatibility`] reads.
pub const COMPATIBILITY_FIELDS: &[&str] = &["date", "flags"];

/// Every key one [`crate::Binding`] reads.
pub const BINDING_FIELDS: &[&str] = &["kind", "app", "environment"];

/// Every place a manifest holds a value from an allow-list that is read
/// narrowly, as a dotted path.
///
/// Not read by the code that walks a manifest - [`unreadable_values`] has one
/// branch per row, because the six live at different depths and three of them
/// are inside arrays, so a table of paths could not drive the walk without a
/// path language of its own. It is here to be *read*: the module
/// documentation above explains what a narrow reading is, and this is the list
/// of everywhere one had to be found. `every_value_path_is_walked` below is
/// what keeps a row from being added without the branch that goes with it.
pub const VALUE_PATHS: &[&str] = &[
    "bindings.*.kind",
    "callable_by[]",
    "site.not_found",
    "site.redirects[].from",
    "site.redirects[].status",
    "compatibility.flags[]",
];

/// One value a manifest may be refused for, for being newer than the build
/// reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gate {
    /// The dotted path, in the spelling [`VALUE_PATHS`] uses.
    pub path: &'static str,
    /// Where the refusal is made, and therefore what it costs.
    pub refused_at: &'static str,
    /// Why no narrow reading exists.
    pub reason: &'static str,
}

/// Every deliberate forward-compatibility gate a manifest has.
///
/// The complete list. A refusal of a bundle for carrying a value from a newer
/// release that is not a row here is a defect, and the module documentation
/// above argues each row on its own. `every_gate_refuses_where_it_says` below
/// checks that each still refuses, and where.
pub const GATES: &[Gate] = &[
    Gate {
        path: "schema",
        refused_at: "Manifest::validate, the whole manifest, on every component",
        reason: "the shape of the document; a change an older build could not ignore safely",
    },
    Gate {
        path: "compatibility.date",
        refused_at: "Manifest::compatibility_profile, per component; the manifest still parses",
        reason: "a day's answers have no narrower day, only a different one",
    },
    Gate {
        path: "site.headers[].pattern",
        refused_at: "HeaderTable::compile, the whole manifest, at upload and on the node",
        reason: "a header block may carry the headers a browser relies on; dropping it widens",
    },
    Gate {
        path: "bindings.*.kind",
        refused_at: "deis_control::api::bindings::check_service_bindings, at upload only; a node \
                     reads it narrowly",
        reason: "the control plane must not deliver a cross-application binding it could not \
                 judge; a newer node would honour it with no authority check ever run",
    },
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
/// `"bindings.HOT.kind = \"vector\""`, `"callable_by[1] = \"shop/prod\""`.
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
/// Stable: `serde_json` keeps an object's keys in a `BTreeMap` and an array's
/// entries in order, so two builds reading one manifest report the same list
/// in the same order - bindings, `callable_by`, `site`, `compatibility`, and
/// within `site` each redirect rule's source before its status.
///
/// The rows of [`GATES`] are deliberately **not** here. Those are refused
/// rather than narrowed, and each refusal already names what it could not
/// read, at the one place that could not read it.
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

    if let Some(Value::Array(callable_by)) = root.get("callable_by") {
        for (index, entry) in callable_by.iter().enumerate() {
            let Some(entry) = entry.as_str() else {
                continue;
            };
            if !crate::bindings::callable_by_entry_is_readable(entry) {
                out.push(format!("callable_by[{index}] = {entry:?}"));
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
                // Only a source that is otherwise a source: one that is not
                // an absolute path is malformed, and `Manifest::parse` has
                // refused the document already, with a better message.
                if let Some(from) = rule.get("from").and_then(Value::as_str)
                    && from.starts_with('/')
                    && !crate::redirects::source_is_readable(from)
                {
                    out.push(format!("site.redirects[{index}].from = {from:?}"));
                }
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
         all, a callable_by entry consents to nobody, an unknown not-found behaviour is \
         \"none\", a redirect rule with an unknown status or an uncompilable source is dropped, \
         an unknown compatibility flag is ignored. The application runs; these declarations do \
         less here than where they were built. Within schema {schema} a value is only added \
         when that narrow reading is safe, so this means a newer builder wrote the bundle - \
         upgrade this component to honour them",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Asset, Binding, BindingKind, BundleError, DeclaredBindings, DeclaredCompatibility,
        DeclaredLimits, Manifest, NotFound, QueueConsumer, Route, SiteConfig,
    };
    use std::collections::{BTreeMap, BTreeSet};

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
            schema: crate::SUPPORTED_SCHEMA,
            app: "demo".to_string(),
            entry: "worker/index.js".to_string(),
            runtime: Some("standard-v1".to_string()),
            services: [("kv".to_string(), true)].into_iter().collect(),
            bindings,
            callable_by: vec!["storefront".to_string()],
            environment: deis_wire::EnvironmentDeclarations::new(
                "edge-canon.next@a41a4caadeaf8e5751ebe57ef5be673d18384654",
                Vec::new(),
            ),
            routes: vec![Route {
                pattern: "/api/users/:id".to_string(),
                methods: vec!["GET".to_string()],
            }],
            cron: vec!["0 */6 * * *".to_string()],
            queues: vec![QueueConsumer {
                name: "jobs".to_string(),
                batch_size: Some(10),
                max_concurrency: Some(2),
                max_retries: Some(3),
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
                max_outbound_connections: Some(16),
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
            "schema": 2, "app": "demo", "entry": "worker/index.js",
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
    /// this build has never seen at every path in [`VALUE_PATHS`], and beside
    /// each one a value it does know, so the test can see that the neighbour
    /// survives.
    ///
    /// Deliberately plausible rather than nonsense, like the unknown keys
    /// above. A value that breaks a rollback is never a hostile one; it is the
    /// next small feature - a vector namespace, a per-environment consent, a
    /// maintenance page, a `410 Gone` rule, a wildcard inside a segment, a
    /// behaviour flag.
    const FROM_A_NEWER_BUILDER: &str = r#"{
        "schema": 2, "app": "demo", "entry": "worker/index.js",
        "bindings": {"SEARCH": {"kind": "vector"}},
        "callable_by": ["storefront", "checkout/production"],
        "site": {
            "not_found": "maintenance-page",
            "redirects": [
                {"from": "/gone", "to": "/gone.html", "status": 410},
                {"from": "/*.js", "to": "/assets/:splat", "status": 301},
                {"from": "/old", "to": "/new", "status": 301}
            ]
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
    fn every_value_path_is_read_narrowly_and_nothing_beside_it_is_lost() {
        // The other half of the row: not only is the value named, the
        // manifest carrying it still parses, and every neighbour this build
        // does know is intact. This is the sentence in the module
        // documentation - "it never refuses the document" - run against the
        // code for all six paths at once, so a refusal creeping back into any
        // one of them fails here by name.
        let manifest = Manifest::parse(&crate::complete_manifest_json(FROM_A_NEWER_BUILDER))
            .unwrap_or_else(|e| {
                panic!("a manifest from a newer builder must still parse; it was refused: {e}")
            });

        assert!(manifest.bindings.get("SEARCH").is_none());
        assert_eq!(
            manifest.bindings.unreadable_kinds(),
            vec![("SEARCH", "vector")]
        );

        assert_eq!(
            manifest.callable_by,
            vec!["storefront".to_string()],
            "the entry this build cannot read consents to nobody; its neighbour still consents"
        );
        assert!(crate::callable_by_allows(
            &manifest.callable_by,
            "storefront"
        ));
        assert!(!crate::callable_by_allows(
            &manifest.callable_by,
            "checkout"
        ));

        assert_eq!(manifest.site.not_found, NotFound::None);

        let table = crate::RedirectTable::compile(&manifest.site.redirects)
            .expect("a status or a pattern from a newer release must not make the table unusable");
        assert_eq!(
            table.len(),
            1,
            "the 410 rule and the /*.js rule are dropped; /old survives"
        );
        assert!(table.apply(&["old".to_string()], "").is_some());
        assert!(table.apply(&["gone".to_string()], "").is_none());
        assert!(
            table.apply(&["app.js".to_string()], "").is_none(),
            "a pattern this build cannot compile matches nothing, not its literal text"
        );
        assert!(
            table.apply(&["*.js".to_string()], "").is_none(),
            "and not the literal asterisk either"
        );

        let profile = manifest
            .compatibility_profile()
            .expect("an unknown flag name must not make the profile unresolvable");
        assert_eq!(
            profile.date().map(|d| d.to_string()).as_deref(),
            Some("2026-09-01")
        );
    }

    #[test]
    fn an_unknown_value_is_reported_by_path_with_the_value_it_could_not_read() {
        assert_eq!(
            unreadable_values(FROM_A_NEWER_BUILDER),
            vec![
                "bindings.SEARCH.kind = \"vector\"".to_string(),
                "callable_by[1] = \"checkout/production\"".to_string(),
                "site.not_found = \"maintenance-page\"".to_string(),
                "site.redirects[0].status = 410".to_string(),
                "site.redirects[1].from = \"/*.js\"".to_string(),
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
                    from: "/a/:slug/*".to_string(),
                    to: "/b/:slug/:splat".to_string(),
                    status: *status,
                })
                .collect(),
            ..SiteConfig::default()
        })
        .expect("serialises");
        assert!(
            unreadable_values(&format!(
                r#"{{"schema":2,"app":"d","entry":"e.js","callable_by":["*"," shop "],"site":{with_rules}}}"#
            ))
            .is_empty(),
            "{with_rules}"
        );
    }

    /// Every row of [`GATES`] still refuses, and refuses where it says.
    ///
    /// The list claims to be complete and each row claims a place; this is
    /// what makes both claims checkable. A gate that moved - a header pattern
    /// that started being dropped, a date that started refusing in `parse` -
    /// changes what a rollback costs, and should change this test on purpose.
    #[test]
    fn every_gate_refuses_where_it_says() {
        let paths: BTreeSet<&str> = GATES.iter().map(|gate| gate.path).collect();
        assert_eq!(
            paths,
            [
                "schema",
                "compatibility.date",
                "site.headers[].pattern",
                "bindings.*.kind"
            ]
            .into_iter()
            .collect::<BTreeSet<_>>(),
            "a gate was added or removed; this test has to say what it does"
        );

        // `schema`: the whole manifest, in `validate`.
        let future = crate::complete_manifest_json(FROM_A_NEWER_BUILDER)
            .replace("\"schema\":2", "\"schema\":3");
        assert!(matches!(
            Manifest::parse(&future),
            Err(BundleError::UnsupportedSchema {
                found: 3,
                supported: crate::SUPPORTED_SCHEMA
            })
        ));

        // `compatibility.date`: the manifest parses, the profile refuses, the
        // refusal names both days.
        let later = crate::complete_manifest_json(
            &FROM_A_NEWER_BUILDER.replace("2026-09-01", "2099-01-01"),
        );
        let manifest = Manifest::parse(&later).expect("only the compatibility answer is refused");
        let err = manifest
            .compatibility_profile()
            .expect_err("a day this build never saw");
        assert!(err.contains("2099-01-01"), "{err}");
        assert!(
            err.contains(&crate::PLATFORM_KNOWN_THROUGH.to_string()),
            "{err}"
        );

        // `site.headers[].pattern`: the whole manifest, in `validate`, with a
        // refusal that says it is deliberate and which release ends it.
        let headers = r#"{
            "schema": 2, "app": "demo", "entry": "worker/index.js",
            "site": {"headers": [
                {"pattern": "/*", "set": [{"name": "x-frame-options", "value": "DENY"}]},
                {"pattern": "/*.js", "set": [{"name": "cache-control", "value": "immutable"}]}
            ]}
        }"#;
        let err = Manifest::parse(&crate::complete_manifest_json(headers))
            .expect_err("a header pattern this build cannot compile");
        let message = err.to_string();
        assert!(matches!(err, BundleError::InvalidManifest(_)), "{message}");
        assert!(message.contains("/*.js"), "{message}");
        assert!(
            message.contains("refused rather than dropped"),
            "the refusal has to say why this value is the exception: {message}"
        );
        assert!(message.contains("upgrade this component"), "{message}");
        // And it is a gate, not a narrowed value, so `report` does not list
        // it among the things this build quietly did less of.
        assert!(
            unreadable_values(headers).is_empty(),
            "{:?}",
            unreadable_values(headers)
        );

        // `bindings.*.kind` at the control plane: that gate lives in
        // deis-control and is tested there
        // (`api::bindings::tests::a_binding_kind_this_control_plane_cannot_read_is_refused_whatever_the_credential_holds`).
        // What this crate can check is the half it owns: the node's reading
        // is narrow, and the gate has the entry to look at.
        let manifest = Manifest::parse(&crate::complete_manifest_json(FROM_A_NEWER_BUILDER))
            .expect("narrow on a node");
        let unreadable: Vec<(&str, &Value)> = manifest.bindings.unreadable_entries().collect();
        assert_eq!(unreadable.len(), 1);
        assert_eq!(unreadable[0].0, "SEARCH");
        assert_eq!(
            unreadable[0].1.get("kind").and_then(Value::as_str),
            Some("vector")
        );
    }

    /// Every call site of an allow-list predicate in the source of every
    /// crate that reads a manifest, classified.
    ///
    /// A new refusal for a value from a newer release can only be written by
    /// asking one of these predicates, so counting their call sites is how
    /// this module finds out about one. The counts are the classification: a
    /// site is *narrow* (it drops or ignores), a *gate* (a row in [`GATES`]),
    /// or *build-only* (it reads the author's own text and refuses by line).
    /// A count that moved means a site was added or removed, and whoever did
    /// it has to say which of the three it is - here, and in the module
    /// documentation.
    ///
    /// The field of view is the promise: "a reader of the manifest outside
    /// deis-bundle that still refuses whole" is what this exists to catch,
    /// so it walks every `.rs` file, recursively, under the `src` tree of
    /// each crate in [`MANIFEST_READERS`] - this one, the builder that writes
    /// the archive, the node that serves it and the control plane that
    /// accepts it. It used to read this crate's top-level `src` and nothing
    /// else, which guarded the module it lived in and not the promise; a
    /// predicate called from a subdirectory here, or from any of the other
    /// three crates, went uncounted. A crate directory that is missing fails
    /// the test rather than narrowing it silently, for the same reason.
    ///
    /// Counted over the non-test part of each file, so a test that exercises
    /// a predicate does not count as a reader of it.
    #[test]
    fn every_reader_of_an_allow_list_is_classified_here() {
        /// The crates that read a manifest, relative to `crates/`. A reader
        /// added anywhere else is a reader this test cannot see, so a new
        /// crate that takes a `Manifest` belongs on this list first.
        const MANIFEST_READERS: &[&str] =
            &["deis-bundle", "deis-build", "deis-runtime", "deis-control"];

        // (predicate, [(file, sites)]), files as `crate/path` under
        // `crates/`. Each site, and what it is:
        const READERS: &[(&str, &[(&str, usize)])] = &[
            (
                "CompiledPattern::compile(",
                &[
                    // `match_pattern`: `.ok()?` - a pattern that does not
                    // compile matches nothing. Narrow.
                    ("deis-bundle/src/routing.rs", 1),
                    // `source_is_readable` (the predicate `unreadable_values`
                    // asks) and `compile_rule`, which the node drops on and
                    // the build refuses on by line. Narrow at load, build-only
                    // at build.
                    ("deis-bundle/src/redirects.rs", 2),
                    // `HeaderTable::compile` - the gate `site.headers[].pattern`
                    // - and `parse_headers`, build-only.
                    ("deis-bundle/src/headers.rs", 2),
                    // `SiteRules::compile` on the node: a function route
                    // whose pattern does not compile is logged at warn and
                    // left out, so the static fallback answers what the
                    // route would have claimed. The version still loads.
                    // Narrow.
                    ("deis-runtime/src/assets/site.rs", 1),
                ],
            ),
            (
                "status_is_known(",
                // The definition, and `compile_rule`: narrow at load,
                // build-only at build. `unreadable_values` names the predicate
                // without calling it with parentheses.
                &[("deis-bundle/src/redirects.rs", 2)],
            ),
            (
                "source_is_readable(",
                // The definition, and the branch in `unreadable_values` that
                // names a dropped rule. Narrow.
                &[
                    ("deis-bundle/src/redirects.rs", 1),
                    ("deis-bundle/src/forward.rs", 1),
                ],
            ),
            (
                "BindingKind::from_name(",
                // `DeclaredBindings::deserialize`, which carries an unknown
                // kind through unread, and the branch in `unreadable_values`.
                // Narrow on a node; the control plane's gate reads the
                // carried-through entry, see `GATES`.
                &[
                    ("deis-bundle/src/bindings.rs", 1),
                    ("deis-bundle/src/forward.rs", 1),
                ],
            ),
            (
                "MANIFEST_SPELLINGS",
                // The definition, and the branch in `unreadable_values`.
                // `NotFound::deserialize` reads narrowly through
                // `from_manifest`, whose match arm is the allow-list itself.
                &[
                    ("deis-bundle/src/site.rs", 1),
                    ("deis-bundle/src/forward.rs", 1),
                ],
            ),
            (
                "callable_by_entry_is_readable(",
                // The definition; `unreadable_callable_by`, which
                // `validate_callable_by` refuses on (build-only, because a
                // parsed list has already been narrowed); `readable_callable_by`,
                // the narrow reader; and the branch in `unreadable_values`.
                &[
                    ("deis-bundle/src/bindings.rs", 3),
                    ("deis-bundle/src/forward.rs", 1),
                ],
            ),
        ];

        // Every `.rs` file under `dir`, recursively, with its path relative
        // to `crates/`. Symlinks are not followed; none exist under `src`,
        // and one that pointed out of the tree would count somebody else's
        // code as ours.
        fn collect(
            crates: &std::path::Path,
            dir: &std::path::Path,
            into: &mut BTreeMap<String, String>,
        ) {
            let entries =
                std::fs::read_dir(dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
            for entry in entries {
                let entry = entry.expect("a directory entry");
                let path = entry.path();
                let kind = entry.file_type().expect("a file type");
                if kind.is_dir() {
                    collect(crates, &path, into);
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !kind.is_file() || !name.ends_with(".rs") || name.ends_with("_test.rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("a readable source file");
                // Everything after the first `#[cfg(test)]` is tests.
                let code = match text.find("#[cfg(test)]") {
                    Some(at) => text[..at].to_string(),
                    None => text,
                };
                let relative = path
                    .strip_prefix(crates)
                    .expect("a file under crates/")
                    .to_string_lossy()
                    .replace('\\', "/");
                into.insert(relative, code);
            }
        }

        let crates = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .canonicalize()
            .expect("the crates directory");
        let mut sources: BTreeMap<String, String> = BTreeMap::new();
        for reader in MANIFEST_READERS {
            let src = crates.join(reader).join("src");
            assert!(
                src.is_dir(),
                "{} has no src directory at {}; this test walks the source of every crate \
                 that reads a manifest, and a crate it cannot find is a reader it cannot \
                 see, so update MANIFEST_READERS rather than letting it look away",
                reader,
                src.display()
            );
            collect(&crates, &src, &mut sources);
        }
        assert!(
            sources.contains_key("deis-bundle/src/forward.rs")
                && sources.contains_key("deis-bundle/src/redirects.rs")
                && sources.contains_key("deis-runtime/src/assets/site.rs"),
            "read the wrong directory: {:?}",
            sources.keys().collect::<Vec<_>>()
        );

        let mut unclassified = Vec::new();
        for (predicate, expected) in READERS {
            let mut found: BTreeMap<&str, usize> = BTreeMap::new();
            for (file, code) in &sources {
                let count = code.matches(predicate).count();
                if count > 0 {
                    found.insert(file.as_str(), count);
                }
            }
            let expected: BTreeMap<&str, usize> = expected.iter().copied().collect();
            if found != expected {
                unclassified.push(format!(
                    "{predicate:?}: expected {expected:?}, found {found:?}"
                ));
            }
        }
        assert!(
            unclassified.is_empty(),
            "a reader of an allow-list was added or removed without being classified. Every \
             such site is either a narrow reading (a row in VALUE_PATHS), a deliberate gate (a \
             row in GATES, with the reason), or build-only (it reads the author's own file). \
             Decide which, write it into the module documentation, and update the table in this \
             test:\n{}",
            unclassified.join("\n")
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
