pub mod database;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::catalog::table::CatalogRoutes;
use crate::catalog::wire::RoutingTableDocument;
use crate::config::{
    AppConfig, ConfigError, ControlSystemConfig, LocalConfig, RouteTable, SystemConfig, Validatable,
};

pub use database::{AppConfigEntry, ConfigDatabase, DatabaseError};

/// How many added or removed routing rules are written out one by one when
/// the table changes. Past this the log says how many more there were: a
/// first fetch on a large platform would otherwise bury everything else.
const MAX_LOGGED_RULE_CHANGES: usize = 50;

/// Configuration manager that owns the complete configuration lifecycle.
///
/// - System config: baseline + local file (+ control overlay), swapped
///   atomically so readers never block.
/// - App configs: persisted in the database, mirrored in memory, and
///   compiled into a [`RouteTable`] whenever they change.
/// - Catalog routes: the control plane's routing table, fetched by
///   [`crate::catalog`] and swapped in whole. Not persisted: it is a copy of
///   something the control plane owns, and a stale copy read off disk at
///   startup would be indistinguishable from a current one.
pub struct ConfigManager {
    system_config: ArcSwap<SystemConfig>,
    /// Raw app configurations keyed by app id, as pushed by the control.
    app_configs: Arc<ArcSwap<HashMap<String, JsonValue>>>,
    /// Lookup structures derived from `app_configs`.
    routes: ArcSwap<RouteTable>,
    /// The catalog's routing table. Starts empty with an empty version,
    /// which is how "never fetched" is told from "fetched and empty".
    catalog_routes: ArcSwap<CatalogRoutes>,
    /// Version timestamp of the newest app config applied.
    app_configs_ver: RwLock<DateTime<Utc>>,
    local_database: RwLock<ConfigDatabase>,
}

impl ConfigManager {
    /// Load the local configuration file, open the database it names, and
    /// build the initial route table.
    pub async fn new(local_config_path: PathBuf) -> Result<Self, ConfigError> {
        let system_config = Self::load_system_config(&local_config_path)?;
        Self::with_system_config(system_config).await
    }

    /// Layer the local configuration file onto the baseline, with every
    /// relative path resolved against the current working directory. This is
    /// the only step that touches the file, so callers can initialize logging
    /// from the result before [`Self::with_system_config`] opens the database
    /// and loads certificates.
    pub fn load_system_config(local_config_path: &Path) -> Result<SystemConfig, ConfigError> {
        let local_config = LocalConfig::from_toml(local_config_path.to_path_buf())?;
        let file_layer = local_config.get_system_config();
        if file_layer.cache.is_some() {
            // Read from the file layer rather than the merged result, which
            // always carries the baseline's cache settings. An operator who
            // wrote the section is told it does nothing; one who did not is
            // not warned about a default they never chose.
            warn!(
                "a [cache] section is configured but this proxy does not cache: no response is \
                 stored, no cache key is computed and no X-Cache-Status header is sent, so these \
                 settings have no effect on traffic. Remove the section, or expect every request \
                 to reach a node"
            );
        }
        let mut system_config = SystemConfig::baseline() + file_layer;
        system_config.resolve_paths(&std::env::current_dir()?);
        // The file was validated on its own; the merge can still combine a
        // local value with a baseline one into something invalid (a port in
        // both listen_ports and tls.ports, say), so validate the result too.
        system_config.validate()?;
        Ok(system_config)
    }

