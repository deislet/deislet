use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use tokio::sync::RwLock;
use tracing::{debug, info};

use super::types::{ConfigEntry, ConfigType};

/// Configuration store
pub struct ConfigStore {
    /// SQLite connection (synchronous, uses tokio::task::spawn_blocking)
    db_path: String,

    /// In-memory cache (zero-copy reads)
    cache: Arc<ArcSwap<HashMap<String, ConfigEntry>>>,

    /// Write lock (ensures sequential write operations)
    write_lock: Arc<RwLock<()>>,
}

impl ConfigStore {
    /// Create a new configuration store
    pub async fn new<P: AsRef<Path>>(db_path: P) -> Result<Self> {
        let db_path = db_path.as_ref().to_string_lossy().to_string();
        let db_path_clone = db_path.clone();

        // Initialize database (in blocking task)
        tokio::task::spawn_blocking(move || Self::init_db(&db_path_clone))
            .await
            .context("Failed to spawn blocking task")?
            .context("Failed to initialize database")?;

        let store = Self {
            db_path: db_path.clone(),
            cache: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            write_lock: Arc::new(RwLock::new(())),
        };

        // Load configurations from database into cache
        store.load_cache_from_db().await?;

        info!("ConfigStore initialized with database: {}", db_path);
        Ok(store)
    }

    /// Initialize database tables (synchronous function)
    fn init_db(db_path: &str) -> Result<()> {
        let conn = rusqlite::Connection::open(db_path).context("Failed to open database")?;

        conn.execute(
            r#"
            CREATE TABLE IF NOT EXISTS configs (
                id TEXT NOT NULL,
                config_type TEXT NOT NULL,
                version INTEGER NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (id, config_type)
            )
            "#,
            [],
        )
        .context("Failed to create configs table")?;

        // Create index
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_config_type ON configs(config_type)",
            [],
        )
        .context("Failed to create index")?;

