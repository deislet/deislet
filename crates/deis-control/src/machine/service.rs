//! What a machine may ask this control plane for.
//!
//! # The check that did not exist before
//!
//! Under a fleet-wide pre-shared key, every machine presented the same
//! credential, so "which node is asking" could only ever be the node's own
//! word for itself - a path parameter. A caller holding the key could ask for
//! any node's assignments, and the answer carries that environment's decrypted
//! secrets.
//!
//! Here the caller has proved exactly one name, and the name says which
//! machine it is. So every RPC below starts by holding the id in the request
//! against the id in the name, and refuses when they differ. That refusal is
//! not expressible at all under a shared secret, which is the point of the
//! change.
//!
//! The id is read out of the name rather than looked up in a table on the
//! side. A table is a second place for the two to disagree, and the operator
//! who chooses a node's id is the same person who has its certificate issued -
//! so the id is in the certificate already, and reading it there is free.

use crate::node_key::NodeProof;
use deis_identity::{SpiffeId, names};
use deis_wire::machine::machine_channel_server::MachineChannel;
use deis_wire::machine::{
    DeregisterNodeRequest, DeregisterNodeResponse, FetchAssignmentsRequest,
    FetchAssignmentsResponse, FetchRoutingTableRequest, FetchRoutingTableResponse,
    RegisterNodeRequest, RegisterNodeResponse,
};
use tonic::{Request, Response, Status};
use tracing::{debug, warn};

use crate::api::catalog::CatalogState;
use crate::api::catalog::{assignments, routing_table};
use crate::machine::PeerIdentity;

/// Serves the machine channel's RPCs.
#[derive(Clone)]
pub struct MachineChannelService {
    state: CatalogState,
}

impl MachineChannelService {
    pub fn new(state: CatalogState) -> Self {
        Self { state }
    }

    /// The name this caller proved, if the connection established one.
    ///
    /// `None` means the request did not arrive over a connection this
    /// process's own acceptor established, which cannot happen on the machine
    /// listener and must never be read as "anonymous but allowed" - hence
    /// [`no_proved_identity`], which is the only thing a caller may do with
    /// the `None`.
    fn caller<T>(request: &Request<T>) -> Option<&SpiffeId> {
        request
            .extensions()
            .get::<PeerIdentity>()
            .map(|PeerIdentity(identity)| identity)
    }
}

/// The refusal for a call that proved nothing.
fn no_proved_identity() -> Status {
    warn!(
        "A machine RPC arrived with no proved identity on it; the connection it came in on was not established by this control plane's machine acceptor"
    );
    Status::unauthenticated(
        "this call carries no proved machine identity, so nothing can be answered for it",
    )
}

/// Refuse a caller that asked for a machine it is not.
///
/// The message names both halves on purpose. An operator who reissued a
/// certificate with the wrong path, or pointed a node at another node's key
/// files, sees which two names disagreed rather than a bare "denied".
fn refuse_mismatch(kind: &str, proved: &SpiffeId, claimed: &str) -> Status {
    warn!(
        proved = %proved,
        claimed = %claimed,
        kind,
        "Refused a machine call that asked for another machine's answer"
    );
    Status::permission_denied(format!(
        "this connection proved {proved}, which does not name the {kind} {claimed} the call asks \
         for; a machine may only ask for its own"
    ))
}

