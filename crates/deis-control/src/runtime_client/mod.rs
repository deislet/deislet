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

/// How this control plane came to know about a node.
///
/// The distinction is not cosmetic: a configured node is a standing promise in
/// a file an operator edits, so it stays on the list whether or not anything
/// ever answers at that address. A registered node is a claim the node itself
/// made and has to keep making; when it stops, the entry is dropped, because
/// otherwise a decommissioned machine keeps collecting deployment pushes and
/// cron fires that silently go nowhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeSource {
    /// Listed in `deis_runtime_nodes`. Never expires.
    Configured,
    /// The node called `PUT /api/nodes/{id}/registration`. Expires when it
    /// stops refreshing that registration.
    Registered,
}

impl NodeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeSource::Configured => "configured",
            NodeSource::Registered => "registered",
        }
    }
}

/// One node this control plane may talk to.
#[derive(Debug, Clone)]
struct NodeEntry {
    config: RuntimeNodeConfig,
    /// How it first appeared. A configured node that also registers keeps
    /// this value - and with it the promise that it is never expired - while
    /// taking the address the live node reported.
    source: NodeSource,
    /// Version the node reported when it registered, before any health probe
    /// has been able to confirm it.
    version: Option<String>,
    /// First time this node registered, absent for a purely configured one.
    registered_at: Option<DateTime<Utc>>,
    /// Last time it registered or refreshed. What expiry measures.
    last_registered_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NodeView {
    pub id: String,
    pub addr: String,
    /// Whether this node is in the configuration file or registered itself.
    pub source: NodeSource,
    /// When it first registered; null for a node that only exists in the
    /// configuration file.
    pub registered_at: Option<DateTime<Utc>>,
    /// When it last refreshed its registration.
    pub last_registered_at: Option<DateTime<Utc>>,
    #[serde(flatten)]
    pub health: NodeHealth,
}

/// What a registration call did, for the answer the node gets and the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registered {
    /// A node this control plane had never heard of.
    New,
    /// A node that was already on the list and moved to another address.
    AddressChanged,
    /// The same node at the same address, refreshing its registration.
    Refreshed,
}

/// Why a registration was refused. Both are the caller's mistake, and both
/// have to be refused rather than stored: an entry nothing can dial would
/// collect deployment pushes and cron fires that never arrive anywhere.
#[derive(Debug, thiserror::Error)]
pub enum RegistrationError {
    #[error("node id must not be empty")]
    EmptyId,
    #[error(
        "node id may only contain letters, digits, '-', '_' or '.', and must be at most 128 bytes"
    )]
    InvalidId,
    #[error("address {addr:?} is not a usable gRPC endpoint: {reason}")]
    InvalidAddress { addr: String, reason: String },
}

/// A node id has to survive being put in a URL path and in a log line, and it
/// keys everything this control plane records about a node, so the accepted
/// shape is spelled out rather than left to whatever a node sends.
fn valid_node_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Refuse anything this control plane could not dial as a gRPC endpoint.
///
/// Parsing alone is not enough: `10.0.0.7:8086` parses perfectly well as a
/// URI in authority form, and would be stored, counted as a delivery target
/// and handed cron occurrences - each of which would then fail at connect
/// time with "URI scheme is missing", one occurrence at a time, for ever. The
/// scheme and host are therefore checked here, where the mistake can still be
/// answered with a 400 the node's own log will show.
fn check_node_address(addr: &str) -> Result<(), RegistrationError> {
    let invalid = |reason: &str| RegistrationError::InvalidAddress {
        addr: addr.to_string(),
        reason: reason.to_string(),
    };

    let uri = tonic::transport::Uri::try_from(addr).map_err(|e| invalid(&e.to_string()))?;

    match uri.scheme_str() {
        Some("http") | Some("https") => {}
        Some(other) => {
            return Err(invalid(&format!(
                "scheme {other:?} cannot be dialled; use http:// or https://"
            )));
        }
        None => return Err(invalid("no scheme; write it as http://host:port")),
    }
    if uri.host().is_none_or(str::is_empty) {
        return Err(invalid("no host"));
    }

    // The last word belongs to the thing that will actually dial it.
    Endpoint::from_shared(addr.to_string())
        .map(|_| ())
        .map_err(|e| invalid(&e.to_string()))
}

