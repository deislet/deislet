use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::{error, info, warn};

use crate::app::{AppConfig as InternalAppConfig, ApplicationManager};
use crate::app_repo::{AppPackageStatus, AppRepo};
use crate::config_store::{ConfigEntry, ConfigStore};

use super::proto::denix_management_server::DenixManagement;
use super::proto::*;

/// Denix 管理服务实现
pub struct DenixManagementService {
    app_manager: Arc<ApplicationManager>,
    config_store: Arc<ConfigStore>,
    app_repo: Arc<AppRepo>,
    worker_id: String,
}

fn map_proto_limits(
    limits: Option<super::proto::ResourceLimits>,
) -> Option<crate::app::types::ResourceLimits> {
    limits.map(|limits| {
        let memory_mb = if limits.memory_mb < 0 {
            warn!("ResourceLimits.memory_mb is negative: {}", limits.memory_mb);
            0
        } else {
            limits.memory_mb as usize
        };

        let cpu_milli = if limits.cpu_milli < 0 {
            warn!("ResourceLimits.cpu_milli is negative: {}", limits.cpu_milli);
            0
        } else {
            limits.cpu_milli as u32
        };

        let resource_table_max = if limits.resource_table_max > 0 {
            Some(limits.resource_table_max as usize)
        } else {
            None
        };

        crate::app::types::ResourceLimits {
            memory_mb,
            cpu_milli,
            resource_table_max,
        }
    })
}

fn map_proto_config(config: Option<super::proto::AppConfig>) -> InternalAppConfig {
    match config {
        Some(config) => {
            let mut internal = InternalAppConfig {
                env: config.env,
                limits: map_proto_limits(config.limits),
                ..Default::default()
            };

            if !config.routes.is_empty() {
                let mut routes = Vec::new();
                for route in config.routes {
                    if !route.methods.is_empty() {
                        warn!(
                            "Route methods are ignored in current runtime: path={}, methods={:?}",
                            route.path, route.methods
                        );
                    }
                    routes.push(route.path);
                }
                internal.routes = routes;
            }

            internal
        }
        None => InternalAppConfig::default(),
    }
}

impl DenixManagementService {
    pub fn new(
        app_manager: Arc<ApplicationManager>,
        config_store: Arc<ConfigStore>,
        app_repo: Arc<AppRepo>,
        worker_id: String,
    ) -> Self {
        Self {
            app_manager,
            config_store,
            app_repo,
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
        info!(
            "部署应用: {} (包大小: {} 字节)",
            req.app_id,
            req.package_data.len()
        );

        let package = match self
            .app_repo
            .install_package(req.app_id.clone(), req.package_data)
            .await
        {
            Ok(pkg) => pkg,
            Err(e) => {
                error!("安装应用包失败: {}", e);
                return Ok(Response::new(DeployAppResponse {
                    success: false,
                    message: format!("安装应用包失败: {}", e),
                    app_id: req.app_id,
                }));
            }
        };

        let entry_point = &package.manifest.entry_point;
        let entry_code = match self.app_repo.get_file(&req.app_id, entry_point).await {
            Ok(Some(bytes)) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(e) => {
                    error!("入口文件不是有效的 UTF-8: {}", e);
                    return Ok(Response::new(DeployAppResponse {
                        success: false,
                        message: format!("入口文件不是有效的 UTF-8: {}", e),
                        app_id: req.app_id,
                    }));
                }
            },
            Ok(None) => {
                error!("入口文件不存在: {}", entry_point);
                return Ok(Response::new(DeployAppResponse {
                    success: false,
                    message: format!("入口文件不存在: {}", entry_point),
                    app_id: req.app_id,
                }));
            }
            Err(e) => {
                error!("读取入口文件失败: {}", e);
                return Ok(Response::new(DeployAppResponse {
                    success: false,
                    message: format!("读取入口文件失败: {}", e),
                    app_id: req.app_id,
                }));
            }
        };

        let config = map_proto_config(req.config);

        match self
            .app_manager
            .load_app(
                req.app_id.clone(),
                entry_code,
                config,
                Some(entry_point.clone()),
            )
            .await
        {
            Ok(_) => {
                info!("应用 {} 部署成功", req.app_id);
                let _ = self
                    .app_repo
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
                let _ = self
                    .app_repo
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
                let _ = self
                    .app_repo
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

        let package = match self
            .app_repo
            .install_package(req.app_id.clone(), req.package_data)
            .await
        {
            Ok(pkg) => pkg,
            Err(e) => {
                error!("安装应用包失败: {}", e);
                return Ok(Response::new(UpdateAppResponse {
                    success: false,
                    message: format!("安装应用包失败: {}", e),
                }));
            }
        };

        let entry_point = &package.manifest.entry_point;
        let entry_code = match self.app_repo.get_file(&req.app_id, entry_point).await {
            Ok(Some(bytes)) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(e) => {
                    error!("入口文件不是有效的 UTF-8: {}", e);
                    return Ok(Response::new(UpdateAppResponse {
                        success: false,
                        message: format!("入口文件不是有效的 UTF-8: {}", e),
                    }));
                }
            },
            Ok(None) => {
                error!("入口文件不存在: {}", entry_point);
                return Ok(Response::new(UpdateAppResponse {
                    success: false,
                    message: format!("入口文件不存在: {}", entry_point),
                }));
            }
            Err(e) => {
                error!("读取入口文件失败: {}", e);
                return Ok(Response::new(UpdateAppResponse {
                    success: false,
                    message: format!("读取入口文件失败: {}", e),
                }));
            }
        };

        let config = req.config.map(|config| map_proto_config(Some(config)));

        match self
            .app_manager
            .update_app(&req.app_id, entry_code, config, Some(entry_point.clone()))
            .await
        {
            Ok(_) => {
                info!("应用 {} 更新成功", req.app_id);
                let _ = self
                    .app_repo
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
                    .app_repo
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

        let op_metrics = self.app_manager.get_op_metrics().await;
        let op_metrics = op_metrics
            .into_iter()
            .map(|(app_id, summary)| AppOpMetrics {
                app_id,
                summary: Some(OpMetricsSummary {
                    ops_dispatched_sync: summary.ops_dispatched_sync,
                    ops_dispatched_async: summary.ops_dispatched_async,
                    ops_dispatched_fast: summary.ops_dispatched_fast,
                    ops_completed_async: summary.ops_completed_async,
                }),
            })
            .collect::<Vec<_>>();

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
            op_metrics,
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

    async fn start_inspect(
        &self,
        request: Request<StartInspectRequest>,
    ) -> Result<Response<StartInspectResponse>, Status> {
        let req = request.into_inner();
        let ttl = if req.ttl_seconds <= 0 {
            300
        } else {
            req.ttl_seconds as u64
        };

        info!("Start inspect for app: {}, ttl: {}s", req.app_id, ttl);

        match self.app_manager.start_inspect(req.app_id.clone(), ttl) {
            Ok((inspect_app_id, token)) => Ok(Response::new(StartInspectResponse {
                inspect_app_id,
                token,
            })),
            Err(e) => {
                warn!("Failed to start inspect session: {}", e);
                Err(Status::internal(e.to_string()))
            }
        }
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
