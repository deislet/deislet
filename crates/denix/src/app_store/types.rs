// Application package type definitions

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::SystemTime;

/// Application manifest (manifest.json)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppManifest {
    /// Application ID
    pub id: String,
    /// Application name
    pub name: String,
    /// Application version
    pub version: String,
    /// Application description
    pub description: Option<String>,
    /// Entry point file (relative path within ZIP package)
    pub entry_point: String,
    /// Environment variables
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Resource dependencies
    #[serde(default)]
    pub resources: Vec<String>,
    /// Custom metadata
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

/// Application package status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppPackageStatus {
    /// Uploaded, not deployed yet
    Uploaded,
    /// Deployed and running
    Deployed,
    /// Stopped
    Stopped,
    /// Deployment failed
    Failed,
}

impl AppPackageStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AppPackageStatus::Uploaded => "uploaded",
            AppPackageStatus::Deployed => "deployed",
            AppPackageStatus::Stopped => "stopped",
            AppPackageStatus::Failed => "failed",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "uploaded" => Some(AppPackageStatus::Uploaded),
            "deployed" => Some(AppPackageStatus::Deployed),
            "stopped" => Some(AppPackageStatus::Stopped),
            "failed" => Some(AppPackageStatus::Failed),
            _ => None,
        }
    }
}

/// 应用包
#[derive(Debug, Clone)]
pub struct AppPackage {
    /// 应用 ID
    pub id: String,
    /// 应用清单
    pub manifest: AppManifest,
    /// ZIP 包数据
    pub package_data: Vec<u8>,
    /// 包大小（字节）
    pub package_size: i64,
    /// SHA256 校验和
    pub checksum: String,
    /// 状态
    pub status: AppPackageStatus,
    /// 上传时间
    pub uploaded_at: SystemTime,
    /// 部署时间
    pub deployed_at: Option<SystemTime>,
    /// 最后更新时间
    pub updated_at: SystemTime,
}

impl AppPackage {
    /// 创建新的应用包
    pub fn new(id: String, manifest: AppManifest, package_data: Vec<u8>) -> Self {
        use sha2::{Digest, Sha256};

        let checksum = {
            let mut hasher = Sha256::new();
            hasher.update(&package_data);
            format!("{:x}", hasher.finalize())
        };

        let now = SystemTime::now();

        Self {
            id,
            manifest,
            package_size: package_data.len() as i64,
            package_data,
            checksum,
            status: AppPackageStatus::Uploaded,
            uploaded_at: now,
            deployed_at: None,
            updated_at: now,
        }
    }

    /// 标记为已部署
    pub fn mark_deployed(&mut self) {
        self.status = AppPackageStatus::Deployed;
        self.deployed_at = Some(SystemTime::now());
        self.updated_at = SystemTime::now();
    }

    /// 标记为已停止
    pub fn mark_stopped(&mut self) {
        self.status = AppPackageStatus::Stopped;
        self.updated_at = SystemTime::now();
    }

    /// 标记为失败
    pub fn mark_failed(&mut self) {
        self.status = AppPackageStatus::Failed;
        self.updated_at = SystemTime::now();
    }
}
