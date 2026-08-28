pub mod database;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::config::{
    AppConfig, ConfigError, LocalConfig, RouteTable, SupervisorSystemConfig, SystemConfig,
    Validatable,
};

pub use database::{AppConfigEntry, ConfigDatabase, DatabaseError};

/// Configuration manager that owns the complete configuration lifecycle.
///
/// - System config: baseline + local file (+ supervisor overlay), swapped
///   atomically so readers never block.
/// - App configs: persisted in the database, mirrored in memory, and
///   compiled into a [`RouteTable`] whenever they change.
pub struct ConfigManager {
    system_config: ArcSwap<SystemConfig>,
    /// Raw app configurations keyed by app id, as pushed by the supervisor.
    app_configs: Arc<ArcSwap<HashMap<String, JsonValue>>>,
    /// Lookup structures derived from `app_configs`.
    routes: ArcSwap<RouteTable>,
    /// Version timestamp of the newest app config applied.
    app_configs_ver: RwLock<DateTime<Utc>>,
    local_database: RwLock<ConfigDatabase>,
}

impl ConfigManager {
    /// Load the local configuration file, open the database it names, and
    /// build the initial route table.
    pub async fn new(local_config_path: PathBuf) -> Result<Self, ConfigError> {
        let local_config = LocalConfig::from_toml(local_config_path)?;
        let system_config = SystemConfig::baseline() + local_config.get_system_config();

        let local_database = ConfigDatabase::init(system_config.database()).await?;
        let app_configs = local_database.load_all_app_configs().await?;
        let latest_version = local_database.get_latest_version_timestamp().await?;
        info!(
            database = system_config.database(),
            apps = app_configs.len(),
            "app configurations loaded"
        );

        let routes = RouteTable::build(
            &app_configs,
            system_config
                .tls
                .as_ref()
                .and_then(|t| t.default_certificate.as_ref()),
        );

        Ok(Self {
            system_config: ArcSwap::from_pointee(system_config),
            app_configs: Arc::new(ArcSwap::from_pointee(app_configs)),
            routes: ArcSwap::from_pointee(routes),
            app_configs_ver: RwLock::new(latest_version),
            local_database: RwLock::new(local_database),
        })
    }

    /// Apply incremental app config updates from the supervisor, persist
    /// them, and rebuild the route table.
    pub async fn update_app_configs(
        &self,
        updates: Vec<AppConfigEntry>,
    ) -> Result<DateTime<Utc>, ConfigError> {
        if updates.is_empty() {
            return Err(ConfigError::ValidationError {
                message: "No updates provided".to_string(),
            });
        }

        let latest_version = updates
            .iter()
            .map(|update| update.version_timestamp)
            .max()
            .ok_or_else(|| ConfigError::ValidationError {
                message: "No updates provided to apply".to_string(),
            })?;

        let app_configs = Arc::clone(&self.app_configs);
        let result_timestamp = {
            let mut db = self.local_database.write().await;
            db.apply_incremental_updates(updates, |configs| {
                app_configs.store(Arc::new(configs.clone()));
                Ok::<(), DatabaseError>(())
            })
            .await?
        };

        *self.app_configs_ver.write().await = latest_version;
        self.rebuild_routes();
        Ok(result_timestamp)
    }

    /// Replace the whole application list, persist it, and rebuild the route
    /// table. Apps missing from `apps` are removed.
    pub async fn replace_app_configs(
        &self,
        apps: Vec<AppConfig>,
        version: DateTime<Utc>,
    ) -> Result<(), ConfigError> {
        let mut configs = HashMap::with_capacity(apps.len());
        for app in apps {
            app.validate()?;
            let id = app.id.clone();
            configs.insert(id, serde_json::to_value(app)?);
        }

        {
            let mut db = self.local_database.write().await;
            db.save_all_app_configs(&configs, version).await?;
        }
        let count = configs.len();
        self.app_configs.store(Arc::new(configs));
        *self.app_configs_ver.write().await = version;
        info!(apps = count, %version, "app configurations replaced");
        self.rebuild_routes();
        Ok(())
    }

    /// Layer a supervisor overlay onto the running system configuration.
    ///
    /// Only traffic policy can change this way; the overlay type already
    /// strips everything else. The balancing strategy is fixed per process,
    /// so a change there is recorded but only takes effect after a restart.
    pub fn apply_system_overlay(&self, overlay: SupervisorSystemConfig) -> Result<(), ConfigError> {
        let current = self.system_config.load_full();
        let merged = (*current).clone() + overlay.get_system_config();
        merged.validate()?;
        if merged.balancing_strategy() != current.balancing_strategy() {
            warn!(
                active = %current.balancing_strategy(),
                requested = %merged.balancing_strategy(),
                "balancing strategy changed by supervisor; restart to apply"
            );
        }
        self.system_config.store(Arc::new(merged));
        info!("system configuration overlay applied");
        self.rebuild_routes();
        Ok(())
    }

    /// Recompile the route table from the current app configurations.
    pub fn rebuild_routes(&self) {
        let system = self.system_config.load();
        let apps = self.app_configs.load();
        let routes = RouteTable::build(
            &apps,
            system
                .tls
                .as_ref()
                .and_then(|t| t.default_certificate.as_ref()),
        );
        info!(apps = routes.len(), "route table rebuilt");
        self.routes.store(Arc::new(routes));
    }

    /// Current system configuration snapshot.
    pub fn get_system_config(&self) -> Arc<SystemConfig> {
        self.system_config.load_full()
    }

    /// Current route table snapshot.
    pub fn routes(&self) -> Arc<RouteTable> {
        self.routes.load_full()
    }

    pub fn get_app_configs(&self) -> HashMap<String, JsonValue> {
        (**self.app_configs.load()).clone()
    }

    pub fn get_app_config(&self, app_id: &str) -> Option<JsonValue> {
        self.app_configs.load().get(app_id).cloned()
    }

    pub fn has_app(&self, app_id: &str) -> bool {
        self.app_configs.load().contains_key(app_id)
    }

    pub fn get_app_count(&self) -> usize {
        self.app_configs.load().len()
    }

    pub fn get_app_ids(&self) -> Vec<String> {
        self.app_configs.load().keys().cloned().collect()
    }

    pub fn get_app_configs_arc(&self) -> &Arc<ArcSwap<HashMap<String, JsonValue>>> {
        &self.app_configs
    }

    pub async fn get_app_configs_ver(&self) -> DateTime<Utc> {
        *self.app_configs_ver.read().await
    }
}
