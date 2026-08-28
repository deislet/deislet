use rusqlite::{Connection, params};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use thiserror::Error;

use chrono::{DateTime, Utc};

/// App configuration entry for database storage
#[derive(Debug, Clone)]
pub struct AppConfigEntry {
    pub app_id: String,
    pub config: JsonValue,
    pub version_timestamp: DateTime<Utc>,
}

/// Database error types
#[derive(Error, Debug)]
pub enum DatabaseError {
    #[error("SQLite error: {0}")]
    SqliteError(#[from] rusqlite::Error),

    #[error("Configuration validation error: {0}")]
    ValidationError(String),

    #[error("Database connection error: {message}")]
    ConnectionError { message: String },

    #[error("Parse error: {message}")]
    ParseError { message: String },

    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
}

/// Database statistics including counts of stored data
#[derive(Debug, Clone)]
pub struct DatabaseStats {
    pub apps_count: u32,
    pub latest_version: DateTime<Utc>,
}

/// Database manager for app configurations
pub struct ConfigDatabase {
    connection: Mutex<Connection>,
}

impl ConfigDatabase {
    /// Initialize the database with the given path
    ///
    /// This function creates the database file if it doesn't exist and
    /// ensures all required tables are created.
    ///
    /// # Arguments
    /// * `path` - Path to the SQLite database file
    ///
    /// # Returns
    /// * `Ok(ConfigDatabase)` - Successfully initialized database
    /// * `Err(DatabaseError)` - Failed to initialize database
    pub async fn init<P: AsRef<Path>>(path: P) -> Result<Self, DatabaseError> {
        let connection = Connection::open(path)?;

        let db = Self {
            connection: Mutex::new(connection),
        };
        db.create_tables().await?;

        Ok(db)
    }

    /// Create all required database tables
    async fn create_tables(&self) -> Result<(), DatabaseError> {
        let conn = self
            .connection
            .lock()
            .map_err(|_| DatabaseError::ConnectionError {
                message: "Database connection mutex poisoned during table creation".to_string(),
            })?;

        // Enable foreign key constraints
        conn.execute("PRAGMA foreign_keys = ON", [])?;

        // Create simplified app_configs table
        conn.execute(
            r#"
            CREATE TABLE IF NOT EXISTS app_configs (
                app_id TEXT PRIMARY KEY,
                config TEXT NOT NULL,
                version_timestamp TEXT NOT NULL
            )
            "#,
            [],
        )?;

        // Create index for version-based queries
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_app_configs_version ON app_configs (version_timestamp)",
            [],
        )?;

        Ok(())
    }

    /// Load all app configurations as a HashMap
    ///
    /// # Returns
    /// * `Ok(HashMap<String, JsonValue>)` - All app configs mapped by app_id
    /// * `Err(DatabaseError)` - Database error
    pub async fn load_all_app_configs(&self) -> Result<HashMap<String, JsonValue>, DatabaseError> {
        let conn = self
            .connection
            .lock()
            .map_err(|_| DatabaseError::ConnectionError {
                message: "Database connection mutex poisoned during app configs load".to_string(),
            })?;
        let mut stmt = conn.prepare("SELECT app_id, config FROM app_configs")?;

        let app_rows = stmt.query_map([], |row| {
            let app_id: String = row.get(0)?;
            let config_str: String = row.get(1)?;
            Ok((app_id, config_str))
        })?;

        let mut configs = HashMap::new();
        for row_result in app_rows {
            let (app_id, config_str) = row_result?;
            let config: JsonValue = serde_json::from_str(&config_str)?;
            configs.insert(app_id, config);
        }

        Ok(configs)
    }

    /// Get the latest version timestamp from all app configs
    ///
    /// # Returns
    /// * `Ok(timestamp)` - Latest version timestamp found, or Unix epoch if no configs
    /// * `Err(DatabaseError)` - Database error
    pub async fn get_latest_version_timestamp(&self) -> Result<DateTime<Utc>, DatabaseError> {
        let conn = self
            .connection
            .lock()
            .map_err(|_| DatabaseError::ConnectionError {
                message: "Database connection mutex poisoned during version timestamp lookup"
                    .to_string(),
            })?;
        let mut stmt = conn.prepare("SELECT MAX(version_timestamp) FROM app_configs")?;

        let timestamp_str: Option<String> =
            stmt.query_row([], |row| row.get::<_, Option<String>>(0))?;

        match timestamp_str {
            Some(ts) => {
                let timestamp = DateTime::parse_from_rfc3339(&ts)
                    .map_err(|e| DatabaseError::ValidationError(format!("Invalid timestamp: {e}")))?
                    .with_timezone(&Utc);
                Ok(timestamp)
            }
            None => {
                // Return Unix epoch if no configs exist
                Ok(
                    DateTime::from_timestamp(0, 0).ok_or_else(|| DatabaseError::ParseError {
                        message: "Failed to create Unix epoch timestamp".to_string(),
                    })?,
                )
            }
        }
    }

