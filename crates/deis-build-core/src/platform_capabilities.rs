/**
 * Platform Capabilities Matrix
 *
 * Defines what features are supported on each platform.
 * Used for compile-time validation to prevent generating
 * incompatible code.
 */
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Platform identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Cloudflare,
    Deno,
    Tencent,
    /// The platform this toolchain is part of: deis-runtime executing a bundle
    /// produced by `deis build --vendor deislet`.
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

/// Extended features that might not be supported on all platforms
///
/// Note: KV Storage and Cache API are NOT included here as they are
/// Basic Features that all Edge Canon compliant platforms must support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feature {
    // Storage
    SQLDatabase,
    ObjectStorage,

    // Runtime
    NodeRuntime,
    SSR,
    ISR,

    // Scheduling
    CronJobs,

    // Network
    WebSockets,

    // Services
    DurableObjects,
    Queues,
    WorkersAI,
    BroadcastChannel,
}

impl Feature {
    pub fn name(&self) -> &'static str {
        match self {
            Feature::SQLDatabase => "SQL Database",
            Feature::ObjectStorage => "Object Storage",
            Feature::NodeRuntime => "Node.js Runtime",
            Feature::SSR => "Server-Side Rendering",
            Feature::ISR => "Incremental Static Regeneration",
            Feature::CronJobs => "Cron Jobs / Scheduled Tasks",
            Feature::WebSockets => "WebSockets",
            Feature::DurableObjects => "Durable Objects",
            Feature::Queues => "Message Queues",
            Feature::WorkersAI => "Workers AI",
            Feature::BroadcastChannel => "BroadcastChannel API",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Feature::SQLDatabase => "SQL database for relational data",
            Feature::ObjectStorage => "S3-compatible object storage for large files",
            Feature::NodeRuntime => "Node.js built-in modules and npm packages",
            Feature::SSR => "Server-side rendering for dynamic content",
            Feature::ISR => "Incremental static regeneration with revalidation",
            Feature::CronJobs => "Scheduled background tasks",
            Feature::WebSockets => "WebSocket connections for real-time communication",
            Feature::DurableObjects => "Stateful objects with strong consistency",
            Feature::Queues => "Guaranteed message delivery queues",
            Feature::WorkersAI => "ML model inference at the edge",
            Feature::BroadcastChannel => "Cross-instance message broadcasting",
        }
    }

    /// Get the platform-specific implementation name
    pub fn platform_name(&self, platform: Platform) -> Option<&'static str> {
        match (self, platform) {
            (Feature::SQLDatabase, Platform::Cloudflare) => Some("D1 Database"),
            (Feature::SQLDatabase, Platform::Deno) => Some("Postgres (via integration)"),
            (Feature::SQLDatabase, Platform::Tencent) => None,
            // deis-store serves per-app SQLite over gRPC, but deis-runtime never
            // constructs a client for it, so every `context.services.database`
            // call fails. See `pending_on`.
            (Feature::SQLDatabase, Platform::Deislet) => Some("Per-app SQLite in deis-store"),

            (Feature::ObjectStorage, Platform::Cloudflare) => Some("R2 Buckets"),
            (Feature::ObjectStorage, Platform::Deno) => None,
            (Feature::ObjectStorage, Platform::Tencent) => None,
            // Same shape as the database: deis-store has the blob service, the
            // isolate has no client. See `pending_on`.
            (Feature::ObjectStorage, Platform::Deislet) => {
                Some("Content-addressed blobs in deis-store")
            }

            (Feature::NodeRuntime, Platform::Cloudflare) => None,
            (Feature::NodeRuntime, Platform::Deno) => Some("Node.js compatibility layer"),
            (Feature::NodeRuntime, Platform::Tencent) => Some("Node Functions"),
            // deis-runtime is bare deno_core: no node built-ins, no npm loader.
            (Feature::NodeRuntime, Platform::Deislet) => None,

            (Feature::SSR, Platform::Cloudflare) => None,
            (Feature::SSR, Platform::Deno) => Some("Native SSR support"),
            (Feature::SSR, Platform::Tencent) => Some("Next.js/Framework SSR"),
            // A handler may render whatever it likes, but there is no framework
            // integration, which is what this feature means.
            (Feature::SSR, Platform::Deislet) => None,

            (Feature::ISR, Platform::Cloudflare) => None,
            (Feature::ISR, Platform::Deno) => Some("ISR with revalidation"),
            (Feature::ISR, Platform::Tencent) => Some("Framework ISR"),
            (Feature::ISR, Platform::Deislet) => None,

            (Feature::CronJobs, Platform::Cloudflare) => Some("Cron Triggers"),
            (Feature::CronJobs, Platform::Deno) => Some("Deno.cron"),
            (Feature::CronJobs, Platform::Tencent) => None,
            // A bundle carries `cron`, and deis-control has a scheduler, but the
            // node's `Trigger.OnScheduled` is still a stub. See `pending_on`.
            (Feature::CronJobs, Platform::Deislet) => {
                Some("Scheduled by deis-control, run on one node")
            }

            (Feature::WebSockets, Platform::Cloudflare) => Some("WebSocket API"),
            (Feature::WebSockets, Platform::Deno) => Some("WebSocket API"),
            (Feature::WebSockets, Platform::Tencent) => None,
            // The one extended feature that really works today: deis-runtime
            // bridges server-side sockets into the isolate.
            (Feature::WebSockets, Platform::Deislet) => Some("WebSocket bridge in deis-runtime"),

            (Feature::DurableObjects, Platform::Cloudflare) => Some("Durable Objects"),
            (Feature::DurableObjects, Platform::Deno) => None,
            (Feature::DurableObjects, Platform::Tencent) => None,
            // Nothing in the platform gives an app a single addressable
            // instance; every node runs every app (architecture principle 2).
            (Feature::DurableObjects, Platform::Deislet) => None,

            (Feature::Queues, Platform::Cloudflare) => Some("Queues"),
            (Feature::Queues, Platform::Deno) => None,
            (Feature::Queues, Platform::Tencent) => None,
            // deis-queue does not exist yet and the node's `Trigger.OnQueue` is
            // a stub; `context.services.queue.send` lands in a per-isolate list
            // that is dropped with the isolate. See `pending_on`.
            (Feature::Queues, Platform::Deislet) => Some("deis-queue, consumed by polling"),

            (Feature::WorkersAI, Platform::Cloudflare) => Some("Workers AI"),
            (Feature::WorkersAI, Platform::Deno) => None,
            (Feature::WorkersAI, Platform::Tencent) => None,
            (Feature::WorkersAI, Platform::Deislet) => None,

            (Feature::BroadcastChannel, Platform::Cloudflare) => None,
            (Feature::BroadcastChannel, Platform::Deno) => Some("BroadcastChannel"),
            (Feature::BroadcastChannel, Platform::Tencent) => None,
            // Isolates do not share a bus, and there is no cross-node fan-out.
            (Feature::BroadcastChannel, Platform::Deislet) => None,
        }
    }

    /// Check if feature is supported on platform
    pub fn is_supported(&self, platform: Platform) -> bool {
        self.platform_name(platform).is_some()
    }

    /// Why an unsupported feature is only *pending*, if it is.
    ///
    /// There are two different kinds of "not supported", and treating them
    /// alike serves nobody. EdgeOne is never going to grow D1, so an app that
    /// needs a SQL database must not be built for it. Deislet, by contrast,
    /// has the server half of several services already running and only lacks
    /// the wiring on the node - an app that declares one is not a mistake, it
    /// is early. So a pending feature warns and keeps the declaration in the
    /// bundle manifest, where the control plane records it, instead of
    /// stopping the build.
    ///
    /// The returned note says what actually happens today, because "not
    /// supported yet" on its own leaves the developer guessing whether their
    /// call throws, hangs or silently no-ops.
    pub fn pending_on(&self, platform: Platform) -> Option<&'static str> {
        if self.is_supported(platform) {
            return None;
        }

        // Nothing is pending today. Wave B finished the four services that
        // were: deis-store hands SQL and blob clients to the isolate,
        // deis-queue is built and polled, and the node's scheduled trigger
        // runs the app's handler instead of returning success without doing
        // anything. What is left unsupported on Deislet is unsupported by
        // shape - there are no Durable Objects to have, no Node built-ins, no
        // framework hook for SSR - so it stops a build rather than warning.
        //
        // The mechanism stays because the next half-built service will want
        // it; an empty match is the honest answer, not a missing case.
        let _ = platform;
        None
    }

    /// Get alternative solutions when feature is not supported
    pub fn alternatives(&self, platform: Platform) -> Vec<&'static str> {
        if self.is_supported(platform) {
            return vec![];
        }

        match (self, platform) {
            (Feature::SQLDatabase, Platform::Tencent) => vec![
                "Use external database service (Postgres, MySQL)",
                "Use EdgeKV for simple key-value data",
                "Deploy to Cloudflare Workers (D1) or Deno Deploy (Postgres)",
            ],

            (Feature::ObjectStorage, Platform::Deno) => vec![
                "Use external S3-compatible service",
                "Deploy to Cloudflare Workers (R2)",
            ],

            (Feature::ObjectStorage, Platform::Tencent) => vec![
                "Use external S3-compatible service",
                "Deploy to Cloudflare Workers (R2)",
            ],

            (Feature::NodeRuntime, Platform::Cloudflare) => vec![
                "Use nodejs_compat compatibility flag (limited support)",
                "Deploy to Deno Deploy (Node.js compatibility layer)",
                "Deploy to EdgeOne Pages (Node Functions)",
            ],

            (Feature::SSR, Platform::Cloudflare) => vec![
                "Use static site generation (SSG) instead",
                "Deploy to Deno Deploy or EdgeOne Pages for SSR",
            ],

            (Feature::ISR, Platform::Cloudflare) => vec![
                "Use static site generation (SSG) with cache headers",
                "Deploy to Deno Deploy or EdgeOne Pages for ISR",
            ],

            (Feature::CronJobs, Platform::Tencent) => vec![
                "Use external cron service (GitHub Actions, etc.)",
                "Deploy to Cloudflare Workers (Cron Triggers)",
                "Deploy to Deno Deploy (Deno.cron)",
            ],

            (Feature::WebSockets, Platform::Tencent) => vec![
                "Use Server-Sent Events (SSE) for one-way communication",
                "Deploy to Cloudflare Workers or Deno Deploy for WebSocket support",
            ],

            // Deislet's own gaps. The advice is deliberately about this
            // platform's own roadmap: telling someone to go deploy their
            // internal app to Cloudflare is not an answer here.
            (Feature::NodeRuntime, Platform::Deislet) => vec![
                "Use Web APIs only; deis-build bundles npm dependencies that stick to them",
                "Move Node-only work out of the handler and behind an HTTP call",
            ],

            (Feature::SSR, Platform::Deislet) => vec![
                "Render in the handler and return a Response; no framework hook exists",
                "Pre-render at build time and ship the result as bundle assets",
            ],

            (Feature::ISR, Platform::Deislet) => vec![
                "Serve from bundle assets and redeploy to refresh",
                "Cache rendered pages in context.services.kv with your own revalidation",
            ],

            (Feature::DurableObjects, Platform::Deislet) => vec![
                "Every node runs every app, so there is no single addressable instance",
                "Keep shared state in deis-store rather than in an isolate",
            ],

            (Feature::WorkersAI, Platform::Deislet) => {
                vec!["Call an external inference API from the handler"]
            }

            (Feature::BroadcastChannel, Platform::Deislet) => {
                vec!["Isolates share no bus; use a queue or KV for cross-instance messages"]
            }

            (Feature::DurableObjects, _) => vec![
                "Use KV storage for simple state",
                "Use external database for stateful data",
                "Deploy to Cloudflare Workers for Durable Objects support",
            ],

            (Feature::Queues, _) => vec![
                "Use external queue service (Redis, RabbitMQ)",
                "Deploy to Cloudflare Workers for native Queues",
            ],

            (Feature::WorkersAI, _) => vec![
                "Use external AI API (OpenAI, Anthropic, etc.)",
                "Deploy to Cloudflare Workers for Workers AI",
            ],

            (Feature::BroadcastChannel, _) => vec![
                "Use external pub/sub service (Redis, Pusher)",
                "Deploy to Deno Deploy for BroadcastChannel support",
            ],

            _ => vec![],
        }
    }
}

