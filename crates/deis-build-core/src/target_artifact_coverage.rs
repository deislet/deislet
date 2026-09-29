/**
 * Transitional target-artifact implementation coverage.
 *
 * This records what Deislet's current packagers can encode for each output
 * target. It is not a vendor capability matrix, an Edge Canon conformance
 * result, or a support declaration. Those statuses come only from the locked
 * conformance registry.
 */
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Output target identifier used by Deislet's packagers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Cloudflare,
    Deno,
    Tencent,
    /// The platform this toolchain is part of: deis-runtime executing a bundle
    /// produced by `deis build --target deislet`.
    Deislet,
}

impl Platform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Cloudflare => "cloudflare",
            Platform::Deno => "deno",
            Platform::Tencent => "tencent",
            Platform::Deislet => "deislet",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Platform::Cloudflare => "Cloudflare Workers",
            Platform::Deno => "Deno Deploy",
            Platform::Tencent => "EdgeOne Pages",
            Platform::Deislet => "Deislet",
        }
    }
}

/// Transitional project features understood by the current packagers.
///
/// This enum is not the normative Edge Canon feature registry. KV and Cache
/// are handled by the common build path and are therefore absent here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feature {
    // These are exactly the declarations detect_required_features can read
    // from .config.json. Runtime APIs with no declaration site do not belong
    // in this table, because the compiler cannot enforce them.
    SQLDatabase,
    ObjectStorage,
    NodeRuntime,
    CronJobs,
    Queues,
}

impl Feature {
    pub fn name(&self) -> &'static str {
        match self {
            Feature::SQLDatabase => "SQL Database",
            Feature::ObjectStorage => "Object Storage",
            Feature::NodeRuntime => "Node.js Runtime",
            Feature::CronJobs => "Cron Jobs / Scheduled Tasks",
            Feature::Queues => "Message Queues",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Feature::SQLDatabase => "SQL database for relational data",
            Feature::ObjectStorage => "S3-compatible object storage for large files",
            Feature::NodeRuntime => "Node.js built-in modules and npm packages",
            Feature::CronJobs => "Scheduled background tasks",
            Feature::Queues => "Guaranteed message delivery queues",
        }
    }

    /// Name of the mapping emitted by Deislet's current target packager.
    pub fn target_implementation_name(&self, platform: Platform) -> Option<&'static str> {
        match (self, platform) {
            // The binding plan maps a named `database` binding onto a D1
            // database; the wrangler.toml `database_id` is the plan's
            // placeholder, resolved by the provider driver at prepare time.
            (Feature::SQLDatabase, Platform::Cloudflare) => {
                Some("D1 database mapped from the binding plan")
            }
            // No database declaration or connection wiring is generated in
            // the Deno artifact today.
            (Feature::SQLDatabase, Platform::Deno) => None,
            (Feature::SQLDatabase, Platform::Tencent) => None,
            // deis-store serves one SQLite database per tenant over gRPC, and
            // deis-runtime builds the client in `runtime/bindings.rs` from the
            // manifest's logical bindings, so `context.env.<NAME>` reaches it.
            (Feature::SQLDatabase, Platform::Deislet) => Some("Per-app SQLite in deis-store"),

            // The binding plan maps a named `blob` binding onto an R2
            // bucket; the `bucket_name` is the plan's placeholder, resolved
            // by the provider driver at prepare time.
            (Feature::ObjectStorage, Platform::Cloudflare) => {
                Some("R2 bucket mapped from the binding plan")
            }
            (Feature::ObjectStorage, Platform::Deno) => None,
            (Feature::ObjectStorage, Platform::Tencent) => None,
            // Same shape as the database: deis-store serves the blobs and
            // deis-runtime builds the isolate's client from the manifest.
            (Feature::ObjectStorage, Platform::Deislet) => {
                Some("Content-addressed blobs in deis-store")
            }

            (Feature::NodeRuntime, Platform::Cloudflare) => {
                Some("locked Node façade plus nodejs_compat")
            }
            (Feature::NodeRuntime, Platform::Deno) => Some("Node.js compatibility layer"),
            (Feature::NodeRuntime, Platform::Tencent) => Some("Node Functions"),
            (Feature::NodeRuntime, Platform::Deislet) => Some("locked Node façade in deis-runtime"),

            // The cloudflare packager writes the project's cron expressions
            // into `[triggers]` and exports the `scheduled` handler.
            (Feature::CronJobs, Platform::Cloudflare) => Some("[triggers] crons"),
            // The packagers that do not yet emit trigger declarations or
            // event wrappers. Product availability is not artifact coverage.
            (Feature::CronJobs, Platform::Deno) => None,
            (Feature::CronJobs, Platform::Tencent) => None,
            // A bundle carries `cron`; the control plane keeps one schedule per
            // (app_id, environment) and calls a node's `Trigger.OnScheduled`
            // when it is due, which runs the app's `scheduled` export.
            (Feature::CronJobs, Platform::Deislet) => {
                Some("Scheduled by deis-control, run on one node")
            }

            // The binding plan maps a named `queue` binding onto a queue
            // producer and each declared consumer onto a `[[queues.consumers]]`
            // entry; the queue is named, and the driver provisions it.
            (Feature::Queues, Platform::Cloudflare) => {
                Some("Cloudflare Queues producers/consumers from the binding plan")
            }
            (Feature::Queues, Platform::Deno) => None,
            (Feature::Queues, Platform::Tencent) => None,
            // deis-queue is its own process; a declared queue binding's `send`
            // reaches it through the client deis-runtime builds, and a node
            // polls it for every queue the manifest declares a consumer for.
            (Feature::Queues, Platform::Deislet) => Some("deis-queue, consumed by polling"),
        }
    }

    /// Whether Deislet's current packager has a mapping for this target.
    pub fn has_target_implementation(&self, platform: Platform) -> bool {
        self.target_implementation_name(platform).is_some()
    }
}

