use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub version: String,
    pub name: String,
    pub runtime: String,
    pub language: String,
    #[serde(rename = "functionRoot", default = "default_function_root")]
    pub function_root: String,
    #[serde(default)]
    pub environment: Option<EnvironmentConfig>,
    #[serde(default)]
    pub services: Option<ServicesConfig>,
    #[serde(default)]
    pub vendors: Option<VendorsConfig>,
    #[serde(default)]
    pub build: Option<BuildConfig>,
    #[serde(default)]
    pub limits: Option<LimitsConfig>,
    /// The day this project's code was written against, as `YYYY-MM-DD`.
    ///
    /// Copied into the bundle manifest and carried to every node, where it
    /// decides which of the platform's answers this application gets. Absent
    /// means the oldest answers the platform has, for ever: a project without
    /// a date never receives a fix that changes an observable behaviour. That
    /// is the safe direction and it is the reason `deis deploy` says so out
    /// loud on every deploy rather than choosing a date on an author's behalf:
    /// a date the build filled in would move on its own, and the same
    /// unchanged source would behave differently the next time it was built.
    ///
    /// Spelled the way the rest of this file spells a two-word key, with the
    /// manifest's own snake_case accepted too.
    #[serde(rename = "compatibilityDate", alias = "compatibility_date", default)]
    pub compatibility_date: Option<String>,
    /// Behaviours taken early or held back by name, overriding what
    /// [`Self::compatibility_date`] alone would give. The names are the ones
    /// in `deis_bundle::compatibility::FLAGS`; one this build does not know
    /// fails the build rather than being ignored.
    #[serde(rename = "compatibilityFlags", alias = "compatibility_flags", default)]
    pub compatibility_flags: Vec<String>,
    /// Static files to carry in the bundle, served before any handler runs.
    #[serde(default)]
    pub assets: Option<AssetsConfig>,
    /// Cron expressions this app wants to be triggered on. Copied verbatim
    /// into the bundle manifest; the control plane owns the schedule.
    #[serde(default)]
    pub cron: Vec<String>,
    /// Queues this app consumes from.
    #[serde(default)]
    pub queues: Vec<QueueConsumerConfig>,
    /// The namespaces and the applications this project reaches by name.
    ///
    /// Copied verbatim into the bundle manifest, where the shape and every
    /// rule about it live (`deis_bundle::bindings`). Nothing is invented here:
    /// a declaration this build cannot honour fails the build, with the
    /// project file still on the author's screen, rather than producing a
    /// bundle the control plane would reject.
    ///
    /// ```json
    /// "bindings": {
    ///   "HOT":  { "kind": "kv" },
    ///   "AUTH": { "kind": "service", "app": "auth-api" }
    /// }
    /// ```
    #[serde(default)]
    pub bindings: HashMap<String, BindingConfig>,
    /// Which applications may call this one through a service binding.
    ///
    /// The consenting half of a service binding, and it belongs here - in the
    /// callee's own project - because it is the callee agreeing. `["*"]` means
    /// every application on this platform.
    #[serde(rename = "callableBy", alias = "callable_by", default)]
    pub callable_by: Vec<String>,
}

/// One entry of the project file's `bindings` map.
///
/// A mirror of `deis_bundle::Binding` rather than a re-export, for the reason
/// every other block in this file is: the project file is camelCase and
/// tolerant of keys a newer build added, while the manifest is snake_case and
/// refuses anything it does not know. The translation, and every check, is
/// `deis-build-adapter-deislet`'s `manifest::bindings`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindingConfig {
    /// `kv`, `database`, `blob`, `queue` or `service`.
    pub kind: String,
    /// The application a `service` binding calls.
    #[serde(default)]
    pub app: Option<String>,
    /// Which environment of that application. Absent means the caller's own,
    /// which is the default a preview stack needs: `pr-42` of one application
    /// reaches `pr-42` of the other rather than production.
    #[serde(default)]
    pub environment: Option<String>,
}

