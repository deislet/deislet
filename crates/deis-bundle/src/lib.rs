//! The Deislet application bundle format.
//!
//! A bundle is the zip that `deis build --target deislet` produces, and it is
//! the only carrier of a version (see `docs/implementation-plan.zh.md`
//! section 2):
//!
//! ```text
//! manifest.json
//! worker/index.js      the bundled function code, one ESM file
//! assets/              static files, directory structure preserved
//! ```
//!
//! Three components handle this format: deis-build writes it, deis-control
//! verifies and stores it, deis-runtime loads it. They must agree exactly on
//! what a manifest means, so the definition lives here rather than being
//! spelled out once per component - two copies of a format drift, and the
//! symptom is a bundle that one side accepts and another rejects.

use serde::{Deserialize, Serialize};

pub mod archive;
pub mod assets;
pub mod bindings;
pub mod compatibility;
pub mod forward;
pub mod headers;
pub mod limits;
pub mod path;
pub mod redirects;
pub mod routing;
pub mod site;

pub use archive::{
    BundleArchive, Limit, MAX_ENTRY_BYTES, MAX_MANIFEST_BYTES, MAX_MEMBER_NAME_BYTES, MAX_MEMBERS,
    MAX_TOTAL_BYTES,
};
pub use assets::{AssetIndex, AssetMatch, guess_content_type};
pub use bindings::{
    Binding, BindingKind, CALLABLE_BY_EVERYONE, DeclaredBindings, MAX_BINDING_NAME_LEN,
    ServiceEnvironment, callable_by_allows, callable_by_entry_is_readable, unreadable_callable_by,
    validate_callable_by,
};
pub use compatibility::{
    CompatibilityDate, CompatibilityProfile, DeclaredCompatibility, FLAGS, Flag, FlagSpec,
    PLATFORM_KNOWN_THROUGH, flag_spec, known_flag_names,
};
pub use forward::{
    BINDING_FIELDS, COMPATIBILITY_FIELDS, GATES, LIMITS_FIELDS, MANIFEST_FIELDS, VALUE_PATHS,
    unknown_fields, unreadable_values,
};
pub use headers::{HeaderField, HeaderNamesWritten, HeaderRule, HeaderTable, parse_headers};
pub use limits::DeclaredLimits;
pub use path::{percent_decode, percent_encode_uri};
pub use redirects::{
    ParsedRedirects, RedirectAction, RedirectRule, RedirectTable, parse_redirects,
};
pub use routing::{
    CATCH_ALL_PARAM, CATCH_ALL_SEGMENT, Captures, CompiledPattern, PLACEHOLDER_PREFIX, Specificity,
    decoded_segments, match_pattern, segments, sort_by_specificity,
};
pub use site::{
    HEADERS_FILE, NOT_FOUND_PAGE, NotFound, REDIRECTS_FILE, RuleError, SPA_SHELL, SiteConfig,
};

/// Manifest schema this version of the platform understands.
///
/// **The manifest's forward-compatibility gate for its shape.** Within one
/// schema the format is additive and a build ignores keys it does not know; a
/// change an older build could not safely ignore takes a new number here, and
/// an older build then refuses the whole manifest by name rather than acting
/// on half of it. It is the first of the few gates [`forward::GATES`] names,
/// and the only one that is about the document rather than one value in it.
/// See [`forward`] for the rule and what it costs an operator rolling back.
pub const SUPPORTED_SCHEMA: u32 = 2;

/// Name of the manifest at the root of the archive.
pub const MANIFEST_ENTRY: &str = "manifest.json";

/// Directory inside the archive holding static assets.
pub const ASSETS_PREFIX: &str = "assets/";

/// Where the bundled entry module sits inside the archive.
///
/// The manifest's `entry` field names it, so nothing reads this constant to
/// *find* the code. It is here because it is what a builder writes there, and
/// a builder that spelled it differently would produce a bundle no node could
/// load - the same reason [`MANIFEST_ENTRY`] and [`ASSETS_PREFIX`] live here.
pub const WORKER_ENTRY: &str = "worker/index.js";

