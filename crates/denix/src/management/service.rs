use std::io::{Cursor, Read};
use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::{error, info, warn};
use zip::ZipArchive;

use crate::app::{AppConfig as InternalAppConfig, ApplicationManager};
use crate::app_store::{AppManifest, AppPackage, AppPackageStatus, AppStore};
use crate::config_store::{ConfigEntry, ConfigStore};

use super::proto::denix_management_server::DenixManagement;
use super::proto::*;

/// Denix 管理服务实现
pub struct DenixManagementService {
    app_manager: Arc<ApplicationManager>,
    config_store: Arc<ConfigStore>,
    app_store: Arc<AppStore>,
    worker_id: String,
}

impl DenixManagementService {
    pub fn new(
        app_manager: Arc<ApplicationManager>,
        config_store: Arc<ConfigStore>,
        app_store: Arc<AppStore>,
        worker_id: String,
    ) -> Self {
        Self {
            app_manager,
            config_store,
            app_store,
            worker_id,
        }
    }

    /// 从 ZIP 包中提取 manifest.json
    fn extract_manifest(package_data: &[u8]) -> Result<AppManifest, String> {
        let cursor = Cursor::new(package_data);
        let mut archive = ZipArchive::new(cursor).map_err(|e| format!("无效的 ZIP 包: {}", e))?;

        // 查找 manifest.json
        let mut manifest_file = archive
            .by_name("manifest.json")
            .map_err(|_| "ZIP 包中缺少 manifest.json".to_string())?;

        let mut manifest_content = String::new();
        manifest_file
            .read_to_string(&mut manifest_content)
            .map_err(|e| format!("读取 manifest.json 失败: {}", e))?;

        serde_json::from_str(&manifest_content)
            .map_err(|e| format!("manifest.json 格式错误: {}", e))
    }

    /// 从 ZIP 包中提取入口文件代码
    fn extract_entry_code(package_data: &[u8], entry_point: &str) -> Result<String, String> {
        let cursor = Cursor::new(package_data);
        let mut archive = ZipArchive::new(cursor).map_err(|e| format!("无效的 ZIP 包: {}", e))?;

        let mut entry_file = archive
            .by_name(entry_point)
            .map_err(|_| format!("入口文件不存在: {}", entry_point))?;

        let mut code = String::new();
        entry_file
            .read_to_string(&mut code)
            .map_err(|e| format!("读取入口文件失败: {}", e))?;

        Ok(code)
    }
}

#[tonic::async_trait]
impl DenixManagement for DenixManagementService {
    async fn push_config(
        &self,
        request: Request<PushConfigRequest>,
    ) -> Result<Response<PushConfigResponse>, Status> {
        let req = request.into_inner();
        info!(
            "Received config push: type={:?}, version={}, force={}",
            req.config_type, req.version, req.force
        );

        // 转换配置类型
        let config_type = match super::proto::ConfigType::try_from(req.config_type) {
            Ok(super::proto::ConfigType::Global) => crate::config_store::ConfigType::Global,
            Ok(super::proto::ConfigType::App) => crate::config_store::ConfigType::App,
            Ok(super::proto::ConfigType::Resource) => crate::config_store::ConfigType::Resource,
            _ => {
                return Ok(Response::new(PushConfigResponse {
                    success: false,
                    message: "Invalid config type".to_string(),
                    applied_version: 0,
                }));
            }
        };

        // 创建配置条目
        let entry = ConfigEntry::new(
            "global".to_string(), // TODO: 根据 config_type 确定 ID
            config_type,
            req.version,
            req.config_json,
        );

        // 保存配置
        match self.config_store.save_config(entry).await {
            Ok(_) => {
                info!("Config saved successfully, version: {}", req.version);
                Ok(Response::new(PushConfigResponse {
                    success: true,
                    message: "Config applied successfully".to_string(),
                    applied_version: req.version,
                }))
            }
            Err(e) => {
                warn!("Failed to save config: {}", e);
                Ok(Response::new(PushConfigResponse {
                    success: false,
                    message: format!("Failed to save config: {}", e),
                    applied_version: 0,
                }))
            }
        }
    }