        Ok(())
    }

    /// Load all configurations from database into cache
    async fn load_cache_from_db(&self) -> Result<()> {
        let db_path = self.db_path.clone();

        let entries: Vec<ConfigEntry> = tokio::task::spawn_blocking(move || {
            let conn = rusqlite::Connection::open(&db_path)?;
            let mut stmt = conn.prepare(
                "SELECT id, config_type, version, content, created_at, updated_at FROM configs",
            )?;

            let rows = stmt.query_map([], |row| {
                let id: String = row.get(0)?;
                let config_type_str: String = row.get(1)?;
                let version: i64 = row.get(2)?;
                let content: String = row.get(3)?;
                let created_at_secs: i64 = row.get(4)?;
                let updated_at_secs: i64 = row.get(5)?;

                let config_type = ConfigType::parse(&config_type_str)
                    .ok_or_else(|| rusqlite::Error::InvalidParameterName(config_type_str))?;

                Ok(ConfigEntry {
                    id: id.clone(),
                    config_type,
                    version,
                    content,
                    created_at: UNIX_EPOCH + std::time::Duration::from_secs(created_at_secs as u64),
                    updated_at: UNIX_EPOCH + std::time::Duration::from_secs(updated_at_secs as u64),
                })
            })?;

            let mut entries = Vec::new();
            for row in rows {
                entries.push(row?);
            }

            Ok::<Vec<ConfigEntry>, anyhow::Error>(entries)
        })
        .await
        .context("Failed to spawn blocking task")??;

        // Build cache map
        let mut cache_map = HashMap::new();
        for entry in entries {
            let key = Self::make_cache_key(&entry.id, entry.config_type);
            cache_map.insert(key, entry);
        }

        let count = cache_map.len();

        // Update cache (zero-copy)
        self.cache.store(Arc::new(cache_map));

        debug!("Loaded {} configs from database", count);
        Ok(())
    }

    /// Save configuration
    pub async fn save_config(&self, entry: ConfigEntry) -> Result<()> {
        let _lock = self.write_lock.write().await;

        let db_path = self.db_path.clone();
        let entry_clone = entry.clone();

        // Save to database
        tokio::task::spawn_blocking(move || {
            let conn = rusqlite::Connection::open(&db_path)?;

            let created_at_secs = entry_clone
                .created_at
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;

            let updated_at_secs = entry_clone
                .updated_at
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;

            conn.execute(
                r#"
                INSERT OR REPLACE INTO configs
                (id, config_type, version, content, created_at, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                "#,
                rusqlite::params![
                    entry_clone.id,
                    entry_clone.config_type.as_str(),
                    entry_clone.version,
                    entry_clone.content,
                    created_at_secs,
                    updated_at_secs,
                ],
            )?;

            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("Failed to spawn blocking task")??;

        // Update cache
        self.update_cache(entry);

        Ok(())
    }

    /// Get configuration (zero-copy)
    pub fn get_config(&self, id: &str, config_type: ConfigType) -> Option<ConfigEntry> {
        let cache = self.cache.load();
        let key = Self::make_cache_key(id, config_type);
        cache.get(&key).cloned()
    }

    /// Get all configurations (by type)
    pub fn get_configs_by_type(&self, config_type: ConfigType) -> Vec<ConfigEntry> {
        let cache = self.cache.load();
        cache
            .values()
            .filter(|entry| entry.config_type == config_type)
            .cloned()
            .collect()
    }

    /// Delete configuration
    pub async fn delete_config(&self, id: &str, config_type: ConfigType) -> Result<bool> {
        let _lock = self.write_lock.write().await;

        let db_path = self.db_path.clone();
        let id_owned = id.to_string();
        let id_for_cache = id.to_string();
        let config_type_str = config_type.as_str().to_string();

        // Delete from database
        let deleted = tokio::task::spawn_blocking(move || {
            let conn = rusqlite::Connection::open(&db_path)?;
            let affected = conn.execute(
                "DELETE FROM configs WHERE id = ?1 AND config_type = ?2",
                rusqlite::params![id_owned, config_type_str],
            )?;
            Ok::<bool, anyhow::Error>(affected > 0)
        })
        .await
        .context("Failed to spawn blocking task")??;

        if deleted {
            // Update cache
            self.remove_from_cache(&id_for_cache, config_type);
        }

        Ok(deleted)
    }

    /// Update cache
    fn update_cache(&self, entry: ConfigEntry) {
        let key = Self::make_cache_key(&entry.id, entry.config_type);
        let entry_clone = entry.clone();
        self.cache.rcu(|cache| {
            let mut new_cache = (**cache).clone();
            new_cache.insert(key.clone(), entry_clone.clone());
            new_cache
        });
    }

    /// Remove from cache
    fn remove_from_cache(&self, id: &str, config_type: ConfigType) {
        let key = Self::make_cache_key(id, config_type);
        let key_clone = key.clone();
        self.cache.rcu(|cache| {
            let mut new_cache = (**cache).clone();
            new_cache.remove(&key_clone);
            new_cache
        });
    }

    /// Generate cache key
    fn make_cache_key(id: &str, config_type: ConfigType) -> String {
        format!("{}:{}", config_type.as_str(), id)
    }

    /// Get configuration count
    pub fn count(&self) -> usize {
        self.cache.load().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_config_store() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("test.db");

        let store = ConfigStore::new(&db_path).await?;

        // Save configuration
        let entry = ConfigEntry::new(
            "app1".to_string(),
            ConfigType::App,
            1,
            r#"{"key":"value"}"#.to_string(),
        );

        store.save_config(entry.clone()).await?;

        // Read configuration
        let loaded = store.get_config("app1", ConfigType::App);
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().content, r#"{"key":"value"}"#);

        // Delete configuration
        let deleted = store.delete_config("app1", ConfigType::App).await?;
        assert!(deleted);

        let loaded = store.get_config("app1", ConfigType::App);
        assert!(loaded.is_none());

        Ok(())
    }
}