fn default_function_root() -> String {
    "./functions".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentConfig {
    #[serde(default)]
    pub variables: Option<HashMap<String, String>>,
    #[serde(default)]
    pub secrets: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServicesConfig {
    #[serde(default)]
    pub kv: Option<ServiceConfig>,
    #[serde(default)]
    pub database: Option<ServiceConfig>,
    #[serde(default)]
    pub blob: Option<ServiceConfig>,
    #[serde(default)]
    pub queue: Option<ServiceConfig>,
}

/// Whether a project declares one of the platform services.
///
/// One field, and deliberately so. This struct used to carry `namespace` and
/// `connectionString` as well, and nothing in the workspace ever read either
/// of them: the bundle manifest records services as plain booleans, and a
/// tenant is already uniquely `(application, environment)`, so an
/// application's KV is its own and has nothing to be named apart from. A
/// field an author can set and no component reads is worse than no field,
/// because it reads as a supported knob.
///
/// An application that wants two logical stores prefixes its keys and
/// enumerates a prefix with `kv.list({ prefix })`; where a boundary the
/// platform actually enforces is wanted, that is a second environment or a
/// second application. Unknown keys in the project file are still accepted,
/// so a config that carries the old two does not fail to parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VendorsConfig {
    #[serde(default)]
    pub cloudflare: Option<VendorConfig>,
    #[serde(default)]
    pub deno: Option<VendorConfig>,
    #[serde(default)]
    pub tencent: Option<VendorConfig>,
    /// This platform's own target. `deislet` is the name everywhere else -
    /// the CLI flag, the manifest, the adapter crate - so it is the name the
    /// config key uses too. The older spellings still parse rather than
    /// silently disabling the target in a project that predates the rename.
    #[serde(rename = "deislet", alias = "deis_runtime", alias = "denix", default)]
    pub deislet: Option<VendorConfig>,
}

/// Whether a project builds for one of the vendor targets.
///
/// One field, for the reason [`ServiceConfig`] has one: this used to carry a
/// free-form `options` map as well, and nothing in the workspace ever looked at
/// it. An adapter reads the project configuration directly - the Cloudflare one
/// reads `name`, `services` and `compatibilityDate` - so an `options` bag was a
/// place to write settings that reached no adapter and changed no output. A
/// knob an author can turn that moves nothing is worse than no knob. Unknown
/// keys in the project file are still accepted, so a config that carries the
/// old map does not fail to parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VendorConfig {
    pub enabled: bool,
}

/// Where the project's static files live, and what happens to a path none of
/// them answers.
///
/// The directory is copied into the bundle under `assets/` with its structure
/// intact, so `public/css/app.css` is served at `/css/app.css`. Two files in
/// that directory are read rather than served - `_redirects` and `_headers`;
/// see `deis_bundle::redirects` and `deis_bundle::headers`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetsConfig {
    #[serde(default = "default_assets_dir")]
    pub dir: String,
    /// What to answer for a path that named no file, matched no redirect rule
    /// and belongs to no function route: `"none"`, `"404-page"` (serve
    /// `404.html` with a 404) or `"single-page-app"` (serve `index.html` with
    /// a 200).
    ///
    /// Absent is not the same as `"none"`: a project that ships a `404.html`
    /// and says nothing gets `"404-page"`, because that is what shipping the
    /// file means everywhere else and requiring a config key to make it work
    /// would be a trap. Writing `"none"` turns that off.
    #[serde(rename = "notFound", alias = "not_found", default)]
    pub not_found: Option<String>,
}

impl Default for AssetsConfig {
    fn default() -> Self {
        Self {
            dir: default_assets_dir(),
            not_found: None,
        }
    }
}

fn default_assets_dir() -> String {
    "./public".to_string()
}

/// One queue this app consumes from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueConsumerConfig {
    pub name: String,
    /// How many messages to pull at once. Absent lets the runtime decide.
    #[serde(rename = "batchSize", alias = "batch_size", default)]
    pub batch_size: Option<u32>,
    /// How many batches of this queue the node may hold at once. Absent lets
    /// the runtime decide, which is one.
    ///
    /// It bounds *outstanding* batches, not parallel ones: one application in
    /// one environment is a single isolate on a single thread, so its handler
    /// runs one batch at a time whatever this says. See
    /// `deis_bundle::QueueConsumer::max_concurrency` for what raising it buys
    /// and what it costs in lease time.
    #[serde(rename = "maxConcurrency", alias = "max_concurrency", default)]
    pub max_concurrency: Option<u32>,
}

/// What the build does with the code, as the project asks for it.
///
/// Each field is also askable from the command line where a command line makes
/// sense, and the two combine by "either one saying no is a no": `--no-minify`
/// is a flag with nothing to say when it is absent, and [`Self::minify`]
/// defaults to on, so neither can force minification on against the other.
///
/// This block used to carry a `target` string - an ECMAScript level - and
/// nothing read it: the bundler is handed a platform, not a language level, and
/// every target this compiler builds for runs a current V8. An author who wrote
/// `"target": "es2015"` got exactly the output of one who wrote nothing.
/// Unknown keys are still accepted, so a config that carries it still parses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildConfig {
    #[serde(rename = "outDir", default = "default_out_dir")]
    pub out_dir: String,
    /// Whether to minify the bundled entry module. Read in
    /// `deis_build::compiler`, which passes it to `BuildBundler::minified`.
    #[serde(default = "default_true")]
    pub minify: bool,
    /// Whether the bundled entry module carries a source map. The map goes
    /// inside the module as a data URL rather than beside it as a `.map` file:
    /// every target here deploys a single module, so a second file would be
    /// left behind and the module would point at nothing.
    #[serde(default = "default_true")]
    pub sourcemap: bool,
}

