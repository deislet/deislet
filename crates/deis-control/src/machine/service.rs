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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use deis_identity::{SpiffeId, names};
use deis_wire::machine::machine_channel_server::MachineChannel;
use deis_wire::machine::{
    DeregisterNodeRequest, DeregisterNodeResponse, FetchAssignmentsRequest,
    FetchAssignmentsResponse, FetchRoutingTableRequest, FetchRoutingTableResponse,
    FetchSecretValuesRequest, FetchSecretValuesResponse, IssueCapabilityRequest,
    IssueCapabilityResponse, RegisterNodeRequest, RegisterNodeResponse,
};
use tonic::{Request, Response, Status};
use tracing::{debug, error, warn};

use crate::api::catalog::CatalogState;
use crate::api::catalog::{assignments, routing_table};
use crate::audit::{AuditRecord, AuditStore, Outcome as AuditOutcome};
use crate::capability_issuer::CapabilityIssuer;
use crate::machine::{PeerIdentity, issuance};

/// Serves the machine channel's RPCs.
#[derive(Clone)]
pub struct MachineChannelService {
    state: CatalogState,
    issuer: CapabilityIssuer,
    /// The same append-only table the human API writes, over the catalog's
    /// database, so a secret fetch is found by the same audit query.
    audit: Arc<AuditStore>,
}

impl MachineChannelService {
    pub fn new(state: CatalogState, issuer: CapabilityIssuer) -> Self {
        let audit = Arc::new(AuditStore::new(state.store.database()));
        Self {
            state,
            issuer,
            audit,
        }
    }

    /// Write down a secret fetch that was not answered, and say so in the
    /// service log. Returns the status so a call site can `return Err` it.
    async fn refuse_secret_fetch(
        &self,
        proved: &SpiffeId,
        deployment_id: Option<&str>,
        status: Status,
    ) -> Status {
        warn!(
            proved = %proved,
            deployment_id = deployment_id.unwrap_or(""),
            reason = status.message(),
            "Refused a node's request for secret values"
        );
        let http_status = match status.code() {
            tonic::Code::InvalidArgument => 400,
            tonic::Code::PermissionDenied => 403,
            tonic::Code::NotFound => 404,
            _ => 500,
        };
        self.audit
            .record(secret_fetch_record(
                proved,
                deployment_id,
                AuditOutcome::from_status(http_status),
                http_status,
                status.message().to_string(),
            ))
            .await;
        status
    }

