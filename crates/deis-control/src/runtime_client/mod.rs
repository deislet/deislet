//! Client side of the deis-runtime gRPC management plane.
//!
//! Runtime nodes are autonomous: they expose a management service and wait
//! for the control to call them, instead of registering through deis-wire
//! like deis-proxy does. This module keeps the list of configured deis-runtime nodes,
//! polls their health in the background, and offers typed helpers used by
//! the REST API layer.
//!
//! This is the transport half of talking to a node. What to send and when
//! is decided next door: [`crate::delivery`] for deployments,
//! [`crate::scheduler`] for cron fires.

pub mod proto {
    tonic::include_proto!("deis.management");
}

/// The invocation API of a node: queue and scheduled events. Only the cron
/// half is used from here.
pub mod trigger {
    tonic::include_proto!("deis.trigger");
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
use trigger::trigger_client::TriggerClient;

/// Connection timeout for a single management call. Nodes are on the same
/// network segment as the control; anything slower than this is down.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Per-RPC deadline for the small calls: health, status, unload.
const RPC_TIMEOUT: Duration = Duration::from_secs(10);
/// Deadline for applying a deployment. The message itself is a pointer,
/// but the node downloads and verifies the bundle before it answers, so
/// this has to cover fetching a version over HTTP.
const APPLY_TIMEOUT: Duration = Duration::from_secs(60);
/// Deadline for a scheduled trigger.
///
/// Deliberately longer than the node's own execution ceiling (30s by
/// default). A shorter deadline would report a slow-but-successful cron
/// run as a failure, and the scheduler would hand the same fire to another
/// node - running the job twice, which is worse than reporting it late.
const TRIGGER_TIMEOUT: Duration = Duration::from_secs(60);

/// Raise both message limits from the tonic default of 4 MB.
///
/// Deployment metadata and status responses grow with the number of
/// applications a node runs, and a truncated response would look like a
/// dead node rather than an oversized message.
fn with_message_limits(
    client: RuntimeManagementClient<Channel>,
) -> RuntimeManagementClient<Channel> {
    client
        .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
        .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
}

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

    /// Every configured node id, in configuration order.
    ///
    /// This is the delivery fan-out: every node runs every application
    /// (architecture principle 2), so a deployment goes to all of them.
    pub fn ids(&self) -> Vec<String> {
        self.nodes.iter().map(|n| n.id.clone()).collect()
    }

    /// Node ids ordered for a call that only needs to reach *one* node.
    ///
    /// Nodes last seen healthy come first, rotated by `rotation` so
    /// repeated fires spread over the fleet instead of always landing on
    /// the first entry. Nodes not currently known to be healthy are kept
    /// at the back rather than dropped: the health cache is up to one poll
    /// interval old, and at startup nothing has been probed yet, so
    /// dropping them would silently skip work a live node could have done.
    pub fn call_order(&self, rotation: usize) -> Vec<String> {
        if self.nodes.is_empty() {
            return Vec::new();
        }
        let healthy: Vec<String> = self
            .nodes
            .iter()
            .filter(|n| self.health.get(&n.id).map(|h| h.healthy).unwrap_or(false))
            .map(|n| n.id.clone())
            .collect();
        let unhealthy: Vec<String> = self
            .nodes
            .iter()
            .filter(|n| !self.health.get(&n.id).map(|h| h.healthy).unwrap_or(false))
            .map(|n| n.id.clone())
            .collect();

        let mut ordered = Vec::with_capacity(self.nodes.len());
        if !healthy.is_empty() {
            let start = rotation % healthy.len();
            ordered.extend(healthy[start..].iter().cloned());
            ordered.extend(healthy[..start].iter().cloned());
        }
        ordered.extend(unhealthy);
        ordered
    }