    /// Open the database named by an already loaded system configuration and
    /// build the initial route table.
    pub async fn with_system_config(system_config: SystemConfig) -> Result<Self, ConfigError> {
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
            catalog_routes: ArcSwap::from_pointee(CatalogRoutes::default()),
            app_configs_ver: RwLock::new(latest_version),
            local_database: RwLock::new(local_database),
        })
    }

    /// Apply incremental app config updates from the control, persist
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

    /// Layer a control overlay onto the running system configuration.
    ///
    /// Only traffic policy can change this way; the overlay type already
    /// strips everything else. The balancing strategy is fixed per process,
    /// so a change there is recorded but only takes effect after a restart.
    pub fn apply_system_overlay(&self, overlay: ControlSystemConfig) -> Result<(), ConfigError> {
        let current = self.system_config.load_full();
        let merged = (*current).clone() + overlay.get_system_config();
        merged.validate()?;
        if merged.balancing_strategy() != current.balancing_strategy() {
            warn!(
                active = %current.balancing_strategy(),
                requested = %merged.balancing_strategy(),
                "balancing strategy changed by control; restart to apply"
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

    /// Current catalog routing table snapshot.
    ///
    /// One atomic load per request; the table behind it never changes, so a
    /// request that starts under one table finishes under it.
    pub fn catalog_routes(&self) -> Arc<CatalogRoutes> {
        self.catalog_routes.load_full()
    }

    /// Publish a routing table fetched from the control plane.
    ///
    /// Returns whether anything changed. The whole table is compiled before
    /// the pointer is swapped, so a request sees either all of the old table
    /// or all of the new one - see the module comment in
    /// [`crate::catalog`] for why nothing partial may ever be applied.
    ///
    /// The document must already have passed
    /// [`RoutingTableDocument::validate`]; the fetch path refuses one that
    /// has not, and there is no other caller.
    pub fn apply_routing_table(&self, document: &RoutingTableDocument) -> bool {
        let current = self.catalog_routes.load();
        if current.version() == document.version {
            debug!(
                version = %document.version,
                "routing table unchanged"
            );
            return false;
        }

        let zone = self.system_config.load().routing().preview_zone();
        let next = CatalogRoutes::build(document, zone.as_ref());

        let before = current.describe();
        let after = next.describe();
        let added: Vec<&String> = after.difference(&before).collect();
        let removed: Vec<&String> = before.difference(&after).collect();

        info!(
            from_version = if current.version().is_empty() { "none" } else { current.version() },
            to_version = %document.version,
            built_at = document.generated_at,
            routes = next.route_count(),
            environments = next.environment_count(),
            automatic_addresses = next.preview_count(),
            added = added.len(),
            removed = removed.len(),
            "routing table changed"
        );
        for rule in added.iter().take(MAX_LOGGED_RULE_CHANGES) {
            info!(rule = %rule, "routing rule now in force");
        }
        for rule in removed.iter().take(MAX_LOGGED_RULE_CHANGES) {
            info!(rule = %rule, "routing rule gone; traffic that matched it now falls through");
        }
        let elided = added.len().saturating_sub(MAX_LOGGED_RULE_CHANGES)
            + removed.len().saturating_sub(MAX_LOGGED_RULE_CHANGES);
        if elided > 0 {
            info!(
                elided,
                limit = MAX_LOGGED_RULE_CHANGES,
                "more routing rules changed than are written out one by one; the control \
                 plane's routing table endpoint has the whole table"
            );
        }

        self.catalog_routes.store(Arc::new(next));
        true
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::wire::RoutingTableDocument;
    use crate::config::system::RoutingConfig;

    fn document(version: &str, routes: &str, environments: &str) -> RoutingTableDocument {
        let document: RoutingTableDocument = serde_json::from_str(&format!(
            r#"{{"version":"{version}","generated_at":7,"routes":{routes},"environments":{environments}}}"#
        ))
        .unwrap();
        document.validate().unwrap();
        document
    }

    /// A manager on an in-memory database, so the test touches no file.
    async fn manager_with(zone: Option<&str>) -> ConfigManager {
        let mut system = SystemConfig::baseline();
        system.database = Some(":memory:".to_string());
        system.routing = Some(RoutingConfig {
            preview_zone: zone.map(str::to_string),
            ..Default::default()
        });
        ConfigManager::with_system_config(system).await.unwrap()
    }

    #[tokio::test]
    async fn a_fresh_instance_has_no_catalog_table_at_all() {
        let manager = manager_with(None).await;
        let table = manager.catalog_routes();
        assert!(
            table.version().is_empty(),
            "an empty version is how the poller tells 'never fetched' from 'fetched and empty'"
        );
        assert!(table.is_empty());
    }

    #[tokio::test]
    async fn applying_a_table_publishes_it_whole() {
        let manager = manager_with(None).await;
        let applied = manager.apply_routing_table(&document(
            "v1",
            r#"[{"app_id":"shop","environment":"production","host":"shop.test","path_prefix":"/"}]"#,
            "[]",
        ));
        assert!(applied);

        let table = manager.catalog_routes();
        assert_eq!(table.version(), "v1");
        assert_eq!(table.generated_at(), 7);
        assert_eq!(
            table
                .match_request("shop.test", "/")
                .map(|found| found.target.app_id),
            Some("shop".to_string())
        );
    }

    #[tokio::test]
    async fn the_same_version_is_not_reapplied() {
        // Every poll returns the whole table; rebuilding and re-logging it
        // when nothing changed would bury the changes that matter.
        let manager = manager_with(None).await;
        let document = document("v1", "[]", "[]");
        assert!(manager.apply_routing_table(&document));
        assert!(!manager.apply_routing_table(&document));
    }

    #[tokio::test]
    async fn a_later_table_replaces_the_earlier_one_entirely() {
        let manager = manager_with(None).await;
        manager.apply_routing_table(&document(
            "v1",
            r#"[{"app_id":"old","environment":"production","host":"a.test","path_prefix":"/"}]"#,
            "[]",
        ));
        manager.apply_routing_table(&document(
            "v2",
            r#"[{"app_id":"new","environment":"production","host":"b.test","path_prefix":"/"}]"#,
            "[]",
        ));

        let table = manager.catalog_routes();
        assert_eq!(table.version(), "v2");
        assert!(
            table.match_request("a.test", "/").is_none(),
            "a route missing from the new table is gone, not merged in from the old one"
        );
        assert!(table.match_request("b.test", "/").is_some());
    }

    #[tokio::test]
    async fn the_configured_zone_reaches_the_compiled_table() {
        let manager = manager_with(Some("preview.test")).await;
        manager.apply_routing_table(&document(
            "v1",
            "[]",
            r#"[{"app_id":"shop","environment":"pr-1"}]"#,
        ));
        let table = manager.catalog_routes();
        assert_eq!(
            table
                .match_request("pr-1--shop.preview.test", "/")
                .map(|found| found.target.environment),
            Some("pr-1".to_string())
        );

        // And with the zone off there is no automatic address at all.
        let without_zone = manager_with(None).await;
        without_zone.apply_routing_table(&document(
            "v1",
            "[]",
            r#"[{"app_id":"shop","environment":"pr-1"}]"#,
        ));
        assert_eq!(without_zone.catalog_routes().preview_count(), 0);
    }

    #[tokio::test]
    async fn a_control_overlay_cannot_move_the_routing_settings() {
        // routing.* points at the control plane and at this machine's DNS,
        // so a push from the control plane must not be able to change it.
        let manager = manager_with(Some("preview.test")).await;
        manager
            .apply_system_overlay(
                ControlSystemConfig::from_json(
                    r#"{"routing":{"preview_zone":"attacker.test","control_poll_interval_secs":1}}"#,
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            manager
                .get_system_config()
                .routing()
                .preview_zone()
                .map(|zone| zone.as_str().to_string()),
            Some("preview.test".to_string())
        );
    }
}