#[tonic::async_trait]
impl MachineChannel for MachineChannelService {
    async fn register(
        &self,
        request: Request<RegisterNodeRequest>,
    ) -> Result<Response<RegisterNodeResponse>, Status> {
        let proved = Self::caller(&request)
            .ok_or_else(no_proved_identity)?
            .clone();
        let asked = request.into_inner();
        let node_id = names::node_id_of(&proved)
            .filter(|node_id| *node_id == asked.node_id)
            .ok_or_else(|| refuse_mismatch("node", &proved, &asked.node_id))?
            .to_string();

        let announced: crate::api::registration::RegisterRequest =
            serde_json::from_str(&asked.registration_json).map_err(|error| {
                Status::invalid_argument(format!(
                    "this registration is not a registration: {error}"
                ))
            })?;

        // The name is what is recorded, not a key. A node that registered this
        // way cannot be evicted by taking a value out of a list of secrets,
        // and the fleet listing shows who it is rather than which of two
        // rotating values it happened to hold.
        let outcome = self
            .state
            .deliverer
            .nodes()
            .register_claim(
                &node_id,
                crate::runtime_client::NodeClaim {
                    addr: announced.address.clone(),
                    machine_addr: announced.machine_address.clone(),
                    version: announced.version.clone(),
                    proof: NodeProof::Certificate(proved.clone()),
                },
                chrono::Utc::now(),
            )
            .await
            .map_err(crate::api::registration::registration_refusal)?;

        let answer = crate::api::registration::registration_answer(
            self.state.deliverer.nodes(),
            &node_id,
            announced.address,
            outcome,
        );
        let registration_json = serde_json::to_string(&answer).map_err(|error| {
            Status::internal(format!(
                "the registration answer could not be encoded: {error}"
            ))
        })?;

        debug!(node = %proved, outcome = ?outcome, "A node registered over the machine channel");
        Ok(Response::new(RegisterNodeResponse { registration_json }))
    }

    async fn deregister(
        &self,
        request: Request<DeregisterNodeRequest>,
    ) -> Result<Response<DeregisterNodeResponse>, Status> {
        let proved = Self::caller(&request)
            .ok_or_else(no_proved_identity)?
            .clone();
        let asked = request.into_inner();
        let node_id = names::node_id_of(&proved)
            .filter(|node_id| *node_id == asked.node_id)
            .ok_or_else(|| refuse_mismatch("node", &proved, &asked.node_id))?
            .to_string();

        let removed = self.state.deliverer.nodes().deregister(&node_id);
        debug!(node = %proved, removed = ?removed, "A node deregistered over the machine channel");
        Ok(Response::new(DeregisterNodeResponse {
            removed: matches!(
                removed,
                Some(crate::runtime_client::Deregistered::FromFleet)
            ),
        }))
    }

    async fn fetch_assignments(
        &self,
        request: Request<FetchAssignmentsRequest>,
    ) -> Result<Response<FetchAssignmentsResponse>, Status> {
        let proved = Self::caller(&request)
            .ok_or_else(no_proved_identity)?
            .clone();
        let claimed = &request.get_ref().node_id;

        // This answer carries every listed environment's decrypted secrets.
        // It is the reason the machine channel exists, and the reason the
        // check below is the first thing that happens.
        match names::node_id_of(&proved) {
            Some(node_id) if node_id == claimed => {}
            _ => return Err(refuse_mismatch("node", &proved, claimed)),
        }

        let base = self.state.deliverer.machine_reconciliation_base_url();
        let answer = assignments::assignments_for(&self.state, claimed, &base)
            .await
            .map_err(|error| {
                Status::internal(format!("the assignment list could not be built: {error:?}"))
            })?;
        let assignments_json = serde_json::to_string(&answer.views).map_err(|error| {
            Status::internal(format!("the assignment list could not be encoded: {error}"))
        })?;

        debug!(
            node = %proved,
            assignments = answer.views.len(),
            complete = answer.complete,
            "Served an assignment list over the machine channel"
        );
        Ok(Response::new(FetchAssignmentsResponse {
            assignments_json,
            complete: answer.complete,
            // Saturating rather than wrapping: a count that does not fit is
            // still "a lot withheld", and reporting it as a small number
            // would read as "nearly complete", which is the one wrong answer.
            withheld: answer.withheld.try_into().unwrap_or(u32::MAX),
        }))
    }