    /// Write down a capability request that was not answered, and say so in
    /// the service log. Returns the status so a call site can `return Err`
    /// it. Issued capabilities are recorded in the issuance table instead.
    async fn refuse_issuance(
        &self,
        proved: &SpiffeId,
        audience: Option<&str>,
        status: Status,
    ) -> Status {
        warn!(
            proved = %proved,
            audience = audience.unwrap_or(""),
            reason = status.message(),
            "Refused a node's request for a capability"
        );
        let http_status = grpc_http_status(status.code());
        self.audit
            .record(AuditRecord {
                actor_kind: "machine".to_string(),
                actor_account: Some(proved.to_string()),
                actor_credential: None,
                action: CAPABILITY_ISSUE_ACTION.to_string(),
                app_id: None,
                environment: None,
                target: audience.map(str::to_string),
                method: "GRPC".to_string(),
                path: ISSUE_CAPABILITY_PATH.to_string(),
                peer: None,
                outcome: AuditOutcome::from_status(http_status),
                status: http_status,
                detail: Some(status.message().to_string()),
                request_id: None,
            })
            .await;
        status
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

/// The path an audit line gives a secret fetch: the gRPC method, so the
/// line reads like the HTTP ones beside it in the same table.
const FETCH_SECRET_VALUES_PATH: &str = "/deis.machine.MachineChannel/FetchSecretValues";

/// The path and action an audit line gives a refused capability request.
const ISSUE_CAPABILITY_PATH: &str = "/deis.machine.MachineChannel/IssueCapability";
const CAPABILITY_ISSUE_ACTION: &str = "capability.issue";

/// The HTTP status an audit line records for a refusal with `code`.
fn grpc_http_status(code: tonic::Code) -> u16 {
    match code {
        tonic::Code::InvalidArgument => 400,
        tonic::Code::Unauthenticated => 401,
        tonic::Code::PermissionDenied => 403,
        tonic::Code::NotFound => 404,
        _ => 500,
    }
}

/// The action an audit line gives a secret fetch.
const SECRET_FETCH_ACTION: &str = "secret.fetch";

/// One audit line for a secret fetch, accepted or refused.
///
/// The actor is the name the connection proved, never the id the request
/// claimed; the detail names revisions, never a value. Decrypted secrets are
/// at least as sensitive as the storage-token mint, which is why this read is
/// written down at all.
fn secret_fetch_record(
    proved: &SpiffeId,
    deployment_id: Option<&str>,
    outcome: AuditOutcome,
    status: u16,
    detail: String,
) -> AuditRecord {
    AuditRecord {
        actor_kind: "machine".to_string(),
        actor_account: Some(proved.to_string()),
        actor_credential: None,
        action: SECRET_FETCH_ACTION.to_string(),
        app_id: None,
        environment: None,
        target: deployment_id.map(str::to_string),
        method: "GRPC".to_string(),
        path: FETCH_SECRET_VALUES_PATH.to_string(),
        peer: None,
        outcome,
        status,
        detail: Some(detail),
        request_id: None,
    }
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
                    identity: proved.clone(),
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

    async fn fetch_secret_values(
        &self,
        request: Request<FetchSecretValuesRequest>,
    ) -> Result<Response<FetchSecretValuesResponse>, Status> {
        let proved = Self::caller(&request)
            .ok_or_else(no_proved_identity)?
            .clone();
        let claimed = &request.get_ref().node_id;
        let node_id = match names::node_id_of(&proved) {
            Some(node_id) if node_id == claimed => node_id.to_string(),
            _ => {
                let refusal = refuse_mismatch("node", &proved, claimed);
                return Err(self.refuse_secret_fetch(&proved, None, refusal).await);
            }
        };

        let entries: Vec<deis_wire::conventions::SecretFetchEntry> =
            match serde_json::from_str(&request.get_ref().entries_json) {
                Ok(entries) => entries,
                Err(error) => {
                    let refusal = Status::invalid_argument(format!(
                        "secret fetch entries could not be parsed: {error}"
                    ));
                    return Err(self.refuse_secret_fetch(&proved, None, refusal).await);
                }
            };
        if entries.is_empty() || entries.iter().any(|entry| entry.revisions.is_empty()) {
            let refusal = Status::invalid_argument(
                "every secret fetch entry must name at least one revision; there is no \
                 \"everything for this deployment\" request",
            );
            return Err(self.refuse_secret_fetch(&proved, None, refusal).await);
        }

        // The node's own name, from the certificate, decides what it may
        // read - never anything the request says about itself.
        let visible = match assignments::deployments_visible_to(&self.state, &node_id).await {
            Ok(visible) => visible,
            Err(error) => {
                let refusal =
                    Status::internal(format!("could not list visible deployments: {error:?}"));
                return Err(self.refuse_secret_fetch(&proved, None, refusal).await);
            }
        };

        // Every entry is checked before anything is decrypted, and one bad
        // entry refuses the whole request: a partial answer would let a
        // caller map out which deployments exist by what came back.
        let mut wanted = Vec::with_capacity(entries.len());
        for entry in &entries {
            let Some(deployment) = visible.get(&entry.deployment_id) else {
                let refusal = Status::permission_denied(format!(
                    "deployment {} is not assigned to node {node_id}",
                    entry.deployment_id
                ));
                return Err(self
                    .refuse_secret_fetch(&proved, Some(&entry.deployment_id), refusal)
                    .await);
            };
            let Some(snapshot_id) = deployment.snapshot_id.as_deref() else {
                let refusal = Status::permission_denied(format!(
                    "deployment {} binds no secret revisions",
                    entry.deployment_id
                ));
                return Err(self
                    .refuse_secret_fetch(&proved, Some(&entry.deployment_id), refusal)
                    .await);
            };
            let revisions: BTreeSet<String> = entry.revisions.iter().cloned().collect();
            wanted.push((entry, deployment.env_id.as_str(), snapshot_id, revisions));
        }

        let mut values: BTreeMap<String, String> = BTreeMap::new();
        let mut accepted = Vec::with_capacity(wanted.len());
        for (entry, env_id, snapshot_id, revisions) in wanted {
            let fetched = match self
                .state
                .deliverer
                .snapshot_secret_values(env_id, snapshot_id, &revisions)
                .await
            {
                Ok(fetched) => fetched,
                Err(error) => {
                    let refusal = match error {
                        crate::catalog::CatalogError::Forbidden(message) => {
                            Status::permission_denied(message)
                        }
                        crate::catalog::CatalogError::NotFound(message) => {
                            Status::not_found(message)
                        }
                        other => {
                            Status::internal(format!("secret values could not be read: {other}"))
                        }
                    };
                    return Err(self
                        .refuse_secret_fetch(&proved, Some(&entry.deployment_id), refusal)
                        .await);
                }
            };
            // Revision ids and a count, never a value.
            let revision_list = revisions.iter().cloned().collect::<Vec<_>>().join(",");
            accepted.push(secret_fetch_record(
                &proved,
                Some(&entry.deployment_id),
                AuditOutcome::Accepted,
                200,
                format!("revisions={revision_list} count={}", fetched.len()),
            ));
            for (revision, value) in fetched {
                values.insert(format!("{}\x00{revision}", entry.deployment_id), value);
            }
        }

        let values_json = match serde_json::to_string(&values) {
            Ok(json) => json,
            Err(error) => {
                let refusal = Status::internal(format!("could not encode secret values: {error}"));
                return Err(self.refuse_secret_fetch(&proved, None, refusal).await);
            }
        };

        // Fail closed: a value leaves this process only once the row saying
        // it did is durable. A fetch the audit log cannot show is the one
        // thing this row exists to rule out, so an audit table that cannot
        // be written means no values, not values and a log line.
        for record in accepted {
            let deployment_id = record.target.clone();
            if let Err(error) = self.audit.try_record(record).await {
                error!(
                    proved = %proved,
                    deployment_id = deployment_id.as_deref().unwrap_or(""),
                    error = %error,
                    "Withheld secret values from a node because the audit row for the fetch could not be written"
                );
                return Err(Status::unavailable(
                    "the audit record for this secret fetch could not be written, so no value \
                     is handed over; retry once the control plane's database accepts writes",
                ));
            }
            debug!(
                node = %node_id,
                deployment_id = deployment_id.as_deref().unwrap_or(""),
                "A node fetched secret values over the machine channel"
            );
        }
        Ok(Response::new(FetchSecretValuesResponse { values_json }))
    }

    async fn issue_capability(
        &self,
        request: Request<IssueCapabilityRequest>,
    ) -> Result<Response<IssueCapabilityResponse>, Status> {
        let proved = Self::caller(&request)
            .ok_or_else(no_proved_identity)?
            .clone();
        let claimed = &request.get_ref().node_id;
        let node_id = match names::node_id_of(&proved) {
            Some(node_id) if node_id == claimed => node_id.to_string(),
            _ => {
                let refusal = refuse_mismatch("node", &proved, claimed);
                return Err(self.refuse_issuance(&proved, None, refusal).await);
            }
        };

        let asked = request.into_inner();
        let audience = asked.audience.as_str();

        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ResourceRequest {
            tenant: String,
            binding: String,
            #[serde(default)]
            generation: Option<String>,
        }
        let resources: Vec<deis_capability::CapabilityResource> =
            match serde_json::from_str::<Vec<ResourceRequest>>(&asked.resources_json) {
                Ok(requested) => requested
                    .into_iter()
                    .map(|r| deis_capability::CapabilityResource {
                        tenant: r.tenant,
                        binding: r.binding,
                        generation: r.generation,
                    })
                    .collect(),
                Err(error) => {
                    let refusal = Status::invalid_argument(format!(
                        "resources_json could not be parsed: {error}"
                    ));
                    return Err(self.refuse_issuance(&proved, Some(audience), refusal).await);
                }
            };

        // What this node runs, from its proved name - never from anything
        // the request says about itself.
        let visible = assignments::deployments_visible_to(&self.state, &node_id)
            .await
            .map_err(|error| {
                Status::internal(format!("could not list visible deployments: {error:?}"))
            })?;
        if let Err(reason) = issuance::authorize(&node_id, audience, &resources, &visible) {
            let refusal = if resources.is_empty() {
                Status::invalid_argument(reason)
            } else {
                Status::permission_denied(reason)
            };
            return Err(self.refuse_issuance(&proved, Some(audience), refusal).await);
        }

        let capability_token = match self
            .issuer
            .issue(audience, proved.as_str(), resources)
            .await
        {
            Ok(token) => token,
            Err(error) => {
                let refusal =
                    Status::internal(format!("the capability could not be issued: {error}"));
                return Err(self.refuse_issuance(&proved, Some(audience), refusal).await);
            }
        };

        debug!(
            node = %proved,
            audience = %audience,
            "Issued a capability; it is in the issuance table under its jti"
        );
        Ok(Response::new(IssueCapabilityResponse { capability_token }))
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

    fn make_service(state: CatalogState) -> MachineChannelService {
        // The harness's issuer, which records into the harness's database,
        // so a test can read back what was issued.
        let issuer = state.issuer.clone();
        MachineChannelService::new(state, issuer)
    }

    /// Registration, over the channel where the caller has proved a name. The
    /// answer records the name, so the fleet listing afterwards shows who the
    /// node is rather than which of a list of shared values it held.
    #[tokio::test]
    async fn a_node_registers_itself_and_is_recorded_by_name() {
        let harness = harness(true).await;
        let service = make_service(harness.state.clone());

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
    }

    /// The refusal that a shared key could not express, on the call that
    /// makes a machine a delivery target: injecting a node is how a caller
    /// gets deployments and decrypted secrets pushed to it.
    #[tokio::test]
    async fn a_node_cannot_register_a_machine_it_is_not() {
        let harness = harness(true).await;
        let service = make_service(harness.state.clone());

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
        let service = make_service(harness.state.clone());

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
        let service = make_service(harness.state.clone());

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
        let service = make_service(harness.state.clone());

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
        let service = make_service(harness.state.clone());

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
        let service = make_service(harness.state.clone());

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
        let service = make_service(harness.state.clone());

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
        let service = make_service(harness.state.clone());

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

    /// One schema-2 deployment of `demo` whose only binding is the secret
    /// `TOKEN`. Returns the deployment id and the bound revision.
    async fn deployed_with_secret(h: &crate::api::catalog::tests::Harness) -> (String, String) {
        use crate::api::catalog::tests::{call, json};
        use axum::body::Body;
        use axum::http::StatusCode;

        let declarations = deis_wire::EnvironmentDeclarations::new(
            deis_wire::CURRENT_EDGE_CANON_STANDARD_VERSION,
            vec![deis_wire::EnvironmentDeclaration {
                name: "TOKEN".to_string(),
                kind: deis_wire::EnvironmentKind::Secret,
                value_type: deis_wire::EnvironmentValueType::String,
                required: true,
            }],
        );
        json(
            &h.router,
            "POST",
            "/api/apps",
            serde_json::json!({"id": "demo"}),
        )
        .await;
        let manifest = serde_json::json!({
            "schema": 2,
            "app": "demo",
            "entry": "worker/index.js",
            "environment": declarations,
        })
        .to_string();
        let (status, uploaded) = call(
            &h.router,
            "POST",
            "/api/apps/demo/versions",
            Body::from(crate::catalog::bundle::tests::bundle_with_manifest(
                &manifest, true,
            )),
            "application/zip",
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{uploaded}");
        let version_id = uploaded["version"]["id"].as_str().unwrap().to_string();
        json(
            &h.router,
            "POST",
            "/api/apps/demo/environments",
            serde_json::json!({"name": "production"}),
        )
        .await;
        let (status, stored) = json(
            &h.router,
            "PUT",
            "/api/apps/demo/environments/production/secrets/TOKEN",
            serde_json::json!({"value": "first-value"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{stored}");
        let (status, deployed) = json(
            &h.router,
            "POST",
            "/api/apps/demo/environments/production/deployments",
            serde_json::json!({"version_id": version_id}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{deployed}");
        let deployment_id = deployed["deployment"]["id"].as_str().unwrap().to_string();
        let snapshot_id = deployed["deployment"]["snapshot_id"].as_str().unwrap();
        let snapshot = h
            .state
            .store
            .get_environment_snapshot(snapshot_id)
            .await
            .unwrap();
        let revision = snapshot
            .bindings
            .iter()
            .find_map(|binding| match binding {
                deis_wire::EnvironmentBinding::Secret(secret) => Some(secret.revision.clone()),
                deis_wire::EnvironmentBinding::Config(_) => None,
            })
            .expect("the snapshot binds TOKEN");
        (deployment_id, revision)
    }

    fn secret_request(
        node_id: &str,
        deployment_id: &str,
        revisions: &[&str],
    ) -> FetchSecretValuesRequest {
        FetchSecretValuesRequest {
            node_id: node_id.to_string(),
            entries_json: serde_json::to_string(&[deis_wire::conventions::SecretFetchEntry {
                deployment_id: deployment_id.to_string(),
                revisions: revisions.iter().map(|r| r.to_string()).collect(),
            }])
            .unwrap(),
        }
    }

    #[tokio::test]
    async fn a_node_fetches_exactly_the_revisions_it_asked_for() {
        let h = harness(true).await;
        let (deployment_id, revision) = deployed_with_secret(&h).await;
        let service = make_service(h.state.clone());

        let answer = service
            .fetch_secret_values(from(
                &node("runtime-test"),
                secret_request("runtime-test", &deployment_id, &[&revision]),
            ))
            .await
            .expect("a node may fetch its own deployment's secrets");
        let values: BTreeMap<String, String> =
            serde_json::from_str(&answer.get_ref().values_json).unwrap();
        assert_eq!(
            values,
            BTreeMap::from([(
                format!("{deployment_id}\x00{revision}"),
                "first-value".to_string()
            )])
        );
    }

    /// A revision the deployment's snapshot does not bind - another
    /// environment's, or one made up - refuses the whole request rather than
    /// being skipped.
    #[tokio::test]
    async fn a_revision_the_deployment_does_not_bind_refuses_the_whole_request() {
        let h = harness(true).await;
        let (deployment_id, revision) = deployed_with_secret(&h).await;
        let service = make_service(h.state.clone());

        let refusal = service
            .fetch_secret_values(from(
                &node("runtime-test"),
                secret_request("runtime-test", &deployment_id, &[&revision, "not-bound"]),
            ))
            .await
            .expect_err("an unbound revision was answered");
        assert_eq!(refusal.code(), tonic::Code::PermissionDenied);

        let refusal = service
            .fetch_secret_values(from(
                &node("runtime-test"),
                secret_request("runtime-test", &deployment_id, &[]),
            ))
            .await
            .expect_err("an entry naming no revision was answered with everything");
        assert_eq!(refusal.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn an_unknown_deployment_is_refused() {
        let h = harness(true).await;
        let (_, revision) = deployed_with_secret(&h).await;
        let service = make_service(h.state.clone());

        let refusal = service
            .fetch_secret_values(from(
                &node("runtime-test"),
                secret_request("runtime-test", "no-such-deployment", &[&revision]),
            ))
            .await
            .expect_err("a deployment that does not exist was answered");
        assert_eq!(refusal.code(), tonic::Code::PermissionDenied);
    }

    /// The `secret.fetch` lines in the persistent audit table, oldest first.
    async fn secret_fetch_lines(state: &CatalogState) -> Vec<crate::audit::AuditEntry> {
        let audit = AuditStore::new(state.store.database());
        let mut lines = audit
            .query_page(
                crate::audit::AuditQuery {
                    action: Some(SECRET_FETCH_ACTION.to_string()),
                    ..Default::default()
                },
                100,
                None,
            )
            .await
            .expect("the audit log reads")
            .items;
        lines.reverse();
        lines
    }

    /// Both outcomes reach the audit table an operator queries, naming the
    /// proved machine, the deployment and the revisions - never the value.
    #[tokio::test]
    async fn a_secret_fetch_is_written_to_the_audit_log_either_way() {
        let h = harness(true).await;
        let (deployment_id, revision) = deployed_with_secret(&h).await;
        let service = make_service(h.state.clone());

        service
            .fetch_secret_values(from(
                &node("runtime-test"),
                secret_request("runtime-test", &deployment_id, &[&revision]),
            ))
            .await
            .expect("a node may fetch its own deployment's secrets");
        service
            .fetch_secret_values(from(
                &node("runtime-test"),
                secret_request("runtime-test", "no-such-deployment", &[&revision]),
            ))
            .await
            .expect_err("a deployment that does not exist was answered");

        let lines = secret_fetch_lines(&h.state).await;
        assert_eq!(lines.len(), 2, "{lines:#?}");
        let proved = node("runtime-test").to_string();

        let accepted = &lines[0];
        assert_eq!(accepted.outcome, AuditOutcome::Accepted);
        assert_eq!(accepted.actor_kind, "machine");
        assert_eq!(accepted.actor_account.as_deref(), Some(proved.as_str()));
        assert_eq!(accepted.target.as_deref(), Some(deployment_id.as_str()));
        assert_eq!(accepted.path, FETCH_SECRET_VALUES_PATH);
        let detail = accepted.detail.as_deref().unwrap_or_default();
        assert!(detail.contains(&revision), "{detail}");

        let refused = &lines[1];
        assert_eq!(refused.outcome, AuditOutcome::Refused);
        assert_eq!(refused.status, 403);
        assert_eq!(refused.actor_account.as_deref(), Some(proved.as_str()));
        assert_eq!(refused.target.as_deref(), Some("no-such-deployment"));

        for line in &lines {
            let text = format!("{line:?}");
            assert!(
                !text.contains("first-value"),
                "a value reached the log: {text}"
            );
        }
    }

    /// Run `sql` against the catalog's database, for a test that has to
    /// break it.
    async fn break_database(state: &CatalogState, sql: &'static str) {
        state
            .store
            .database()
            .with_connection(move |conn| {
                conn.execute_batch(sql)?;
                Ok::<(), crate::database::DatabaseError>(())
            })
            .await
            .expect("the statement runs");
    }

    /// A value leaves only once its audit row is durable. With the audit
    /// table refusing writes, the node gets no values at all.
    #[tokio::test]
    async fn no_secret_value_is_handed_over_without_its_audit_row() {
        let h = harness(true).await;
        let (deployment_id, revision) = deployed_with_secret(&h).await;
        let service = make_service(h.state.clone());
        break_database(
            &h.state,
            "CREATE TRIGGER refuse_audit BEFORE INSERT ON audit_log \
             BEGIN SELECT RAISE(ABORT, 'audit is read-only'); END;",
        )
        .await;

        let refusal = service
            .fetch_secret_values(from(
                &node("runtime-test"),
                secret_request("runtime-test", &deployment_id, &[&revision]),
            ))
            .await
            .expect_err("values were handed over with no audit row");
        assert_eq!(refusal.code(), tonic::Code::Unavailable);
        assert!(
            !refusal.message().contains("first-value"),
            "{}",
            refusal.message()
        );
    }

    /// A fetch the control plane could not serve is still a fetch someone
    /// asked for, and is written down like a refusal.
    #[tokio::test]
    async fn an_internal_failure_is_written_to_the_audit_log() {
        let h = harness(true).await;
        let (deployment_id, revision) = deployed_with_secret(&h).await;
        let service = make_service(h.state.clone());
        break_database(
            &h.state,
            "ALTER TABLE deployment_plans RENAME TO deployment_plans_gone;",
        )
        .await;

        let refusal = service
            .fetch_secret_values(from(
                &node("runtime-test"),
                secret_request("runtime-test", &deployment_id, &[&revision]),
            ))
            .await
            .expect_err("a broken catalog answered");
        assert_eq!(refusal.code(), tonic::Code::Internal);

        let lines = secret_fetch_lines(&h.state).await;
        assert_eq!(lines.len(), 1, "{lines:#?}");
        assert_eq!(lines[0].status, 500);
        assert_eq!(
            lines[0].actor_account.as_deref(),
            Some(node("runtime-test").to_string().as_str())
        );
    }

    /// The node the answer is for comes from the certificate: claiming
    /// another node's id is refused, and a deployment in flight only to
    /// another node is not visible to this one.
    #[tokio::test]
    async fn a_node_cannot_fetch_another_nodes_deployment_secrets() {
        let h = harness(true).await;
        let (deployment_id, revision) = deployed_with_secret(&h).await;
        let service = make_service(h.state.clone());

        let refusal = service
            .fetch_secret_values(from(
                &node("runtime-other"),
                secret_request("runtime-test", &deployment_id, &[&revision]),
            ))
            .await
            .expect_err("one node fetched secrets as another");
        assert_eq!(refusal.code(), tonic::Code::PermissionDenied);

        // A plan still preparing, frozen to `runtime-test` only.
        let env = h
            .state
            .store
            .get_environment("demo", "production")
            .await
            .unwrap();
        let current = h
            .state
            .store
            .current_deployment(&env.id)
            .await
            .unwrap()
            .unwrap();
        let version = h
            .state
            .store
            .get_version("demo", &current.version_id)
            .await
            .unwrap();
        let snapshot = h
            .state
            .store
            .get_environment_snapshot(current.snapshot_id.as_deref().unwrap())
            .await
            .unwrap();
        let plan = h
            .state
            .store
            .create_deployment_plan(
                &env,
                &version,
                crate::catalog::store::DeploymentPlanSpec {
                    snapshot: Some(snapshot),
                    expected_secret_heads: Default::default(),
                    compare_secret_heads: false,
                    target_ids: vec!["runtime-test".to_string()],
                    proxy_ids: Vec::new(),
                    queue_fence: false,
                    operation_kind: "deployment".to_string(),
                    secret_change: None,
                    note: None,
                },
                deis_wire::CanonicalId::new_v7(),
                30,
            )
            .await
            .unwrap();

        service
            .fetch_secret_values(from(
                &node("runtime-test"),
                secret_request("runtime-test", &plan.id, &[&revision]),
            ))
            .await
            .expect("the plan's target may fetch while preparing");
        let refusal = service
            .fetch_secret_values(from(
                &node("runtime-other"),
                secret_request("runtime-other", &plan.id, &[&revision]),
            ))
            .await
            .expect_err("a node fetched secrets of a plan that does not target it");
        assert_eq!(refusal.code(), tonic::Code::PermissionDenied);
    }

    // -----------------------------------------------------------------------
    // IssueCapability: only for what the node runs.
    // -----------------------------------------------------------------------

    fn capability_request(
        node_id: &str,
        audience: &str,
        resources: &[deis_capability::CapabilityResource],
    ) -> IssueCapabilityRequest {
        IssueCapabilityRequest {
            node_id: node_id.to_string(),
            audience: audience.to_string(),
            resources_json: serde_json::to_string(resources).unwrap(),
        }
    }

    async fn issuance_refusals(state: &CatalogState) -> Vec<crate::audit::AuditEntry> {
        AuditStore::new(state.store.database())
            .query_page(
                crate::audit::AuditQuery {
                    action: Some(CAPABILITY_ISSUE_ACTION.to_string()),
                    ..Default::default()
                },
                100,
                None,
            )
            .await
            .expect("the audit log reads")
            .items
    }

    async fn issued_jtis(state: &CatalogState) -> Vec<(String, String)> {
        state
            .store
            .database()
            .with_connection(|conn| {
                let mut statement = conn
                    .prepare("SELECT jti, subject FROM capability_issuances ORDER BY issued_at")?;
                let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(crate::database::DatabaseError::from)
            })
            .await
            .unwrap()
    }

    /// A node running `demo` in production is issued store and queue
    /// capabilities for it, each written to the issuance table under the
    /// name the node proved.
    #[tokio::test]
    async fn a_node_is_issued_a_capability_for_what_it_runs_and_it_is_recorded() {
        use deis_capability::{AUD_QUEUE, AUD_STORE, resources_for_audience};
        let h = harness(true).await;
        deployed_with_secret(&h).await;
        let service = make_service(h.state.clone());

        for aud in [AUD_STORE, AUD_QUEUE] {
            let answer = service
                .issue_capability(from(
                    &node("runtime-test"),
                    capability_request(
                        "runtime-test",
                        aud,
                        &resources_for_audience("demo.production", aud),
                    ),
                ))
                .await
                .expect("a node may have a capability for what it runs");
            let claims = deis_capability::decode(
                &answer.get_ref().capability_token,
                &h.state.issuer.verifying_key().await,
            )
            .unwrap();
            assert_eq!(claims.sub, node("runtime-test").to_string());
            assert!(
                claims.exp - claims.iat <= 15 * 60,
                "a node capability lives at most fifteen minutes"
            );
        }
        let issued = issued_jtis(&h.state).await;
        assert_eq!(issued.len(), 2);
        assert!(
            issued
                .iter()
                .all(|(_, subject)| *subject == node("runtime-test").to_string())
        );
        assert!(issuance_refusals(&h.state).await.is_empty());
    }

    /// The property this whole check exists for: a node - compromised or
    /// not - cannot mint access to a tenant it does not run, even alongside
    /// one it does. The refusal is on the audit log under its proved name.
    #[tokio::test]
    async fn a_node_cannot_mint_for_a_tenant_it_does_not_run() {
        use deis_capability::{AUD_STORE, resources_for_audience};
        let h = harness(true).await;
        deployed_with_secret(&h).await;
        let service = make_service(h.state.clone());

        let mut asked = resources_for_audience("demo.production", AUD_STORE);
        asked.extend(resources_for_audience("victim.production", AUD_STORE));
        let refusal = service
            .issue_capability(from(
                &node("runtime-test"),
                capability_request("runtime-test", AUD_STORE, &asked),
            ))
            .await
            .expect_err("a node minted a capability for a tenant it does not run");
        assert_eq!(refusal.code(), tonic::Code::PermissionDenied);
        assert!(refusal.message().contains("victim.production"));
        assert!(issued_jtis(&h.state).await.is_empty(), "nothing was issued");

        let lines = issuance_refusals(&h.state).await;
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].actor_account.as_deref(),
            Some(node("runtime-test").to_string().as_str())
        );
        assert_eq!(lines[0].status, 403);
    }

    /// A machine with a certificate this fleet signed but no place in the
    /// fleet runs nothing of ours, so it is owed nothing - and neither is a
    /// node asking for an audience nothing checks per tenant.
    #[tokio::test]
    async fn a_node_outside_the_fleet_or_asking_for_an_unknown_audience_gets_nothing() {
        use deis_capability::{AUD_STORE, resources_for_audience};
        let h = harness(true).await;
        deployed_with_secret(&h).await;
        let service = make_service(h.state.clone());

        let refusal = service
            .issue_capability(from(
                &node("runtime-stranger"),
                capability_request(
                    "runtime-stranger",
                    AUD_STORE,
                    &resources_for_audience("demo.production", AUD_STORE),
                ),
            ))
            .await
            .expect_err("a node outside the fleet was issued a capability");
        assert_eq!(refusal.code(), tonic::Code::PermissionDenied);

        let refusal = service
            .issue_capability(from(
                &node("runtime-test"),
                capability_request(
                    "runtime-test",
                    "telemetry.read",
                    &resources_for_audience("demo.production", AUD_STORE),
                ),
            ))
            .await
            .expect_err("a node was issued a read capability");
        assert_eq!(refusal.code(), tonic::Code::PermissionDenied);

        let refusal = service
            .issue_capability(from(
                &node("runtime-test"),
                capability_request(
                    "runtime-other",
                    AUD_STORE,
                    &resources_for_audience("demo.production", AUD_STORE),
                ),
            ))
            .await
            .expect_err("a node asked as another node");
        assert_eq!(refusal.code(), tonic::Code::PermissionDenied);
        assert_eq!(issuance_refusals(&h.state).await.len(), 3);
    }
}