    /// Apply incremental app config updates in a transaction with callback
    ///
    /// # Arguments
    /// * `updates` - List of app config updates to apply
    /// * `update_callback` - Callback function to update external state (like ArcSwap)
    ///
    /// # Returns
    /// * `Ok(DateTime<Utc>)` - Latest version timestamp
    /// * `Err(DatabaseError)` - Database transaction failed
    pub async fn apply_incremental_updates<F>(
        &mut self,
        updates: Vec<AppConfigEntry>,
        update_callback: F,
    ) -> Result<DateTime<Utc>, DatabaseError>
    where
        F: FnOnce(&HashMap<String, JsonValue>) -> Result<(), DatabaseError>,
    {
        if updates.is_empty() {
            return Err(DatabaseError::ValidationError(
                "No updates provided".to_string(),
            ));
        }

        let conn = self
            .connection
            .lock()
            .map_err(|_| DatabaseError::ConnectionError {
                message: "Database connection mutex poisoned during incremental updates"
                    .to_string(),
            })?;
        let tx = conn.unchecked_transaction()?;

        // Replace into the updated app configs (duplicate by key app_id)
        for update in &updates {
            let version_str = update.version_timestamp.to_rfc3339();
            let config_str = serde_json::to_string(&update.config)?;

            tx.execute(
                r#"
                INSERT OR REPLACE INTO app_configs 
                (app_id, config, version_timestamp) 
                VALUES (?1, ?2, ?3)
                "#,
                params![update.app_id, config_str, version_str,],
            )?;
        }

        // Get all app configs for callback in separate scope
        let app_configs = {
            let mut stmt = tx.prepare("SELECT app_id, config FROM app_configs")?;
            let app_rows = stmt.query_map([], |row| {
                let app_id: String = row.get(0)?;
                let config_str: String = row.get(1)?;
                Ok((app_id, config_str))
            })?;

            let mut configs = HashMap::new();
            for row_result in app_rows {
                let (app_id, config_str) = row_result?;
                let config: JsonValue = serde_json::from_str(&config_str)?;
                configs.insert(app_id, config);
            }
            configs
        }; // stmt is dropped here

        // Update external state (ArcSwap cache)
        update_callback(&app_configs)?;

        // transaction commit
        tx.commit()?;

        // Return the latest version timestamp
        let latest_version = updates
            .iter()
            .map(|update| update.version_timestamp)
            .max()
            .ok_or_else(|| {
                DatabaseError::ValidationError(
                    "No updates provided to calculate latest version".to_string(),
                )
            })?;

        Ok(latest_version)
    }

    /// Get app configs updated after a specific timestamp
    ///
    /// This is used for incremental updates from supervisor
    ///
    /// # Arguments
    /// * `after_timestamp` - Only return configs updated after this timestamp
    ///
    /// # Returns
    /// * `Ok(Vec<AppConfigEntry>)` - App configs updated after the timestamp
    /// * `Err(DatabaseError)` - Database error
    pub async fn get_app_configs_after(
        &self,
        after_timestamp: DateTime<Utc>,
    ) -> Result<Vec<AppConfigEntry>, DatabaseError> {
        let after_str = after_timestamp.to_rfc3339();

        let conn = self
            .connection
            .lock()
            .map_err(|_| DatabaseError::ConnectionError {
                message: "Database connection mutex poisoned during incremental config query"
                    .to_string(),
            })?;
        let mut stmt = conn.prepare(
            "SELECT app_id, config, version_timestamp FROM app_configs WHERE version_timestamp > ?1 ORDER BY version_timestamp"
        )?;

        let app_rows = stmt.query_map([after_str], |row| {
            let app_id: String = row.get(0)?;
            let config_str: String = row.get(1)?;
            let version_str: String = row.get(2)?;
            Ok((app_id, config_str, version_str))
        })?;

        let mut configs = Vec::new();
        for row_result in app_rows {
            let (app_id, config_str, version_str) = row_result?;
            let config: JsonValue = serde_json::from_str(&config_str)?;
            let version_timestamp = DateTime::parse_from_rfc3339(&version_str)
                .map_err(|e| DatabaseError::ValidationError(format!("Invalid timestamp: {e}")))?
                .with_timezone(&Utc);

            configs.push(AppConfigEntry {
                app_id,
                config,
                version_timestamp,
            });
        }

        Ok(configs)
    }

    /// Save all app configurations (used for bulk updates)
    ///
    /// # Arguments
    /// * `configs` - HashMap of all app configurations
    /// * `version_timestamp` - Timestamp for this version
    ///
    /// # Returns
    /// * `Ok(())` - Successfully saved
    /// * `Err(DatabaseError)` - Save operation failed
    pub async fn save_all_app_configs(
        &mut self,
        configs: &HashMap<String, JsonValue>,
        version_timestamp: DateTime<Utc>,
    ) -> Result<(), DatabaseError> {
        let conn = self
            .connection
            .lock()
            .map_err(|_| DatabaseError::ConnectionError {
                message: "Database connection mutex poisoned during system config save".to_string(),
            })?;
        let tx = conn.unchecked_transaction()?;

        let version_str = version_timestamp.to_rfc3339();

        // Clear existing data and insert new
        tx.execute("DELETE FROM app_configs", [])?;

        for (app_id, config) in configs {
            let config_str = serde_json::to_string(config)?;

            tx.execute(
                r#"
                INSERT INTO app_configs 
                (app_id, config, version_timestamp) 
                VALUES (?1, ?2, ?3)
                "#,
                params![app_id, config_str, version_str,],
            )?;
        }

        tx.commit()?;

        Ok(())
    }

    /// Get database statistics
    ///
    /// # Returns
    /// Database statistics including counts of stored data
    pub async fn get_stats(&self) -> Result<DatabaseStats, DatabaseError> {
        // Scope the connection guard so it is released before the await below:
        // `get_latest_version_timestamp` locks the same mutex, so holding the
        // guard across that call would deadlock (and trip `await_holding_lock`).
        let apps_count: u32 = {
            let conn = self
                .connection
                .lock()
                .map_err(|_| DatabaseError::ConnectionError {
                    message: "Database connection mutex poisoned during stats query".to_string(),
                })?;
            conn.query_row("SELECT COUNT(*) FROM app_configs", [], |row| row.get(0))?
        };

        let latest_version = self.get_latest_version_timestamp().await?;

        Ok(DatabaseStats {
            apps_count,
            latest_version,
        })
    }
}
