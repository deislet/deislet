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
}

impl Platform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Cloudflare => "cloudflare",
            Platform::Deno => "deno",
            Platform::Tencent => "tencent",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Platform::Cloudflare => "Cloudflare Workers",
            Platform::Deno => "Deno Deploy",
            Platform::Tencent => "EdgeOne Pages",
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

            (Feature::ObjectStorage, Platform::Cloudflare) => Some("R2 Buckets"),
            (Feature::ObjectStorage, Platform::Deno) => None,
            (Feature::ObjectStorage, Platform::Tencent) => None,

            (Feature::NodeRuntime, Platform::Cloudflare) => None,
            (Feature::NodeRuntime, Platform::Deno) => Some("Node.js compatibility layer"),
            (Feature::NodeRuntime, Platform::Tencent) => Some("Node Functions"),

            (Feature::SSR, Platform::Cloudflare) => None,
            (Feature::SSR, Platform::Deno) => Some("Native SSR support"),
            (Feature::SSR, Platform::Tencent) => Some("Next.js/Framework SSR"),

            (Feature::ISR, Platform::Cloudflare) => None,
            (Feature::ISR, Platform::Deno) => Some("ISR with revalidation"),
            (Feature::ISR, Platform::Tencent) => Some("Framework ISR"),

            (Feature::CronJobs, Platform::Cloudflare) => Some("Cron Triggers"),
            (Feature::CronJobs, Platform::Deno) => Some("Deno.cron"),
            (Feature::CronJobs, Platform::Tencent) => None,

            (Feature::WebSockets, Platform::Cloudflare) => Some("WebSocket API"),
            (Feature::WebSockets, Platform::Deno) => Some("WebSocket API"),
            (Feature::WebSockets, Platform::Tencent) => None,

            (Feature::DurableObjects, Platform::Cloudflare) => Some("Durable Objects"),
            (Feature::DurableObjects, Platform::Deno) => None,
            (Feature::DurableObjects, Platform::Tencent) => None,

            (Feature::Queues, Platform::Cloudflare) => Some("Queues"),
            (Feature::Queues, Platform::Deno) => None,
            (Feature::Queues, Platform::Tencent) => None,

            (Feature::WorkersAI, Platform::Cloudflare) => Some("Workers AI"),
            (Feature::WorkersAI, Platform::Deno) => None,
            (Feature::WorkersAI, Platform::Tencent) => None,

            (Feature::BroadcastChannel, Platform::Cloudflare) => None,
            (Feature::BroadcastChannel, Platform::Deno) => Some("BroadcastChannel"),
            (Feature::BroadcastChannel, Platform::Tencent) => None,
        }
    }

    /// Check if feature is supported on platform
    pub fn is_supported(&self, platform: Platform) -> bool {
        self.platform_name(platform).is_some()
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
