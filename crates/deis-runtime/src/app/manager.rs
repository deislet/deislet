// Application manager

use super::key::AppKey;
use super::types::{App, AppConfig, AppState, ResourceLimits};
use crate::app_repo::AppRepo;
use crate::assets::AssetBundle;
use crate::config::ExecutionLimits;
use crate::runtime::{FetchPolicy, IsolateSpec, RuntimePool};
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use deno_core::OpMetricsSummary;
use std::sync::Arc;
use std::time::SystemTime;

/// One deployed version of one app, as this node currently runs it.
///
/// Held next to the isolate rather than inside it for two reasons: the HTTP
/// entry needs the asset index on every request and must not reopen the zip
/// for it, and a version that fails to load needs something to fall back to.
pub struct VersionRecord {
    pub version_id: String,
    /// Whether the `env` this copy is running with came from the control
    /// plane.
    ///
    /// False means the version was loaded from this node's own disk, where
    /// secret *values* are deliberately never written: the code is the right
    /// code but every secret the app declares is missing. Two copies of one
    /// version are therefore not interchangeable, and this is what stops a
    /// later delivery of the same version id from being waved through as
    /// "already running" - see `DeploymentManager::apply_from_assignment`.
    pub env_confirmed: bool,
    pub entry_point: String,
    /// Bundled module source. Kept because an isolate is built from it, and
    /// a version change builds a second one while this record is still the
    /// one serving; without it the code would have to be fetched again to
    /// stage a replacement.
    code: String,
    config: AppConfig,
    pub assets: Arc<AssetBundle>,
}

impl std::fmt::Debug for VersionRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VersionRecord")
            .field("version_id", &self.version_id)
            .field("env_confirmed", &self.env_confirmed)
            .field("entry_point", &self.entry_point)
            .field("assets", &self.assets)
            .finish()
    }
}

/// Everything needed to put one version of one app on this node.
pub struct VersionInstall {
    /// Which application *and which environment of it*: the two are one key,
    /// so a preview cannot land in production's slot.
    pub key: AppKey,
    pub version_id: String,
    pub entry_point: String,
    pub code: String,
    /// Carries the decrypted `env`; see [`AppConfig`] for why it neither
    /// serialises nor prints its values.
    pub config: AppConfig,
    /// See [`VersionRecord::env_confirmed`]. False for a version recovered
    /// from local disk.
    pub env_confirmed: bool,
    pub assets: Arc<AssetBundle>,
}

/// What the node is running for one (app, environment) pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledVersion {
    pub version_id: String,
    /// See [`VersionRecord::env_confirmed`].
    pub env_confirmed: bool,
}

/// Why a scheduled run did not complete.
///
/// The two cases look the same from the outside and mean opposite things to
/// whoever fired the tick, which is why they are separate variants rather than
/// one error string: one asks for another node, the other asks for no node at
/// all. See `crate::trigger`.
#[derive(Debug)]
pub enum ScheduledFailure {
    /// This node never entered the application's code: it does not have this
    /// version of this environment loaded. Another node can still run the
    /// tick, and nothing has happened here that a second attempt would repeat.
    NotRunnableHere(String),
    /// The application's own scheduled handler was entered and did not
    /// finish - it threw, it timed out, or the bundle exports no scheduled
    /// handler at all. The same code on another node fails the same way, and
    /// whatever the handler did before it failed (a queue send, a write) has
    /// already happened once.
    HandlerFailed(String),
}