/// The fleet: which nodes exist, how to reach them, and how they are doing.
///
/// Two sources feed it, and the difference matters at startup. Nodes named in
/// `deis_runtime_nodes` are seeded here before anything is running, which is
/// what a control plane and its nodes started together by one script rely on.
/// Nodes that register themselves arrive later and are added as they call in,
/// which is what lets a machine join a running platform without editing the
/// control plane's file (architecture principle 8, the same direction the
/// assignment pull already takes).
///
/// A registered node has to keep saying it is there. Its registration expires
/// after [`Self::registration_ttl`] without a refresh, and it is then dropped
/// from every fan-out: an entry left behind by a machine that was thrown away
/// would take cron fires that never run and would be counted as a delivery
/// target that always fails.
pub struct RuntimeNodeManager {
    /// Ordered by id, so the fan-out and the rotation are the same on every
    /// call regardless of the order nodes happened to register in. A plain
    /// `RwLock` rather than a lock-free map because writes are rare - a
    /// registration or an expiry - and every read is a short, non-async
    /// snapshot.
    nodes: std::sync::RwLock<std::collections::BTreeMap<String, NodeEntry>>,
    health: DashMap<String, NodeHealth>,
    poll_interval: Duration,
    registration_ttl: Duration,
}

impl RuntimeNodeManager {
    /// Seed the fleet from the configuration file.
    ///
    /// `registration_ttl_secs` is how long a self-registered node stays on the
    /// list without refreshing; it does not apply to the configured ones.
    pub fn new_with_ttl(
        nodes: Vec<RuntimeNodeConfig>,
        poll_interval_secs: u64,
        registration_ttl_secs: u64,
    ) -> Self {
        let health = DashMap::new();
        let mut entries = std::collections::BTreeMap::new();
        for node in nodes {
            // The file's entries are not refused - the file is a statement
            // that the node exists, and dropping one would make the running
            // fleet disagree with what an operator can read. But an address
            // nothing can dial only shows up otherwise as a connect failure
            // per deployment and per cron occurrence, so it is said once,
            // here, at the level it deserves.
            if let Err(e) = check_node_address(&node.addr) {
                tracing::error!(
                    node = %node.id,
                    error = %e,
                    "A node in deis_runtime_nodes has an address this control plane cannot dial; it stays on the fleet list because the file says it exists, so every deployment push to it and every cron occurrence handed to it will fail until the address is corrected"
                );
            }
            health.insert(node.id.clone(), NodeHealth::default());
            entries.insert(
                node.id.clone(),
                NodeEntry {
                    config: node,
                    source: NodeSource::Configured,
                    version: None,
                    registered_at: None,
                    last_registered_at: None,
                },
            );
        }
        Self {
            nodes: std::sync::RwLock::new(entries),
            health,
            poll_interval: Duration::from_secs(poll_interval_secs.max(1)),
            registration_ttl: Duration::from_secs(registration_ttl_secs.max(1)),
        }
    }

    /// Seed the fleet with the default registration lifetime. Kept for call
    /// sites - tests, mostly - that have no opinion about expiry.
    pub fn new(nodes: Vec<RuntimeNodeConfig>, poll_interval_secs: u64) -> Self {
        Self::new_with_ttl(
            nodes,
            poll_interval_secs,
            crate::config::default_node_registration_ttl(),
        )
    }