    /// Whether this node was healthy at the last probe.
    pub fn is_healthy(&self, id: &str) -> bool {
        self.health.get(id).map(|h| h.healthy).unwrap_or(false)
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

    /// Open a channel to one node with the deadline this kind of call needs.
    async fn channel(&self, id: &str, rpc_timeout: Duration) -> Result<Channel, NodeError> {
        let node = self.node(id).ok_or(NodeError::UnknownNode)?;
        Endpoint::from_shared(node.addr.clone())
            .map_err(|e| NodeError::Transport(format!("invalid endpoint {}: {e}", node.addr)))?
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(rpc_timeout)
            .connect()
            .await
            .map_err(|e| NodeError::Transport(format!("connect {}: {e}", node.addr)))
    }

    async fn connect(&self, id: &str) -> Result<RuntimeManagementClient<Channel>, NodeError> {
        let channel = self.channel(id, RPC_TIMEOUT).await?;
        Ok(with_message_limits(RuntimeManagementClient::new(channel)))
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

    /// Tell one node which version of an app it should be running.
    ///
    /// Only the pointer travels: the node downloads the bundle from
    /// `bundle_url` itself and checks it against `bundle_sha`
    /// (docs/implementation-plan.zh.md section 4). The request is built by
    /// [`crate::delivery`], which is where variables and decrypted secrets
    /// are merged.
    ///
    /// Never log, print or store `request` as a whole: its `env` map holds
    /// secret values in the clear and prost's `Debug` would render every
    /// one of them. The individual fields logged below are all safe.
    pub async fn apply_deployment(
        &self,
        id: &str,
        request: proto::ApplyDeploymentRequest,
    ) -> Result<proto::ApplyDeploymentResponse, NodeError> {
        let channel = self.channel(id, APPLY_TIMEOUT).await?;
        let mut client = with_message_limits(RuntimeManagementClient::new(channel));

        let (app_id, environment, version_id) = (
            request.app_id.clone(),
            request.environment.clone(),
            request.version_id.clone(),
        );
        debug!(
            node = id,
            app_id = %app_id,
            environment = %environment,
            version_id = %version_id,
            bundle_sha = %request.bundle_sha,
            env_entries = request.env.len(),
            "Sending deployment pointer to runtime node; it downloads the bundle itself"
        );

        let resp = client
            .apply_deployment(request)
            .await
            .map_err(NodeError::from_status)?
            .into_inner();

        if resp.success {
            info!(
                node = id,
                app_id = %app_id,
                environment = %environment,
                version_id = %version_id,
                "Node applied the deployment and now serves this version"
            );
        } else {
            warn!(
                node = id,
                app_id = %app_id,
                environment = %environment,
                version_id = %version_id,
                running_version = %resp.version_id,
                message = %resp.message,
                "Node rejected the deployment; it keeps serving the version it had"
            );
        }
        Ok(resp)
    }

    /// Fire one scheduled event on one node.
    ///
    /// Exactly one node runs a given cron occurrence (architecture
    /// principle 6); picking which one is the scheduler's job.
    pub async fn trigger_scheduled(
        &self,
        id: &str,
        app_id: &str,
        cron: &str,
        scheduled_time: DateTime<Utc>,
    ) -> Result<trigger::TriggerResponse, NodeError> {
        let channel = self.channel(id, TRIGGER_TIMEOUT).await?;
        let mut client = TriggerClient::new(channel)
            .max_decoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(crate::MAX_GRPC_MESSAGE_BYTES);

        let resp = client
            .on_scheduled(trigger::ScheduledEvent {
                app_id: app_id.to_string(),
                scheduled_time: scheduled_time.timestamp_millis(),
                cron: cron.to_string(),
            })
            .await
            .map_err(NodeError::from_status)?
            .into_inner();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(ids: &[&str]) -> RuntimeNodeManager {
        RuntimeNodeManager::new(
            ids.iter()
                .map(|id| RuntimeNodeConfig {
                    id: (*id).to_string(),
                    addr: format!("http://127.0.0.1:{}", 9000 + id.len()),
                })
                .collect(),
            15,
        )
    }

    fn mark_healthy(manager: &RuntimeNodeManager, id: &str, healthy: bool) {
        manager.health.insert(
            id.to_string(),
            NodeHealth {
                healthy,
                version: None,
                last_checked: Some(Utc::now()),
                last_error: None,
            },
        );
    }

    #[test]
    fn call_order_puts_healthy_nodes_first_and_rotates_them() {
        let manager = manager(&["a", "bb", "ccc"]);
        mark_healthy(&manager, "a", true);
        mark_healthy(&manager, "bb", false);
        mark_healthy(&manager, "ccc", true);

        // Healthy first, unhealthy still listed so a stale cache never
        // means "do not run this at all".
        assert_eq!(manager.call_order(0), vec!["a", "ccc", "bb"]);
        assert_eq!(manager.call_order(1), vec!["ccc", "a", "bb"]);
        assert_eq!(manager.call_order(2), vec!["a", "ccc", "bb"]);
    }

    #[test]
    fn call_order_falls_back_to_every_node_when_none_is_known_healthy() {
        let manager = manager(&["a", "bb"]);
        // Nothing probed yet: both are "not checked yet", not "dead".
        assert_eq!(manager.call_order(0), vec!["a", "bb"]);
        assert!(manager.ids() == vec!["a".to_string(), "bb".to_string()]);
        assert!(!manager.is_healthy("a"));
    }

    #[test]
    fn call_order_of_an_empty_fleet_is_empty() {
        let manager = manager(&[]);
        assert!(manager.call_order(0).is_empty());
        assert!(manager.is_empty());
    }
}
