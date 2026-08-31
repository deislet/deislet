use std::sync::Arc;
use std::time::Instant;
use tonic::{Request, Response, Status};
use tracing::{error, info, warn};

use crate::app::types::ResourceLimits;
use crate::app::{AppKey, ApplicationManager};
use crate::deployment::{Applied, DeploymentManager, DeploymentOrder};
use crate::health::NodeHealth;
use crate::runtime::metrics::{recorder, REQUESTS_TOTAL, REQUEST_ERRORS_TOTAL};

use super::proto::runtime_management_server::RuntimeManagement;
use super::proto::*;
// The wire type and the node's own type have the same name and different
// shapes: the wire uses a zero sentinel where the node uses `Option`. Aliased
// rather than qualified at the call site so the one place that converts
// between them names both halves.
use super::proto::ResourceLimits as ResourceLimitsMessage;

/// Runtime management service implementation
pub struct RuntimeManagementService {
    app_manager: Arc<ApplicationManager>,
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
        deployer: Arc<DeploymentManager>,
        health: Arc<NodeHealth>,
        node_id: String,
        started_at: Instant,
    ) -> Self {
        Self {
            app_manager,
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
    /// Refused, not stored.
    ///
    /// A node's configuration comes from two places, and neither of them is
    /// this RPC: the file it is started with (`deis-runtime.toml`) and the
    /// assignment list it fetches from the control plane, which carries the
    /// per-environment variables and secrets with each deployment. What this
    /// method used to do was write the payload into a local table that nothing
    /// on this node ever read back, then answer "Config applied successfully" -
    /// so a caller was told its configuration had taken effect when it had
    /// changed nothing at all. The request also carries no identifier, so
    /// every application-scoped push landed on one row keyed "global",
    /// overwriting the one before it.
    ///
    /// `Unimplemented` for the same reason `UpdateApp` is: a caller that still
    /// sends configuration here has to see that nothing was applied.
    async fn push_config(
        &self,
        request: Request<PushConfigRequest>,
    ) -> Result<Response<PushConfigResponse>, Status> {
        let req = request.into_inner();
        warn!(
            config_type = req.config_type,
            version = req.version,
            force = req.force,
            "Refusing a PushConfig: this node takes its configuration from its own file and \
             from the deployments the control plane assigns it. Nothing was stored and nothing \
             changed"
        );
        Err(Status::unimplemented(
            "PushConfig is not supported: a node reads its own configuration file and receives \
             per-environment variables and secrets with each deployment",
        ))
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

        let limits = req.limits.map(decode_limits);
        if let Some(limits) = &limits {
            info!(
                app_id = %req.app_id,
                version_id = %req.version_id,
                memory_mb = limits.memory_mb,
                timeout_ms = ?limits.timeout_ms,
                cpu_time_limit_ms = ?limits.cpu_time_limit_ms,
                rate_limit_ops_per_sec = ?limits.rate_limit_ops_per_sec,
                resource_table_max = ?limits.resource_table_max,
                "This deployment asks to be held to limits of its own; each one is clamped to \
                 this node's ceiling when the isolate is built"
            );
        }

        let order = DeploymentOrder {
            app_id: req.app_id.clone(),
            version_id: req.version_id.clone(),
            environment: req.environment,
            bundle_url: req.bundle_url,
            bundle_sha: req.bundle_sha,
            env: req.env,
            limits,
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
            // A push is never superseded: only a reconciliation carries a
            // stamp saying how old its instruction is, and `apply` has none.
            // Matched rather than caught by a wildcard so that a future path
            // returning it here has to be thought about instead of silently
            // reported as a successful deployment.
            Ok(Applied::Superseded { running_version_id }) => {
                let kept = running_version_id.unwrap_or_else(|| self.running_version(&key));
                error!(
                    app_id = %key.app_id(),
                    environment = %key.environment(),
                    version_id = %req.version_id,
                    kept_version = %kept,
                    "A pushed deployment came back as superseded, which only a reconciliation \
                     can be; this node did not switch to version {} and the control plane is \
                     told the push failed",
                    req.version_id
                );
                Ok(Response::new(ApplyDeploymentResponse {
                    success: false,
                    message: "deployment was superseded by a newer change on this node".to_string(),
                    version_id: kept,
                }))
            }
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
    /// The archive field is gone from the request as well (`reserved 2` in
    /// `management.proto`), so a caller cannot even put one on the wire; what
    /// arrives here is an app id and, at most, a configuration block.
    ///
    /// `Unimplemented` rather than a silent success: a caller that still asks
    /// for this must see that nothing was deployed.
    async fn update_app(
        &self,
        request: Request<UpdateAppRequest>,
    ) -> Result<Response<UpdateAppResponse>, Status> {
        let req = request.into_inner();
        warn!(
            app_id = %req.app_id,
            "Refusing an UpdateApp: this node only takes deployments through ApplyDeployment, \
             which points at a bundle it fetches and verifies itself. Nothing was installed and \
             the app keeps running whatever it was running"
        );
        Err(Status::unimplemented(
            "UpdateApp is superseded by ApplyDeployment: upload the version to the control \
             plane and point an environment at it. This node does not install an application \
             from anything a caller hands it",
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

    /// Refused: this node runs no commands.
    ///
    /// There is no command surface here at all - no shutdown, no restart, no
    /// status verb. The operations a caller might want are already RPCs of
    /// their own on this service (`GetStatus`, `HealthCheck`,
    /// `ApplyDeployment`, `UnloadApp`), and process control belongs to
    /// whatever supervises the node.
    ///
    /// `Unimplemented` for the same reason as `UpdateApp` and `PushConfig`
    /// above: an answer of `success: false` reads as "this command failed",
    /// which invites a retry of something that will never work, and buries
    /// the difference between a command that ran and lost and a command that
    /// was never going to run.
    async fn execute_command(
        &self,
        request: Request<ExecuteCommandRequest>,
    ) -> Result<Response<ExecuteCommandResponse>, Status> {
        let req = request.into_inner();
        warn!(
            command = %req.command,
            "Refusing an ExecuteCommand: this node has no command surface. Nothing ran"
        );
        Err(Status::unimplemented(
            "ExecuteCommand is not supported: this node exposes GetStatus, HealthCheck, \
             ApplyDeployment and UnloadApp, and nothing that runs a command string",
        ))
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

        // A node runs every environment of an application at once, so the
        // request has to say which one to attach to. An empty field means the
        // one a caller means when it says nothing - the default environment -
        // which keeps older callers working unchanged.
        let key = if req.environment.trim().is_empty() {
            AppKey::production(req.app_id.clone())
        } else {
            AppKey::new(req.app_id.clone(), req.environment.clone())
        };
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

/// Read the wire's `ResourceLimits` into the node's own.
///
/// Proto3 has no way to tell an absent scalar from a zero one, so `0` is what
/// every producer sends for "not set" and is what this reads it back as. The
/// control plane refuses an explicit zero when it accepts the bundle
/// (`deis_control::catalog::limits`), so a zero arriving here is an omission
/// and never an application asking for a budget it could not run in.
///
/// Nothing is judged here. A value narrower than this node's ceiling and a
/// value far above it look identical at this point; which one wins is
/// [`ApplicationManager::effective_limits`]'s decision, made once, where the
/// node's own configuration is in scope.
fn decode_limits(limits: ResourceLimitsMessage) -> ResourceLimits {
    ResourceLimits {
        memory_mb: limits.memory_mb.max(0) as usize,
        // Field 2 of the message, enforced by nothing on this node; carried
        // into the struct only so that what the control plane sent and what
        // the node holds are the same value.
        cpu_milli: limits.cpu_milli.max(0) as u32,
        resource_table_max: (limits.resource_table_max > 0)
            .then_some(limits.resource_table_max as usize),
        timeout_ms: (limits.timeout_ms > 0).then_some(limits.timeout_ms as u64),
        rate_limit_ops_per_sec: (limits.rate_limit_ops_per_sec > 0.0)
            .then_some(limits.rate_limit_ops_per_sec),
        cpu_time_limit_ms: (limits.cpu_time_limit_ms > 0)
            .then_some(limits.cpu_time_limit_ms as u64),
    }
}

#[cfg(test)]
#[path = "service_test.rs"]
mod service_test;