fn default_out_dir() -> String {
    "./dist".to_string()
}

fn default_true() -> bool {
    true
}

/// The ceilings a project asks to be held to, carried into the bundle
/// manifest's `limits` block and from there to every node.
///
/// **Narrowing only.** A node clamps each of these against its own
/// configuration, so a project asking for more than the node allows is held to
/// the node's number and the node says so in its log. Asking for *less* is the
/// use: an application that knows a request of its own should never take more
/// than a second says so here, and a handler that goes wrong is cut off in a
/// second instead of holding a runtime thread for the node's full ceiling.
///
/// Every field is optional and absent means "whatever the node allows". A
/// misspelled one is refused rather than dropped: a limit that looks set and
/// is not would be discovered on the day the ceiling an author believed in
/// turned out never to have been in force.
///
/// The names are this file's camelCase, one for one with the snake_case the
/// manifest and the node use - the same translation `notFound` and the queue
/// entries already get. Both spellings are accepted here so that a manifest
/// pasted into a project config still reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    /// V8 heap ceiling for this application's isolates, in MB.
    #[serde(rename = "memoryMb", alias = "memory_mb", default)]
    pub memory_mb: Option<u32>,
    /// Wall-clock ceiling for one invocation, in milliseconds.
    #[serde(rename = "timeoutMs", alias = "timeout_ms", default)]
    pub timeout_ms: Option<u64>,
    /// Ceiling on the rate at which this application's code may dispatch ops.
    #[serde(
        rename = "rateLimitOpsPerSec",
        alias = "rate_limit_ops_per_sec",
        default
    )]
    pub rate_limit_ops_per_sec: Option<f64>,
    /// CPU-time ceiling for one invocation, in milliseconds.
    #[serde(rename = "cpuTimeLimitMs", alias = "cpu_time_limit_ms", default)]
    pub cpu_time_limit_ms: Option<u64>,
    /// How many entries this application's isolate may hold in its resource
    /// table at once.
    #[serde(rename = "resourceTableMax", alias = "resource_table_max", default)]
    pub resource_table_max: Option<u32>,
    /// How many outbound calls one invocation may make. Every hop counts, so
    /// a `fetch()` that follows two redirects costs three.
    #[serde(rename = "maxSubrequests", alias = "max_subrequests", default)]
    pub max_subrequests: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: serde_json::Value) -> ProjectConfig {
        serde_json::from_value(json).expect("config parses")
    }

    fn minimal() -> serde_json::Value {
        serde_json::json!({
            "version": "1.0",
            "name": "demo",
            "runtime": "standard-v1",
            "language": "typescript"
        })
    }

    /// A project that still carries the old `routing` block keeps building.
    ///
    /// The block declared `caseSensitive`, `trailingSlash` and a `routes`
    /// map, and not one of the three was ever read: URL patterns come from
    /// the file tree (`deis_build_adapter_deislet::plan_routes`) and matching
    /// is the runtime's, in `deis_bundle::routing`. Keeping the fields
    /// suggested a project could change routing by editing them, which it
    /// could not. Dropping them from the struct is deliberately not a parse
    /// error - an unknown key is ignored - so nobody has to edit a config to
    /// build again.
    #[test]
    fn a_config_with_the_removed_routing_block_still_parses() {
        let mut json = minimal();
        json["routing"] = serde_json::json!({
            "caseSensitive": true,
            "trailingSlash": "always",
            "routes": { "/old": "./functions/old.ts" }
        });
        let config = parse(json);
        assert_eq!(config.name, "demo");
    }

    #[test]
    fn the_compatibility_keys_take_either_spelling() {
        for (date_key, flags_key) in [
            ("compatibilityDate", "compatibilityFlags"),
            ("compatibility_date", "compatibility_flags"),
        ] {
            let mut json = minimal();
            json[date_key] = serde_json::json!("2026-09-01");
            json[flags_key] = serde_json::json!(["single_set_cookie_header"]);
            let config = parse(json);
            assert_eq!(config.compatibility_date.as_deref(), Some("2026-09-01"));
            assert_eq!(config.compatibility_flags, vec!["single_set_cookie_header"]);
        }
    }

    #[test]
    fn a_config_without_the_new_fields_still_parses() {
        // Projects predate every field added here; none of them may become
        // required, or an existing project stops building.
        let config = parse(minimal());
        assert!(config.assets.is_none());
        assert!(config.cron.is_empty());
        assert!(config.queues.is_empty());
        assert!(config.compatibility_date.is_none());
        assert!(config.compatibility_flags.is_empty());
    }

    #[test]
    fn a_config_with_the_removed_build_target_still_parses() {
        // The key named an ECMAScript level and reached no bundler: an author
        // who set it got the same output as one who did not. Dropping it from
        // the struct must not be a parse error, or a project that wrote it
        // stops building.
        let mut json = minimal();
        json["build"] = serde_json::json!({ "outDir": "./out", "target": "es2015" });
        let config = parse(json);
        let build = config.build.expect("the build block parses");
        assert_eq!(build.out_dir, "./out");
        assert!(build.minify, "the defaults are unchanged");
        assert!(build.sourcemap);
    }

    #[test]
    fn a_config_with_the_removed_vendor_options_still_parses() {
        let mut json = minimal();
        json["vendors"] = serde_json::json!({
            "cloudflare": { "enabled": true, "options": { "anything": 1 } }
        });
        let config = parse(json);
        assert!(config.vendors.unwrap().cloudflare.unwrap().enabled);
    }

    #[test]
    fn the_deislet_vendor_key_accepts_its_older_spellings() {
        for key in ["deislet", "deis_runtime", "denix"] {
            let mut json = minimal();
            json["vendors"] = serde_json::json!({ key: { "enabled": true } });
            let config = parse(json);
            let vendor = config
                .vendors
                .and_then(|v| v.deislet)
                .unwrap_or_else(|| panic!("vendor key {key} maps to the deislet target"));
            assert!(vendor.enabled);
        }
    }

    #[test]
    fn a_queue_consumer_takes_either_spelling_of_batch_size() {
        let mut json = minimal();
        json["queues"] = serde_json::json!([
            { "name": "jobs", "batchSize": 10 },
            { "name": "mail", "batch_size": 5 },
            { "name": "slow" }
        ]);
        let config = parse(json);
        let sizes: Vec<Option<u32>> = config.queues.iter().map(|q| q.batch_size).collect();
        assert_eq!(sizes, vec![Some(10), Some(5), None]);
    }

    #[test]
    fn a_queue_consumer_takes_either_spelling_of_its_concurrency() {
        let mut json = minimal();
        json["queues"] = serde_json::json!([
            { "name": "jobs", "batchSize": 10, "maxConcurrency": 2 },
            { "name": "mail", "max_concurrency": 3 },
            { "name": "slow" }
        ]);
        let config = parse(json);
        let outstanding: Vec<Option<u32>> =
            config.queues.iter().map(|q| q.max_concurrency).collect();
        assert_eq!(outstanding, vec![Some(2), Some(3), None]);
    }

    #[test]
    fn the_subrequest_ceiling_takes_either_spelling_and_a_typo_is_refused() {
        let mut json = minimal();
        json["limits"] = serde_json::json!({ "maxSubrequests": 6 });
        assert_eq!(parse(json).limits.unwrap().max_subrequests, Some(6));

        let mut json = minimal();
        json["limits"] = serde_json::json!({ "max_subrequests": 6 });
        assert_eq!(parse(json).limits.unwrap().max_subrequests, Some(6));

        // The whole point of `deny_unknown_fields`: a ceiling that looks set
        // and is not would be found on the day it was needed.
        let mut json = minimal();
        json["limits"] = serde_json::json!({ "maxSubRequests": 6 });
        assert!(
            serde_json::from_value::<ProjectConfig>(json).is_err(),
            "a misspelled limit must fail the build, not be dropped"
        );
    }

    #[test]
    fn the_assets_directory_has_a_default() {
        let mut json = minimal();
        json["assets"] = serde_json::json!({});
        let config = parse(json);
        let assets = config.assets.unwrap();
        assert_eq!(assets.dir, "./public");
        assert!(
            assets.not_found.is_none(),
            "absent means \"work it out from the files\", not \"none\""
        );
    }

    #[test]
    fn the_not_found_behaviour_takes_either_spelling_of_its_key() {
        for key in ["notFound", "not_found"] {
            let mut json = minimal();
            json["assets"] = serde_json::json!({ key: "single-page-app" });
            let config = parse(json);
            assert_eq!(
                config.assets.unwrap().not_found.as_deref(),
                Some("single-page-app"),
                "key {key}"
            );
        }
    }
}
