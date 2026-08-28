use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::{error, info, warn};

use crate::app::{AppConfig as InternalAppConfig, ApplicationManager};
use crate::app_repo::{AppPackageStatus, AppRepo};
use crate::config_store::{ConfigEntry, ConfigStore};

use super::proto::runtime_management_server::RuntimeManagement;
use super::proto::*;

/// Runtime management service implementation
pub struct RuntimeManagementService {
    app_manager: Arc<ApplicationManager>,
    config_store: Arc<ConfigStore>,
    app_repo: Arc<AppRepo>,
    node_id: String,
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

impl RuntimeManagementService {
    pub fn new(
        app_manager: Arc<ApplicationManager>,
        config_store: Arc<ConfigStore>,
        app_repo: Arc<AppRepo>,
        node_id: String,
    ) -> Self {
        Self {
            app_manager,
            config_store,
            app_repo,
            node_id,
        }
    }
}

#[tonic::async_trait]
impl RuntimeManagement for RuntimeManagementService {
    async fn push_config(
        &self,
        request: Request<PushConfigRequest>,
    ) -> Result<Response<PushConfigResponse>, Status> {
        let req = request.into_inner();
        info!(
            "Received config push: type={:?}, version={}, force={}",
            req.config_type, req.version, req.force
        );

        // Convert configuration type
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

        // Create configuration entry
        let entry = ConfigEntry::new(
            "global".to_string(), // TODO: Determine ID based on config_type
            config_type,
            req.version,
            req.config_json,
        );

        // Save configuration
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
            "Deploying app: {} (package size: {} bytes)",
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
                error!("Failed to install app package: {}", e);
                return Ok(Response::new(DeployAppResponse {
                    success: false,
                    message: format!("Failed to install app package: {}", e),
                    app_id: req.app_id,
                }));
            }
        };

        let entry_point = &package.manifest.entry_point;
        let entry_code = match self.app_repo.get_file(&req.app_id, entry_point).await {
            Ok(Some(bytes)) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(e) => {
                    error!("Entry file is not valid UTF-8: {}", e);
                    return Ok(Response::new(DeployAppResponse {
                        success: false,
                        message: format!("Entry file is not valid UTF-8: {}", e),
                        app_id: req.app_id,
                    }));
                }
            },
            Ok(None) => {
                error!("Entry file does not exist: {}", entry_point);
                return Ok(Response::new(DeployAppResponse {
                    success: false,
                    message: format!("Entry file does not exist: {}", entry_point),
                    app_id: req.app_id,
                }));
            }
            Err(e) => {
                error!("Failed to read entry file: {}", e);
                return Ok(Response::new(DeployAppResponse {
                    success: false,
                    message: format!("Failed to read entry file: {}", e),
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
                info!("App {} deployed successfully", req.app_id);
                let _ = self
                    .app_repo
                    .update_status(&req.app_id, AppPackageStatus::Deployed)
                    .await;
                Ok(Response::new(DeployAppResponse {
                    success: true,
                    message: "App deployed successfully".to_string(),
                    app_id: req.app_id,
                }))
            }
            Err(e) => {
                error!("App {} deployment failed: {}", req.app_id, e);
                let _ = self
                    .app_repo
                    .update_status(&req.app_id, AppPackageStatus::Failed)
                    .await;
                Ok(Response::new(DeployAppResponse {
                    success: false,
                    message: format!("App deployment failed: {}", e),
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
        info!("Uninstalling app: {}", req.app_id);
        match self.app_manager.unload_app(&req.app_id).await {
            Ok(_) => {
                info!("App {} uninstalled successfully", req.app_id);
                Ok(Response::new(UnloadAppResponse {
                    success: true,
                    message: "App uninstalled successfully".to_string(),
                }))
            }
            Err(e) => {
                warn!("App {} uninstallation failed: {}", req.app_id, e);
                Ok(Response::new(UnloadAppResponse {
                    success: false,
                    message: format!("App uninstallation failed: {}", e),
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
            "Updating app: {} (package size: {} bytes)",
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
                error!("Failed to install app package: {}", e);
                return Ok(Response::new(UpdateAppResponse {
                    success: false,
                    message: format!("Failed to install app package: {}", e),
                }));
            }
        };

        let entry_point = &package.manifest.entry_point;
        let entry_code = match self.app_repo.get_file(&req.app_id, entry_point).await {
            Ok(Some(bytes)) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(e) => {
                    error!("Entry file is not valid UTF-8: {}", e);
                    return Ok(Response::new(UpdateAppResponse {
                        success: false,
                        message: format!("Entry file is not valid UTF-8: {}", e),
                    }));
                }
            },
            Ok(None) => {
                error!("Entry file does not exist: {}", entry_point);
                return Ok(Response::new(UpdateAppResponse {
                    success: false,
                    message: format!("Entry file does not exist: {}", entry_point),
                }));
            }
            Err(e) => {
                error!("Failed to read entry file: {}", e);
                return Ok(Response::new(UpdateAppResponse {
                    success: false,
                    message: format!("Failed to read entry file: {}", e),
                }));
            }
        };

        let config = map_proto_config(req.config);

        match self
            .app_manager
            .update_app(
                &req.app_id,
                entry_code,
                Some(config),
                Some(entry_point.clone()),
            )
            .await
        {
            Ok(_) => {
                info!("App {} updated successfully", req.app_id);
                let _ = self
                    .app_repo
                    .update_status(&req.app_id, AppPackageStatus::Deployed)
                    .await;
                Ok(Response::new(UpdateAppResponse {
                    success: true,
                    message: "App updated successfully".to_string(),
                }))
            }
            Err(e) => {
                error!("App {} update failed: {}", req.app_id, e);
                let _ = self
                    .app_repo
                    .update_status(&req.app_id, AppPackageStatus::Failed)
                    .await;
                Ok(Response::new(UpdateAppResponse {
                    success: false,
                    message: format!("App update failed: {}", e),
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

        // Basic Worker information
        let node_info = Some(NodeInfo {
            node_id: self.node_id.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: 0, // TODO: Calculate actual uptime
            hostname: hostname::get()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
        });

        // Application list
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
                    request_count: 0, // TODO: Real-world statistics
                    error_count: 0,   // TODO: Real-world statistics
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

        // Resource usage (simplified version)
        let resource_usage = Some(ResourceUsage {
            cpu_percent: 0.0, // TODO: Actual CPU usage
            memory_bytes: 0,  // TODO: Actual memory usage
            goroutines: 0,    // Rust doesn't have goroutines, can return thread count instead
        });

        Ok(Response::new(GetStatusResponse {
            node_info,
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

        // TODO: Implement command execution logic
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
