//! Nodes announcing themselves.
//!
//! Discovery used to run backwards: the control plane learned which nodes
//! existed only from its own configuration file, so adding a machine meant
//! editing that file and restarting, while the machine itself already knew
//! everything needed - who it is and where its management port is. The pull
//! half of the design has worked for a while (a node asks for its assignments
//! and fetches its own bundles, architecture principle 8); this is the other
//! half, and it uses the credential that channel already uses.
//!
//! What registering buys the node: it becomes a delivery target, so a deploy
//! reaches it as a push instead of only on its next reconcile, and it becomes
//! eligible to run cron occurrences. What it costs: the node has to keep
//! saying it is there. A registration expires after
//! `node_registration_ttl_secs` without a refresh, and the answer to a
//! registration says how often to come back.
//!
//! Registration is *not* how a node gets work. Nothing here writes to the
//! catalog, and a node that never registers still serves every application it
//! pulls; it is only invisible to pushes and to cron.


use serde::{Deserialize, Serialize};

use crate::runtime_client::{
    NodeSource, Registered, RegistrationError,
};

/// What a node sends about itself.
#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    /// gRPC address of this node's management plane, as the control plane has
    /// to dial it: `http://host:port`.
    ///
    /// The node reports it rather than the control plane inferring it from
    /// the connection, because the port that answers management calls is not
    /// the port the registration arrived on, and a node behind a forwarder
    /// knows the address that reaches it while the socket does not.
    pub address: String,
    /// Version of the node process. Only used for display until the first
    /// health probe confirms it.
    #[serde(default)]
    pub version: Option<String>,
    /// Where this node serves its management plane over mutual TLS, if it
    /// serves one: `https://host:port`.
    ///
    /// A different socket from `address`, which is the plaintext plane the
    /// fleet key guards. The node reports both because only the node knows
    /// whether it bound the second, and the control plane dials the
    /// authenticated one when it has an identity of its own to prove and this
    /// node registered by certificate.
    #[serde(default)]
    pub machine_address: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub node_id: String,
    pub address: String,
    /// `new`, `address_changed` or `refreshed`.
    pub outcome: &'static str,
    /// `configured` or `registered`: whether this id is also in the control
    /// plane's file, which is what decides that it never expires.
    pub source: NodeSource,
    /// Seconds after which the node should register again.
    ///
    /// The same timer can carry the assignment reconcile - see
    /// `assignment_refresh_seconds`, which is deliberately the same number -
    /// so a node needs one loop and not two.
    pub refresh_after_seconds: u64,
    /// Seconds without a refresh after which this registration is dropped.
    pub registration_ttl_seconds: u64,
    /// How often the control plane expects this node to reconcile against
    /// `GET /api/nodes/{id}/assignments`.
    pub assignment_refresh_seconds: u64,
}

/// The answer a node gets when its registration was accepted.
///
/// Built here and not at each caller because the two channels that register a
/// node - this HTTP route and the machine channel's `Register` - must tell a
/// node the same thing about how often to come back. Two copies would drift,
/// and the symptom would be a fleet where half the nodes refresh on a cadence
/// nobody chose.
pub fn registration_answer(
    nodes: &crate::runtime_client::RuntimeNodeManager,
    node_id: &str,
    address: String,
    outcome: Registered,
) -> RegisterResponse {
    let refresh = nodes.registration_refresh_interval().as_secs();
    RegisterResponse {
        node_id: node_id.to_string(),
        address,
        outcome: match outcome {
            Registered::New => "new",
            Registered::AddressChanged => "address_changed",
            Registered::Refreshed => "refreshed",
        },
        source: nodes
            .list()
            .into_iter()
            .find(|view| view.id == node_id)
            .map(|view| view.source)
            .unwrap_or(crate::runtime_client::NodeSource::Registered),
        refresh_after_seconds: refresh,
        registration_ttl_seconds: nodes.registration_ttl().as_secs(),
        assignment_refresh_seconds: refresh,
    }
}

/// How a registration refusal reads on the machine channel.
///
/// The HTTP route answers these with a status code; an RPC has its own codes,
/// and mapping them by hand keeps the distinction the HTTP side already makes:
/// a malformed request is the caller's mistake, an id already held by another
/// machine is a conflict, and neither is an internal failure.
pub fn registration_refusal(error: RegistrationError) -> tonic::Status {
    match error {
        RegistrationError::EmptyId
        | RegistrationError::InvalidId
        | RegistrationError::InvalidAddress { .. } => {
            tonic::Status::invalid_argument(error.to_string())
        }
        RegistrationError::IdHeldByAnotherMachine { .. } => {
            tonic::Status::already_exists(error.to_string())
        }
    }
}