/// What went wrong with a bundle.
///
/// Every variant is a rejection of untrusted input, so each message says what
/// was wrong in terms the person who ran the build can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleError {
    /// The bytes are not a readable zip.
    NotAnArchive(String),
    /// The archive is missing something the format requires.
    Malformed(String),
    /// The manifest parsed but describes something unusable.
    InvalidManifest(String),
    /// The manifest is from a schema this build does not know.
    UnsupportedSchema { found: u32, supported: u32 },
    /// The archive, or one member of it, is past a ceiling in [`archive`].
    ///
    /// Its own variant rather than a [`Self::Malformed`]: nothing is wrong
    /// with the bytes, and what the person who ran the build has to be told is
    /// the number - which ceiling, and what it is - so that they can see
    /// whether they hit it with a real bundle or handed the platform a bomb.
    TooLarge {
        /// Which ceiling was hit.
        limit: archive::Limit,
        /// The member the figure is about, when it is about one. A name too
        /// long to quote whole is shortened, because the length is the fault.
        member: Option<String>,
        /// What was measured. `None` when the read stopped at the ceiling and
        /// the true figure was never learned - which is the point of stopping.
        found: Option<u64>,
    },
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnArchive(m) => write!(f, "bundle is not a readable zip archive: {m}"),
            Self::Malformed(m) => write!(f, "bundle is malformed: {m}"),
            Self::InvalidManifest(m) => write!(f, "bundle {MANIFEST_ENTRY} is invalid: {m}"),
            Self::UnsupportedSchema { found, supported } => write!(
                f,
                "bundle manifest schema {found} is not supported; this build understands schema \
                 {supported}. A schema number changes only for something an older build cannot \
                 safely ignore, so this bundle is refused whole rather than acted on in part: \
                 upgrade this component to schema {found}, or deploy a version built against \
                 schema {supported}"
            ),
            Self::TooLarge {
                limit,
                member,
                found,
            } => {
                let about = match member {
                    Some(member) => format!(" in member {member:?}"),
                    None => String::new(),
                };
                match found {
                    Some(found) => write!(
                        f,
                        "bundle is over a format limit{about}: {} is {found}, and this platform \
                         accepts at most {}",
                        limit.measures(),
                        limit.ceiling()
                    ),
                    None => write!(
                        f,
                        "bundle is over a format limit{about}: {} is past the {} this platform \
                         accepts, and it was refused without being unpacked any further",
                        limit.measures(),
                        limit.ceiling()
                    ),
                }
            }
        }
    }
}

impl std::error::Error for BundleError {}

pub type Result<T> = std::result::Result<T, BundleError>;

