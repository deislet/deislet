use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// 配置类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigType {
    /// 全局配置
    Global,
    /// 应用配置
    App,
    /// 资源配置
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

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "global" => Some(ConfigType::Global),
            "app" => Some(ConfigType::App),
            "resource" => Some(ConfigType::Resource),
            _ => None,
        }
    }
}

/// 配置条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigEntry {
    /// 配置 ID（如 app_id 或 "global"）
    pub id: String,

    /// 配置类型
    pub config_type: ConfigType,

    /// 配置版本
    pub version: i64,

    /// 配置内容（JSON 字符串）
    pub content: String,

    /// 创建时间
    pub created_at: SystemTime,

    /// 更新时间
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
