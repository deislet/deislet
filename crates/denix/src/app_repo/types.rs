use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::SystemTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub entry_point: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub resources: Vec<String>,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppPackageStatus {
    Uploaded,
    Deployed,
    Stopped,
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

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "uploaded" => Some(AppPackageStatus::Uploaded),
            "deployed" => Some(AppPackageStatus::Deployed),
            "stopped" => Some(AppPackageStatus::Stopped),
            "failed" => Some(AppPackageStatus::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppPackage {
    pub id: String,
    pub manifest: AppManifest,
    pub package_size: i64,
    pub checksum: String,
    pub status: AppPackageStatus,
    pub uploaded_at: SystemTime,
    pub deployed_at: Option<SystemTime>,
    pub updated_at: SystemTime,
}

impl AppPackage {
    pub fn new(id: String, manifest: AppManifest, size: i64, checksum: String) -> Self {
        let now = SystemTime::now();
        Self {
            id,
            manifest,
            package_size: size,
            checksum,
            status: AppPackageStatus::Uploaded,
            uploaded_at: now,
            deployed_at: None,
            updated_at: now,
        }
    }

    pub fn mark_deployed(&mut self) {
        self.status = AppPackageStatus::Deployed;
        self.deployed_at = Some(SystemTime::now());
        self.updated_at = SystemTime::now();
    }

    pub fn mark_stopped(&mut self) {
        self.status = AppPackageStatus::Stopped;
        self.updated_at = SystemTime::now();
    }

    pub fn mark_failed(&mut self) {
        self.status = AppPackageStatus::Failed;
        self.updated_at = SystemTime::now();
    }
}
