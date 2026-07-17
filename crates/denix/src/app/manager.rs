// Application manager

use super::types::{App, AppConfig, AppState};
use crate::app_repo::AppRepo;
use crate::runtime::RuntimePool;
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use deno_core::OpMetricsSummary;
use std::sync::Arc;
use std::time::SystemTime;

/// Application manager
pub struct ApplicationManager {
    apps: DashMap<String, App>,
    runtime_pool: Arc<RuntimePool>,
    // Debug sessions: app_id -> (token, expires_at)
    inspect_sessions: DashMap<String, (String, SystemTime)>,
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
            runtime_pool: Arc::new(RuntimePool::new(num_threads)),
            inspect_sessions: DashMap::new(),
        }
    }

    pub fn with_threads_and_repo(num_threads: Option<usize>, app_repo: Arc<AppRepo>) -> Self {
        Self {
            apps: DashMap::new(),
            runtime_pool: Arc::new(RuntimePool::new_with_repo(num_threads, Some(app_repo))),
            inspect_sessions: DashMap::new(),
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
        let memory_limit = config.limits.as_ref().map(|l| l.memory_mb);
        let resource_table_max = config.limits.as_ref().and_then(|l| l.resource_table_max);
        match self
            .runtime_pool
            .load_app(
                id.clone(),
                code,
                entry_point,
                config.env.clone(),
                config.permissions.clone(),
                memory_limit,
                resource_table_max,
            )
            .await
        {
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

        // Remove application information
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
        let memory_limit = current_config.limits.as_ref().map(|l| l.memory_mb);
        let resource_table_max = current_config
            .limits
            .as_ref()
            .and_then(|l| l.resource_table_max);
        self.runtime_pool
            .load_app(
                id.to_string(),
                code,
                entry_point,
                current_config.env.clone(),
                current_config.permissions.clone(),
                memory_limit,
                resource_table_max,
            )
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
