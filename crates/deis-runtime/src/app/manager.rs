// Application manager

use super::types::{App, AppConfig, AppState, ResourceLimits};
use crate::app_repo::AppRepo;
use crate::assets::AssetBundle;
use crate::config::ExecutionLimits;
use crate::runtime::{IsolateSpec, RuntimePool};
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
    /// Environment *name* (`production` or a preview name), which is what
    /// application code branches on.
    pub environment: String,
    pub entry_point: String,
    /// Bundled module source. Kept so that a failed replacement can put the
    /// previous version back without another download.
    code: String,
    config: AppConfig,
    pub assets: Arc<AssetBundle>,
}

impl std::fmt::Debug for VersionRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VersionRecord")
            .field("version_id", &self.version_id)
            .field("environment", &self.environment)
            .field("entry_point", &self.entry_point)
            .field("assets", &self.assets)
            .finish()
    }
}

/// Everything needed to put one version of one app on this node.
pub struct VersionInstall {
    pub app_id: String,
    pub version_id: String,
    pub environment: String,
    pub entry_point: String,
    pub code: String,
    /// Carries the decrypted `env`; see [`AppConfig`] for why it neither
    /// serialises nor prints its values.
    pub config: AppConfig,
    pub assets: Arc<AssetBundle>,
}

/// What the node is running for one app, in the terms the control plane uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledVersion {
    pub version_id: String,
    pub environment: String,
}

