use thiserror::Error;

/// Static configuration error types
#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Failed to read config file: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Failed to parse toml file: {0}")]
    TomlParseError(#[from] toml::de::Error),

    #[error("Failed to parse json file: {0}")]
    JsonParseError(#[from] serde_json::Error),

    #[error("Failed to parse cache key: {0}")]
    CacheKeyParseError(String),

    #[error("Invalid configuration: {message}")]
    ValidationError { message: String },

    #[error("Database error: {0}")]
    DatabaseError(#[from] crate::config::manager::database::DatabaseError),

    #[error("Supervisor error: {0}")]
    SupervisorError(String),
}
