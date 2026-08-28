//! Client side of the deis-runtime gRPC management plane.
//!
//! Runtime nodes are autonomous: they expose a management service and wait
//! for the control to call them, instead of registering through deis-wire
//! like deis-proxy does. This module keeps the list of configured deis-runtime nodes,
//! polls their health in the background, and offers typed helpers used by
//! the REST API layer.

pub mod proto {
    tonic::include_proto!("deis.management");
}

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::Serialize;
use tonic::transport::{Channel, Endpoint};
use tracing::{debug, info, warn};

use crate::config::RuntimeNodeConfig;
use proto::runtime_management_client::RuntimeManagementClient;

/// Connection timeout for a single management call. Nodes are on the same
/// network segment as the control; anything slower than this is down.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Per-RPC deadline. Deploys upload a package, so it is more generous.
const RPC_TIMEOUT: Duration = Duration::from_secs(10);
const DEPLOY_TIMEOUT: Duration = Duration::from_secs(60);

/// Last observed health of one deis-runtime node.
#[derive(Debug, Clone, Serialize)]
pub struct NodeHealth {
    pub healthy: bool,
    pub version: Option<String>,
    pub last_checked: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

impl Default for NodeHealth {
    fn default() -> Self {
        Self {
            healthy: false,
            version: None,
            last_checked: None,
            last_error: Some("not checked yet".to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeView {
    pub id: String,
    pub addr: String,
    #[serde(flatten)]
    pub health: NodeHealth,
}

pub struct RuntimeNodeManager {
    nodes: Vec<RuntimeNodeConfig>,
    health: DashMap<String, NodeHealth>,
    poll_interval: Duration,
}

impl RuntimeNodeManager {
    pub fn new(nodes: Vec<RuntimeNodeConfig>, poll_interval_secs: u64) -> Self {
        let health = DashMap::new();
        for node in &nodes {
            health.insert(node.id.clone(), NodeHealth::default());
        }
        Self {
            nodes,
            health,
            poll_interval: Duration::from_secs(poll_interval_secs.max(1)),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn node(&self, id: &str) -> Option<&RuntimeNodeConfig> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Snapshot of all nodes with their cached health for the REST API.
    pub fn list(&self) -> Vec<NodeView> {
        self.nodes
            .iter()
            .map(|n| NodeView {
                id: n.id.clone(),
                addr: n.addr.clone(),
                health: self
                    .health
                    .get(&n.id)
                    .map(|h| h.clone())
                    .unwrap_or_default(),
            })
            .collect()
    }

    async fn connect(&self, id: &str) -> Result<RuntimeManagementClient<Channel>, NodeError> {
        let node = self.node(id).ok_or(NodeError::UnknownNode)?;
        let endpoint = Endpoint::from_shared(node.addr.clone())
            .map_err(|e| NodeError::Transport(format!("invalid endpoint {}: {e}", node.addr)))?
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(RPC_TIMEOUT);
        let channel = endpoint
            .connect()
            .await
            .map_err(|e| NodeError::Transport(format!("connect {}: {e}", node.addr)))?;
        Ok(RuntimeManagementClient::new(channel))
    }

    /// One health probe; updates the cache and returns the fresh state.
    pub async fn check_node(&self, id: &str) -> Result<NodeHealth, NodeError> {
        let result = async {
            let mut client = self.connect(id).await?;
            let resp = client
                .health_check(proto::HealthCheckRequest {})
                .await
                .map_err(NodeError::from_status)?
                .into_inner();
            Ok::<_, NodeError>(resp)
        }
        .await;

        let health = match &result {
            Ok(resp) => NodeHealth {
                healthy: resp.healthy,
                version: Some(resp.version.clone()),
                last_checked: Some(Utc::now()),
                last_error: None,
            },
            Err(e) => NodeHealth {
                healthy: false,
                version: self.health.get(id).and_then(|h| h.version.clone()),
                last_checked: Some(Utc::now()),
                last_error: Some(e.to_string()),
            },
        };
        self.health.insert(id.to_string(), health.clone());
        result.map(|_| health)
    }

    /// Live status query, optionally including per-app details.
    pub async fn get_status(
        &self,
        id: &str,
        include_apps: bool,
    ) -> Result<proto::GetStatusResponse, NodeError> {
        let mut client = self.connect(id).await?;
        let resp = client
            .get_status(proto::GetStatusRequest { include_apps })
            .await
            .map_err(NodeError::from_status)?;
        Ok(resp.into_inner())
    }

    /// Deploy an application package (ZIP with a manifest.json) to a node.
    pub async fn deploy_app(
        &self,
        id: &str,
        app_id: &str,
        package: Vec<u8>,
    ) -> Result<proto::DeployAppResponse, NodeError> {
        let node = self.node(id).ok_or(NodeError::UnknownNode)?;
        let endpoint = Endpoint::from_shared(node.addr.clone())
            .map_err(|e| NodeError::Transport(format!("invalid endpoint {}: {e}", node.addr)))?
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(DEPLOY_TIMEOUT);
        let channel = endpoint
            .connect()
            .await
            .map_err(|e| NodeError::Transport(format!("connect {}: {e}", node.addr)))?;
        let mut client = RuntimeManagementClient::new(channel);

        info!(
            node = id,
            app_id,
            package_bytes = package.len(),
            "Deploying app to deis_runtime node"
        );
        let resp = client
            .deploy_app(proto::DeployAppRequest {
                app_id: app_id.to_string(),
                package_data: package,
                config: None,
            })
            .await
            .map_err(NodeError::from_status)?
            .into_inner();
        if resp.success {
            info!(node = id, app_id, "App deployed");
        } else {
            warn!(node = id, app_id, message = %resp.message, "Deploy rejected by node");
        }
        Ok(resp)
    }

    /// Unload (stop and remove) an application from a node.
    pub async fn unload_app(
        &self,
        id: &str,
        app_id: &str,
    ) -> Result<proto::UnloadAppResponse, NodeError> {
        let mut client = self.connect(id).await?;
        info!(node = id, app_id, "Unloading app from deis_runtime node");
        let resp = client
            .unload_app(proto::UnloadAppRequest {
                app_id: app_id.to_string(),
            })
            .await
            .map_err(NodeError::from_status)?;
        Ok(resp.into_inner())
    }

    /// Background loop keeping the health cache warm. Runs until the
    /// process exits; state transitions are logged at info, repeated
    /// failures stay at debug to avoid log spam.
    pub async fn poll_loop(self: Arc<Self>) {
        if self.nodes.is_empty() {
            info!("No deis_runtime nodes configured; management polling disabled");
            return;
        }
        info!(
            nodes = self.nodes.len(),
            interval_secs = self.poll_interval.as_secs(),
            "Starting deis_runtime node health polling"
        );
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            for node in &self.nodes {
                let was_healthy = self
                    .health
                    .get(&node.id)
                    .map(|h| h.healthy)
                    .unwrap_or(false);
                match self.check_node(&node.id).await {
                    Ok(h) => {
                        if !was_healthy {
                            info!(node = %node.id, version = ?h.version, "Runtime node is healthy");
                        }
                    }
                    Err(e) => {
                        if was_healthy {
                            warn!(node = %node.id, error = %e, "Runtime node became unreachable");
                        } else {
                            debug!(node = %node.id, error = %e, "Runtime node still unreachable");
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("unknown deis_runtime node")]
    UnknownNode,
    #[error("transport error: {0}")]
    Transport(String),
    #[error("rpc error: {0}")]
    Rpc(String),
}

impl NodeError {
    fn from_status(status: tonic::Status) -> Self {
        NodeError::Rpc(format!("{}: {}", status.code(), status.message()))
    }
}