/// Platform capabilities checker
pub struct PlatformCapabilities {
    platform: Platform,
    supported_features: HashSet<Feature>,
}

impl PlatformCapabilities {
    pub fn new(platform: Platform) -> Self {
        let supported_features = ALL_FEATURES
            .iter()
            .filter(|f| f.is_supported(platform))
            .copied()
            .collect();

        Self {
            platform,
            supported_features,
        }
    }

    pub fn platform(&self) -> Platform {
        self.platform
    }

    pub fn supports(&self, feature: Feature) -> bool {
        self.supported_features.contains(&feature)
    }

    pub fn check_feature(&self, feature: Feature) -> Result<(), UnsupportedFeatureError> {
        if self.supports(feature) {
            Ok(())
        } else {
            Err(UnsupportedFeatureError {
                feature,
                platform: self.platform,
            })
        }
    }

    /// What this platform is missing, split by whether it should stop a build.
    ///
    /// The first list is a hard stop: the platform will not grow the feature,
    /// so code that needs it can only fail after deployment. The second is a
    /// warning: the platform is specified to have it and does not yet, so the
    /// declaration is worth carrying in the bundle even though nothing serves
    /// it today.
    pub fn partition_unsupported(
        &self,
        features: &[Feature],
    ) -> (Vec<UnsupportedFeatureError>, Vec<PendingFeature>) {
        let mut blocking = Vec::new();
        let mut pending = Vec::new();

        for feature in features.iter().filter(|f| !self.supports(**f)) {
            match feature.pending_on(self.platform) {
                Some(note) => pending.push(PendingFeature {
                    feature: *feature,
                    platform: self.platform,
                    note,
                }),
                None => blocking.push(UnsupportedFeatureError {
                    feature: *feature,
                    platform: self.platform,
                }),
            }
        }

        (blocking, pending)
    }