    /// Read the node map, surviving a poisoned lock rather than panicking.
    ///
    /// Nothing here panics while holding the lock, so a poisoned lock can only
    /// come from an unrelated panic elsewhere in the process; refusing to serve
    /// the fleet after that would take down cron and delivery for a reason
    /// that has nothing to do with either.
    fn read(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, std::collections::BTreeMap<String, NodeEntry>> {
        self.nodes.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, std::collections::BTreeMap<String, NodeEntry>> {
        self.nodes.write().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// How to reach one node, by id.
    pub fn node(&self, id: &str) -> Option<RuntimeNodeConfig> {
        self.read().get(id).map(|entry| entry.config.clone())
    }

    /// How long a self-registered node stays on the list without refreshing.
    pub fn registration_ttl(&self) -> Duration {
        self.registration_ttl
    }

    /// How often a node should refresh its registration.
    ///
    /// A third of the lifetime, so two refreshes can be lost - a restart of
    /// this process, a dropped request - before a node that is perfectly
    /// healthy disappears from the fleet.
    pub fn registration_refresh_interval(&self) -> Duration {
        Duration::from_secs((self.registration_ttl.as_secs() / 3).max(1))
    }

    /// Record that a node is there and reachable at `addr`.
    ///
    /// Called by `PUT /api/nodes/{id}/registration`, which authenticates the
    /// caller with the node credential. The address is validated here rather than at
    /// first use: an entry that cannot be dialled at all is worse than no
    /// entry, because it is counted as a delivery target and takes cron fires
    /// that then have to be handed to another node.
    ///
    /// **A configured node keeps the address from the file.** A node reports
    /// the address it believes reaches it, which it derives from its own
    /// hostname; the operator who wrote the file knows the address that
    /// actually routes from here. Letting the self-report win would let a node
    /// whose hostname does not resolve from this process replace a working
    /// address with a broken one, and the symptom - every deployment push and
    /// every cron fire to that node failing - would appear on a fleet that
    /// nobody had touched. The registration still counts: it refreshes the
    /// lease and updates the version, which is what it is for.
    pub fn register(
        &self,
        id: &str,
        addr: &str,
        version: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<Registered, RegistrationError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(RegistrationError::EmptyId);
        }
        if !valid_node_id(id) {
            return Err(RegistrationError::InvalidId);
        }
        let addr = addr.trim().to_string();
        check_node_address(&addr)?;

        let mut nodes = self.write();
        match nodes.get_mut(id) {
            Some(entry) => {
                let outcome = if entry.config.addr == addr {
                    Registered::Refreshed
                } else if entry.source == NodeSource::Configured {
                    // Said at warn because it is nearly always a real
                    // disagreement worth fixing: the file is stale, or the node
                    // is not where the operator thinks. Traffic is unaffected
                    // either way, since the file's address is the one used.
                    warn!(
                        node = %id,
                        configured_addr = %entry.config.addr,
                        reported_addr = %addr,
                        "A node in deis_runtime_nodes reported a different address; the file's                          address is kept and every management call still goes there. Update the                          file if the node has really moved, otherwise this node is reporting a                          hostname that does not route from here"
                    );
                    Registered::Refreshed
                } else {
                    entry.config.addr = addr;
                    Registered::AddressChanged
                };
                entry.version = version;
                entry.registered_at.get_or_insert(now);
                entry.last_registered_at = Some(now);
                Ok(outcome)
            }
            None => {
                nodes.insert(
                    id.to_string(),
                    NodeEntry {
                        config: RuntimeNodeConfig {
                            id: id.to_string(),
                            addr,
                        },
                        source: NodeSource::Registered,
                        version,
                        registered_at: Some(now),
                        last_registered_at: Some(now),
                    },
                );
                drop(nodes);
                self.health.insert(id.to_string(), NodeHealth::default());
                Ok(Registered::New)
            }
        }
    }

    /// Take a node off the list because it said it is going away.
    ///
    /// Returns how the entry had arrived, or `None` when there was none. A
    /// configured node cannot be removed this way: the file says it exists, so
    /// dropping it would make the running fleet disagree with the file until
    /// somebody restarted the process.
    pub fn deregister(&self, id: &str) -> Option<NodeSource> {
        let mut nodes = self.write();
        let source = nodes.get(id)?.source;
        if source == NodeSource::Registered {
            nodes.remove(id);
            drop(nodes);
            self.health.remove(id);
        }
        Some(source)
    }

    /// Drop registrations nobody has refreshed inside the lifetime.
    ///
    /// Returns the ids dropped, so the caller can say what stopped being a
    /// delivery target rather than only how many did.
    pub fn expire_stale_registrations(&self, now: DateTime<Utc>) -> Vec<String> {
        let ttl = match chrono::Duration::from_std(self.registration_ttl) {
            Ok(ttl) => ttl,
            // Only an absurd configured value gets here; keeping every
            // registration is the safe reading of "cannot tell how old".
            Err(_) => return Vec::new(),
        };
        let mut nodes = self.write();
        let stale: Vec<String> = nodes
            .iter()
            .filter(|(_, entry)| entry.source == NodeSource::Registered)
            .filter(|(_, entry)| entry.last_registered_at.is_none_or(|last| now - last > ttl))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &stale {
            nodes.remove(id);
        }
        drop(nodes);
        for id in &stale {
            self.health.remove(id);
        }
        stale
    }

    /// Every node id, ordered by id.
    ///
    /// This is the delivery fan-out: every node runs every application
    /// (architecture principle 2), so a deployment goes to all of them.
    pub fn ids(&self) -> Vec<String> {
        self.read().keys().cloned().collect()
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
        let nodes = self.read();
        if nodes.is_empty() {
            return Vec::new();
        }
        let (healthy, unhealthy): (Vec<String>, Vec<String>) = nodes
            .keys()
            .cloned()
            .partition(|id| self.health.get(id).map(|h| h.healthy).unwrap_or(false));
        drop(nodes);

        let mut ordered = Vec::with_capacity(healthy.len() + unhealthy.len());
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
        self.read()
            .values()
            .map(|entry| {
                let mut health = self
                    .health
                    .get(&entry.config.id)
                    .map(|h| h.clone())
                    .unwrap_or_default();
                // Before the first probe the only version anybody knows is
                // the one the node reported when it registered; showing an
                // empty column there reads as "this node told us nothing".
                if health.version.is_none() {
                    health.version = entry.version.clone();
                }
                NodeView {
                    id: entry.config.id.clone(),
                    addr: entry.config.addr.clone(),
                    source: entry.source,
                    registered_at: entry.registered_at,
                    last_registered_at: entry.last_registered_at,
                    health,
                }
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

        if matches!(result, Err(NodeError::UnknownNode)) {
            // The node left the fleet between the caller deciding to probe it
            // and this call. Writing a health row now would put an entry back
            // for a node that is no longer on the list.
            return result.map(|_| NodeHealth::default());
        }

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
    ///
    /// `environment` is the other half of the target's identity. A node runs
    /// production and every preview of the same application side by side,
    /// each with its own version and its own secrets, so a fire that named
    /// only the application would run whichever one the node picked - in
    /// practice the default environment, with the wrong code and the wrong
    /// secrets whenever the schedule belonged to a preview.
    pub async fn trigger_scheduled(
        &self,
        id: &str,
        app_id: &str,
        environment: &str,
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
                environment: environment.to_string(),
            })
            .await
            .map_err(NodeError::from_status)?
            .into_inner();
        Ok(resp)
    }

    /// Unload (stop and remove) an application from a node.
    ///
    /// `environment` names which environment of the app to take off; an
    /// empty string means every environment of it, which is what deleting
    /// the application does. The pair is the identity of a running
    /// application on a node, so unloading `demo` in a preview must leave
    /// `demo` in production serving.
    pub async fn unload_app(
        &self,
        id: &str,
        app_id: &str,
        environment: &str,
    ) -> Result<proto::UnloadAppResponse, NodeError> {
        let mut client = self.connect(id).await?;
        info!(
            node = id,
            app_id,
            environment = if environment.is_empty() {
                "<all>"
            } else {
                environment
            },
            "Telling a runtime node to stop serving this application; it will not come back from an assignment refresh, because the catalog no longer names it"
        );
        let resp = client
            .unload_app(proto::UnloadAppRequest {
                app_id: app_id.to_string(),
                environment: environment.to_string(),
            })
            .await
            .map_err(NodeError::from_status)?;
        Ok(resp.into_inner())
    }

    /// Background loop keeping the health cache warm and dropping
    /// registrations nobody refreshed.
    ///
    /// It runs even with nothing configured, because a node can register at
    /// any moment; an empty fleet simply costs one wake-up per interval.
    ///
    /// It stops on `shutdown` instead of being aborted. An abort can land in
    /// the middle of a probe and leave this node's health cache saying
    /// whatever it said before, which is not the sort of thing to leave behind
    /// on the way out; more importantly the same rule has to hold for the cron
    /// scheduler, which does write commands, and one stop mechanism for both
    /// is one thing to reason about.
    pub async fn poll_loop(self: Arc<Self>, shutdown: crate::shutdown::Shutdown) {
        info!(
            nodes = self.len(),
            interval_secs = self.poll_interval.as_secs(),
            registration_ttl_secs = self.registration_ttl.as_secs(),
            "Starting deis_runtime node health polling; nodes may also register themselves at any time"
        );
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.wait() => {
                    info!(
                        "Node health polling stopped for shutdown; no further probe runs from this instance, so the health this process last reported is the health it reports"
                    );
                    return;
                }
                _ = ticker.tick() => {}
            }

            let expired = self.expire_stale_registrations(Utc::now());
            if !expired.is_empty() {
                warn!(
                    nodes = ?expired,
                    ttl_secs = self.registration_ttl.as_secs(),
                    remaining = self.len(),
                    "These nodes stopped refreshing their registration and were dropped from the fleet; they receive no further deployment push and no cron fire, and they come back on their own by registering again"
                );
            }

            for node in self.ids() {
                if shutdown.is_triggered() {
                    return;
                }
                let was_healthy = self.health.get(&node).map(|h| h.healthy).unwrap_or(false);
                match self.check_node(&node).await {
                    Ok(h) => {
                        if !was_healthy {
                            info!(node = %node, version = ?h.version, "Runtime node is healthy");
                        }
                    }
                    Err(NodeError::UnknownNode) => {
                        // Registered away, or expired, between listing the
                        // ids and probing this one. Nothing to report.
                    }
                    Err(e) => {
                        if was_healthy {
                            warn!(node = %node, error = %e, "Runtime node became unreachable");
                        } else {
                            debug!(node = %node, error = %e, "Runtime node still unreachable");
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

    /// The point of self-registration: a node nobody wrote into a file is a
    /// full member of the fleet, so a deployment fans out to it and a cron
    /// occurrence can be handed to it.
    #[test]
    fn a_registered_node_is_a_delivery_target_and_a_cron_candidate() {
        let manager = manager(&["from-file"]);
        let now = Utc::now();

        assert_eq!(
            manager
                .register("joined", "http://10.0.0.5:8086", Some("0.1.0".into()), now)
                .unwrap(),
            Registered::New
        );

        // ids() is the delivery fan-out and call_order() is what the cron
        // scheduler picks from; both have to contain it.
        assert_eq!(
            manager.ids(),
            vec!["from-file".to_string(), "joined".to_string()]
        );
        assert!(manager.call_order(0).contains(&"joined".to_string()));
        assert_eq!(manager.node("joined").unwrap().addr, "http://10.0.0.5:8086");

        let view = manager
            .list()
            .into_iter()
            .find(|v| v.id == "joined")
            .expect("a registered node must be visible to the dashboard");
        assert_eq!(view.source, NodeSource::Registered);
        assert_eq!(view.registered_at, Some(now));
        // Nothing has probed it yet, so the version it claimed is the only
        // one anybody has.
        assert_eq!(view.health.version.as_deref(), Some("0.1.0"));
        assert!(!view.health.healthy);
    }

    /// A node in the file reports whatever hostname it has; the file says
    /// what routes from here. If the self-report won, a node whose hostname
    /// does not resolve from this process would replace a working address
    /// with a broken one, and every deployment push and cron fire to it would
    /// start failing on a fleet nobody had touched.
    #[test]
    fn a_configured_node_keeps_the_address_from_the_file() {
        let manager = manager(&["from-file"]);
        let from_file = manager.node("from-file").unwrap().addr;
        let now = Utc::now();

        assert_eq!(
            manager
                .register("from-file", "http://short-hostname:8086", None, now)
                .unwrap(),
            Registered::Refreshed,
            "the lease is refreshed; the address is not adopted"
        );
        assert_eq!(
            manager.node("from-file").unwrap().addr,
            from_file,
            "the operator's address has to survive the node's self-report"
        );

        // The registration still counts for liveness, which is the whole
        // reason a configured node registers at all.
        let view = manager
            .list()
            .into_iter()
            .find(|v| v.id == "from-file")
            .unwrap();
        assert_eq!(view.last_registered_at, Some(now));
        assert_eq!(view.source, NodeSource::Configured);
    }

    /// A node that registered itself is the only source of its own address, so
    /// moving is the one thing it can always report.
    #[test]
    fn a_registered_node_may_move() {
        let manager = manager(&[]);
        let now = Utc::now();

        manager
            .register("joined", "http://10.0.0.5:8086", None, now)
            .unwrap();
        assert_eq!(
            manager
                .register("joined", "http://10.0.0.9:8086", None, now)
                .unwrap(),
            Registered::AddressChanged
        );
        assert_eq!(manager.node("joined").unwrap().addr, "http://10.0.0.9:8086");
    }

    /// An id or an address that cannot be used is refused rather than stored:
    /// an entry nothing can dial would still be counted as a delivery target
    /// and would take cron occurrences away from nodes that work.
    #[test]
    fn an_unusable_registration_is_refused() {
        let manager = manager(&[]);
        let now = Utc::now();

        assert!(matches!(
            manager.register("  ", "http://10.0.0.5:8086", None, now),
            Err(RegistrationError::EmptyId)
        ));
        assert!(matches!(
            manager.register("a b", "http://10.0.0.5:8086", None, now),
            Err(RegistrationError::InvalidId)
        ));
        assert!(matches!(
            manager.register("node", "10.0.0.5:8086", None, now),
            Err(RegistrationError::InvalidAddress { .. })
        ));
        assert!(manager.is_empty());
    }

    /// A machine that was thrown away must stop collecting work. The entry
    /// goes when nobody refreshes it, and the file's entries do not, because
    /// the file is a standing statement that the node exists.
    #[test]
    fn a_registration_nobody_refreshes_expires_but_a_configured_node_does_not() {
        let manager = RuntimeNodeManager::new_with_ttl(
            vec![RuntimeNodeConfig {
                id: "from-file".to_string(),
                addr: "http://127.0.0.1:8086".to_string(),
            }],
            15,
            90,
        );
        let start = Utc::now();
        manager
            .register("joined", "http://10.0.0.5:8086", None, start)
            .unwrap();
        // A node the operator also wrote into the file: registering must not
        // turn it into something that can expire.
        manager
            .register("from-file", "http://10.0.0.9:8086", None, start)
            .unwrap();

        // Inside the lifetime nothing is dropped.
        assert!(
            manager
                .expire_stale_registrations(start + chrono::Duration::seconds(89))
                .is_empty()
        );
        assert_eq!(manager.len(), 2);

        // A refresh restarts the clock.
        assert_eq!(
            manager
                .register(
                    "joined",
                    "http://10.0.0.5:8086",
                    None,
                    start + chrono::Duration::seconds(80),
                )
                .unwrap(),
            Registered::Refreshed
        );
        assert!(
            manager
                .expire_stale_registrations(start + chrono::Duration::seconds(150))
                .is_empty(),
            "a node that refreshed 70 seconds ago is still there"
        );

        let dropped = manager.expire_stale_registrations(start + chrono::Duration::seconds(300));
        assert_eq!(dropped, vec!["joined".to_string()]);
        assert_eq!(
            manager.ids(),
            vec!["from-file".to_string()],
            "the file still says this node exists, whatever it has been doing"
        );
        assert!(manager.node("joined").is_none());
        assert!(!manager.list().iter().any(|v| v.id == "joined"));
    }

    /// How often a node is told to come back: often enough that two lost
    /// refreshes do not cost a healthy node its place on the fleet.
    #[test]
    fn the_refresh_interval_leaves_room_for_two_lost_refreshes() {
        let manager = RuntimeNodeManager::new_with_ttl(Vec::new(), 15, 90);
        assert_eq!(manager.registration_ttl().as_secs(), 90);
        assert_eq!(manager.registration_refresh_interval().as_secs(), 30);

        // Even an absurdly short lifetime must not ask for a zero-second
        // refresh loop, which would be a node calling in as fast as it can.
        let manager = RuntimeNodeManager::new_with_ttl(Vec::new(), 15, 1);
        assert_eq!(manager.registration_refresh_interval().as_secs(), 1);
    }

    /// A node that says it is going away stops being a target immediately,
    /// instead of collecting pushes and cron fires until its lease runs out.
    #[test]
    fn deregistering_takes_a_node_off_at_once() {
        let manager = manager(&["from-file"]);
        manager
            .register("joined", "http://10.0.0.5:8086", None, Utc::now())
            .unwrap();

        assert_eq!(manager.deregister("joined"), Some(NodeSource::Registered));
        assert_eq!(manager.ids(), vec!["from-file".to_string()]);
        assert_eq!(
            manager.deregister("from-file"),
            Some(NodeSource::Configured),
            "the answer says why it stayed"
        );
        assert_eq!(manager.ids(), vec!["from-file".to_string()]);
        assert_eq!(manager.deregister("never-seen"), None);
    }
}