    async fn deploy_app(
        &self,
        request: Request<DeployAppRequest>,
    ) -> Result<Response<DeployAppResponse>, Status> {
        let req = request.into_inner();
        info!(
            "部署应用: {} (包大小: {} 字节)",
            req.app_id,
            req.package_data.len()
        );

        // 1. 提取 manifest
        let manifest = match Self::extract_manifest(&req.package_data) {
            Ok(m) => m,
            Err(e) => {
                error!("提取 manifest 失败: {}", e);
                return Ok(Response::new(DeployAppResponse {
                    success: false,
                    message: format!("提取 manifest 失败: {}", e),
                    app_id: req.app_id,
                }));
            }
        };

        // 验证应用 ID 匹配
        if manifest.id != req.app_id {
            warn!(
                "应用 ID 不匹配: 请求={}, manifest={}",
                req.app_id, manifest.id
            );
            return Ok(Response::new(DeployAppResponse {
                success: false,
                message: format!(
                    "应用 ID 不匹配: 请求={}, manifest={}",
                    req.app_id, manifest.id
                ),
                app_id: req.app_id,
            }));
        }

        // 2. 创建应用包
        let app_package = AppPackage::new(
            req.app_id.clone(),
            manifest.clone(),
            req.package_data.clone(),
        );

        // 3. 保存到 AppStore
        if let Err(e) = self.app_store.save_package(app_package).await {
            error!("保存应用包失败: {}", e);
            return Ok(Response::new(DeployAppResponse {
                success: false,
                message: format!("保存应用包失败: {}", e),
                app_id: req.app_id,
            }));
        }

        // 4. 提取入口文件代码
        let entry_code = match Self::extract_entry_code(&req.package_data, &manifest.entry_point) {
            Ok(code) => code,
            Err(e) => {
                error!("提取入口文件失败: {}", e);
                // 标记应用为失败状态
                let _ = self
                    .app_store
                    .update_status(&req.app_id, AppPackageStatus::Failed)
                    .await;
                return Ok(Response::new(DeployAppResponse {
                    success: false,
                    message: format!("提取入口文件失败: {}", e),
                    app_id: req.app_id,
                }));
            }
        };

        // 5. 转换配置
        let config = InternalAppConfig::default(); // TODO: 从 manifest 和 req.config 合并配置

        // 6. 部署应用
        match self
            .app_manager
            .load_app(req.app_id.clone(), entry_code, config)
            .await
        {
            Ok(_) => {
                info!("应用 {} 部署成功", req.app_id);
                // 标记应用为已部署状态
                let _ = self
                    .app_store
                    .update_status(&req.app_id, AppPackageStatus::Deployed)
                    .await;
                Ok(Response::new(DeployAppResponse {
                    success: true,
                    message: "应用部署成功".to_string(),
                    app_id: req.app_id,
                }))
            }
            Err(e) => {
                error!("应用 {} 部署失败: {}", req.app_id, e);
                // 标记应用为失败状态
                let _ = self
                    .app_store
                    .update_status(&req.app_id, AppPackageStatus::Failed)
                    .await;
                Ok(Response::new(DeployAppResponse {
                    success: false,
                    message: format!("部署应用失败: {}", e),
                    app_id: req.app_id,
                }))
            }
        }
    }

    async fn unload_app(
        &self,
        request: Request<UnloadAppRequest>,
    ) -> Result<Response<UnloadAppResponse>, Status> {
        let req = request.into_inner();
        info!("卸载应用: {}", req.app_id);

        match self.app_manager.unload_app(&req.app_id).await {
            Ok(_) => {
                info!("应用 {} 卸载成功", req.app_id);
                // 更新应用包状态为已停止
                let _ = self
                    .app_store
                    .update_status(&req.app_id, AppPackageStatus::Stopped)
                    .await;
                Ok(Response::new(UnloadAppResponse {
                    success: true,
                    message: "应用卸载成功".to_string(),
                }))
            }
            Err(e) => {
                warn!("应用 {} 卸载失败: {}", req.app_id, e);
                Ok(Response::new(UnloadAppResponse {
                    success: false,
                    message: format!("卸载应用失败: {}", e),
                }))
            }
        }
    }

