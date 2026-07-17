pub mod database;

use crate::config::{LocalConfig, SystemConfig, error::ConfigError};
use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

pub use database::{ConfigDatabase, DatabaseError, AppConfigEntry};


/// Configuration manager that handles the complete configuration lifecycle
///
/// Manages:
/// - System config layering: default + local + supervisor
/// - App config storage using ArcSwap<HashMap<AppId, JsonValue>>
/// - Database persistence and incremental updates from supervisor
pub struct ConfigManager {
    /// System configuration, finally, it is the default config + local config + supervisor config
    system_config: Arc<RwLock<SystemConfig>>,
    /// App configurations using ArcSwap for atomic updates
    /// Key: app_id, Value: app configuration as JSON
    app_configs: Arc<ArcSwap<HashMap<String, JsonValue>>>,
    /// Last known app config version timestamp
    app_configs_ver: Arc<RwLock<DateTime<Utc>>>,
    /// Database handle for persistence
    local_database: Arc<RwLock<ConfigDatabase>>,
}

impl ConfigManager {
    /// Initialize the config manager by loading the local config and database
    /// 
    /// # Arguments
    /// * `local_config_path` - Path to local config.toml file
    /// * `local_database` - Database handle for persistence
    /// 
    /// # Returns
    pub async fn new(
        local_config_path: PathBuf,
        local_database: ConfigDatabase,
    ) -> Result<Self, ConfigError> {
        // Load local config
        let local_config = LocalConfig::from_toml(local_config_path)?;
        // System config = the default config + local config
        let system_config = SystemConfig::default() + local_config.get_system_config();

        // Load init app configs from database
        let local_database = Arc::new(RwLock::new(local_database));
        let app_configs: HashMap<String, JsonValue> = local_database
            .read()
            .await
            .load_all_app_configs()
            .await?;
        let latest_app_configs_version = local_database
            .read()
            .await
            .get_latest_version_timestamp()
            .await?;

        Ok(Self {
            system_config: Arc::new(RwLock::new(system_config)),
            app_configs: Arc::new(ArcSwap::from_pointee(app_configs)),
            app_configs_ver: Arc::new(RwLock::new(latest_app_configs_version)),
            local_database
        })
    }

    /// Update the app configurations by applying the incremental updates from supervisor
    /// 
    /// # Arguments
    /// * `updates` - The incremental updates from supervisor
    /// 
    /// # Returns
    /// * `DateTime<Utc>` - The latest version timestamp
    pub async fn update_app_configs(&self, updates: Vec<AppConfigEntry>) -> Result<DateTime<Utc>, ConfigError> {
        if updates.is_empty() {
            return Err(ConfigError::ValidationError { 
                message: "No updates provided".to_string() 
            });
        }

        // Clone the Arc references for use in closure
        let app_configs_arc = Arc::clone(&self.app_configs);
        let app_configs_ver_arc = Arc::clone(&self.app_configs_ver);
        
        // Get the latest version timestamp from updates
        let latest_version_timestamp = updates.iter()
            .map(|update| update.version_timestamp)
            .max()
            .ok_or_else(|| ConfigError::ValidationError {
                message: "No updates provided to apply".to_string(),
            })?;

        let result_timestamp = {
            let mut db = self.local_database.write().await;
            db.apply_incremental_updates(updates, |app_configs| {
                // Update ArcSwap cache (app_configs)
                app_configs_arc.store(Arc::new(app_configs.clone()));
                Ok::<(), DatabaseError>(())
            }).await?
        };

        // Update app configs version
        {
            let mut app_configs_ver = app_configs_ver_arc.write().await;
            *app_configs_ver = latest_version_timestamp;
        }

        Ok(result_timestamp)
    }

    /// Get the current system configuration
    /// 
    /// # Returns
    /// * `SystemConfig` - The current system configuration
    pub async fn get_system_config(&self) -> SystemConfig {
        let system_config = self.system_config.read().await;
        system_config.clone()
    }

    /// Get the current app configurations as a snapshot
    ///
    /// # Returns
    /// * `HashMap<String, JsonValue>` - The current app configurations
    pub fn get_app_configs(&self) -> HashMap<String, JsonValue> {
        (**self.app_configs.load()).clone()
    }

    /// Get a specific app configuration
    ///
    /// # Arguments
    /// * `app_id` - The app ID to get configuration for
    ///
    /// # Returns
    /// * `Option<JsonValue>` - The app configuration if found
    pub fn get_app_config(&self, app_id: &str) -> Option<JsonValue> {
        self.app_configs.load().get(app_id).cloned()
    }

    /// Check if an app exists
    ///
    /// # Arguments
    /// * `app_id` - The app ID to check
    ///
    /// # Returns
    /// * `bool` - True if the app exists
    pub fn has_app(&self, app_id: &str) -> bool {
        self.app_configs.load().contains_key(app_id)
    }

    /// Get the number of configured apps
    ///
    /// # Returns
    /// * `usize` - Number of apps
    pub fn get_app_count(&self) -> usize {
        self.app_configs.load().len()
    }

    /// Get all app IDs
    ///
    /// # Returns
    /// * `Vec<String>` - List of all app IDs
    pub fn get_app_ids(&self) -> Vec<String> {
        self.app_configs.load().keys().cloned().collect()
    }

    /// Get the ArcSwap reference for direct access by business logic
    ///
    /// # Returns
    /// * `&Arc<ArcSwap<HashMap<String, JsonValue>>>` - Direct reference to ArcSwap
    pub fn get_app_configs_arc(&self) -> &Arc<ArcSwap<HashMap<String, JsonValue>>> {
        &self.app_configs
    }

    /// Get the latest app configurations version timestamp
    /// 
    /// # Returns
    /// * `DateTime<Utc>` - The latest app configurations version timestamp
    pub async fn get_app_configs_ver(&self) -> DateTime<Utc> {
        *self.app_configs_ver.read().await
    }
}