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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VendorConfig {
    pub enabled: bool,
    #[serde(default)]
    pub options: Option<HashMap<String, serde_json::Value>>,
}

/// Where the project's static files live.
///
/// The directory is copied into the bundle under `assets/` with its structure
/// intact, so `public/css/app.css` is served at `/css/app.css`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetsConfig {
    #[serde(default = "default_assets_dir")]
    pub dir: String,
}

impl Default for AssetsConfig {
    fn default() -> Self {
        Self {
            dir: default_assets_dir(),
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildConfig {
    #[serde(rename = "outDir", default = "default_out_dir")]
    pub out_dir: String,
    #[serde(default = "default_true")]
    pub minify: bool,
    #[serde(default = "default_true")]
    pub sourcemap: bool,
    #[serde(default)]
    pub target: Option<String>,
}

fn default_out_dir() -> String {
    "./dist".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    #[serde(default)]
    pub timeout: Option<u32>,
    #[serde(rename = "maxPayloadSize", default)]
    pub max_payload_size: Option<usize>,
    #[serde(rename = "maxConcurrency", default)]
    pub max_concurrency: Option<u32>,
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
    fn a_config_without_the_new_fields_still_parses() {
        // Projects predate every field added here; none of them may become
        // required, or an existing project stops building.
        let config = parse(minimal());
        assert!(config.assets.is_none());
        assert!(config.cron.is_empty());
        assert!(config.queues.is_empty());
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
    fn the_assets_directory_has_a_default() {
        let mut json = minimal();
        json["assets"] = serde_json::json!({});
        let config = parse(json);
        assert_eq!(config.assets.unwrap().dir, "./public");
    }
}
