use std::sync::Arc;
use std::time::Instant;
use tonic::{Request, Response, Status};
use tracing::{error, info, warn};

use crate::app::{AppKey, ApplicationManager};
use crate::config_store::{ConfigEntry, ConfigStore};
use crate::deployment::{Applied, DeploymentManager, DeploymentOrder};
use crate::health::NodeHealth;
use crate::runtime::metrics::{recorder, REQUESTS_TOTAL, REQUEST_ERRORS_TOTAL};

use super::proto::runtime_management_server::RuntimeManagement;
use super::proto::*;

/// Runtime management service implementation
pub struct RuntimeManagementService {
    app_manager: Arc<ApplicationManager>,
    config_store: Arc<ConfigStore>,
    deployer: Arc<DeploymentManager>,
    health: Arc<NodeHealth>,
    node_id: String,
    /// When this node's process started, so `GetStatus` can report an uptime
    /// the control plane can compare against its own view of the fleet.
    started_at: Instant,
}

impl RuntimeManagementService {
    pub fn new(
        app_manager: Arc<ApplicationManager>,
        config_store: Arc<ConfigStore>,
        deployer: Arc<DeploymentManager>,
        health: Arc<NodeHealth>,
        node_id: String,
        started_at: Instant,
    ) -> Self {
        Self {
            app_manager,
            config_store,
            deployer,
            health,
            node_id,
            started_at,
        }
    }

