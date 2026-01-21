// 应用类型定义

use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// 应用状态
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AppState {
    Loading,
    Running,
    Stopped,
    Error,
}

/// 应用配置
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,

    pub limits: Option<ResourceLimits>,

    #[serde(default)]
    pub resources: Vec<String>,

    #[serde(default)]
    pub routes: Vec<String>,
}

/// 资源限制
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub memory_mb: usize,
    pub cpu_milli: u32,
}

/// 应用信息
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

/// 创建应用请求
#[derive(Debug, Deserialize)]
pub struct CreateAppRequest {
    pub id: String,
    pub code: String,

    #[serde(default)]
    pub config: AppConfig,
}

/// 更新应用请求
#[derive(Debug, Deserialize)]
pub struct UpdateAppRequest {
    pub code: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<AppConfig>,
}