/// `manifest.json`.
///
/// Unknown fields are accepted on purpose, at every level of the document -
/// see [`forward`] for the rule and why one unknown key must never cost the
/// whole bundle. The raw text is stored verbatim alongside the parsed form
/// (see [`ParsedManifest::json`]), so a key this build drops is still carried
/// to every component and still there when the fleet is upgraded; nothing but
/// this process's own view of the manifest loses it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    /// Application this bundle belongs to. Must match the upload target.
    pub app: String,
    /// Bundled entry point inside the archive, for example `worker/index.js`.
    pub entry: String,
    #[serde(default)]
    pub runtime: Option<String>,
    /// The namespaces and the applications this app reaches by name, as
    /// `context.env.<NAME>`. These are the only platform resources an
    /// application has: there is no unnamed namespace beside them.
    ///
    /// Judged by [`Self::validate`], unlike [`Self::limits`] and
    /// [`Self::compatibility`], because what is wrong with a binding
    /// declaration is never a request a node could clamp: two names that are
    /// one namespace on a case-insensitive filesystem, or a storage binding
    /// carrying another application's id, are two meanings in one place. There
    /// is no later moment at which either could be noticed - the first symptom
    /// is data in the wrong namespace. See [`bindings`].
    #[serde(default, skip_serializing_if = "DeclaredBindings::is_empty")]
    pub bindings: DeclaredBindings,

    /// Which applications this one agrees to be called by, through a service
    /// binding.
    ///
    /// Consent, and it lives here - in the callee's own bundle, versioned with
    /// the callee's own code - rather than anywhere a caller can reach. An
    /// application naming another in its `bindings` is only *asking*; this is
    /// the answer, and the node reads it out of the callee's current version
    /// on every call, so withdrawing consent takes effect at the callee's next
    /// deployment and nothing has to be redelivered to whoever named it.
    ///
    /// Empty is the default and consents to nobody, which is what an
    /// application that has never thought about being called should mean.
    /// [`CALLABLE_BY_EVERYONE`] is the one wildcard, and a node says so out
    /// loud when it loads a version carrying it.
    ///
    /// Read narrowly: an entry this build cannot read is left out of the
    /// parsed list rather than refusing the manifest, because the list's
    /// vocabulary grows and a left-out entry consents to nobody, which is
    /// less than it meant and never more. The build's own list never passes
    /// through that reader, so [`validate_callable_by`] still refuses the
    /// author's typo by name. See `bindings::readable_callable_by` and
    /// [`forward`].
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "bindings::readable_callable_by"
    )]
    pub callable_by: Vec<String>,
    /// The exact Edge Canon environment declaration: which config values and
    /// secrets the application reads, never their values. Config values are
    /// given at deployment and secrets are held by the control plane; both
    /// reach a node only through the deployment's environment snapshot.
    pub environment: deis_wire::EnvironmentDeclarations,
    #[serde(default)]
    pub routes: Vec<Route>,
    /// Cron expressions this app wants to be triggered on.
    #[serde(default)]
    pub cron: Vec<String>,
    #[serde(default)]
    pub queues: Vec<QueueConsumer>,
    #[serde(default)]
    pub assets: Vec<Asset>,
    /// What the static half of the app does beyond serving files by name:
    /// `_redirects`, `_headers`, and the choice between an SPA fallback and a
    /// custom 404 page. Absent in a bundle that ships none of them.
    #[serde(default, skip_serializing_if = "SiteConfig::is_empty")]
    pub site: SiteConfig,
    /// The ceilings this application asks to be held to, or `None` when it
    /// asks for nothing and takes each node's defaults.
    ///
    /// Read but not judged here: `Manifest::validate` says nothing about it,
    /// because the values are a node's business and the only refusals worth
    /// making are made once, at upload, where the person who wrote the block
    /// is still watching (`deis_control::catalog::declared_limits`). What this
    /// field does enforce is the spelling - see [`DeclaredLimits`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<DeclaredLimits>,
    /// Which day's platform this application was written against, and any
    /// behaviour it takes early or holds back by name. `None` when the
    /// application predates the mechanism, which pins it to the oldest
    /// behaviour the platform has - see [`CompatibilityProfile::resolve`].
    ///
    /// Not judged by [`Manifest::validate`], for the reason [`Self::limits`]
    /// is not: whether a declaration can be honoured is a question about the
    /// *build that is asking*, not about the manifest. The same bundle is
    /// perfectly good on a node built after the date it names and unusable on
    /// one built before it, and a check inside `validate` would have to answer
    /// for both. [`Manifest::compatibility_profile`] is the single place that
    /// answers it, and every component calls it - the build with the project
    /// file still on screen, the control plane at upload and again at
    /// delivery, the node when it installs the version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<DeclaredCompatibility>,
}

/// A route pattern the runtime matches against to fill `context.params`.
///
/// This does not select the app - deis-proxy does that - and it does not gate
/// a request. A pattern that matches nothing shows up only as empty params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    /// Exact segments plus `:name` placeholders, for example
    /// `/api/users/:id`. Spelled the same way in `RouteRule.pattern`.
    pub pattern: String,
    /// Methods this route applies to. Empty means any method.
    #[serde(default)]
    pub methods: Vec<String>,
}