    /// The version this node is running for one environment of an app, for the
    /// "what did you keep" half of an `ApplyDeploymentResponse`. Empty when
    /// that environment is not loaded at all, which is what distinguishes "the
    /// deploy failed and the old version is still up" from "this app is down".
    fn running_version(&self, key: &AppKey) -> String {
        self.app_manager
            .installed_version(key)
            .map(|v| v.version_id)
            .unwrap_or_default()
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

    /// Take delivery of one deployment.
    ///
    /// The request is a pointer, not a payload: this node downloads the
    /// bundle, checks it against `bundle_sha`, unpacks it and loads it. Every
    /// failure short of the final isolate swap leaves the app serving the
    /// version it was already on, and the response says which version that is.
    ///
    /// The request carries decrypted secrets in `env`. It is never logged as a
    /// whole, and neither is anything derived from it; see
    /// [`crate::deployment::DeploymentOrder`].
    async fn apply_deployment(
        &self,
        request: Request<ApplyDeploymentRequest>,
    ) -> Result<Response<ApplyDeploymentResponse>, Status> {
        let req = request.into_inner();
        info!(
            app_id = %req.app_id,
            version_id = %req.version_id,
            environment = %req.environment,
            bundle_sha = %req.bundle_sha,
            env_entries = req.env.len(),
            "Control plane asked this node to run a version"
        );

        let order = DeploymentOrder {
            app_id: req.app_id.clone(),
            version_id: req.version_id.clone(),
            environment: req.environment,
            bundle_url: req.bundle_url,
            bundle_sha: req.bundle_sha,
            env: req.env,
        };

        let key = order.key();

        match self.deployer.apply(order).await {
            Ok(Applied::Installed { reused_bundle }) => {
                info!(
                    app_id = %key.app_id(),
                    environment = %key.environment(),
                    version_id = %req.version_id,
                    reused_bundle,
                    "Deployment applied"
                );
                Ok(Response::new(ApplyDeploymentResponse {
                    success: true,
                    message: "Deployment applied".to_string(),
                    version_id: req.version_id,
                }))
            }
            Ok(Applied::AlreadyRunning) => Ok(Response::new(ApplyDeploymentResponse {
                success: true,
                message: "Already running this version".to_string(),
                version_id: req.version_id,
            })),
            Err(e) => {
                let kept = self.running_version(&key);
                error!(
                    app_id = %key.app_id(),
                    environment = %key.environment(),
                    version_id = %req.version_id,
                    kept_version = %kept,
                    error = %e,
                    "Deployment rejected; this node did not switch to version {}",
                    req.version_id
                );
                Ok(Response::new(ApplyDeploymentResponse {
                    success: false,
                    message: e.to_string(),
                    version_id: kept,
                }))
            }
        }
    }

    /// Stop serving an application on this node.
    ///
    /// The request names an application and, optionally, one of its
    /// environments; an empty environment means all of them. The response
    /// lists what was actually unloaded, so the control plane's report says
    /// which copies stopped rather than just "done".
    async fn unload_app(
        &self,
        request: Request<UnloadAppRequest>,
    ) -> Result<Response<UnloadAppResponse>, Status> {
        let req = request.into_inner();
        let environment = if req.environment.trim().is_empty() {
            None
        } else {
            Some(req.environment.as_str())
        };
        info!(
            app_id = %req.app_id,
            environment = %environment.unwrap_or("<all>"),
            "Uninstalling app"
        );

        // Through the deployer, so the bundle files and the installed-version
        // record go with the isolate; otherwise a restart without a reachable
        // control plane would bring the app back from disk.
        match self.deployer.unload(&req.app_id, environment).await {
            Ok(environments) => {
                let message = if environments.is_empty() {
                    format!("{} was not running on this node", req.app_id)
                } else {
                    format!("Unloaded {}", environments.join(", "))
                };
                info!(
                    app_id = %req.app_id,
                    environments = ?environments,
                    "{}",
                    message
                );
                Ok(Response::new(UnloadAppResponse {
                    success: true,
                    message,
                    environments,
                }))
            }
            Err(e) => {
                warn!(
                    app_id = %req.app_id,
                    error = %e,
                    "App {} was not fully unloaded and may still be serving here",
                    req.app_id
                );
                Ok(Response::new(UnloadAppResponse {
                    success: false,
                    message: format!("App uninstallation failed: {}", e),
                    environments: Vec::new(),
                }))
            }
        }
    }

    /// Superseded by `ApplyDeployment`, and refused rather than half-honoured.
    ///
    /// This RPC uploaded a zip straight into the node and read its own
    /// `manifest.json` with a second, incompatible manifest definition, so a
    /// real deislet bundle (`entry`, not `entry_point`) failed here on a serde
    /// error while the deployment path accepted it. The bundle format has one
    /// definition, in `deis-bundle`, and one way onto a node: the control plane
    /// points at a bundle and this node fetches and verifies it
    /// (docs/implementation-plan.zh.md section 4).
    ///
    /// `Unimplemented` rather than a silent success: a caller that still sends
    /// a package must see that nothing was deployed.
    async fn update_app(
        &self,
        request: Request<UpdateAppRequest>,
    ) -> Result<Response<UpdateAppResponse>, Status> {
        let req = request.into_inner();
        warn!(
            app_id = %req.app_id,
            package_bytes = req.package_data.len(),
            "Refusing an UpdateApp package upload: this node only takes deployments through \
             ApplyDeployment, which points at a verified bundle. Nothing was installed and the \
             app keeps running whatever it was running"
        );
        Err(Status::unimplemented(
            "UpdateApp is superseded by ApplyDeployment: upload the version to the control \
             plane and point an environment at it. This node does not accept package uploads",
        ))
    }

    async fn get_status(
        &self,
        request: Request<GetStatusRequest>,
    ) -> Result<Response<GetStatusResponse>, Status> {
        let req = request.into_inner();
        info!("Status query received, include_apps: {}", req.include_apps);

        // Node identity
        let node_info = Some(NodeInfo {
            node_id: self.node_id.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: self.started_at.elapsed().as_secs() as i64,
            hostname: hostname::get()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
        });

        // Application list, one entry per (app, environment) this node runs.
        //
        // The counters are the ones the isolates have been recording all
        // along, read back per environment: a preview's traffic is not added
        // to production's. They are cumulative since this process started,
        // which is what `uptime_seconds` above is for.
        let apps = if req.include_apps {
            let counters = recorder();
            self.app_manager
                .list_apps()
                .into_iter()
                .map(|app| {
                    let key = AppKey::new(app.id.clone(), app.environment.clone());
                    let tenant = key.tenant_id();
                    AppStatus {
                        app_id: app.id,
                        environment: app.environment,
                        state: match app.state {
                            crate::app::AppState::Loading => AppState::Loading as i32,
                            crate::app::AppState::Running => AppState::Running as i32,
                            crate::app::AppState::Stopped => AppState::Stopped as i32,
                            crate::app::AppState::Error => AppState::Error as i32,
                        },
                        request_count: counters.value(REQUESTS_TOTAL, &tenant) as i64,
                        error_count: counters.value(REQUEST_ERRORS_TOTAL, &tenant) as i64,
                        error_message: app.error.unwrap_or_default(),
                    }
                })
                .collect()
        } else {
            Vec::new()
        };

        let op_metrics = self.app_manager.get_op_metrics().await;
        let op_metrics = op_metrics
            .into_iter()
            .map(|(key, summary)| AppOpMetrics {
                app_id: key.app_id().to_string(),
                environment: key.environment().to_string(),
                summary: Some(OpMetricsSummary {
                    ops_dispatched_sync: summary.ops_dispatched_sync,
                    ops_dispatched_async: summary.ops_dispatched_async,
                    ops_dispatched_fast: summary.ops_dispatched_fast,
                    ops_completed_async: summary.ops_completed_async,
                }),
            })
            .collect::<Vec<_>>();

        // Left absent, not zeroed. This node does not measure its own CPU or
        // memory yet, and a `ResourceUsage` full of zeros reads as "this node
        // is idle" rather than "nobody looked", which is worse than saying
        // nothing.
        let resource_usage = None;

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

        // The request names no environment, so it means the one a caller means
        // when it does not say: production. Inspecting a preview needs an
        // environment on this RPC, which the proto cannot express yet.
        let key = AppKey::production(req.app_id.clone());
        info!(
            app_id = %key.app_id(),
            environment = %key.environment(),
            "Start inspect for app: {}, ttl: {}s",
            key,
            ttl
        );

        match self.app_manager.start_inspect(key, ttl) {
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

    /// Report readiness, not liveness.
    ///
    /// `healthy` is true only once this node has the control plane's
    /// assignment list and has loaded everything in it (principle 8). A node
    /// that is up and serving versions it recovered from its own disk answers
    /// false with the reason in `details`, so that the control plane never
    /// mistakes "still running yesterday's version" for "running what I told
    /// it to".
    async fn health_check(
        &self,
        _request: Request<HealthCheckRequest>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        Ok(Response::new(HealthCheckResponse {
            healthy: self.health.is_healthy(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            details: self.health.details(),
        }))
    }
}

#[cfg(test)]
#[path = "service_test.rs"]
mod service_test;
