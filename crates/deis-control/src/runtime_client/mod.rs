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

/// The invocation API of a node: `OnScheduled` and `OnQueue`.
///
/// **This control plane calls exactly one of the two, and that is a decision
/// rather than a gap.**
///
/// `OnScheduled` is cron. A schedule belongs to an application and not to a
/// node (architecture principle 6), so this process is the one that keeps it,
/// picks a node per occurrence and fires it - see [`crate::scheduler`].
///
/// `OnQueue` has no caller here and is not going to get one. A node leases its
/// own messages straight from deis-queue: the deployment payload hands it that
/// service's address (`crate::delivery::StorageWiring`) and this process is
/// then out of the path entirely - it holds no queue client, never sees a
/// message and has no lease it could pass on. What `OnQueue` is for is a
/// caller that *does* hold the lease and wants the handler run elsewhere,
/// which is why its response names the messages the handler did not complete
/// and leaves acknowledging the rest to whoever called. For this control plane
/// to use it, it would first have to start leasing messages it has no reason
/// to hold, and then own the acknowledgement of every batch it pushed - a
/// second delivery path racing the nodes' own pollers for messages that were
/// already being delivered.
///
/// The generated client below therefore carries an `on_queue` method that
/// nothing calls: the service is generated whole from
/// `deis-runtime/proto/trigger.proto`. A call site added here would be that
/// design change, not an oversight being corrected.
pub mod trigger {
    tonic::include_proto!("deis.trigger");
}

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use deis_wire::conventions::{NODE_AUTH_METADATA, node_auth_value};
use serde::Serialize;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint};
use tracing::{debug, error, info, warn};

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
/// default), so that a slow-but-successful cron run answers inside it rather
/// than being cut off at 60s while the handler is still going.
///
/// What happens when it expires anyway is not this constant's job to prevent:
/// tonic turns the elapsed deadline into a `Cancelled` status,
/// [`NodeError::left_the_outcome_unknown`] reads that as "the node was given
/// this and never answered", and `crate::scheduler` stops the fleet walk there
/// instead of handing the occurrence to the next node. Widening this number
/// makes the case rarer; it is the reading that makes a job charge a card
/// once instead of once per node.
const TRIGGER_TIMEOUT: Duration = Duration::from_secs(60);

/// How many lost nodes are remembered at once. See [`DepartedNode`].
///
/// Two orders of magnitude above any fleet this platform runs, so reaching it
/// means node ids are being invented rather than reused - and the alternative,
/// an unbounded map fed by whoever holds a node key, is worse than forgetting
/// the oldest entry and saying so.
const MAX_DEPARTED_NODES: usize = 256;

/// The node key this control plane presents on every call into a node's own
/// management plane.
///
/// A node serves `ApplyDeployment`, `UnloadApp`, `GetStatus`, `StartInspect`,
/// `PushConfig` and the trigger RPCs only to a caller that proves it is
/// entitled to drive it, because those RPCs between them can take an
/// application offline, list every tenant on the node and replace one
/// tenant's code with a bundle of the caller's choosing. Of the trigger pair
/// this control plane only ever dials `OnScheduled`; see [`mod@trigger`] for
/// why `OnQueue` has no caller here. The proof is one of
/// this control plane's `node_preshared_keys` - the same value the node
/// already holds as `[control] secret` for the other direction - and it is
/// attached here as an interceptor rather than at each call site so that an
/// RPC added later cannot go out anonymous.
///
/// `None` is a control plane with no node credential at all, which
/// [`crate::config::ControlConfig::ensure_node_credential`] makes unreachable
/// in the binary. Calls then go out unauthenticated and every guarded node
/// refuses them, which is the safe end of the two.
#[derive(Clone, Default)]
pub struct NodeCallCredential {
    value: Option<MetadataValue<Ascii>>,
}

impl NodeCallCredential {
    /// Build the metadata this control plane presents, from a
    /// `node_preshared_keys` value.
    pub fn new(key: Option<&str>) -> Self {
        let Some(key) = key.map(str::trim).filter(|key| !key.is_empty()) else {
            return Self { value: None };
        };
        match MetadataValue::try_from(node_auth_value(key)) {
            Ok(value) => Self { value: Some(value) },
            Err(_) => {
                // gRPC metadata admits only visible ASCII. A key with a
                // newline or a non-ASCII character in it cannot be sent at
                // all, and saying so here is the only chance an operator has
                // to connect the fleet going quiet with the value in the file.
                error!(
                    "A node_preshared_keys value contains characters that cannot be sent as gRPC metadata, so this control plane presents no credential to its nodes; every deployment push and every cron fire will be refused. Use printable ASCII, for instance the output of `openssl rand -hex 32`"
                );
                Self { value: None }
            }
        }
    }

    /// Whether anything is presented. Reported at startup rather than
    /// inferred from a fleet that has gone quiet.
    pub fn is_present(&self) -> bool {
        self.value.is_some()
    }
}

/// Redacted on purpose: this is the credential that drives every node in the
/// fleet, and a `Debug` that printed it would put it in every error context
/// that formats the node manager.
impl std::fmt::Debug for NodeCallCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeCallCredential")
            .field("present", &self.is_present())
            .finish()
    }
}

impl tonic::service::Interceptor for NodeCallCredential {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(value) = &self.value {
            request
                .metadata_mut()
                .insert(NODE_AUTH_METADATA, value.clone());
        }
        Ok(request)
    }
}