/// A queue this app consumes from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueConsumer {
    pub name: String,
    /// How many messages to pull at once. None lets the runtime decide.
    #[serde(default)]
    pub batch_size: Option<u32>,
    /// How many batches of this queue may be outstanding at once. None lets
    /// the runtime decide, which is one.
    ///
    /// **Outstanding, not parallel.** One `(application, environment)` is one
    /// isolate on one thread on each node, so two batches of the same queue
    /// can never run at the same time whatever this says: the handler is
    /// serialised by the isolate. What this raises is how many batches the
    /// node's poller may hold at once -- leased from deis-queue, waiting their
    /// turn on the runtime thread, or being acknowledged -- so that the next
    /// batch is already in hand when the handler finishes instead of costing
    /// a fresh round trip.
    ///
    /// Raising it costs lease time. Every outstanding batch has been leased
    /// since it was pulled, and a batch waiting its turn is spending that
    /// lease doing nothing; past a point the lease lapses while the batch is
    /// still queued and deis-queue hands the same messages to somebody else.
    /// `deis-runtime`'s consumer clamps the value and says what it applied.
    #[serde(default)]
    pub max_concurrency: Option<u32>,
    /// How many times a failing delivery is retried before the message is
    /// dead-lettered.
    ///
    /// The standard requires the number to be declared, so the build always
    /// states one here (the standard's default of 3 fills an absent project
    /// value). The deislet queue service counts attempts server-side and
    /// applies the same default, so a manifest that says nothing and one that
    /// says 3 behave identically; the declared value is what a provider
    /// driver writes into its own consumer configuration, so both targets
    /// retry the same number of times.
    #[serde(default)]
    pub max_retries: Option<u32>,
}

/// One static file carried in the bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    /// Path relative to `assets/`, with no leading slash.
    pub path: String,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