/// Coverage of one Deislet target packager's currently implemented mappings.
pub struct TargetArtifactCoverage {
    platform: Platform,
    implemented_features: HashSet<Feature>,
}

impl TargetArtifactCoverage {
    pub fn new(platform: Platform) -> Self {
        let implemented_features = ALL_FEATURES
            .iter()
            .filter(|f| f.has_target_implementation(platform))
            .copied()
            .collect();

        Self {
            platform,
            implemented_features,
        }
    }

    pub fn platform(&self) -> Platform {
        self.platform
    }

    pub fn implements(&self, feature: Feature) -> bool {
        self.implemented_features.contains(&feature)
    }

    pub fn check_feature(&self, feature: Feature) -> Result<(), MissingTargetImplementationError> {
        if self.implements(feature) {
            Ok(())
        } else {
            Err(MissingTargetImplementationError {
                feature,
                platform: self.platform,
            })
        }
    }

    pub fn missing(&self, features: &[Feature]) -> Vec<MissingTargetImplementationError> {
        features
            .iter()
            .filter(|feature| !self.implements(**feature))
            .map(|feature| MissingTargetImplementationError {
                feature: *feature,
                platform: self.platform,
            })
            .collect()
    }

    pub fn check_features(
        &self,
        features: &[Feature],
    ) -> Result<(), Vec<MissingTargetImplementationError>> {
        let errors = self.missing(features);

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    pub fn list_implemented(&self) -> Vec<Feature> {
        self.implemented_features.iter().copied().collect()
    }

    pub fn list_missing(&self) -> Vec<Feature> {
        ALL_FEATURES
            .iter()
            .filter(|f| !self.implemented_features.contains(f))
            .copied()
            .collect()
    }
}

/// Deislet's current packager has no mapping for this target and feature.
#[derive(Debug, Clone)]
pub struct MissingTargetImplementationError {
    pub feature: Feature,
    pub platform: Platform,
}

impl MissingTargetImplementationError {
    pub fn detailed_message(&self) -> String {
        format!(
            "Deislet's {} target implementation cannot encode feature '{}'\n\
             Description: {}\n\
             This is an artifact-coverage result, not a vendor capability, conformance, or support decision.",
            self.platform.display_name(),
            self.feature.name(),
            self.feature.description()
        )
    }
}

impl std::fmt::Display for MissingTargetImplementationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Deislet's {} target implementation cannot encode {}",
            self.platform.display_name(),
            self.feature.name()
        )
    }
}