/// A channel to one node with this control plane's credential on every call.
type NodeChannel = InterceptedService<Channel, NodeCallCredential>;

/// Raise both message limits from the tonic default of 4 MB.
///
/// Deployment metadata and status responses grow with the number of
/// applications a node runs, and a truncated response would look like a
/// dead node rather than an oversized message.
fn with_message_limits(
    client: RuntimeManagementClient<NodeChannel>,
) -> RuntimeManagementClient<NodeChannel> {
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

/// A node this control plane had on its fleet list and then lost.
///
/// Losing a registration is not the same as a node going away. It expires
/// because the node **stopped talking to this control plane**, which is
/// exactly the state in which it is still running whatever it last received
/// and cannot be told anything new: it is not reachable for a push, and it is
/// not asking for its assignments either, so nothing corrects it. Dropping the
/// entry and remembering nothing is what let a deploy - and, worse, an
/// application deletion - answer "every node took it" while a live node kept
/// serving the old code and the deleted app's secrets.
///
/// The record is cleared when the node registers again, or when somebody says
/// the machine is really gone with `DELETE /api/nodes/{id}/registration`.
#[derive(Debug, Clone, Serialize)]
pub struct DepartedNode {
    pub id: String,
    /// Where it was last known to answer.
    pub addr: String,
    /// Last time it refreshed its registration.
    pub last_registered_at: Option<DateTime<Utc>>,
    /// When this control plane dropped it from the fleet list.
    pub departed_at: DateTime<Utc>,
}

impl DepartedNode {
    /// Why a fleet-wide operation could not include this node, in the words
    /// the answer to a deploy or a delete carries back to whoever ran it.
    pub fn describe(&self) -> String {
        let last_seen = match self.last_registered_at {
            Some(at) => at.to_rfc3339(),
            None => "never".to_string(),
        };
        format!(
            "this node stopped refreshing its registration (last seen {last_seen}) and was \
             dropped from the fleet at {}, so nothing could be sent to it and it is still \
             running whatever it last received. It rejoins by registering again; if the machine \
             is really gone, DELETE /api/nodes/{}/registration clears this record",
            self.departed_at.to_rfc3339(),
            self.id
        )
    }
}

/// Who answers at one address, as that node describes itself.
///
/// Read from `GetStatus`, which is the only RPC that carries a node's own id
/// back. It is what lets a registration tell "this machine moved" from "a
/// second machine is claiming the first one's name".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIdentity {
    /// The id the node calls itself. Empty when the node did not say.
    pub node_id: String,
    /// Hostname the node reports, so the refusal names a machine an operator
    /// can go and look at rather than only an address.
    pub hostname: String,
    /// Seconds that node's process has been up.
    pub uptime_seconds: i64,
}

/// A node id that two live machines are claiming at the same time.
///
/// The damage this records is quiet and total. A node id is the whole of a
/// node's identity here: it keys the fleet list, the health cache, the
/// delivery fan-out and the cron rotation. When a second machine registers the
/// id a first machine is still answering for, everything this control plane
/// sends goes to exactly one of them - and the report for a deployment, or for
/// an application deletion, counts one node and says every node took it, while
/// the other machine keeps serving the previous version with the previous
/// secrets and is never told anything again.
///
/// So the claim is refused, the machine already answering keeps the id, and
/// this record exists to make the machine that was refused visible: every
/// fan-out names it as unreached, exactly as a [`DepartedNode`] is named.
///
/// The probe cannot tell two machines from one machine that has started
/// reporting a second address while the first still reaches it, and it does
/// not pretend to: both are answered the same way, because in both the address
/// that was proven live keeps the id and nothing moves silently. The message
/// names both readings and the operator action for each.
///
/// It is cleared when the refusals stop for a whole registration lifetime -
/// which is what happens when whichever machine was wrong is corrected or shut
/// down - or when an operator says the machine is gone with
/// `DELETE /api/nodes/{id}/registration`.
#[derive(Debug, Clone, Serialize)]
pub struct ContestedNodeId {
    pub id: String,
    /// The address that holds the id, and the only one anything is sent to.
    pub holder_addr: String,
    /// Hostname of the machine holding the id, when it said one.
    pub holder_hostname: String,
    /// The address that was refused the id. Nothing is ever sent here, and
    /// this machine is still running whatever it last had.
    pub claimant_addr: String,
    /// First time this claim was refused.
    pub first_refused_at: DateTime<Utc>,
    /// Most recent time it was refused. The claimant keeps registering every
    /// `registration_refresh_interval`, so this is how the record stays alive
    /// and how it expires once the claim stops coming.
    pub last_refused_at: DateTime<Utc>,
}

impl ContestedNodeId {
    /// Why a fleet-wide operation could not include this machine, in the words
    /// the answer to a deploy or a delete carries back to whoever ran it.
    pub fn describe(&self) -> String {
        format!(
            "{} registered this node id while a live node at {}{} was still answering for it, so \
             the claim was refused and nothing is ever sent to {}: whatever is there is still \
             running what it last received, including the previous version of this application. \
             Either two machines were given one node id, in which case change one of them; or \
             this is one machine that now reports a different address from the one that already \
             reaches it, in which case DELETE /api/nodes/{}/registration and let it register \
             again at the address you want used. First refused at {}, last at {}",
            self.claimant_addr,
            self.holder_addr,
            if self.holder_hostname.is_empty() {
                String::new()
            } else {
                format!(" ({})", self.holder_hostname)
            },
            self.claimant_addr,
            self.id,
            self.first_refused_at.to_rfc3339(),
            self.last_refused_at.to_rfc3339(),
        )
    }
}