    async fn update_app(
        &self,
        request: Request<UpdateAppRequest>,
    ) -> Result<Response<UpdateAppResponse>, Status> {
        let req = request.into_inner();
        info!(
            "更新应用: {} (包大小: {} 字节)",
            req.app_id,
            req.package_data.len()
        );

        // 1. 提取 manifest
        let manifest = match Self::extract_manifest(&req.package_data) {
            Ok(m) => m,
            Err(e) => {
                error!("提取 manifest 失败: {}", e);
                return Ok(Response::new(UpdateAppResponse {
                    success: false,
                    message: format!("提取 manifest 失败: {}", e),
                }));
            }
        };

        // 验证应用 ID 匹配
        if manifest.id != req.app_id {
            warn!(
                "应用 ID 不匹配: 请求={}, manifest={}",
                req.app_id, manifest.id
            );
            return Ok(Response::new(UpdateAppResponse {
                success: false,
                message: format!(
                    "应用 ID 不匹配: 请求={}, manifest={}",
                    req.app_id, manifest.id
                ),
            }));
        }

        // 2. 创建应用包
        let app_package = AppPackage::new(
            req.app_id.clone(),
            manifest.clone(),
            req.package_data.clone(),
        );

        // 3. 保存到 AppStore（会覆盖现有包）
        if let Err(e) = self.app_store.save_package(app_package).await {
            error!("保存应用包失败: {}", e);
            return Ok(Response::new(UpdateAppResponse {
                success: false,
                message: format!("保存应用包失败: {}", e),
            }));
        }

        // 4. 提取入口文件代码
        let entry_code = match Self::extract_entry_code(&req.package_data, &manifest.entry_point) {
            Ok(code) => code,
            Err(e) => {
                error!("提取入口文件失败: {}", e);
                let _ = self
                    .app_store
                    .update_status(&req.app_id, AppPackageStatus::Failed)
                    .await;
                return Ok(Response::new(UpdateAppResponse {
                    success: false,
                    message: format!("提取入口文件失败: {}", e),
                }));
            }
        };

        // 5. 转换配置
        let config = req.config.map(|_| InternalAppConfig::default()); // TODO: 从 manifest 和 req.config 合并

        // 6. 更新应用
        match self
            .app_manager
            .update_app(&req.app_id, entry_code, config)
            .await
        {
            Ok(_) => {
                info!("应用 {} 更新成功", req.app_id);
                let _ = self
                    .app_store
                    .update_status(&req.app_id, AppPackageStatus::Deployed)
                    .await;
                Ok(Response::new(UpdateAppResponse {
                    success: true,
                    message: "应用更新成功".to_string(),
                }))
            }
            Err(e) => {
                error!("应用 {} 更新失败: {}", req.app_id, e);
                let _ = self
                    .app_store
                    .update_status(&req.app_id, AppPackageStatus::Failed)
                    .await;
                Ok(Response::new(UpdateAppResponse {
                    success: false,
                    message: format!("更新应用失败: {}", e),
                }))
            }
        }
    }

    async fn get_status(
        &self,
        request: Request<GetStatusRequest>,
    ) -> Result<Response<GetStatusResponse>, Status> {
        let req = request.into_inner();
        info!("Status query received, include_apps: {}", req.include_apps);

        // Worker 基本信息
        let worker_info = Some(WorkerInfo {
            worker_id: self.worker_id.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: 0, // TODO: 计算实际 uptime
            hostname: hostname::get()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
        });

        // 应用列表
        let apps = if req.include_apps {
            self.app_manager
                .list_apps()
                .into_iter()
                .map(|app| AppStatus {
                    app_id: app.id,
                    state: match app.state {
                        crate::app::AppState::Loading => AppState::Loading as i32,
                        crate::app::AppState::Running => AppState::Running as i32,
                        crate::app::AppState::Stopped => AppState::Stopped as i32,
                        crate::app::AppState::Error => AppState::Error as i32,
                    },
                    request_count: 0, // TODO: 实际统计
                    error_count: 0,   // TODO: 实际统计
                    error_message: app.error.unwrap_or_default(),
                })
                .collect()
        } else {
            Vec::new()
        };

        // 资源使用情况（简化版）
        let resource_usage = Some(ResourceUsage {
            cpu_percent: 0.0, // TODO: 实际 CPU 使用率
            memory_bytes: 0,  // TODO: 实际内存使用
            goroutines: 0,    // Rust 没有 goroutines，可以返回线程数
        });

        Ok(Response::new(GetStatusResponse {
            worker_info,
            apps,
            resource_usage,
        }))
    }

    async fn execute_command(
        &self,
        request: Request<ExecuteCommandRequest>,
    ) -> Result<Response<ExecuteCommandResponse>, Status> {
        let req = request.into_inner();
        warn!(
            "Execute command received (not implemented): {}",
            req.command
        );

        // TODO: 实现命令执行逻辑
        Ok(Response::new(ExecuteCommandResponse {
            success: false,
            output: String::new(),
            error: "Command execution not implemented".to_string(),
        }))
    }

    async fn health_check(
        &self,
        _request: Request<HealthCheckRequest>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        Ok(Response::new(HealthCheckResponse {
            healthy: true,
            version: env!("CARGO_PKG_VERSION").to_string(),
            details: Default::default(),
        }))
    }
}