impl std::fmt::Display for ScheduledFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunnableHere(m) | Self::HandlerFailed(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ScheduledFailure {}

/// Application manager
pub struct ApplicationManager {
    /// Loaded applications, keyed by (app, environment). Both maps are keyed
    /// the same way on purpose: an application whose preview and production
    /// shared one entry would have one of them silently overwrite the other's
    /// version, bindings and secrets.
    apps: DashMap<AppKey, App>,
    /// Deployed versions. Only apps installed from a bundle appear here; the
    /// built-in `_system` app has no version.
    versions: DashMap<AppKey, Arc<VersionRecord>>,
    runtime_pool: Arc<RuntimePool>,
    /// Debug sessions: (app, environment) -> (token, expires_at)
    inspect_sessions: DashMap<AppKey, (String, SystemTime)>,
    // Platform-wide defaults for per-app execution limits (timeout, rate limit).
    // An app's own `ResourceLimits` may override either value; see
    // `effective_limits`.
    default_limits: ExecutionLimits,
    /// Where every isolate on this node may send an outbound `fetch()`. One
    /// policy for the node, because it is the node's network that is being
    /// protected, not any one application's. The default refuses everything
    /// that is not a public internet address.
    fetch_policy: FetchPolicy,
}

impl ApplicationManager {
    /// Create a new application manager
    pub fn new() -> Self {
        Self::with_threads(None)
    }

    pub fn new_with_repo(app_repo: Arc<AppRepo>) -> Self {
        Self::with_threads_and_repo(None, app_repo)
    }

    /// Create a new application manager (with specified thread count)
    pub fn with_threads(num_threads: Option<usize>) -> Self {
        Self {
            apps: DashMap::new(),
            versions: DashMap::new(),
            runtime_pool: Arc::new(RuntimePool::new(num_threads)),
            inspect_sessions: DashMap::new(),
            default_limits: ExecutionLimits::default(),
            fetch_policy: FetchPolicy::default(),
        }
    }

    pub fn with_threads_and_repo(num_threads: Option<usize>, app_repo: Arc<AppRepo>) -> Self {
        Self {
            apps: DashMap::new(),
            versions: DashMap::new(),
            runtime_pool: Arc::new(RuntimePool::new_with_repo(num_threads, Some(app_repo))),
            inspect_sessions: DashMap::new(),
            default_limits: ExecutionLimits::default(),
            fetch_policy: FetchPolicy::default(),
        }
    }

    /// Override the platform-wide default execution limits (timeout, rate
    /// limit) used for apps that do not configure their own. Intended to be
    /// called once at startup with the values loaded from `RuntimeConfig`.
    pub fn with_limits(mut self, default_limits: ExecutionLimits) -> Self {
        self.default_limits = default_limits;
        self
    }

    /// Set the node's outbound-fetch policy. Intended to be called once at
    /// startup with the operator's `[fetch] allow` list and the addresses of
    /// this node's own platform services.
    ///
    /// Leaving it unset is not a hole: the default policy is the strictest
    /// one, refusing every destination that is not a public address.
    pub fn with_fetch_policy(mut self, policy: FetchPolicy) -> Self {
        self.fetch_policy = policy;
        self
    }

    /// Resolve the effective execution limits for an app: its own
    /// `ResourceLimits` override where present, the platform default
    /// otherwise. Logs at info level whenever an app overrides a default, so
    /// operators can see which apps deviate from the platform baseline.
    fn effective_limits(&self, key: &AppKey, limits: Option<&ResourceLimits>) -> ExecutionLimits {
        let timeout_override = limits.and_then(|l| l.timeout_ms);
        let timeout_ms = timeout_override.unwrap_or(self.default_limits.timeout_ms);
        if let Some(overridden) = timeout_override {
            tracing::info!(
                target: "app",
                app_id = %key.app_id(),
                environment = %key.environment(),
                "App {} overrides default execution timeout: {}ms (platform default {}ms)",
                key,
                overridden,
                self.default_limits.timeout_ms
            );
        }

        let rate_limit_override = limits.and_then(|l| l.rate_limit_ops_per_sec);
        let rate_limit_ops_per_sec =
            rate_limit_override.unwrap_or(self.default_limits.rate_limit_ops_per_sec);
        if let Some(overridden) = rate_limit_override {
            tracing::info!(
                target: "app",
                app_id = %key.app_id(),
                environment = %key.environment(),
                "App {} overrides default rate limit: {} ops/sec (platform default {} ops/sec)",
                key,
                overridden,
                self.default_limits.rate_limit_ops_per_sec
            );
        }

        ExecutionLimits {
            timeout_ms,
            // No per-app override: `ResourceLimits` has no key for the tail
            // budget, so every app on this node is held to the platform value.
            wait_until_timeout_ms: self.default_limits.wait_until_timeout_ms,
            rate_limit_ops_per_sec,
        }
    }

    /// Load application
    pub async fn load_app(
        &self,
        key: AppKey,
        code: String,
        config: AppConfig,
        entry_point: Option<String>,
    ) -> Result<()> {
        // Check if application already exists
        if self.apps.contains_key(&key) {
            return Err(anyhow!("App {} already exists", key));
        }

        tracing::info!("Loading app: {}", key);

        // Create application information
        let now = SystemTime::now();
        let mut app_info = App {
            id: key.app_id().to_string(),
            environment: key.environment().to_string(),
            state: AppState::Loading,
            config: config.clone(),
            created_at: now,
            updated_at: now,
            error: None,
        };

        // Load application in runtime pool
        let spec = IsolateSpec {
            entry_point,
            env: config.env.clone(),
            permissions: config.permissions.clone(),
            memory_limit_mb: config.limits.as_ref().map(|l| l.memory_mb),
            resource_table_max: config.limits.as_ref().and_then(|l| l.resource_table_max),
            route_patterns: config.routes.clone(),
            limits: self.effective_limits(&key, config.limits.as_ref()),
            fetch_policy: self.fetch_policy.clone(),
        };
        match self
            .runtime_pool
            .load_app(key.tenant_id(), code, spec)
            .await
        {
            Ok(()) => {
                app_info.state = AppState::Running;
                app_info.updated_at = SystemTime::now();
                self.apps.insert(key.clone(), app_info);
                tracing::info!("App {} loaded successfully", key);
                Ok(())
            }
            Err(e) => {
                app_info.state = AppState::Error;
                app_info.error = Some(e.to_string());
                app_info.updated_at = SystemTime::now();
                tracing::error!("Failed to load app {}: {}", key, e);
                Err(e)
            }
        }
    }

    /// Put one version of one app on this node, replacing whatever it was
    /// running.
    ///
    /// "Whatever it was running" is per environment: installing a preview
    /// leaves production exactly where it was, because the two are separate
    /// keys down to the isolate.
    ///
    /// The caller has already proven the bundle: digest checked, manifest
    /// parsed, entry point present, assets read. What is left is the one step
    /// that can still fail on code the node has never executed - creating the
    /// isolate - and that step happens *beside* the running version, not in
    /// place of it: the new isolate is built on a runtime thread of its own
    /// and only then made the one that serves
    /// ([`RuntimePool::stage_app`] followed by [`RuntimePool::activate`]).
    ///
    /// So a request during a version change gets the old version or the new
    /// one and never a gap, however long the new isolate takes to build. The
    /// cost is that the application briefly holds two threads and two
    /// isolates.
    ///
    /// A new version that will not build leaves the old one serving and
    /// untouched -- nothing was taken away, so there is nothing to put back.
    pub async fn install_version(&self, install: VersionInstall) -> Result<()> {
        let VersionInstall {
            key,
            version_id,
            entry_point,
            code,
            config,
            env_confirmed,
            assets,
        } = install;

        let record = Arc::new(VersionRecord {
            version_id: version_id.clone(),
            env_confirmed,
            entry_point: entry_point.clone(),
            code: code.clone(),
            config: config.clone(),
            assets: assets.clone(),
        });

        let previous = self.versions.get(&key).map(|entry| entry.value().clone());
        let spec = self.spec_for(&key, &record);

        let staged = match self
            .runtime_pool
            .stage_app(key.tenant_id(), record.code.clone(), spec)
            .await
        {
            Ok(staged) => staged,
            Err(e) => {
                match &previous {
                    Some(previous) => tracing::error!(
                        app_id = %key.app_id(),
                        environment = %key.environment(),
                        version_id = %version_id,
                        serving_version = %previous.version_id,
                        error = %e,
                        "Version {} of {} would not build; it was built beside the running \
                         isolate and never reached traffic, so {} keeps serving version {} and \
                         no request was affected. The deployment did not take",
                        version_id,
                        key,
                        key,
                        previous.version_id
                    ),
                    None => tracing::error!(
                        app_id = %key.app_id(),
                        environment = %key.environment(),
                        version_id = %version_id,
                        error = %e,
                        "Version {} of {} would not build and it had no earlier version to fall \
                         back to; it is not serving and requests for it will fail until a \
                         working version is deployed",
                        version_id,
                        key
                    ),
                }
                return Err(anyhow!(
                    "version {version_id} of {key} would not build: {e}"
                ));
            }
        };

        if let Err(e) = self.runtime_pool.activate(staged).await {
            // The only way here is the application having been unloaded while
            // the new isolate was being built, which discarded it.
            self.apps.remove(&key);
            self.versions.remove(&key);
            tracing::error!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                version_id = %version_id,
                error = %e,
                "Version {} of {} was built but could not be made the one that serves; {} is \
                 not running on this node",
                version_id,
                key,
                key
            );
            return Err(anyhow!(
                "version {version_id} of {key} could not be activated: {e}"
            ));
        }

        let now = SystemTime::now();
        self.apps.insert(
            key.clone(),
            App {
                id: key.app_id().to_string(),
                environment: key.environment().to_string(),
                state: AppState::Running,
                config,
                created_at: now,
                updated_at: now,
                error: None,
            },
        );
        self.versions.insert(key.clone(), record);
        tracing::info!(
            app_id = %key.app_id(),
            environment = %key.environment(),
            version_id = %version_id,
            replaced_version = previous.as_ref().map(|p| p.version_id.as_str()),
            env_confirmed,
            assets = assets.len(),
            asset_bytes = assets.total_bytes(),
            "App {} is now serving version {} of environment {}",
            key.app_id(),
            version_id,
            key.environment()
        );
        Ok(())
    }

    /// How one version's code is to be built into an isolate.
    fn spec_for(&self, key: &AppKey, record: &VersionRecord) -> IsolateSpec {
        IsolateSpec {
            entry_point: Some(record.entry_point.clone()),
            env: record.config.env.clone(),
            permissions: record.config.permissions.clone(),
            memory_limit_mb: record.config.limits.as_ref().map(|l| l.memory_mb),
            resource_table_max: record
                .config
                .limits
                .as_ref()
                .and_then(|l| l.resource_table_max),
            route_patterns: record.config.routes.clone(),
            limits: self.effective_limits(key, record.config.limits.as_ref()),
            fetch_policy: self.fetch_policy.clone(),
        }
    }

    /// The static files of the version currently running, for the HTTP entry
    /// to consult before anything reaches an isolate.
    pub fn assets(&self, key: &AppKey) -> Option<Arc<AssetBundle>> {
        self.versions.get(key).map(|e| e.assets.clone())
    }

    /// Which version this node runs for one environment of one app.
    pub fn installed_version(&self, key: &AppKey) -> Option<InstalledVersion> {
        self.versions.get(key).map(|e| InstalledVersion {
            version_id: e.version_id.clone(),
            env_confirmed: e.env_confirmed,
        })
    }

    /// Every (app, environment) with a deployed version, and what it runs.
    pub fn installed_versions(&self) -> Vec<(AppKey, InstalledVersion)> {
        self.versions
            .iter()
            .map(|e| {
                (
                    e.key().clone(),
                    InstalledVersion {
                        version_id: e.version_id.clone(),
                        env_confirmed: e.env_confirmed,
                    },
                )
            })
            .collect()
    }

    /// Every environment of one app that this node currently has loaded.
    ///
    /// Used by callers that only know the application - the management API
    /// unloads by app id, and the trigger service is told an app id with no
    /// environment - so they can say out loud which environments they mean
    /// instead of silently acting on one of them.
    pub fn environments_of(&self, app_id: &str) -> Vec<AppKey> {
        let mut keys: Vec<AppKey> = self
            .apps
            .iter()
            .map(|e| e.key().clone())
            .filter(|key| key.app_id() == app_id)
            .collect();
        keys.sort();
        keys
    }

    pub fn is_loaded(&self, key: &AppKey) -> bool {
        self.apps.contains_key(key)
    }

    /// Start debug session
    pub fn start_inspect(&self, key: AppKey, ttl_seconds: u64) -> Result<(String, String)> {
        // Check if application exists
        if !self.apps.contains_key(&key) {
            return Err(anyhow!("App {} not found", key));
        }

        // Generate token
        let token = uuid::Uuid::new_v4().to_string();
        let expires_at = SystemTime::now() + std::time::Duration::from_secs(ttl_seconds);

        let inspect_app_id = format!("_inspect:{}", key.app_id());

        // Store session
        self.inspect_sessions
            .insert(key.clone(), (token.clone(), expires_at));

        tracing::info!(
            app_id = %key.app_id(),
            environment = %key.environment(),
            "Started debug session for app {}: token={}, ttl={}s",
            key,
            token,
            ttl_seconds
        );

        Ok((inspect_app_id, token))
    }

    /// Validate debug token
    pub fn validate_inspect_token(&self, key: &AppKey, token: &str) -> bool {
        if let Some(entry) = self.inspect_sessions.get(key) {
            let (stored_token, expires_at) = entry.value();
            if stored_token == token && SystemTime::now() < *expires_at {
                return true;
            }
        }
        false
    }

    /// Connect debugger
    pub async fn connect_inspector(
        &self,
        key: &AppKey,
    ) -> Result<(
        tokio::sync::mpsc::UnboundedSender<String>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    )> {
        self.runtime_pool.connect_inspector(&key.tenant_id()).await
    }

    /// Unload one environment of one application.
    pub async fn unload_app(&self, key: &AppKey) -> Result<()> {
        tracing::info!("Unloading app: {}", key);

        // Unload from runtime pool
        self.runtime_pool.unload_app(&key.tenant_id()).await?;

        // Remove application information. The version record goes with it: its
        // asset index must not answer requests for an app that is no longer
        // loaded.
        self.versions.remove(key);
        self.inspect_sessions.remove(key);
        match self.apps.remove(key) {
            Some(_) => {
                tracing::info!("App {} unloaded successfully", key);
                Ok(())
            }
            None => Err(anyhow!("App {} not found", key)),
        }
    }

    /// Execute application.
    ///
    /// The request body goes in as bytes and the response body comes back as
    /// bytes: what an application receives and answers with is not always
    /// text, and text is what this used to be.
    pub async fn execute_app(
        &self,
        key: &AppKey,
        url: String,
        method: String,
        headers: std::collections::HashMap<String, String>,
        body: Option<Vec<u8>>,
    ) -> Result<(
        u16,
        std::collections::HashMap<String, String>,
        Option<Vec<u8>>,
    )> {
        self.require_running(key)?;

        // Execute request via runtime pool
        self.runtime_pool
            .execute_request(&key.tenant_id(), url, method, headers, body)
            .await
    }

    /// Hand a batch of queue messages to one application's queue handler.
    ///
    /// Used by the push path (`Trigger/OnQueue`); this node's own pollers
    /// reach the same code through the runtime pool directly. The returned
    /// outcome names the messages the handler did not complete, which is what
    /// the caller acknowledges or hands back.
    pub async fn execute_queue_batch(
        &self,
        key: &AppKey,
        queue: String,
        messages: Vec<crate::runtime::LeasedMessage>,
    ) -> Result<crate::runtime::BatchOutcome> {
        self.require_running(key)?;

        self.runtime_pool
            .execute_queue_batch(&key.tenant_id(), queue, messages)
            .await
    }

    /// Run one application's scheduled handler for one due cron expression.
    ///
    /// The schedule lives in the control plane, which picks a single node per
    /// tick (architecture principle 6), so the answer has to say *which* kind
    /// of failure happened: a node that cannot run the app at all should be
    /// replaced by another node, while a handler that threw has already had
    /// whatever effect it had and must not be run again elsewhere. Everything
    /// from the isolate inwards counts as the second kind, because by then the
    /// application's own code has been entered.
    pub async fn execute_scheduled(
        &self,
        key: &AppKey,
        cron: String,
        scheduled_time_ms: i64,
    ) -> Result<(), ScheduledFailure> {
        self.require_running(key)
            .map_err(|e| ScheduledFailure::NotRunnableHere(e.to_string()))?;

        self.runtime_pool
            .execute_scheduled(&key.tenant_id(), cron, scheduled_time_ms)
            .await
            .map_err(|e| ScheduledFailure::HandlerFailed(e.to_string()))
    }

    /// Refuse work for an app this node is not serving.
    fn require_running(&self, key: &AppKey) -> Result<()> {
        let entry = self
            .apps
            .get(key)
            .ok_or_else(|| anyhow!("App {} not found", key))?;

        if entry.state != AppState::Running {
            return Err(anyhow!(
                "App {} is not running (state: {:?})",
                key,
                entry.state
            ));
        }

        Ok(())
    }

    /// Get application information
    #[allow(dead_code)]
    pub fn get_app(&self, key: &AppKey) -> Option<App> {
        self.apps.get(key).map(|entry| entry.clone())
    }

    /// List all applications
    pub fn list_apps(&self) -> Vec<App> {
        self.apps.iter().map(|entry| entry.clone()).collect()
    }

    /// Get statistics
    #[allow(dead_code)]
    pub async fn get_stats(&self) -> serde_json::Value {
        let apps: Vec<App> = self.apps.iter().map(|entry| entry.clone()).collect();
        let pool_stats = self.runtime_pool.get_stats().await;

        let mut stats = serde_json::json!({
            "total": apps.len(),
            "running": apps.iter().filter(|a| a.state == AppState::Running).count(),
            "stopped": apps.iter().filter(|a| a.state == AppState::Stopped).count(),
            "error": apps.iter().filter(|a| a.state == AppState::Error).count(),
            "loading": apps.iter().filter(|a| a.state == AppState::Loading).count(),
        });

        // Add thread pool statistics
        if let Some(obj) = stats.as_object_mut() {
            obj.insert(
                "pool".to_string(),
                serde_json::to_value(pool_stats).unwrap(),
            );
        }

        stats
    }

    /// Get aggregate OpMetrics for each (app, environment) this node runs.
    pub async fn get_op_metrics(&self) -> Vec<(AppKey, OpMetricsSummary)> {
        let keys: Vec<AppKey> = self.apps.iter().map(|entry| entry.key().clone()).collect();
        let mut results = Vec::new();

        for key in keys {
            match self.runtime_pool.get_op_metrics(&key.tenant_id()).await {
                Ok(summary) => {
                    results.push((key, summary));
                }
                Err(e) => {
                    tracing::warn!(
                        app_id = %key.app_id(),
                        environment = %key.environment(),
                        error = %e,
                        "Failed to get OpMetrics summary for {}; its op counts are missing from \
                         this status answer",
                        key
                    );
                }
            }
        }

        results
    }

    /// Register WebSocket connection
    ///
    /// Establish bidirectional message channel between Gateway and JavaScript WebSocketServer
    ///
    /// # Parameters
    /// - key: which environment of which application
    /// - ws_id: WebSocket connection ID
    /// - js_to_client_tx: Channel for JavaScript to send messages to the client
    pub async fn register_ws_connection(
        &self,
        key: &AppKey,
        ws_id: String,
        js_to_client_tx: tokio::sync::mpsc::UnboundedSender<crate::runtime::WsServerMessage>,
    ) -> Result<()> {
        self.runtime_pool
            .register_ws_connection(&key.tenant_id(), ws_id, js_to_client_tx)
            .await
    }

    /// Send WebSocket message to JavaScript
    ///
    /// # Parameters
    /// - key: which environment of which application
    /// - ws_id: WebSocket connection ID
    /// - message: Message content
    pub async fn send_ws_message(
        &self,
        key: &AppKey,
        ws_id: String,
        message: String,
    ) -> Result<()> {
        self.runtime_pool
            .send_ws_message(&key.tenant_id(), ws_id, message)
            .await
    }

    /// Unregister WebSocket connection
    ///
    /// # Parameters
    /// - key: which environment of which application
    /// - ws_id: WebSocket connection ID
    pub async fn unregister_ws_connection(&self, key: &AppKey, ws_id: String) -> Result<()> {
        self.runtime_pool
            .unregister_ws_connection(&key.tenant_id(), ws_id)
            .await
    }
}

impl Default for ApplicationManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "manager_test.rs"]
mod manager_test;