/// What a deregistration did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deregistered {
    /// A node that was on the fleet list and asked to be taken off. It gets no
    /// further push and no cron fire until it registers again.
    FromFleet,
    /// A node this control plane had already given up on: only the record that
    /// it was lost is cleared, so fleet-wide operations stop reporting it.
    Forgotten,
    /// Named in `deis_runtime_nodes`; nothing was removed, because the file
    /// says the node exists.
    Configured,
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
    /// A live node at another address already answers for this id. See
    /// [`ContestedNodeId`] for what accepting it silently used to cost.
    #[error(
        "node id is already held by a live node at {holder_addr}{}, which answered for it just now; give this machine a different node id, or - if this is the same machine reporting a new address - deregister it first so the new address can be taken",
        if holder_hostname.is_empty() { String::new() } else { format!(" ({holder_hostname})") }
    )]
    IdHeldByAnotherMachine {
        holder_addr: String,
        holder_hostname: String,
    },
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
    /// Registered nodes whose lease ran out, kept so that a fleet-wide
    /// operation can say it did not reach them. See [`DepartedNode`].
    departed: std::sync::RwLock<std::collections::BTreeMap<String, DepartedNode>>,
    /// Machines that were refused a node id another machine is still
    /// answering for, kept for the same reason `departed` is: a fan-out that
    /// named only the machines it can reach reports a deploy as complete while
    /// one of them serves the previous version. Keyed by
    /// `(node id, claimant address)`, because one id can be claimed by more
    /// than one machine. See [`ContestedNodeId`].
    contested: std::sync::RwLock<std::collections::BTreeMap<(String, String), ContestedNodeId>>,
    health: DashMap<String, NodeHealth>,
    poll_interval: Duration,
    registration_ttl: Duration,
    /// What this control plane proves itself with on every call into a node.
    /// Set by [`RuntimeNodeManager::with_node_credential`] from
    /// `node_preshared_keys`; empty until then, which is what the tests that
    /// never dial a node use.
    credential: NodeCallCredential,
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
            departed: std::sync::RwLock::new(std::collections::BTreeMap::new()),
            contested: std::sync::RwLock::new(std::collections::BTreeMap::new()),
            health,
            poll_interval: Duration::from_secs(poll_interval_secs.max(1)),
            registration_ttl: Duration::from_secs(registration_ttl_secs.max(1)),
            credential: NodeCallCredential::default(),
        }
    }

    /// Present `key` on every call this fleet makes into a node.
    ///
    /// Chained onto the constructor rather than passed to it, because the
    /// fleet is also built in places that never dial anything - the delivery
    /// and scheduler tests, the catalog router - and making those name a
    /// credential would say the credential matters there, which it does not.
    /// What matters is that the two places which do serve a live fleet, the
    /// binary and [`crate::api::start_api_server`], both call this.
    pub fn with_node_credential(mut self, key: Option<&str>) -> Self {
        self.credential = NodeCallCredential::new(key);
        if self.credential.is_present() {
            info!(
                header = NODE_AUTH_METADATA,
                "Calls into a node's management plane carry this control plane's node key; a node that does not accept it refuses the push and keeps serving the version it already had"
            );
        } else {
            error!(
                "This control plane has no node key to present, so every deployment push, unload and cron fire it makes will be refused by any node that is not running unauthenticated on loopback. Set node_preshared_keys"
            );
        }
        self
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

    fn departed_write(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, std::collections::BTreeMap<String, DepartedNode>> {
        self.departed.write().unwrap_or_else(|e| e.into_inner())
    }

    fn contested_write(
        &self,
    ) -> std::sync::RwLockWriteGuard<
        '_,
        std::collections::BTreeMap<(String, String), ContestedNodeId>,
    > {
        self.contested.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Machines refused a node id another machine is still answering for,
    /// oldest refusal first.
    ///
    /// Read by every fan-out beside [`Self::departed`]: both answer the same
    /// question - which machines are running this platform's code and cannot
    /// be told anything - and a report that left either of them out is the
    /// report that says a deploy succeeded when a live node still serves the
    /// version before it.
    pub fn contested(&self) -> Vec<ContestedNodeId> {
        let mut contested: Vec<ContestedNodeId> = self
            .contested
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        contested.sort_by_key(|node| node.first_refused_at);
        contested
    }

    /// Nodes this control plane has lost and has not been told about since,
    /// oldest departure first.
    ///
    /// Every fan-out reads this, not only [`Self::ids`]: a node that is no
    /// longer on the list is not a node that stopped existing, and an answer
    /// that describes only the reachable half of the fleet is the answer that
    /// says a deploy succeeded when it did not.
    pub fn departed(&self) -> Vec<DepartedNode> {
        let mut departed: Vec<DepartedNode> = self
            .departed
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        departed.sort_by_key(|node| node.departed_at);
        departed
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

        // Back on the list, so this control plane no longer has a node it has
        // lost: whatever it missed while it was away, its next assignment
        // reconcile and the next push both reach it again.
        let rejoined = self.departed_write().remove(id).is_some();
        if rejoined {
            info!(
                node = %id,
                address = %addr,
                "A node this control plane had given up on registered again; fleet-wide operations stop reporting it as unreached, and it is a delivery target once more"
            );
        }

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
                        "A node in deis_runtime_nodes reported a different address; the file's \
                         address is kept and every management call still goes there. \
                         Update the file if the node has really moved, otherwise this \
                         node is reporting a hostname that does not route from here"
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

    /// Accept a node's registration, refusing a machine that claims an id
    /// another live machine is still answering for.
    ///
    /// This is what `PUT /api/nodes/{id}/registration` calls. [`Self::register`]
    /// underneath it is pure bookkeeping and cannot tell the two ways an
    /// address changes apart:
    ///
    /// * one machine moved - it has a new address and nothing answers at the
    ///   old one, so taking the new address is right; and
    /// * a second machine was given the same node id by mistake - a cloned
    ///   image, a copied configuration file, a scale-out that reused a name -
    ///   in which case both machines are alive and each registration hands the
    ///   id to whichever one called last.
    ///
    /// The second case used to be indistinguishable from the first, and it is
    /// silent: the fleet list still holds one entry, so a deployment is pushed
    /// to one machine, counted against one node and reported as delivered to
    /// the whole fleet, while the other machine goes on serving the previous
    /// version with the previous secrets. So before an address is moved the
    /// address on file is asked who is there. A node that answers and gives
    /// this same id is a machine that has not moved anywhere, and the claim is
    /// refused rather than granted - see [`ContestedNodeId`].
    pub async fn register_claim(
        &self,
        id: &str,
        addr: &str,
        version: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<Registered, RegistrationError> {
        self.register_claim_with(id, addr, version, now, |holder_addr| async move {
            self.identify_at(&holder_addr).await
        })
        .await
    }

    /// [`Self::register_claim`] with the "who is at this address?" call as a
    /// parameter.
    ///
    /// Split out for the same reason `crate::scheduler`'s fleet walk is: the
    /// decision being tested here is which of two live machines keeps a name,
    /// and getting it wrong is not a failure anything would notice - it is a
    /// deploy that reports success while half the fleet serves the old code.
    /// That has to be testable without two live nodes.
    pub(crate) async fn register_claim_with<F, Fut>(
        &self,
        id: &str,
        addr: &str,
        version: Option<String>,
        now: DateTime<Utc>,
        probe: F,
    ) -> Result<Registered, RegistrationError>
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = Option<NodeIdentity>>,
    {
        let trimmed = id.trim();
        let claimed = addr.trim().to_string();

        // Refuse what [`Self::register`] would refuse, before anything is
        // dialled. Without this a node reporting an address nothing can parse
        // would be told its id is held by another machine, which is a true
        // sentence about a request whose real fault is somewhere else.
        if trimmed.is_empty() {
            return Err(RegistrationError::EmptyId);
        }
        if !valid_node_id(trimmed) {
            return Err(RegistrationError::InvalidId);
        }
        check_node_address(&claimed)?;

        // Only an address change on a self-registered node can hand an id
        // from one machine to another. A new id takes nothing from anybody, a
        // refresh at the same address is the same machine, and a configured
        // node keeps the file's address whatever it reports - so none of those
        // is worth a network call.
        let holder = self
            .read()
            .get(trimmed)
            .filter(|entry| entry.source == NodeSource::Registered)
            .map(|entry| entry.config.addr.clone())
            .filter(|held| *held != claimed);

        if let Some(holder_addr) = holder
            && let Some(identity) = probe(holder_addr.clone()).await
        {
            // Something answered `GetStatus` at the address on file, which
            // means a live node holding this control plane's node credential
            // is there. Whether it names this id or another one decides
            // everything: the same id is two machines with one name, and a
            // different id is an address that was reused after the first
            // machine really did move away.
            if identity.node_id.is_empty() || identity.node_id == trimmed {
                let mut contested = self.contested_write();
                let key = (trimmed.to_string(), claimed.clone());
                let first_refused_at = contested
                    .get(&key)
                    .map(|record| record.first_refused_at)
                    .unwrap_or(now);
                let record = ContestedNodeId {
                    id: trimmed.to_string(),
                    holder_addr: holder_addr.clone(),
                    holder_hostname: identity.hostname.clone(),
                    claimant_addr: claimed.clone(),
                    first_refused_at,
                    last_refused_at: now,
                };
                let first_time = first_refused_at == now;
                contested.insert(key, record);
                Self::trim_contested(&mut contested);
                drop(contested);

                if first_time {
                    error!(
                        node = %trimmed,
                        holder_addr = %holder_addr,
                        holder_hostname = %identity.hostname,
                        holder_uptime_seconds = identity.uptime_seconds,
                        claimant_addr = %claimed,
                        "A node id was claimed from a second address while a live node at the address on file was still answering for it. The machine already answering keeps the id and this registration is refused, so nothing is sent to the claimant and it keeps serving whatever it last received - which from the next deployment onwards is the previous version, with the previous secrets. Every deploy and every application deletion now reports it as unreached. Either two machines were given one node id, in which case change one of them; or this is one machine reporting a new address while its old one still reaches it, in which case deregister it and let it register again"
                    );
                } else {
                    warn!(
                        node = %trimmed,
                        holder_addr = %holder_addr,
                        claimant_addr = %claimed,
                        refused_since = %first_refused_at.to_rfc3339(),
                        "The refused machine is still claiming a node id another machine answers for; the claim is refused again and it remains unreachable for deployments and cron"
                    );
                }

                return Err(RegistrationError::IdHeldByAnotherMachine {
                    holder_addr,
                    holder_hostname: identity.hostname,
                });
            }

            warn!(
                node = %trimmed,
                previous_addr = %holder_addr,
                previous_node_id = %identity.node_id,
                new_addr = %claimed,
                "This node's previous address is now answering as a different node, so the address was reused after this one moved; the registration is accepted and every management call goes to the new address"
            );
        }

        let outcome = self.register(trimmed, &claimed, version, now)?;
        // The claim was granted, so this machine is not being refused
        // anything and there is nothing left to report as unreached for it.
        self.contested_write()
            .remove(&(trimmed.to_string(), claimed));
        Ok(outcome)
    }

    /// Ask whichever node is at `addr` who it thinks it is.
    ///
    /// `None` covers everything that is not a live deis-runtime answering with
    /// this control plane's node credential: nothing listening, a connection
    /// refused, a node that refuses the credential, a timeout. All of those
    /// read the same way at the one call site - the machine that held this id
    /// is not there any more - and a registration must not be refused because
    /// a probe was slow.
    async fn identify_at(&self, addr: &str) -> Option<NodeIdentity> {
        let channel = Endpoint::from_shared(addr.to_string())
            .ok()?
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(RPC_TIMEOUT)
            .connect()
            .await
            .map_err(|e| {
                debug!(address = %addr, error = %e, "Nothing answers at this address");
            })
            .ok()?;
        let info = with_message_limits(RuntimeManagementClient::new(self.authorized(channel)))
            .get_status(proto::GetStatusRequest {
                include_apps: false,
            })
            .await
            .map_err(|e| {
                debug!(address = %addr, error = %e, "A node at this address would not say who it is");
            })
            .ok()?
            .into_inner()
            .node_info?;
        Some(NodeIdentity {
            node_id: info.node_id,
            hostname: info.hostname,
            uptime_seconds: info.uptime_seconds,
        })
    }

    /// Keep the contested list bounded, for the reason the departed list is
    /// bounded: the ids and addresses in it come from whoever holds a node
    /// key.
    fn trim_contested(
        contested: &mut std::collections::BTreeMap<(String, String), ContestedNodeId>,
    ) {
        while contested.len() > MAX_DEPARTED_NODES {
            let Some(oldest) = contested
                .values()
                .min_by_key(|node| node.last_refused_at)
                .map(|node| (node.id.clone(), node.claimant_addr.clone()))
            else {
                break;
            };
            contested.remove(&oldest);
            warn!(
                node = %oldest.0,
                claimant_addr = %oldest.1,
                kept = MAX_DEPARTED_NODES,
                "More than {MAX_DEPARTED_NODES} machines have been refused a node id at once, so the oldest refusal is being forgotten; if that machine is still running it will no longer be reported as unreached by a deployment or a deletion"
            );
        }
    }

    /// Drop every refused claim on one node id. Returns whether there was
    /// one, so a deregistration can say it removed something.
    fn forget_contested(&self, id: &str) -> bool {
        let mut contested = self.contested_write();
        let keys: Vec<(String, String)> = contested
            .keys()
            .filter(|(node, _)| node == id)
            .cloned()
            .collect();
        for key in &keys {
            contested.remove(key);
        }
        !keys.is_empty()
    }

    /// Take a node off the list because it said it is going away.
    ///
    /// Returns what was done, or `None` when this control plane has never
    /// heard of the id. A configured node cannot be removed this way: the file
    /// says it exists, so dropping it would make the running fleet disagree
    /// with the file until somebody restarted the process.
    ///
    /// It also clears a node from [`Self::departed`], which is the only way an
    /// operator has to say "that machine is really gone". Without it, a node
    /// that was decommissioned without deregistering would be named as
    /// unreached by every deployment and every deletion for ever.
    pub fn deregister(&self, id: &str) -> Option<Deregistered> {
        let mut nodes = self.write();
        let Some(entry) = nodes.get(id) else {
            drop(nodes);
            // `DELETE /api/nodes/{id}/registration` is the one lever an
            // operator has for "that machine is really gone", so it clears
            // both records this control plane keeps about machines it cannot
            // reach - the lost one and the refused one - and not only the
            // first.
            let forgotten = self.departed_write().remove(id).is_some();
            let uncontested = self.forget_contested(id);
            return (forgotten || uncontested).then_some(Deregistered::Forgotten);
        };
        if entry.source == NodeSource::Configured {
            return Some(Deregistered::Configured);
        }
        nodes.remove(id);
        drop(nodes);
        self.health.remove(id);
        // A node that says it is stopping is not a node this control plane
        // lost track of, so it leaves no record behind.
        self.departed_write().remove(id);
        self.forget_contested(id);
        Some(Deregistered::FromFleet)
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
        let stale: Vec<NodeEntry> = nodes
            .values()
            .filter(|entry| entry.source == NodeSource::Registered)
            .filter(|entry| entry.last_registered_at.is_none_or(|last| now - last > ttl))
            .cloned()
            .collect();
        for entry in &stale {
            nodes.remove(&entry.config.id);
        }
        drop(nodes);

        // Remembered, not forgotten. This node stopped talking to this control
        // plane, which is precisely the state in which it is still serving what
        // it last received and nothing is going to correct it; a fan-out that
        // only knew about the reachable half of the fleet used to report a
        // deploy, and an application deletion, as complete.
        let mut departed = self.departed_write();
        for entry in &stale {
            departed.insert(
                entry.config.id.clone(),
                DepartedNode {
                    id: entry.config.id.clone(),
                    addr: entry.config.addr.clone(),
                    last_registered_at: entry.last_registered_at,
                    departed_at: now,
                },
            );
        }
        // Bounded, because the ids come from whoever holds a node key and an
        // entry is only ever cleared by that node coming back or by somebody
        // saying the machine is gone. Far above any real fleet, so trimming is
        // worth saying out loud when it happens.
        while departed.len() > MAX_DEPARTED_NODES {
            let Some(oldest) = departed
                .values()
                .min_by_key(|node| node.departed_at)
                .map(|node| node.id.clone())
            else {
                break;
            };
            departed.remove(&oldest);
            warn!(
                node = %oldest,
                kept = MAX_DEPARTED_NODES,
                "More than {MAX_DEPARTED_NODES} nodes have been dropped from the fleet without coming back, so the oldest is being forgotten; if that machine is still running it will no longer be reported as unreached by a deployment or a deletion. Clear the ones that are really gone with DELETE /api/nodes/{{id}}/registration"
            );
        }
        drop(departed);

        for entry in &stale {
            self.health.remove(&entry.config.id);
        }

        // A refused machine keeps registering every
        // `registration_refresh_interval`, so a record that has gone a whole
        // registration lifetime without another refusal is a contest that
        // ended: one of the two machines was given a different id, or was shut
        // down. Keeping it would name a machine that is no longer there in
        // every deployment report, which is the same untruth in the other
        // direction.
        let mut contested = self.contested_write();
        let ended: Vec<(String, String)> = contested
            .values()
            .filter(|record| now - record.last_refused_at > ttl)
            .map(|record| (record.id.clone(), record.claimant_addr.clone()))
            .collect();
        for key in &ended {
            contested.remove(key);
            info!(
                node = %key.0,
                claimant_addr = %key.1,
                ttl_seconds = self.registration_ttl.as_secs(),
                "A machine has stopped claiming a node id another machine holds; after {} seconds without another attempt the contest is treated as over and deployments stop reporting it as unreached",
                self.registration_ttl.as_secs()
            );
        }
        drop(contested);

        stale.into_iter().map(|entry| entry.config.id).collect()
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

    /// Wrap a channel so every call over it carries this control plane's node
    /// key. The only way a client is built in this module, so no RPC can be
    /// added that goes out anonymous.
    fn authorized(&self, channel: Channel) -> NodeChannel {
        InterceptedService::new(channel, self.credential.clone())
    }

    async fn connect(&self, id: &str) -> Result<RuntimeManagementClient<NodeChannel>, NodeError> {
        let channel = self.channel(id, RPC_TIMEOUT).await?;
        Ok(with_message_limits(RuntimeManagementClient::new(
            self.authorized(channel),
        )))
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
        let mut client =
            with_message_limits(RuntimeManagementClient::new(self.authorized(channel)));

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
        // The trigger service is guarded on the node exactly like the
        // management one: both of its RPCs run an application's handler with
        // that environment's secrets, so the same credential rides the same
        // wrapper. Fixing one service and not the other would have left the
        // cron door open.
        let mut client = TriggerClient::new(self.authorized(channel))
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
    /// The call went out over an established connection and came back as a
    /// status instead of an answer.
    ///
    /// The code is kept as a [`tonic::Code`] rather than folded into the
    /// message: [`Self::left_the_outcome_unknown`] has to branch on it, and a
    /// caller that had to parse it back out of a string would get it wrong the
    /// first time the formatting changed.
    #[error("rpc error: {code}: {message}")]
    Rpc { code: tonic::Code, message: String },
}

impl NodeError {
    fn from_status(status: tonic::Status) -> Self {
        NodeError::Rpc {
            code: status.code(),
            message: status.message().to_string(),
        }
    }

    /// Whether this failure leaves it unknown what the node did with the call.
    ///
    /// The distinction only matters for a call that is **not** safe to
    /// repeat: a cron occurrence, which charges a card or sends an email once.
    /// For those, "the call failed" is two different events:
    ///
    /// * the request never reached the node, so nothing happened and another
    ///   node may be asked; or
    /// * the request reached the node and this process stopped listening
    ///   before the node answered, so the handler may be running right now,
    ///   may have finished, and asking a second node would run it again.
    ///
    /// `RuntimeNodeManager::channel` connects before any RPC is sent, so
    /// [`NodeError::Transport`] is always the first case: the connection was
    /// never established, and nothing was sent over it.
    /// [`NodeError::UnknownNode`] never left this process at all.
    ///
    /// For a status the list runs the other way round - the codes named below
    /// are the ones a node produces *before* it runs anything, and everything
    /// else counts as unknown. That direction is deliberate: the cost of
    /// reading "unknown" as "did not run" is a duplicated side effect, and the
    /// cost of reading "did not run" as "unknown" is one occurrence skipped
    /// and said out loud. A status that is not in the list, including one
    /// invented by a future node, therefore stops the fleet walk.
    ///
    /// `Cancelled` is left out of the list on purpose: tonic maps the
    /// client-side `Endpoint::timeout` elapsing to `Cancelled`, not to
    /// `DeadlineExceeded`, so a trigger that outlived `TRIGGER_TIMEOUT`
    /// arrives here as `Cancelled` and is exactly the case this method exists
    /// for.
    pub fn left_the_outcome_unknown(&self) -> bool {
        use tonic::Code;
        match self {
            NodeError::UnknownNode | NodeError::Transport(_) => false,
            NodeError::Rpc { code, .. } => !matches!(
                code,
                Code::Unauthenticated
                    | Code::PermissionDenied
                    | Code::Unimplemented
                    | Code::InvalidArgument
                    | Code::NotFound
                    | Code::AlreadyExists
                    | Code::FailedPrecondition
                    | Code::OutOfRange
            ),
        }
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

    /// A live node that answers `GetStatus` at the address on file and calls
    /// itself `node_id`.
    fn answering_as(node_id: &str, hostname: &str) -> Option<NodeIdentity> {
        Some(NodeIdentity {
            node_id: node_id.to_string(),
            hostname: hostname.to_string(),
            uptime_seconds: 4_212,
        })
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
    fn a_fleet_presents_the_node_key_on_every_call_it_makes() {
        // The other half of the fix. The node now refuses a caller that
        // proves nothing, so a control plane that attaches nothing would
        // deliver no deployment and fire no cron at all.
        let fleet = manager(&["a"]).with_node_credential(Some("node-psk"));
        assert!(fleet.credential.is_present());

        // Blank is not a credential: an empty string in the file is an
        // operator who meant to fill it in, and sending it would be a call
        // that looks authenticated and is refused.
        for blank in [None, Some(""), Some("   ")] {
            assert!(
                !manager(&["a"])
                    .with_node_credential(blank)
                    .credential
                    .is_present(),
                "{blank:?} must not become a credential"
            );
        }
    }

    #[test]
    fn the_node_key_never_reaches_a_debug_line() {
        // This value drives every node in the fleet; an error context that
        // formatted it would put it in the log.
        let credential = NodeCallCredential::new(Some("node-psk"));
        let rendered = format!("{credential:?}");
        assert!(!rendered.contains("node-psk"), "{rendered}");
        assert!(rendered.contains("present"), "{rendered}");
    }

    #[test]
    fn the_credential_goes_out_as_the_header_the_node_reads() {
        // Both sides spell this through `deis_wire::conventions`; the point of
        // the test is that the interceptor actually stamps it, because an RPC
        // that quietly went out anonymous would be refused by every node and
        // look like a fleet-wide outage.
        use tonic::service::Interceptor;
        let mut credential = NodeCallCredential::new(Some("node-psk"));
        let request = Interceptor::call(&mut credential, tonic::Request::new(())).unwrap();
        assert_eq!(
            request
                .metadata()
                .get(NODE_AUTH_METADATA)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer node-psk")
        );

        let mut none = NodeCallCredential::new(None);
        let request = Interceptor::call(&mut none, tonic::Request::new(())).unwrap();
        assert!(request.metadata().get(NODE_AUTH_METADATA).is_none());
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

        assert_eq!(manager.deregister("joined"), Some(Deregistered::FromFleet));
        assert_eq!(manager.ids(), vec!["from-file".to_string()]);
        assert_eq!(
            manager.deregister("from-file"),
            Some(Deregistered::Configured),
            "the answer says why it stayed"
        );
        assert_eq!(manager.ids(), vec!["from-file".to_string()]);
        assert_eq!(manager.deregister("never-seen"), None);
    }

    /// Two machines, one node id, and a fleet list with room for one entry.
    ///
    /// A node id is the whole of a node's identity here: the fleet list, the
    /// health cache, the delivery fan-out and the cron rotation are all keyed
    /// by it. A second machine registering an id the first is still answering
    /// for used to simply move the address, after which every push went to one
    /// of the two, was counted against one node, and came back reported as
    /// delivered to the whole fleet - while the other machine kept serving the
    /// previous version with the previous secrets and was never told anything
    /// again.
    #[tokio::test]
    async fn a_second_machine_cannot_take_a_node_id_the_first_still_answers_for() {
        let manager = manager(&[]);
        let now = Utc::now();

        // The first machine. Nothing holds the id, so nothing is probed.
        let probed = std::sync::Mutex::new(Vec::<String>::new());
        manager
            .register_claim_with("node-a", "http://10.0.0.7:8086", None, now, |addr| {
                probed.lock().unwrap().push(addr);
                async { None }
            })
            .await
            .unwrap();
        assert!(
            probed.lock().unwrap().is_empty(),
            "a new id takes nothing from anybody"
        );

        // The second machine, while the first is up and answering for the id.
        let error = manager
            .register_claim_with("node-a", "http://10.0.0.9:8086", None, now, |addr| {
                probed.lock().unwrap().push(addr);
                async { answering_as("node-a", "box-one") }
            })
            .await
            .unwrap_err();

        assert_eq!(
            probed.into_inner().unwrap(),
            ["http://10.0.0.7:8086"],
            "the address on file is what gets asked who is there"
        );
        assert!(
            matches!(
                &error,
                RegistrationError::IdHeldByAnotherMachine { holder_addr, holder_hostname }
                    if holder_addr == "http://10.0.0.7:8086" && holder_hostname == "box-one"
            ),
            "{error}"
        );
        assert_eq!(
            manager.node("node-a").unwrap().addr,
            "http://10.0.0.7:8086",
            "the machine that was answering keeps the id; nothing silently moves"
        );
        assert_eq!(manager.len(), 1);

        // And the refused machine is on the record, so a fan-out can say it
        // was not reached.
        let contested = manager.contested();
        assert_eq!(contested.len(), 1);
        assert_eq!(contested[0].id, "node-a");
        assert_eq!(contested[0].claimant_addr, "http://10.0.0.9:8086");
        assert_eq!(contested[0].holder_addr, "http://10.0.0.7:8086");
        assert!(contested[0].describe().contains("http://10.0.0.9:8086"));
    }

    /// The other side of the same check, and the one that must not regress: a
    /// machine that really moved has to be able to take its new address, or a
    /// node whose IP changed would never be a delivery target again.
    #[tokio::test]
    async fn a_node_that_really_moved_takes_its_new_address() {
        let now = Utc::now();

        // Nothing answers at the old address any more.
        let manager = manager(&[]);
        manager
            .register_claim_with("node-a", "http://10.0.0.7:8086", None, now, |_| async {
                None
            })
            .await
            .unwrap();
        let outcome = manager
            .register_claim_with("node-a", "http://10.0.0.9:8086", None, now, |_| async {
                None
            })
            .await
            .unwrap();
        assert_eq!(outcome, Registered::AddressChanged);
        assert_eq!(manager.node("node-a").unwrap().addr, "http://10.0.0.9:8086");
        assert!(manager.contested().is_empty());

        // Something does answer at the old address, but it is a different
        // node: the address was reused after this one moved away.
        let manager = RuntimeNodeManager::new(Vec::new(), 15);
        manager
            .register_claim_with("node-a", "http://10.0.0.7:8086", None, now, |_| async {
                None
            })
            .await
            .unwrap();
        let outcome = manager
            .register_claim_with("node-a", "http://10.0.0.9:8086", None, now, |_| async {
                answering_as("node-b", "box-two")
            })
            .await
            .unwrap();
        assert_eq!(outcome, Registered::AddressChanged);
        assert_eq!(manager.node("node-a").unwrap().addr, "http://10.0.0.9:8086");
        assert!(manager.contested().is_empty());
    }

    /// A refresh at the address already on file is the same machine saying it
    /// is still there, so it must never cost a probe or be refused.
    #[tokio::test]
    async fn refreshing_at_the_same_address_is_never_contested() {
        let manager = manager(&[]);
        let now = Utc::now();
        manager
            .register_claim_with("node-a", "http://10.0.0.7:8086", None, now, |_| async {
                None
            })
            .await
            .unwrap();

        let probed = std::sync::Mutex::new(0usize);
        let outcome = manager
            .register_claim_with("node-a", "http://10.0.0.7:8086", None, now, |_| {
                *probed.lock().unwrap() += 1;
                async { answering_as("node-a", "box-one") }
            })
            .await
            .unwrap();
        assert_eq!(outcome, Registered::Refreshed);
        assert_eq!(probed.into_inner().unwrap(), 0);
        assert!(manager.contested().is_empty());
    }

    /// The record has to end when the contest does, or a machine that was
    /// corrected months ago would be named as unreached by every deploy for
    /// ever - the same untruth in the other direction.
    #[tokio::test]
    async fn a_claim_that_stops_coming_stops_being_reported() {
        let manager = RuntimeNodeManager::new_with_ttl(Vec::new(), 15, 90);
        let start = Utc::now();
        manager
            .register_claim_with("node-a", "http://10.0.0.7:8086", None, start, |_| async {
                None
            })
            .await
            .unwrap();
        manager
            .register_claim_with("node-a", "http://10.0.0.9:8086", None, start, |_| async {
                answering_as("node-a", "box-one")
            })
            .await
            .unwrap_err();
        assert_eq!(manager.contested().len(), 1);

        // Still inside the registration lifetime: the claimant may simply not
        // have come back round yet.
        manager.expire_stale_registrations(start + chrono::Duration::seconds(89));
        assert_eq!(manager.contested().len(), 1);

        manager.expire_stale_registrations(start + chrono::Duration::seconds(91));
        assert!(manager.contested().is_empty());
    }

    /// `DELETE /api/nodes/{id}/registration` is the one lever an operator has
    /// for "that machine is really gone", and it has to clear both records
    /// this control plane keeps about machines it cannot reach.
    #[tokio::test]
    async fn clearing_a_registration_also_clears_a_refused_claim() {
        let manager = manager(&[]);
        let now = Utc::now();
        manager
            .register_claim_with("node-a", "http://10.0.0.7:8086", None, now, |_| async {
                None
            })
            .await
            .unwrap();
        manager
            .register_claim_with("node-a", "http://10.0.0.9:8086", None, now, |_| async {
                answering_as("node-a", "box-one")
            })
            .await
            .unwrap_err();

        assert_eq!(manager.deregister("node-a"), Some(Deregistered::FromFleet));
        assert!(
            manager.contested().is_empty(),
            "the holder was taken off the fleet, so the refusal it caused is over too"
        );
    }
}
