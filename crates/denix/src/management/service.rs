use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::app::{ApplicationManager, AppConfig as InternalAppConfig};
use crate::config_store::{ConfigEntry, ConfigStore, ConfigType};

use super::proto::denix_management_server::DenixManagement;
use super::proto::*;

/// Denix 管理服务实现
pub struct DenixManagementService {
    app_manager: Arc<ApplicationManager>,
    config_store: Arc<ConfigStore>,
    worker_id: String,
}

impl DenixManagementService {
    pub fn new(
        app_manager: Arc<ApplicationManager>,
        config_store: Arc<ConfigStore>,
        worker_id: String,
    ) -> Self {
        Self {
            app_manager,
            config_store,
            worker_id,
        }
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
        info!("Deploying app: {}", req.app_id);

        // 转换配置
        let config = InternalAppConfig::default(); // TODO: 转换 req.config

        // 部署应用
        match self
            .app_manager
            .load_app(req.app_id.clone(), req.code, config)
            .await
        {
            Ok(_) => {
                info!("App {} deployed successfully", req.app_id);
                Ok(Response::new(DeployAppResponse {
                    success: true,
                    message: "App deployed successfully".to_string(),
                    app_id: req.app_id,
                }))
            }
            Err(e) => {
                warn!("Failed to deploy app {}: {}", req.app_id, e);
                Ok(Response::new(DeployAppResponse {
                    success: false,
                    message: format!("Failed to deploy app: {}", e),
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
        info!("Unloading app: {}", req.app_id);

        match self.app_manager.unload_app(&req.app_id).await {
            Ok(_) => {
                info!("App {} unloaded successfully", req.app_id);
                Ok(Response::new(UnloadAppResponse {
                    success: true,
                    message: "App unloaded successfully".to_string(),
                }))
            }
            Err(e) => {
                warn!("Failed to unload app {}: {}", req.app_id, e);
                Ok(Response::new(UnloadAppResponse {
                    success: false,
                    message: format!("Failed to unload app: {}", e),
                }))
            }
        }
    }

    async fn update_app(
        &self,
        request: Request<UpdateAppRequest>,
    ) -> Result<Response<UpdateAppResponse>, Status> {
        let req = request.into_inner();
        info!("Updating app: {}", req.app_id);

        let config = req.config.map(|_| InternalAppConfig::default()); // TODO: 转换

        match self
            .app_manager
            .update_app(&req.app_id, req.code, config)
            .await
        {
            Ok(_) => {
                info!("App {} updated successfully", req.app_id);
                Ok(Response::new(UpdateAppResponse {
                    success: true,
                    message: "App updated successfully".to_string(),
                }))
            }
            Err(e) => {
                warn!("Failed to update app {}: {}", req.app_id, e);
                Ok(Response::new(UpdateAppResponse {
                    success: false,
                    message: format!("Failed to update app: {}", e),
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