    async fn fetch_routing_table(
        &self,
        request: Request<FetchRoutingTableRequest>,
    ) -> Result<Response<FetchRoutingTableResponse>, Status> {
        let proved = Self::caller(&request)
            .ok_or_else(no_proved_identity)?
            .clone();
        let asked = request.into_inner();

        // A proxy may say it has taken up a table it is already running, and
        // the control plane records that as the acknowledgement half of an
        // ordered switch. Left empty, it is only asking for the table.
        let observing = if asked.proxy_id.is_empty() {
            None
        } else {
            match names::proxy_id_of(&proved) {
                Some(proxy_id) if proxy_id == asked.proxy_id => Some(asked.proxy_id.as_str()),
                _ => return Err(refuse_mismatch("proxy", &proved, &asked.proxy_id)),
            }
        };

        let table = routing_table::routing_table_for(
            &self.state,
            observing,
            Some(asked.observed_routing_table_version.as_str()),
        )
        .await
        .map_err(|error| {
            Status::internal(format!("the routing table could not be built: {error:?}"))
        })?;
        let routing_table_json = serde_json::to_string(&table).map_err(|error| {
            Status::internal(format!("the routing table could not be encoded: {error}"))
        })?;

        debug!(caller = %proved, "Served the routing table over the machine channel");
        Ok(Response::new(FetchRoutingTableResponse {
            routing_table_json,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::catalog::tests::harness;
    use deis_identity::names;

    const TRUST_DOMAIN: &str = "deis.internal";

    /// A request as it arrives on a connection whose peer proved `identity`.
    fn from<T>(identity: &SpiffeId, message: T) -> Request<T> {
        let mut request = Request::new(message);
        request
            .extensions_mut()
            .insert(PeerIdentity(identity.clone()));
        request
    }

    fn node(id: &str) -> SpiffeId {
        names::node(TRUST_DOMAIN, id).expect("a node name")
    }

    fn proxy(id: &str) -> SpiffeId {
        names::proxy(TRUST_DOMAIN, id).expect("a proxy name")
    }

    /// Registration, over the channel where the caller has proved a name. The
    /// answer records the name, so the fleet listing afterwards shows who the
    /// node is rather than which of a list of shared values it held.
    #[tokio::test]
    async fn a_node_registers_itself_and_is_recorded_by_name() {
        let harness = harness(true).await;
        let service = MachineChannelService::new(harness.state.clone());

        let answer = service
            .register(from(
                &node("runtime-test"),
                RegisterNodeRequest {
                    node_id: "runtime-test".to_string(),
                    registration_json: r#"{"address":"http://10.0.0.7:8086"}"#.to_string(),
                },
            ))
            .await
            .expect("a node may register itself");
        let answered: serde_json::Value =
            serde_json::from_str(&answer.get_ref().registration_json).expect("a JSON object");
        assert_eq!(answered["node_id"], "runtime-test");

        let listed = harness
            .state
            .deliverer
            .nodes()
            .list()
            .into_iter()
            .find(|view| view.id == "runtime-test")
            .expect("the node is in the fleet");
        assert_eq!(
            listed.identity.as_deref(),
            Some("spiffe://deis.internal/deislet/node/runtime-test"),
            "the fleet records the name that was proved"
        );
        assert_eq!(listed.key_fingerprint, None);
    }

    /// The refusal that a shared key could not express, on the call that
    /// makes a machine a delivery target: injecting a node is how a caller
    /// gets deployments and decrypted secrets pushed to it.
    #[tokio::test]
    async fn a_node_cannot_register_a_machine_it_is_not() {
        let harness = harness(true).await;
        let service = MachineChannelService::new(harness.state.clone());

        let refusal = service
            .register(from(
                &node("runtime-test"),
                RegisterNodeRequest {
                    node_id: "somebody-else".to_string(),
                    registration_json: r#"{"address":"http://10.0.0.9:8086"}"#.to_string(),
                },
            ))
            .await
            .expect_err("one machine registered another");
        assert_eq!(refusal.code(), tonic::Code::PermissionDenied);

        assert!(
            harness
                .state
                .deliverer
                .nodes()
                .list()
                .into_iter()
                .all(|view| view.id != "somebody-else"),
            "the refused registration still put a node in the fleet"
        );
    }

    #[tokio::test]
    async fn a_registration_that_is_not_one_is_a_bad_request_rather_than_a_failure() {
        let harness = harness(true).await;
        let service = MachineChannelService::new(harness.state.clone());

        let refusal = service
            .register(from(
                &node("runtime-test"),
                RegisterNodeRequest {
                    node_id: "runtime-test".to_string(),
                    registration_json: "not json at all".to_string(),
                },
            ))
            .await
            .expect_err("a malformed registration was accepted");
        assert_eq!(refusal.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn a_node_gets_its_own_assignments() {
        let harness = harness(true).await;
        let service = MachineChannelService::new(harness.state.clone());

        let answer = service
            .fetch_assignments(from(
                &node("runtime-test"),
                FetchAssignmentsRequest {
                    node_id: "runtime-test".to_string(),
                },
            ))
            .await
            .expect("a node may ask for its own assignments");

        let listed: serde_json::Value =
            serde_json::from_str(&answer.get_ref().assignments_json).expect("a JSON array");
        assert!(listed.is_array(), "{listed}");
        assert!(answer.get_ref().complete);
    }

    /// The check the whole change exists for. Under a fleet-wide key both
    /// machines presented the same credential, so this refusal could not be
    /// expressed at all: whoever held the key could ask for any node's
    /// assignments, and the answer carries that environment's decrypted
    /// secrets.
    #[tokio::test]
    async fn a_node_cannot_ask_for_another_nodes_assignments() {
        let harness = harness(true).await;
        let service = MachineChannelService::new(harness.state.clone());

        let refusal = service
            .fetch_assignments(from(
                &node("runtime-test"),
                FetchAssignmentsRequest {
                    node_id: "runtime-other".to_string(),
                },
            ))
            .await
            .expect_err("one node was handed another node's secrets");

        assert_eq!(refusal.code(), tonic::Code::PermissionDenied);
        assert!(
            refusal.message().contains("runtime-test")
                && refusal.message().contains("runtime-other"),
            "the refusal names neither who asked nor what they asked for: {}",
            refusal.message()
        );
    }

    /// A machine of the right fleet, holding a real certificate, is still only
    /// the machine its own name says it is - and a proxy is not a node.
    #[tokio::test]
    async fn a_proxy_cannot_ask_for_a_nodes_assignments() {
        let harness = harness(true).await;
        let service = MachineChannelService::new(harness.state.clone());

        let refusal = service
            .fetch_assignments(from(
                &proxy("edge-1"),
                FetchAssignmentsRequest {
                    node_id: "runtime-test".to_string(),
                },
            ))
            .await
            .expect_err("a proxy was handed a node's secrets");

        assert_eq!(refusal.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn a_call_with_no_proved_identity_is_refused_rather_than_treated_as_anonymous() {
        let harness = harness(true).await;
        let service = MachineChannelService::new(harness.state.clone());

        let refusal = service
            .fetch_assignments(Request::new(FetchAssignmentsRequest {
                node_id: "runtime-test".to_string(),
            }))
            .await
            .expect_err("a call carrying no identity was answered");

        assert_eq!(refusal.code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn any_machine_of_the_fleet_may_read_the_routing_table() {
        let harness = harness(true).await;
        let service = MachineChannelService::new(harness.state.clone());

        // Asking for the table claims nothing about who is asking, so no id
        // is checked - the connection already proved this is one of ours.
        let answer = service
            .fetch_routing_table(from(&proxy("edge-1"), FetchRoutingTableRequest::default()))
            .await
            .expect("a proxy may read the routing table");
        let table: serde_json::Value =
            serde_json::from_str(&answer.get_ref().routing_table_json).expect("a JSON object");
        assert!(table.get("version").is_some(), "{table}");
    }

    /// Saying "I am already running version X" is a claim about a particular
    /// proxy, and the control plane records it against that proxy. So that
    /// half is checked even though reading the table is not.
    #[tokio::test]
    async fn a_proxy_cannot_acknowledge_a_table_on_another_proxys_behalf() {
        let harness = harness(true).await;
        let service = MachineChannelService::new(harness.state.clone());

        let refusal = service
            .fetch_routing_table(from(
                &proxy("edge-1"),
                FetchRoutingTableRequest {
                    proxy_id: "edge-2".to_string(),
                    observed_routing_table_version: "whatever".to_string(),
                },
            ))
            .await
            .expect_err("one proxy acknowledged a table as another");

        assert_eq!(refusal.code(), tonic::Code::PermissionDenied);
        assert!(
            refusal.message().contains("edge-1") && refusal.message().contains("edge-2"),
            "{}",
            refusal.message()
        );
    }
}