    pub fn check_features(&self, features: &[Feature]) -> Result<(), Vec<UnsupportedFeatureError>> {
        let errors: Vec<_> = features
            .iter()
            .filter(|f| !self.supports(**f))
            .map(|f| UnsupportedFeatureError {
                feature: *f,
                platform: self.platform,
            })
            .collect();

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    pub fn list_supported(&self) -> Vec<Feature> {
        self.supported_features.iter().copied().collect()
    }

    pub fn list_unsupported(&self) -> Vec<Feature> {
        ALL_FEATURES
            .iter()
            .filter(|f| !self.supported_features.contains(f))
            .copied()
            .collect()
    }
}

/// A feature the target platform is specified to have but has not wired up.
///
/// Not an error: the build goes on and the bundle keeps the declaration. It
/// exists so the person building can be told, once, in terms of what breaks.
#[derive(Debug, Clone)]
pub struct PendingFeature {
    pub feature: Feature,
    pub platform: Platform,
    /// What happens today when the app uses it.
    pub note: &'static str,
}

impl std::fmt::Display for PendingFeature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is declared but {} does not serve it yet: {}",
            self.feature.name(),
            self.platform.display_name(),
            self.note
        )
    }
}

/// Error when a feature is not supported on target platform
#[derive(Debug, Clone)]
pub struct UnsupportedFeatureError {
    pub feature: Feature,
    pub platform: Platform,
}

