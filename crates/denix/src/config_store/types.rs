use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// Configuration type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigType {
    /// Global configuration
    Global,
    /// Application configuration
    App,
    /// Resource configuration
    Resource,
}

impl ConfigType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConfigType::Global => "global",
            ConfigType::App => "app",
            ConfigType::Resource => "resource",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "global" => Some(ConfigType::Global),
            "app" => Some(ConfigType::App),
            "resource" => Some(ConfigType::Resource),
            _ => None,
        }
    }
}

/// Configuration entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigEntry {
    /// Configuration ID (e.g., app_id or "global")
    pub id: String,

    /// Configuration type
    pub config_type: ConfigType,

    /// Configuration version
    pub version: i64,

    /// Configuration content (JSON string)
    pub content: String,

    /// Creation time
    pub created_at: SystemTime,

    /// Update time
    pub updated_at: SystemTime,
}

impl ConfigEntry {
    pub fn new(id: String, config_type: ConfigType, version: i64, content: String) -> Self {
        let now = SystemTime::now();
        Self {
            id,
            config_type,
            version,
            content,
            created_at: now,
            updated_at: now,
        }
    }
}