impl std::error::Error for MissingTargetImplementationError {}

/// All transitional project features considered by target packagers.
const ALL_FEATURES: &[Feature] = &[
    Feature::SQLDatabase,
    Feature::ObjectStorage,
    Feature::NodeRuntime,
    Feature::CronJobs,
    Feature::Queues,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloudflare_packager_coverage_is_explicit() {
        let caps = TargetArtifactCoverage::new(Platform::Cloudflare);

        // Only mappings the current packager actually writes. The four
        // services are mapped through the binding plan; each provider
        // identifier in the emitted wrangler.toml is the plan's placeholder,
        // resolved by the driver at prepare time.
        assert!(caps.implements(Feature::SQLDatabase));
        assert!(caps.implements(Feature::NodeRuntime));
        assert!(caps.implements(Feature::ObjectStorage));
        assert!(caps.implements(Feature::CronJobs));
        assert!(caps.implements(Feature::Queues));
    }

    #[test]
    fn deno_packager_coverage_is_explicit() {
        let caps = TargetArtifactCoverage::new(Platform::Deno);

        // Extended features
        assert!(caps.implements(Feature::NodeRuntime));
        assert!(!caps.implements(Feature::SQLDatabase));
        assert!(!caps.implements(Feature::CronJobs));
        assert!(!caps.implements(Feature::ObjectStorage));
        assert!(!caps.implements(Feature::Queues));
    }

    #[test]
    fn tencent_packager_coverage_is_explicit() {
        let caps = TargetArtifactCoverage::new(Platform::Tencent);

        // Extended features
        assert!(caps.implements(Feature::NodeRuntime));

        assert!(!caps.implements(Feature::SQLDatabase));
        assert!(!caps.implements(Feature::ObjectStorage));
        assert!(!caps.implements(Feature::CronJobs));
        assert!(!caps.implements(Feature::Queues));
    }

    /// This fails when the Deislet packager changes without updating its
    /// implementation inventory. It says nothing about certification.
    #[test]
    fn deislet_packager_coverage_is_explicit() {
        let caps = TargetArtifactCoverage::new(Platform::Deislet);

        // The four services wave B finished: deis-store serves per-app SQL
        // and blobs, deis-queue serves messages and the node polls them, and
        // deis-control schedules cron onto one node's Trigger/OnScheduled.
        // Each of them is exercised end to end by scripts/demo.sh.
        assert!(caps.implements(Feature::SQLDatabase));
        assert!(caps.implements(Feature::ObjectStorage));
        assert!(caps.implements(Feature::Queues));
        assert!(caps.implements(Feature::CronJobs));

        assert!(caps.implements(Feature::NodeRuntime));
    }

    #[test]
    fn missing_mappings_are_hard_build_failures() {
        let caps = TargetArtifactCoverage::new(Platform::Tencent);
        let blocking = caps.missing(ALL_FEATURES);
        assert!(
            blocking.iter().any(|b| b.feature == Feature::Queues),
            "a declared queue has to be a hard stop when no target mapping exists"
        );
    }

    #[test]
    fn missing_mapping_error_names_implementation_boundary() {
        let error = MissingTargetImplementationError {
            feature: Feature::SQLDatabase,
            platform: Platform::Tencent,
        };

        let msg = error.detailed_message();
        assert!(msg.contains("SQL Database"));
        assert!(msg.contains("EdgeOne Pages"));
        assert!(msg.contains("artifact-coverage"));
        assert!(msg.contains("not a vendor capability"));
    }

    #[test]
    fn test_check_features() {
        let caps = TargetArtifactCoverage::new(Platform::Tencent);

        // These features have no current Tencent artifact mapping.
        let features = vec![
            Feature::SQLDatabase,
            Feature::ObjectStorage,
            Feature::CronJobs,
        ];

        let result = caps.check_features(&features);
        assert!(result.is_err());

        let errors = result.unwrap_err();
        assert_eq!(errors.len(), 3);
    }
}