/// Application manager
pub struct ApplicationManager {
    apps: DashMap<String, App>,
    /// Deployed versions, keyed by app id. Only apps installed from a bundle
    /// appear here; the built-in `_system` app has no version.
    versions: DashMap<String, Arc<VersionRecord>>,
    runtime_pool: Arc<RuntimePool>,
    // Debug sessions: app_id -> (token, expires_at)
    inspect_sessions: DashMap<String, (String, SystemTime)>,
    // Platform-wide defaults for per-app execution limits (timeout, rate limit).
    // An app's own `ResourceLimits` may override either value; see
    // `effective_limits`.
    default_limits: ExecutionLimits,
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
        }
    }

    pub fn with_threads_and_repo(num_threads: Option<usize>, app_repo: Arc<AppRepo>) -> Self {
        Self {
            apps: DashMap::new(),
            versions: DashMap::new(),
            runtime_pool: Arc::new(RuntimePool::new_with_repo(num_threads, Some(app_repo))),
            inspect_sessions: DashMap::new(),
            default_limits: ExecutionLimits::default(),
        }
    }

    /// Override the platform-wide default execution limits (timeout, rate
    /// limit) used for apps that do not configure their own. Intended to be
    /// called once at startup with the values loaded from `RuntimeConfig`.
    pub fn with_limits(mut self, default_limits: ExecutionLimits) -> Self {
        self.default_limits = default_limits;
        self
    }

    /// Resolve the effective execution limits for an app: its own
    /// `ResourceLimits` override where present, the platform default
    /// otherwise. Logs at info level whenever an app overrides a default, so
    /// operators can see which apps deviate from the platform baseline.
    fn effective_limits(&self, app_id: &str, limits: Option<&ResourceLimits>) -> ExecutionLimits {
        let timeout_override = limits.and_then(|l| l.timeout_ms);
        let timeout_ms = timeout_override.unwrap_or(self.default_limits.timeout_ms);
        if let Some(overridden) = timeout_override {
            tracing::info!(
                target: "app",
                app_id = %app_id,
                "App {} overrides default execution timeout: {}ms (platform default {}ms)",
                app_id,
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
                app_id = %app_id,
                "App {} overrides default rate limit: {} ops/sec (platform default {} ops/sec)",
                app_id,
                overridden,
                self.default_limits.rate_limit_ops_per_sec
            );
        }

        ExecutionLimits {
            timeout_ms,
            rate_limit_ops_per_sec,
        }
    }

    /// Load application
    pub async fn load_app(
        &self,
        id: String,
        code: String,
        config: AppConfig,
        entry_point: Option<String>,
    ) -> Result<()> {
        // Check if application already exists
        if self.apps.contains_key(&id) {
            return Err(anyhow!("App {} already exists", id));
        }

        tracing::info!("Loading app: {}", id);

        // Create application information
        let now = SystemTime::now();
        let mut app_info = App {
            id: id.clone(),
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
            limits: self.effective_limits(&id, config.limits.as_ref()),
        };
        match self.runtime_pool.load_app(id.clone(), code, spec).await {
            Ok(()) => {
                app_info.state = AppState::Running;
                app_info.updated_at = SystemTime::now();
                self.apps.insert(id.clone(), app_info);
                tracing::info!("App {} loaded successfully", id);
                Ok(())
            }
            Err(e) => {
                app_info.state = AppState::Error;
                app_info.error = Some(e.to_string());
                app_info.updated_at = SystemTime::now();
                tracing::error!("Failed to load app {}: {}", id, e);
                Err(e)
            }
        }
    }

    /// Put one version of one app on this node, replacing whatever it was
    /// running.
    ///
    /// The caller has already proven the bundle: digest checked, manifest
    /// parsed, entry point present, assets read. What is left is the one step
    /// that can still fail on code the node has never executed - creating the
    /// isolate - and the pool only offers unload-then-load, so there is a
    /// window where the app is not loaded.
    ///
    /// That window is why the previous version is kept in memory: if the new
    /// code will not load, the old code goes straight back in. An app is only
    /// left down when its previous version also refuses to load, which means
    /// the isolate host itself is in trouble, not the deployment.
    pub async fn install_version(&self, install: VersionInstall) -> Result<()> {
        let VersionInstall {
            app_id,
            version_id,
            environment,
            entry_point,
            code,
            config,
            assets,
        } = install;

        let previous = self
            .versions
            .get(&app_id)
            .map(|entry| entry.value().clone());
        let was_loaded = self.apps.contains_key(&app_id);

        if was_loaded {
            if let Err(e) = self.runtime_pool.unload_app(&app_id).await {
                // Nothing has been swapped yet, so the app keeps serving the
                // version it already had.
                tracing::error!(
                    app_id = %app_id,
                    version_id = %version_id,
                    error = %e,
                    "Could not retire the running isolate, so version {} was not \
                     installed; the app keeps serving the version it was already on",
                    version_id
                );
                return Err(anyhow!(
                    "could not unload the running version of {app_id}: {e}"
                ));
            }
        }

        let record = Arc::new(VersionRecord {
            version_id: version_id.clone(),
            environment: environment.clone(),
            entry_point: entry_point.clone(),
            code: code.clone(),
            config: config.clone(),
            assets: assets.clone(),
        });

        match self.load_record(&app_id, &record).await {
            Ok(()) => {
                let now = SystemTime::now();
                self.apps.insert(
                    app_id.clone(),
                    App {
                        id: app_id.clone(),
                        state: AppState::Running,
                        config,
                        created_at: now,
                        updated_at: now,
                        error: None,
                    },
                );
                self.versions.insert(app_id.clone(), record);
                tracing::info!(
                    app_id = %app_id,
                    version_id = %version_id,
                    environment = %environment,
                    assets = assets.len(),
                    asset_bytes = assets.total_bytes(),
                    "App {} is now serving version {} of environment {}",
                    app_id,
                    version_id,
                    environment
                );
                Ok(())
            }
            Err(e) => {
                self.apps.remove(&app_id);
                self.restore(&app_id, previous, &version_id, &e).await;
                Err(e)
            }
        }
    }

    /// Load one version's code into the pool.
    async fn load_record(&self, app_id: &str, record: &VersionRecord) -> Result<()> {
        let spec = IsolateSpec {
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
            limits: self.effective_limits(app_id, record.config.limits.as_ref()),
        };
        self.runtime_pool
            .load_app(app_id.to_string(), record.code.clone(), spec)
            .await
    }

    /// Put the previous version back after a failed install.
    async fn restore(
        &self,
        app_id: &str,
        previous: Option<Arc<VersionRecord>>,
        attempted_version: &str,
        cause: &anyhow::Error,
    ) {
        let Some(previous) = previous else {
            self.versions.remove(app_id);
            tracing::error!(
                app_id = %app_id,
                version_id = %attempted_version,
                error = %cause,
                "Version {} of {} would not load and the app had no earlier version to \
                 fall back to; it is not serving and requests for it will fail until a \
                 working version is deployed",
                attempted_version,
                app_id
            );
            return;
        };

        match self.load_record(app_id, &previous).await {
            Ok(()) => {
                let now = SystemTime::now();
                self.apps.insert(
                    app_id.to_string(),
                    App {
                        id: app_id.to_string(),
                        state: AppState::Running,
                        config: previous.config.clone(),
                        created_at: now,
                        updated_at: now,
                        error: None,
                    },
                );
                self.versions.insert(app_id.to_string(), previous.clone());
                tracing::warn!(
                    app_id = %app_id,
                    version_id = %attempted_version,
                    restored_version = %previous.version_id,
                    error = %cause,
                    "Version {} of {} would not load; the node put version {} back and \
                     is serving it, so traffic is unaffected but the deployment did not take",
                    attempted_version,
                    app_id,
                    previous.version_id
                );
            }
            Err(restore_error) => {
                self.versions.remove(app_id);
                tracing::error!(
                    app_id = %app_id,
                    version_id = %attempted_version,
                    restored_version = %previous.version_id,
                    error = %cause,
                    restore_error = %restore_error,
                    "Version {} of {} would not load and neither would the version it \
                     replaced; {} is not serving on this node and needs a working version \
                     deployed or the node restarted",
                    attempted_version,
                    app_id,
                    app_id
                );
            }
        }
    }

    /// The static files of the version currently running, for the HTTP entry
    /// to consult before anything reaches an isolate.
    pub fn assets(&self, app_id: &str) -> Option<Arc<AssetBundle>> {
        self.versions.get(app_id).map(|e| e.assets.clone())
    }

    /// Which version of which environment this node runs for an app.
    pub fn installed_version(&self, app_id: &str) -> Option<InstalledVersion> {
        self.versions.get(app_id).map(|e| InstalledVersion {
            version_id: e.version_id.clone(),
            environment: e.environment.clone(),
        })
    }

    /// Every app with a deployed version, and what it is running.
    pub fn installed_versions(&self) -> Vec<(String, InstalledVersion)> {
        self.versions
            .iter()
            .map(|e| {
                (
                    e.key().clone(),
                    InstalledVersion {
                        version_id: e.version_id.clone(),
                        environment: e.environment.clone(),
                    },
                )
            })
            .collect()
    }

    pub fn is_loaded(&self, app_id: &str) -> bool {
        self.apps.contains_key(app_id)
    }

    /// Start debug session
    pub fn start_inspect(&self, app_id: String, ttl_seconds: u64) -> Result<(String, String)> {
        // Check if application exists
        if !self.apps.contains_key(&app_id) {
            return Err(anyhow!("App {} not found", app_id));
        }

        // Generate token
        let token = uuid::Uuid::new_v4().to_string();
        let expires_at = SystemTime::now() + std::time::Duration::from_secs(ttl_seconds);

        let inspect_app_id = format!("_inspect:{}", app_id);

        // Store session
        self.inspect_sessions
            .insert(app_id.clone(), (token.clone(), expires_at));

        tracing::info!(
            "Started debug session for app {}: token={}, ttl={}s",
            app_id,
            token,
            ttl_seconds
        );

        Ok((inspect_app_id, token))
    }

    /// Validate debug token
    pub fn validate_inspect_token(&self, app_id: &str, token: &str) -> bool {
        if let Some(entry) = self.inspect_sessions.get(app_id) {
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
        app_id: &str,
    ) -> Result<(
        tokio::sync::mpsc::UnboundedSender<String>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    )> {
        self.runtime_pool.connect_inspector(app_id).await
    }

    /// Unload application
    pub async fn unload_app(&self, id: &str) -> Result<()> {
        tracing::info!("Unloading app: {}", id);

        // Unload from runtime pool
        self.runtime_pool.unload_app(id).await?;

        // Remove application information. The version record goes with it: its
        // asset index must not answer requests for an app that is no longer
        // loaded.
        self.versions.remove(id);
        match self.apps.remove(id) {
            Some(_) => {
                tracing::info!("App {} unloaded successfully", id);
                Ok(())
            }
            None => Err(anyhow!("App {} not found", id)),
        }
    }

    /// Update application
    pub async fn update_app(
        &self,
        id: &str,
        code: String,
        config: Option<AppConfig>,
        entry_point: Option<String>,
    ) -> Result<()> {
        tracing::info!("Updating app: {}", id);

        let entry = self
            .apps
            .get(id)
            .ok_or_else(|| anyhow!("App {} not found", id))?;

        // Get current configuration
        let current_config = config.unwrap_or_else(|| entry.config.clone());
        drop(entry);

        // Unload old version from runtime pool
        self.runtime_pool.unload_app(id).await?;

        // Load new version
        let spec = IsolateSpec {
            entry_point,
            env: current_config.env.clone(),
            permissions: current_config.permissions.clone(),
            memory_limit_mb: current_config.limits.as_ref().map(|l| l.memory_mb),
            resource_table_max: current_config
                .limits
                .as_ref()
                .and_then(|l| l.resource_table_max),
            route_patterns: current_config.routes.clone(),
            limits: self.effective_limits(id, current_config.limits.as_ref()),
        };
        self.runtime_pool
            .load_app(id.to_string(), code, spec)
            .await?;

        // Update application information
        if let Some(mut entry) = self.apps.get_mut(id) {
            entry.state = AppState::Running;
            entry.config = current_config;
            entry.updated_at = SystemTime::now();
            entry.error = None;
        }

        tracing::info!("App {} updated successfully", id);
        Ok(())
    }

    /// Execute application
    pub async fn execute_app(
        &self,
        id: &str,
        url: String,
        method: String,
        headers: std::collections::HashMap<String, String>,
        body: Option<String>,
    ) -> Result<(
        u16,
        std::collections::HashMap<String, String>,
        Option<String>,
    )> {
        let entry = self
            .apps
            .get(id)
            .ok_or_else(|| anyhow!("App {} not found", id))?;

        if entry.state != AppState::Running {
            return Err(anyhow!(
                "App {} is not running (state: {:?})",
                id,
                entry.state
            ));
        }

        drop(entry);

        // Execute request via runtime pool
        self.runtime_pool
            .execute_request(id, url, method, headers, body)
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
        id: &str,
        queue: String,
        messages: Vec<crate::runtime::LeasedMessage>,
    ) -> Result<crate::runtime::BatchOutcome> {
        let entry = self
            .apps
            .get(id)
            .ok_or_else(|| anyhow!("App {} not found", id))?;

        if entry.state != AppState::Running {
            return Err(anyhow!(
                "App {} is not running (state: {:?})",
                id,
                entry.state
            ));
        }

        drop(entry);

        self.runtime_pool
            .execute_queue_batch(id, queue, messages)
            .await
    }

    /// Run one application's scheduled handler for one due cron expression.
    ///
    /// The schedule lives in the control plane, which picks a single healthy
    /// node per tick (architecture principle 6). An app that is not running
    /// here is an error the caller has to see, so the control plane can try
    /// another node instead of counting the tick as done.
    pub async fn execute_scheduled(
        &self,
        id: &str,
        cron: String,
        scheduled_time_ms: i64,
    ) -> Result<()> {
        let entry = self
            .apps
            .get(id)
            .ok_or_else(|| anyhow!("App {} not found", id))?;

        if entry.state != AppState::Running {
            return Err(anyhow!(
                "App {} is not running (state: {:?})",
                id,
                entry.state
            ));
        }

        drop(entry);

        self.runtime_pool
            .execute_scheduled(id, cron, scheduled_time_ms)
            .await
    }

    /// Get application information
    #[allow(dead_code)]
    pub fn get_app(&self, id: &str) -> Option<App> {
        self.apps.get(id).map(|entry| entry.clone())
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

    /// Get aggregate OpMetrics for each application
    pub async fn get_op_metrics(&self) -> Vec<(String, OpMetricsSummary)> {
        let app_ids: Vec<String> = self.apps.iter().map(|entry| entry.key().clone()).collect();
        let mut results = Vec::new();

        for app_id in app_ids {
            match self.runtime_pool.get_op_metrics(&app_id).await {
                Ok(summary) => {
                    results.push((app_id, summary));
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to get OpMetrics summary: app_id={}, error={}",
                        app_id,
                        e
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
    /// - app_id: Application ID
    /// - ws_id: WebSocket connection ID
    /// - js_to_client_tx: Channel for JavaScript to send messages to the client
    pub async fn register_ws_connection(
        &self,
        app_id: &str,
        ws_id: String,
        js_to_client_tx: tokio::sync::mpsc::UnboundedSender<crate::runtime::WsServerMessage>,
    ) -> Result<()> {
        self.runtime_pool
            .register_ws_connection(app_id, ws_id, js_to_client_tx)
            .await
    }

    /// Send WebSocket message to JavaScript
    ///
    /// # Parameters
    /// - app_id: Application ID
    /// - ws_id: WebSocket connection ID
    /// - message: Message content
    pub async fn send_ws_message(
        &self,
        app_id: &str,
        ws_id: String,
        message: String,
    ) -> Result<()> {
        self.runtime_pool
            .send_ws_message(app_id, ws_id, message)
            .await
    }

    /// Unregister WebSocket connection
    ///
    /// # Parameters
    /// - app_id: Application ID
    /// - ws_id: WebSocket connection ID
    pub async fn unregister_ws_connection(&self, app_id: &str, ws_id: String) -> Result<()> {
        self.runtime_pool
            .unregister_ws_connection(app_id, ws_id)
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
