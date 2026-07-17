use serde::{Deserialize, Serialize};
use std::time::SystemTime;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AppState {
    Loading,
    Running,
    Stopped,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,

    pub limits: Option<ResourceLimits>,

    #[serde(default)]
    pub resources: Vec<String>,

    #[serde(default)]
    pub routes: Vec<String>,

    #[serde(default)]
    pub permissions: AppPermissions,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppPermissions {
    #[serde(default)]
    pub allow_net: Vec<String>,

    #[serde(default)]
    pub allow_env: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub memory_mb: usize,
    pub cpu_milli: u32,
    #[serde(default)]
    pub resource_table_max: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct App {
    pub id: String,
    pub state: AppState,
    pub config: AppConfig,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateAppRequest {
    pub id: String,
    pub code: String,

    #[serde(default)]
    pub config: AppConfig,
}

#[derive(Debug, Deserialize)]
pub struct UpdateAppRequest {
    pub code: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<AppConfig>,
}