impl Manifest {
    /// Parse and check a manifest on its own, without an archive around it.
    pub fn parse(json: &str) -> Result<Self> {
        let manifest: Manifest = serde_json::from_str(json)
            .map_err(|e| BundleError::InvalidManifest(format!("does not parse: {e}")))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Checks that hold regardless of who is reading the bundle.
    pub fn validate(&self) -> Result<()> {
        if self.schema != SUPPORTED_SCHEMA {
            return Err(BundleError::UnsupportedSchema {
                found: self.schema,
                supported: SUPPORTED_SCHEMA,
            });
        }
        if self.app.trim().is_empty() {
            return Err(BundleError::InvalidManifest("\"app\" is empty".into()));
        }
        if self.entry.trim().is_empty() {
            return Err(BundleError::InvalidManifest("\"entry\" is empty".into()));
        }
        // Compiled here rather than where they are first used, so a bundle
        // carrying a rule nothing could apply is refused wherever a manifest
        // is read - by the control plane at upload and by a node at load -
        // rather than on the one request that would have matched it.
        self.site
            .validate()
            .map_err(|e| BundleError::InvalidManifest(e.to_string()))?;

        // Same reason, and one more: a binding name becomes a namespace on the
        // storage service, so a name that is wrong is a place an application
        // writes without knowing. `self.app` goes in because an application
        // binding itself in its own environment is a cycle, and the manifest
        // is the only place that knows which application this is.
        self.bindings
            .validate(&self.app)
            .map_err(BundleError::InvalidManifest)?;
        validate_callable_by(&self.callable_by).map_err(BundleError::InvalidManifest)?;

        // Also refuses a declaration that shares a name with a binding: a
        // binding is published on the environment table and the platform's
        // property wins, so the value would never reach a handler.
        self.environment
            .validate(
                deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
                self.bindings.all_names(),
            )
            .map_err(|error| BundleError::InvalidManifest(error.to_string()))?;

        for asset in &self.assets {
            if asset.path.starts_with('/') {
                return Err(BundleError::InvalidManifest(format!(
                    "asset path {:?} has a leading slash; paths are relative to {ASSETS_PREFIX}",
                    asset.path
                )));
            }
            // A path that climbs out of the assets directory would let a
            // bundle address files outside its own tree once unpacked.
            if asset.path.split('/').any(|seg| seg == "..") {
                return Err(BundleError::InvalidManifest(format!(
                    "asset path {:?} escapes the assets directory",
                    asset.path
                )));
            }
        }
        Ok(())
    }

    /// Reject a bundle filed under the wrong application.
    ///
    /// Without this check an upload could file one app's code under another
    /// app's versions, which is both a correctness and an isolation problem.
    pub fn check_app(&self, app_id: &str) -> Result<()> {
        if self.app != app_id {
            return Err(BundleError::InvalidManifest(format!(
                "manifest declares app {:?} but the bundle was uploaded to app {:?}",
                self.app, app_id
            )));
        }
        Ok(())
    }

    /// The resolved compatibility answers for this bundle, or a refusal.
    ///
    /// **The single reader of the `compatibility` block.** The build calls it
    /// so an author learns at compile time, the control plane calls it at
    /// upload and again when it builds a delivery, and a node calls it when it
    /// installs the version. Four callers, one answer, and none of them
    /// repeats the reasoning.
    ///
    /// `Err` means *this build* cannot honour what the manifest declares: a
    /// flag name it does not know, two flags that contradict each other, or a
    /// date later than [`PLATFORM_KNOWN_THROUGH`]. It is a real answer and not
    /// a formality - the same bundle returns `Ok` on a newer build, which is
    /// exactly the mixed-fleet case this exists for, and it is why the refusal
    /// lives here rather than in [`Manifest::validate`], which has no way to
    /// say "not on this one".
    ///
    /// A manifest that declares nothing resolves to the oldest behaviour,
    /// which is what an application written before any of this existed was
    /// written against.
    pub fn compatibility_profile(&self) -> std::result::Result<CompatibilityProfile, String> {
        match &self.compatibility {
            Some(declared) => {
                declared.validate()?;
                CompatibilityProfile::resolve(declared)
            }
            None => Ok(CompatibilityProfile::default()),
        }
    }

    /// Route patterns in declaration order, which is what the runtime's
    /// matcher consumes.
    pub fn route_patterns(&self) -> Vec<String> {
        self.routes.iter().map(|r| r.pattern.clone()).collect()
    }
}

/// Complete a manifest written by hand: set `schema` to [`SUPPORTED_SCHEMA`]
/// and, unless `partial` declares one, an `environment` declaring nothing.
///
/// For fixtures - tests across the workspace and the demo's hand-made
/// bundles - that are about some other part of the manifest and would
/// otherwise each spell out the whole Edge Canon declaration document.
/// `partial` must be a JSON object; anything else is returned unchanged, so
/// the parse that follows reports it.
pub fn complete_manifest_json(partial: &str) -> String {
    let Ok(serde_json::Value::Object(mut object)) = serde_json::from_str(partial) else {
        return partial.to_string();
    };
    object.insert("schema".to_string(), serde_json::json!(SUPPORTED_SCHEMA));
    object.entry("environment").or_insert_with(|| {
        serde_json::to_value(deis_wire::EnvironmentDeclarations::new(
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
            Vec::new(),
        ))
        .expect("an environment declaration serializes")
    });
    serde_json::Value::Object(object).to_string()
}

/// A manifest read out of an archive, together with the exact text it came
/// from and the module it names.
///
/// The verbatim text matters: the control plane stores it in the version row,
/// so a field this build does not understand still survives a round trip.
#[derive(Debug, Clone)]
pub struct ParsedManifest {
    pub json: String,
    pub manifest: Manifest,
    /// Source text of the module `manifest.entry` names.
    ///
    /// Carried here because [`read_manifest`] has to unpack it anyway - the
    /// only honest way to hold a member to a ceiling is to read it and stop -
    /// and whoever reads it a second time pays for the same bytes twice.
    /// `deis-runtime` still has its own read of the same member; it is bounded
    /// only because this one already proved the member fits in
    /// [`MAX_ENTRY_BYTES`], and it should be taking the text from here.
    pub entry_module: String,
}

/// Read and validate `manifest.json` out of a zip held in memory.
///
/// Also unpacks the declared entry point, so a bundle that would fail at load
/// time - no such member, not text, or a member that expands past what any
/// node will read - is refused at upload time, while the person who ran the
/// build is still watching.
///
/// Every member is read through [`BundleArchive`], which holds each one to a
/// ceiling. Nothing here reads the zip directly: an uploaded bundle is the
/// least trusted input the platform has, and a member read without a ceiling
/// is a few hundred kilobytes on the wire turning into gigabytes in this
/// process and in every node that fetches the version.
pub fn read_manifest(bytes: &[u8]) -> Result<ParsedManifest> {
    let mut archive = BundleArchive::open(bytes)?;

    let json = archive
        .read_text(MANIFEST_ENTRY, Limit::ManifestBytes)?
        .ok_or_else(|| {
            BundleError::Malformed(format!(
                "no {MANIFEST_ENTRY} at the archive root; it was not produced by a deislet build"
            ))
        })?;

    let manifest = Manifest::parse(&json)?;
    // Here rather than in `Manifest::parse`, because this is the path a whole
    // archive comes in on - once at the build, once at the upload, once per
    // node that installs the version. `parse` is also what the control plane's
    // scheduler calls for every deployed environment on every tick, and a line
    // per application per minute would bury the one an operator needs.
    forward::report(&json, manifest.schema);

    let entry_module = archive
        .read_text(&manifest.entry, Limit::EntryBytes)?
        .ok_or_else(|| {
            BundleError::InvalidManifest(format!(
                "entry {:?} is not in the archive",
                manifest.entry
            ))
        })?;

    Ok(ParsedManifest {
        json,
        manifest,
        entry_module,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn minimal() -> String {
        complete_manifest_json(r#"{"app":"demo","entry":"worker/index.js"}"#)
    }

    #[test]
    fn reads_a_minimal_bundle() {
        let minimal = minimal();
        let z = zip_with(&[(MANIFEST_ENTRY, &minimal), ("worker/index.js", "export {}")]);
        let parsed = read_manifest(&z).unwrap();
        assert_eq!(parsed.manifest.app, "demo");
        assert_eq!(parsed.json, minimal, "the verbatim text must survive");
    }

    #[test]
    fn keeps_fields_it_does_not_understand() {
        let json = complete_manifest_json(r#"{"app":"demo","entry":"e.js","futureKey":{"a":1}}"#);
        let z = zip_with(&[(MANIFEST_ENTRY, &json), ("e.js", "")]);
        let parsed = read_manifest(&z).unwrap();
        assert!(parsed.json.contains("futureKey"));
    }

    #[test]
    fn rejects_a_missing_entry_point() {
        let z = zip_with(&[(MANIFEST_ENTRY, &minimal())]);
        let err = read_manifest(&z).unwrap_err();
        assert!(matches!(err, BundleError::InvalidManifest(_)), "{err}");
    }

    #[test]
    fn rejects_a_future_schema() {
        let json = complete_manifest_json(r#"{"app":"demo","entry":"e.js"}"#)
            .replace("\"schema\":2", "\"schema\":99");
        assert_eq!(
            Manifest::parse(&json).unwrap_err(),
            BundleError::UnsupportedSchema {
                found: 99,
                supported: SUPPORTED_SCHEMA
            }
        );
    }

    #[test]
    fn a_manifest_requires_the_standard_environment_declaration() {
        let missing = r#"{"schema":2,"app":"demo","entry":"e.js"}"#;
        let error = Manifest::parse(missing).unwrap_err().to_string();
        assert!(error.contains("environment"), "{error}");

        let declaration = serde_json::to_value(deis_wire::EnvironmentDeclarations::new(
            "edge-canon.next@a41a4caadeaf8e5751ebe57ef5be673d18384654",
            vec![deis_wire::EnvironmentDeclaration {
                name: "TOKEN".into(),
                kind: deis_wire::EnvironmentKind::Secret,
                value_type: deis_wire::EnvironmentValueType::String,
                required: true,
            }],
        ))
        .unwrap();
        let valid = serde_json::json!({
            "schema": 2,
            "app": "demo",
            "entry": "e.js",
            "environment": declaration
        });
        let parsed = Manifest::parse(&valid.to_string()).unwrap();
        assert_eq!(parsed.environment.declarations.len(), 1);

        let mut foreign_revision = valid.clone();
        foreign_revision["environment"]["standardVersion"] =
            serde_json::json!("edge-canon.next@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let error = Manifest::parse(&foreign_revision.to_string())
            .unwrap_err()
            .to_string();
        assert!(error.contains("EC_ENV_STANDARD_PIN_INVALID"), "{error}");

        let mut schema_one = valid;
        schema_one["schema"] = serde_json::json!(1);
        assert_eq!(
            Manifest::parse(&schema_one.to_string()).unwrap_err(),
            BundleError::UnsupportedSchema {
                found: 1,
                supported: SUPPORTED_SCHEMA
            }
        );
    }

    /// The other half of the reserved-name rule, on the environment side.
    ///
    /// A logical resource called `ASSETS` has been refused since bindings
    /// existed; an *environment declaration* of that name was not, and it
    /// travels in a different field of the same manifest. A bundle carrying
    /// one is refused here rather than only where it was built, because this
    /// is the check the control plane makes when a bundle is uploaded and a
    /// node makes when one is loaded - including a bundle that reached a node
    /// without passing through a control plane of ours. Without it the
    /// declaration would be minted as required, every layer would demand a
    /// value for it, and the value would then be unreadable: the platform
    /// installs `env.ASSETS` after the declared values are copied.
    #[test]
    fn rejects_an_environment_declaration_that_takes_a_platform_name() {
        for name in ["ASSETS", "DEIS_STORE_ENDPOINT"] {
            let declaration = serde_json::to_value(deis_wire::EnvironmentDeclarations::new(
                "edge-canon.next@a41a4caadeaf8e5751ebe57ef5be673d18384654",
                vec![deis_wire::EnvironmentDeclaration {
                    name: name.into(),
                    kind: deis_wire::EnvironmentKind::Config,
                    value_type: deis_wire::EnvironmentValueType::String,
                    required: true,
                }],
            ))
            .unwrap();
            let manifest = serde_json::json!({
                "schema": 2,
                "app": "demo",
                "entry": "e.js",
                "environment": declaration
            });
            let error = Manifest::parse(&manifest.to_string())
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("EC_ENV_NAMESPACE_COLLISION"),
                "a declaration called {name:?} was accepted into a bundle: {error}"
            );
        }
    }

    #[test]
    fn rejects_an_asset_path_that_escapes_the_bundle() {
        let json = complete_manifest_json(
            r#"{"app":"d","entry":"e.js","assets":[{"path":"../etc/passwd"}]}"#,
        );
        assert!(matches!(
            Manifest::parse(&json).unwrap_err(),
            BundleError::InvalidManifest(_)
        ));
    }

    #[test]
    fn rejects_a_bundle_filed_under_another_app() {
        let m = Manifest::parse(&minimal()).unwrap();
        assert!(m.check_app("demo").is_ok());
        assert!(m.check_app("other").is_err());
    }

    #[test]
    fn rejects_a_binding_that_would_be_shadowed_by_a_declaration() {
        // A binding is published on the environment table and the platform's
        // property wins, so a config value or a secret of the same name is a
        // value the author set and no handler will ever read. Refused where
        // it is written.
        for kind in [
            deis_wire::EnvironmentKind::Config,
            deis_wire::EnvironmentKind::Secret,
        ] {
            let environment = serde_json::to_value(deis_wire::EnvironmentDeclarations::new(
                deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
                vec![deis_wire::EnvironmentDeclaration {
                    name: "HOT".into(),
                    kind,
                    value_type: deis_wire::EnvironmentValueType::String,
                    required: true,
                }],
            ))
            .unwrap();
            let json = serde_json::json!({
                "schema": 2, "app": "d", "entry": "e.js",
                "bindings": {"HOT": {"kind": "kv"}},
                "environment": environment,
            });
            let err = Manifest::parse(&json.to_string()).unwrap_err();
            assert!(
                err.to_string().contains("EC_ENV_NAMESPACE_COLLISION"),
                "{err}"
            );
        }
    }

    #[test]
    fn a_binding_declaration_is_refused_wherever_a_manifest_is_read() {
        // `validate` runs at build, at upload and at load, so a bundle that
        // carries two names that are one namespace on a case-insensitive
        // filesystem never reaches a node - and if one somehow did, the node
        // would refuse it rather than run it.
        let json = complete_manifest_json(
            r#"{"app":"d","entry":"e.js","bindings":{"HOT":{"kind":"kv"},"hot":{"kind":"kv"}}}"#,
        );
        let err = Manifest::parse(&json).unwrap_err();
        assert!(err.to_string().contains("differ only in case"), "{err}");
    }

    #[test]
    fn a_manifest_without_bindings_declares_none() {
        // Every application written before bindings existed. The field is
        // absent from what it serialises back out, so a round trip does not
        // start writing an empty object into everybody's manifest.
        let manifest = Manifest::parse(&minimal()).unwrap();
        assert!(manifest.bindings.is_empty());
        assert!(manifest.callable_by.is_empty());
        let json = serde_json::to_string(&manifest).unwrap();
        assert!(!json.contains("bindings"), "{json}");
        assert!(!json.contains("callable_by"), "{json}");
    }

    #[test]
    fn a_manifest_that_unpacks_without_end_is_refused_before_it_is_parsed() {
        // Half a megabyte on the wire used to become half a gigabyte in this
        // process, and the refusal that followed was a JSON syntax error -
        // after the whole thing had been materialised. The ceiling is the
        // reader's now, so the bytes are never held.
        let manifest = "a".repeat((MAX_MANIFEST_BYTES + 1) as usize);
        let z = zip_with(&[(MANIFEST_ENTRY, &manifest), (WORKER_ENTRY, "export {}")]);
        assert!(
            (z.len() as u64) < MAX_MANIFEST_BYTES / 100,
            "a bomb is small on the wire; this fixture is {} bytes",
            z.len()
        );

        let err = read_manifest(&z).expect_err("a manifest nothing could have written");
        assert!(
            matches!(
                err,
                BundleError::TooLarge {
                    limit: Limit::ManifestBytes,
                    ..
                }
            ),
            "{err}"
        );
        assert!(
            err.to_string().contains(&MAX_MANIFEST_BYTES.to_string()),
            "the refusal has to name the ceiling: {err}"
        );
    }

    #[test]
    fn an_entry_module_that_unpacks_without_end_is_refused_at_upload() {
        // This one used to be accepted here and paid for on every node that
        // fetched the version: the control plane only checked that the member
        // existed, and the node read it with no ceiling at all.
        let json = minimal();
        let module = "/".repeat((MAX_ENTRY_BYTES + 1) as usize);
        let z = zip_with(&[(MANIFEST_ENTRY, &json), (WORKER_ENTRY, &module)]);

        let err = read_manifest(&z).expect_err("no node will read a module this size");
        assert!(
            matches!(
                err,
                BundleError::TooLarge {
                    limit: Limit::EntryBytes,
                    ..
                }
            ),
            "{err}"
        );
        assert!(err.to_string().contains(WORKER_ENTRY), "{err}");
    }

    #[test]
    fn the_entry_module_comes_back_with_the_manifest() {
        // Read once, here, under a ceiling - so that nothing downstream has
        // to unpack the same member a second time to get at it.
        let z = zip_with(&[
            (MANIFEST_ENTRY, &minimal()),
            (WORKER_ENTRY, "export default 1"),
        ]);
        let parsed = read_manifest(&z).unwrap();
        assert_eq!(parsed.entry_module, "export default 1");
    }

    #[test]
    fn rejects_bytes_that_are_not_a_zip() {
        assert!(matches!(
            read_manifest(b"not a zip").unwrap_err(),
            BundleError::NotAnArchive(_)
        ));
    }
}