impl UnsupportedFeatureError {
    pub fn detailed_message(&self) -> String {
        let alternatives = self.feature.alternatives(self.platform);
        let alt_text = if alternatives.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nAlternatives:\n{}",
                alternatives
                    .iter()
                    .enumerate()
                    .map(|(i, alt)| format!("  {}. {}", i + 1, alt))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };

        format!(
            "Feature '{}' is not supported on {}\n\
             Description: {}\n\
             Platform: {} does not provide this capability{}",
            self.feature.name(),
            self.platform.display_name(),
            self.feature.description(),
            self.platform.display_name(),
            alt_text
        )
    }
}

impl std::fmt::Display for UnsupportedFeatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is not supported on {}",
            self.feature.name(),
            self.platform.display_name()
        )
    }
}

impl std::error::Error for UnsupportedFeatureError {}

/// All extended features across all platforms
///
/// Note: This only includes extended features. Basic features (KV Storage, Cache API)
/// are not listed here as they are guaranteed to be available on all platforms.
const ALL_FEATURES: &[Feature] = &[
    Feature::SQLDatabase,
    Feature::ObjectStorage,
    Feature::NodeRuntime,
    Feature::SSR,
    Feature::ISR,
    Feature::CronJobs,
    Feature::WebSockets,
    Feature::DurableObjects,
    Feature::Queues,
    Feature::WorkersAI,
    Feature::BroadcastChannel,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cloudflare_capabilities() {
        let caps = PlatformCapabilities::new(Platform::Cloudflare);

        // Extended features
        assert!(caps.supports(Feature::SQLDatabase));
        assert!(caps.supports(Feature::ObjectStorage));
        assert!(caps.supports(Feature::CronJobs));
        assert!(caps.supports(Feature::WebSockets));
        assert!(caps.supports(Feature::DurableObjects));
        assert!(caps.supports(Feature::Queues));
        assert!(caps.supports(Feature::WorkersAI));

        assert!(!caps.supports(Feature::NodeRuntime));
        assert!(!caps.supports(Feature::SSR));
        assert!(!caps.supports(Feature::BroadcastChannel));
    }

    #[test]
    fn test_deno_capabilities() {
        let caps = PlatformCapabilities::new(Platform::Deno);

        // Extended features
        assert!(caps.supports(Feature::SQLDatabase));
        assert!(caps.supports(Feature::NodeRuntime));
        assert!(caps.supports(Feature::SSR));
        assert!(caps.supports(Feature::ISR));
        assert!(caps.supports(Feature::CronJobs));
        assert!(caps.supports(Feature::WebSockets));
        assert!(caps.supports(Feature::BroadcastChannel));

        assert!(!caps.supports(Feature::ObjectStorage));
        assert!(!caps.supports(Feature::DurableObjects));
        assert!(!caps.supports(Feature::Queues));
    }

    #[test]
    fn test_tencent_capabilities() {
        let caps = PlatformCapabilities::new(Platform::Tencent);

        // Extended features
        assert!(caps.supports(Feature::NodeRuntime));
        assert!(caps.supports(Feature::SSR));
        assert!(caps.supports(Feature::ISR));

        assert!(!caps.supports(Feature::SQLDatabase));
        assert!(!caps.supports(Feature::ObjectStorage));
        assert!(!caps.supports(Feature::CronJobs));
        assert!(!caps.supports(Feature::WebSockets));
        assert!(!caps.supports(Feature::Queues));
    }

    /// The point of this test is that it should FAIL when deis-runtime grows a
    /// capability and nobody updates the matrix. Every assertion here is a
    /// statement about code that exists today, not about what is planned.
    #[test]
    fn test_deislet_capabilities() {
        let caps = PlatformCapabilities::new(Platform::Deislet);

        // deis-runtime bridges server-side sockets into the isolate today.
        assert!(caps.supports(Feature::WebSockets));

        // The four services wave B finished: deis-store serves per-app SQL
        // and blobs, deis-queue serves messages and the node polls them, and
        // deis-control schedules cron onto one node's Trigger/OnScheduled.
        // Each of them is exercised end to end by scripts/demo.sh.
        assert!(caps.supports(Feature::SQLDatabase));
        assert!(caps.supports(Feature::ObjectStorage));
        assert!(caps.supports(Feature::Queues));
        assert!(caps.supports(Feature::CronJobs));

        // These are shapes this platform does not have, not work in progress.
        assert!(!caps.supports(Feature::NodeRuntime));
        assert!(!caps.supports(Feature::SSR));
        assert!(!caps.supports(Feature::ISR));
        assert!(!caps.supports(Feature::DurableObjects));
        assert!(!caps.supports(Feature::WorkersAI));
        assert!(!caps.supports(Feature::BroadcastChannel));
    }

    #[test]
    fn test_deislet_has_nothing_half_built_left() {
        // "Pending" means the platform intends to have this and does not yet,
        // so a build warns and keeps the declaration. Wave B closed the four
        // that were pending; what remains unsupported is unsupported by shape
        // and must stop a build instead of warning, or an app ships believing
        // in a service that will never answer.
        let caps = PlatformCapabilities::new(Platform::Deislet);

        let (blocking, pending) = caps.partition_unsupported(ALL_FEATURES);

        assert!(
            pending.is_empty(),
            "still pending: {:?}",
            pending.iter().map(|p| p.feature).collect::<Vec<_>>()
        );
        assert!(
            blocking
                .iter()
                .any(|b| b.feature == Feature::DurableObjects),
            "Durable Objects has to be a hard stop"
        );
    }

    #[test]
    fn test_other_platforms_have_nothing_pending() {
        // "Pending" is a claim about this platform's own roadmap. Making it
        // about a third party's would be a guess.
        for platform in [
            Platform::Cloudflare,
            Platform::Deno,
            Platform::Tencent,
            Platform::Deislet,
        ] {
            let caps = PlatformCapabilities::new(platform);
            let (_, pending) = caps.partition_unsupported(ALL_FEATURES);
            assert!(
                pending.is_empty(),
                "{} has no pending features",
                platform.as_str()
            );
        }
    }

    #[test]
    fn test_unsupported_feature_error() {
        let error = UnsupportedFeatureError {
            feature: Feature::SQLDatabase,
            platform: Platform::Tencent,
        };

        let msg = error.detailed_message();
        assert!(msg.contains("SQL Database"));
        assert!(msg.contains("EdgeOne Pages"));
        assert!(msg.contains("Alternatives"));
    }

    #[test]
    fn test_check_features() {
        let caps = PlatformCapabilities::new(Platform::Tencent);

        // Test with features that Tencent doesn't support
        let features = vec![Feature::SQLDatabase, Feature::WebSockets, Feature::CronJobs];

        let result = caps.check_features(&features);
        assert!(result.is_err());

        let errors = result.unwrap_err();
        assert_eq!(errors.len(), 3); // SQL, WebSockets, and CronJobs not supported on Tencent
    }
}
