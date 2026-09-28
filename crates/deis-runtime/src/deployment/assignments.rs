//! Principle 8: the node asks what it should be running.
//!
//! On startup this node calls `GET /api/nodes/{node_id}/assignments` on the
//! control plane, fetches and loads every version it names, and only then
//! reports healthy. The direction is deliberate. The control plane does not
//! have to know how to reach a node before that node can run anything; a node
//! that comes back from a reboot puts itself back to work on its own; and
//! adding a node is one process start rather than an edit to the control
//! plane's configuration.
//!
//! The same question is asked again on a timer, and the answer is treated as
//! the whole truth rather than as a set of additions: an environment this node
//! runs and the list does not name stops here. That is the only way a deletion
//! ever reaches a node that was unreachable when it happened, because a
//! deletion has nothing left to push.
//!
//! Stopping something is the one thing a node cannot take back cheaply, so it
//! takes two independent pieces of evidence, not one:
//!
//!   - the control plane must say the answer *is* the whole truth
//!     ([`AssignmentList::complete`]). An answer that cannot promise it is
//!     loaded from and never removed on.
//!   - two consecutive complete answers must both omit the environment. The
//!     `complete` flag means only that nothing was dropped while the answer
//!     was assembled; it is not a check that the catalog behind it was
//!     readable, so a single short answer is exactly what a control-plane
//!     fault looks like. Making the same absence appear twice costs a deleted
//!     application one more reconcile interval on this node - a push carries
//!     the ordinary case anyway - and stops one bad answer from taking a whole
//!     node's applications down. See [`Reconciler`].
//!
//! And the answer is dated, because it takes time to arrive: the node reads a
//! change stamp (`DeploymentManager::change_stamp`) before it asks, and
//! anything it did to itself after that stamp - a push that landed while the
//! request was in flight - is the newer instruction and is left alone.
//!
//! The bundle itself is never in the list: each line is a pointer - app,
//! version, environment, URL, digest - plus the environment map that a push
//! would have carried, so an app that comes back this way comes back with its
//! variables and its storage addresses rather than with an empty environment;
//! the secrets its snapshot binds are fetched separately.
//! An older control plane that sends no `env` still works; the app then runs
//! on its manifest's own variables until the next push, which is what this
//! path used to do for every app and is said out loud when it happens.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use deis_identity::{Dialler, MachineIdentity};
use deis_wire::machine::machine_channel_client::MachineChannelClient;
use deis_wire::machine::{FetchAssignmentsRequest, FetchAssignmentsResponse};
use deis_wire::{CanonicalId, CanonicalTenantId};
use serde::Deserialize;
use tonic::transport::{Channel, Endpoint};
use tracing::{debug, error, info, warn};

use super::offline_cache::OfflineCache;
use super::registration::{NodeRegistrar, Registered};
use super::{Applied, DeploymentManager, DeploymentOrder};
use crate::app::{AppKey, InstalledVersion};
use crate::health::NodeHealth;

/// One line of `GET /api/nodes/{node_id}/assignments`, in exactly the shape
/// the control plane serves (see deis-control `api::catalog::assignments`).
#[derive(Clone, Deserialize)]
pub struct Assignment {
    pub app_id: String,
    /// Additive for a rolling upgrade. Both canonical fields must be present
    /// together; an older control plane sends neither.
    #[serde(default)]
    pub canonical_application_id: Option<CanonicalId>,
    pub version_id: String,
    #[serde(default)]
    pub deployment_id: Option<String>,
    #[serde(default)]
    pub routing_generation: Option<String>,
    /// Absent on older control planes, where every assignment was an
    /// immediate activation. New control planes explicitly hold an
    /// in-flight generation in prepare state until the durable plan commits.
    #[serde(default)]
    pub deployment_action: AssignmentDeploymentAction,
    pub environment: String,
    #[serde(default)]
    pub canonical_environment_id: Option<CanonicalId>,
    pub bundle_url: String,
    pub bundle_sha: String,
    /// Variables and platform wiring, exactly as a push would have sent
    /// them; never a secret value. Absent from an older control plane.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// What this version's manifest asked to be held to, exactly as a push
    /// would have sent it. Absent from an older control plane, and absent for
    /// an application that declared nothing; either way the node's own
    /// ceilings apply.
    ///
    /// Carried here and not only on the push because this is the path a node
    /// comes back on after a restart: without it an application that narrowed
    /// its CPU budget would silently return to the node default and stay there
    /// until somebody deployed again.
    #[serde(default)]
    pub limits: Option<crate::app::types::ResourceLimits>,
    /// Which of the platform's answers this version expects, exactly as a
    /// push would have sent them. Absent from an older control plane, in
    /// which case the version's own manifest is the only thing that says -
    /// and it always does, because the declaration lives in the bundle.
    ///
    /// Held as the *declaration* rather than as resolved answers so that this
    /// field can never fail to parse. A control plane newer than this node
    /// will eventually send a flag name or a date this build has never heard
    /// of, and a strict field here would take the whole list down with it -
    /// one application this node cannot run would stop it reconciling the
    /// other forty, which are fine. Resolving happens per application in
    /// [`Assignment::into_order`], and the application this node genuinely
    /// cannot honour is refused on its own when its bundle is unpacked.
    #[serde(default)]
    pub compatibility: Option<deis_bundle::DeclaredCompatibility>,
    #[serde(default)]
    pub environment_declarations: Option<deis_wire::EnvironmentDeclarations>,
    #[serde(default)]
    pub environment_snapshot: Option<deis_wire::EnvironmentBindingSnapshot>,
    #[serde(default)]
    pub environment_snapshot_id: Option<String>,
}

/// Hand-written so a `{:?}` on an assignment - in a log line, in an error
/// path - cannot print the secrets the control plane just decrypted.
impl std::fmt::Debug for Assignment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&str> = self.env.keys().map(String::as_str).collect();
        names.sort_unstable();
        f.debug_struct("Assignment")
            .field("app_id", &self.app_id)
            .field("canonical_application_id", &self.canonical_application_id)
            .field("version_id", &self.version_id)
            .field("deployment_id", &self.deployment_id)
            .field("routing_generation", &self.routing_generation)
            .field("deployment_action", &self.deployment_action)
            .field("environment", &self.environment)
            .field("canonical_environment_id", &self.canonical_environment_id)
            .field("bundle_url", &self.bundle_url)
            .field("bundle_sha", &self.bundle_sha)
            .field("env", &format_args!("{names:?} = <redacted>"))
            .field("limits", &self.limits)
            .field("compatibility", &self.compatibility)
            .field("environment_snapshot_id", &self.environment_snapshot_id)
            .finish()
    }
}

impl Assignment {
    fn into_order(self) -> Result<DeploymentOrder, String> {
        let standard_environment_id = self
            .canonical_environment_id
            .as_ref()
            .map(ToString::to_string);
        let canonical_tenant = match (self.canonical_application_id, self.canonical_environment_id)
        {
            (Some(application), Some(environment)) => {
                Some(CanonicalTenantId::new(application, environment))
            }
            (None, None) => None,
            (None, Some(_)) => {
                return Err("canonical_application_id is missing".to_string());
            }
            (Some(_), None) => {
                return Err("canonical_environment_id is missing".to_string());
            }
        };

        // Resolved here, per application, so that a declaration this build
        // cannot read costs only the cross-check in `install` and not the
        // whole list. Nothing is decided by dropping it: the order's copy
        // exists to catch a control plane and a bundle disagreeing, while the
        // bundle's own manifest - which this node is about to unpack, and
        // which is what the digest covers - is what actually refuses a version
        // this node cannot honour.
        let resolved = self.compatibility.as_ref().and_then(|declared| {
            match deis_bundle::CompatibilityProfile::resolve(declared) {
                Ok(profile) => Some(profile),
                Err(reason) => {
                    warn!(
                        app_id = %self.app_id,
                        environment = %self.environment,
                        version_id = %self.version_id,
                        reason = %reason,
                        "The assignment list names platform behaviour this node's build does \
                         not have. The declaration inside the bundle decides, and if it says \
                         the same thing this version is refused when it is unpacked; nothing \
                         else on this list is affected"
                    );
                    None
                }
            }
        });

        let standard_environment = match (
            self.environment_declarations,
            self.environment_snapshot,
            self.environment_snapshot_id,
        ) {
            (None, None, None) => None,
            (Some(declarations), Some(snapshot), Some(snapshot_id)) => {
                Some(super::StandardEnvironmentOrder::new_without_secrets(
                    declarations,
                    snapshot,
                    snapshot_id,
                    &self.version_id,
                    standard_environment_id.as_deref().ok_or_else(|| {
                        "EC_ENV_SNAPSHOT_INVALID: canonical environment identity is missing"
                            .to_string()
                    })?,
                )?)
            }
            _ => {
                return Err(
                    "EC_ENV_SNAPSHOT_INVALID: assignment environment fields are incomplete"
                        .to_string(),
                );
            }
        };

        Ok(DeploymentOrder {
            app_id: self.app_id,
            canonical_tenant,
            version_id: self.version_id,
            environment: self.environment,
            bundle_url: self.bundle_url,
            bundle_sha: self.bundle_sha,
            env: self.env,
            standard_environment,
            limits: self.limits,
            compatibility: resolved,
            deployment_id: self.deployment_id,
            routing_generation: self.routing_generation,
        })
    }
}

/// Assignment-list phase. The default preserves compatibility with control
/// planes that predate durable deployment plans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentDeploymentAction {
    Prepare,
    #[default]
    Activate,
}

/// Why the assignment list could not be obtained.
#[derive(Debug)]
pub enum AssignmentError {
    Transport { url: String, reason: String },
    Status { url: String, code: u16 },
    Malformed { url: String, reason: String },
}

impl std::fmt::Display for AssignmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport { url, reason } => {
                write!(f, "could not reach the control plane at {url}: {reason}")
            }
            Self::Status { url, code } => {
                write!(f, "the control plane answered HTTP {code} for {url}")
            }
            Self::Malformed { url, reason } => write!(
                f,
                "the assignment list from {url} is not in the expected shape: {reason}"
            ),
        }
    }
}

impl std::error::Error for AssignmentError {}

impl AssignmentError {
    /// Whether the control plane could not be reached at all - a refused
    /// socket, a failed handshake, a timeout - as opposed to having answered.
    ///
    /// Only this may send a node back to its offline cache. A control plane
    /// that answered with a refusal (this node is revoked, or its
    /// certificate is not accepted) or with anything else has been heard
    /// from, and what it said is not "carry on with what you had".
    pub fn is_unreachable(&self) -> bool {
        matches!(self, Self::Transport { .. })
    }
}

/// Asks the control plane what this node should run.
///
/// The question and the answer are the same regardless of transport:
/// which versions this node should be running, and whether that list is
/// the whole truth.  Since the machine channel landed (ADR-025), the only
/// transport is mutual TLS over the machine listener — the connection
/// itself proves which machine is asking, and the control plane refuses a
/// call whose `node_id` is not the name the certificate proved.
///
/// The transport is decided once at startup by
/// [`AssignmentClient::from_config`].  A node without a configured
/// `[machine_identity]` refuses to start rather than silently failing to
/// fetch assignments on a path that is no longer served.
pub struct AssignmentClient {
    transport: Transport,
    /// What every log line and every error names — the machine channel
    /// endpoint followed by the RPC's path, which is literally what goes
    /// on the wire over HTTP/2, so an operator can match this against the
    /// control plane's own log.
    url: String,
}

/// How this node asks, and what it proves while asking.
enum Transport {
    /// `deis.machine.MachineChannel/FetchAssignments` over mutual TLS.
    Machine {
        /// Built once, at startup, and kept for the life of the process. It
        /// is a channel and not a connection: it dials when it is first used
        /// and redials after a broken one, and the dialler behind it resolves
        /// the certificate per connection, so a renewal reaches the wire
        /// without this being rebuilt. Building one per fetch would instead
        /// pay a fresh TCP connection and a fresh TLS handshake on every
        /// reconcile interval, on every node, for no gain at all.
        client: MachineChannelClient<Channel>,
        /// Kept so a refusal can name what this node proved, which is the
        /// other half of diagnosing one - and read through the identity
        /// rather than copied at startup, so it says what the connection
        /// actually presented after a renewal.
        identity: Arc<MachineIdentity>,
        /// The name this node asks about. It has to equal the name in the
        /// certificate, or the control plane refuses; see
        /// [`machine_failure`].
        node_id: String,
    },
}

/// The path `FetchAssignments` is addressed at on the wire. gRPC over HTTP/2
/// is `POST /<package>.<service>/<method>`, so this is the real path and not a
/// label invented for the log.
const FETCH_ASSIGNMENTS_PATH: &str = "/deis.machine.MachineChannel/FetchAssignments";

impl AssignmentClient {
    /// Ask over the machine channel, proving `identity` and refusing to talk
    /// to anything that cannot prove it is `control_identity`.
    ///
    /// No credential is carried and there is none to carry: the connection has
    /// already said which machine is asking, which is more than any string
    /// could have said.
    pub fn over_machine_channel(
        machine_endpoint: &str,
        node_id: &str,
        timeout: Duration,
        identity: Arc<MachineIdentity>,
        control_identity: deis_identity::SpiffeId,
    ) -> anyhow::Result<Self> {
        let endpoint = Endpoint::from_shared(machine_endpoint.to_string())
            .map_err(|e| {
                anyhow::anyhow!(
                    "control.machine_endpoint {machine_endpoint:?} is not an address this node \
                     can dial: {e}"
                )
            })?
            // The same ceiling `[control] request_timeout_secs` puts on the
            // HTTP fetch, applied to the same question: one that hangs is one
            // this node stops waiting on and retries.
            .timeout(timeout)
            .connect_timeout(timeout);

        // gRPC runs on HTTP/2 and on nothing else, so that is what the
        // handshake asks for. Stating it is not decoration - see
        // `deis_identity::Protocol`.
        let dialler = Dialler::new(
            Arc::clone(&identity),
            control_identity,
            &[deis_identity::Protocol::Http2],
        );

        // Lazily, so a control plane that is not up yet is a failed fetch that
        // retries rather than a node that refuses to start. This matches what
        // the HTTP client does: neither one connects while it is being built.
        let client = MachineChannelClient::new(endpoint.connect_with_connector_lazy(dialler));

        Ok(Self {
            transport: Transport::Machine {
                client,
                identity,
                node_id: node_id.to_string(),
            },
            url: format!(
                "{}{FETCH_ASSIGNMENTS_PATH}",
                machine_endpoint.trim_end_matches('/')
            ),
        })
    }

    /// The machine transport over a channel somebody else built.
    ///
    /// For the harness. A test can put a real `MachineChannel` server on a
    /// plain socket and drive this client against it, which is what covers the
    /// decode, the refusal and the error mapping without a certificate
    /// authority in the loop; the handshake itself is covered where it lives,
    /// against the control plane's own listener. The only difference from
    /// [`Self::over_machine_channel`] is who dialled.
    #[cfg(test)]
    pub(crate) fn over_channel(
        channel: Channel,
        node_id: &str,
        identity: Arc<MachineIdentity>,
        url: String,
    ) -> Self {
        Self {
            transport: Transport::Machine {
                client: MachineChannelClient::new(channel),
                identity,
                node_id: node_id.to_string(),
            },
            url,
        }
    }

    /// Returns a clone of the inner machine channel client, if this
    /// assignment client is backed by one.  Background tasks such as the
    /// capability renewal loop use this to reach the control plane without
    /// sharing `&mut self` with the main reconciliation loop.
    pub fn machine_channel_client(&self) -> Option<MachineChannelClient<Channel>> {
        match &self.transport {
            Transport::Machine { client, .. } => Some(client.clone()),
        }
    }

    /// The node name this client speaks for, or the empty string if there
    /// is no machine transport.
    pub fn node_id(&self) -> &str {
        match &self.transport {
            Transport::Machine { node_id, .. } => node_id,
        }
    }

    /// The client this configuration calls for, or `None` when this node was
    /// told about no control plane at all and so asks nobody.
    ///
    /// `identity` is what `main` loaded from `[machine_identity]`. An identity
    /// with an address is a node that asks; an identity with no address is a
    /// node that only serves its management plane and is handed what to run -
    /// `deis dev` runs exactly that - and it asks nobody. The remaining
    /// mismatch, an address with no identity, is a startup that skipped
    /// `RuntimeConfig::validate` rather than anything an operator can write,
    /// and is refused rather than returning `None`, because a `None` there
    /// would be a node that fetches nothing while every other part of it looks
    /// configured.
    pub fn from_config(
        config: &crate::config::RuntimeConfig,
        node_id: &str,
        identity: Option<Arc<MachineIdentity>>,
    ) -> anyhow::Result<Option<Self>> {
        let timeout = Duration::from_secs(config.control.request_timeout_secs);
        match (
            config
                .machine_identity
                .as_ref()
                .and_then(|m| m.control_endpoint.as_deref()),
            identity,
        ) {
            (Some(machine_endpoint), Some(identity)) => {
                let control_identity = config
                    .machine_identity
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "a machine identity was loaded but [machine_identity] is absent from \
                             the configuration it was loaded from"
                        )
                    })?
                    .expected_control_identity()?;
                Ok(Some(Self::over_machine_channel(
                    machine_endpoint,
                    node_id,
                    timeout,
                    identity,
                    control_identity,
                )?))
            }
            (Some(machine_endpoint), None) => Err(anyhow::anyhow!(
                "control.machine_endpoint is {machine_endpoint:?} but this node has no machine \
                 identity loaded, so it could not complete a handshake with that listener"
            )),
            // An identity and nowhere to ask: a node that serves its
            // management plane and pulls nothing, which `validate` accepts
            // only alongside a management listener. It asks nobody, the same
            // as a node with no identity at all.
            (None, Some(_)) => Ok(None),
            // No identity and no machine endpoint: a node that asks nobody
            // what to run and serves what is already in its local store.
            // There is no fourth case - a node that names a control plane
            // without a certificate is refused by `RuntimeConfig::validate`,
            // because everything it would ask for is served only on the
            // machine channel.
            (None, None) => Ok(None),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// How this node is asking, for the one startup line that says so.
    pub fn transport_name(&self) -> &'static str {
        match self.transport {
            Transport::Machine { .. } => "the machine channel, over mutual TLS",
        }
    }

    pub async fn fetch(&self) -> Result<AssignmentList, AssignmentError> {
        let Transport::Machine {
            client,
            identity,
            node_id,
        } = &self.transport;
        self.fetch_over_machine_channel(client, identity, node_id)
            .await
    }

    async fn fetch_over_machine_channel(
        &self,
        client: &MachineChannelClient<Channel>,
        identity: &MachineIdentity,
        node_id: &str,
    ) -> Result<AssignmentList, AssignmentError> {
        // tonic's generated client takes `&mut self` per call, and it is a
        // handle rather than a connection: this clone shares the one channel
        // built at startup, and with it whatever connection that channel
        // already has open.
        let mut client = client.clone();
        let answer = client
            .fetch_assignments(FetchAssignmentsRequest {
                node_id: node_id.to_string(),
            })
            .await
            .map_err(|status| {
                machine_failure(&self.url, node_id, &identity.id().to_string(), &status)
            })?;

        decode_machine_answer(&self.url, answer.into_inner())
    }
}

/// Turn what the RPC carries into the same answer the HTTP route produces.
///
/// The payload did not move when the transport did: `assignments_json` is the
/// identical JSON array the HTTP route served, so this is the whole decode.
///
/// `complete` is an explicit field here and was a header there, and both mean
/// exactly the same thing - whether the list may be acted on for removal. On
/// HTTP an absent header is read as false on purpose, because a control plane
/// too old to send it cannot promise anything; on the RPC the field is always
/// present, so it is read straight and needs no such rule. A node must remove
/// nothing on the strength of an answer whose `complete` is false, whichever
/// door the answer came through.
fn decode_machine_answer(
    url: &str,
    answer: FetchAssignmentsResponse,
) -> Result<AssignmentList, AssignmentError> {
    let assignments =
        serde_json::from_str::<Vec<Assignment>>(&answer.assignments_json).map_err(|e| {
            AssignmentError::Malformed {
                url: url.to_string(),
                reason: e.to_string(),
            }
        })?;

    Ok(AssignmentList {
        assignments,
        complete: answer.complete,
        withheld: answer.withheld,
        raw_json: answer.assignments_json,
    })
}

/// Put a failed RPC into the taxonomy an operator already diagnoses this node
/// from.
///
/// The three classes are what the logs and the runbooks are written around, so
/// a gRPC failure is placed in one of them rather than given a fourth:
///
/// * a call that never reached a control plane willing to answer - a refused
///   connection, a handshake that failed, a deadline, a cancelled call - is
///   `Transport`, which is the same thing the HTTP client reports when the
///   socket does not open.
/// * anything the control plane answered with a status is `Status`, carrying
///   the HTTP code gRPC's own specification maps that code onto, so that
///   "refused this node's credential" reads as 401/403 whichever transport it
///   arrived on.
///
/// `PERMISSION_DENIED` gets its own message because it is not a fault of the
/// network or of the control plane: it means this node asked for a node it is
/// not. The connection proved one name and the request named another, so both
/// names are in the line - without them an operator is left with a bare
/// "permission denied" and no way to see that the certificate and the
/// configured `node_id` disagree.
fn machine_failure(
    url: &str,
    asked_for: &str,
    proving: &str,
    status: &tonic::Status,
) -> AssignmentError {
    use tonic::Code;

    match status.code() {
        Code::PermissionDenied => {
            error!(
                url = %url,
                asked_for = %asked_for,
                proving = %proving,
                message = status.message(),
                "The control plane refused to hand this node another node's assignments: the \
                 connection proved one name and the request asked about a different one. A node \
                 may only fetch its own list, so this node gets none at all and picks up no new \
                 version. Either node_id is wrong for the certificate in [machine_identity], or \
                 the certificate is the wrong machine's"
            );
            AssignmentError::Status {
                url: url.to_string(),
                code: 403,
            }
        }
        Code::Unauthenticated => {
            error!(
                url = %url,
                proving = %proving,
                message = status.message(),
                "The control plane would not authenticate this node on the machine channel, so \
                 this node gets no assignment list at all: it keeps serving whatever it already \
                 had and picks up no new version. Check that the certificate in \
                 [machine_identity] is one this fleet's trust root signed and that it has not \
                 expired"
            );
            AssignmentError::Status {
                url: url.to_string(),
                code: 401,
            }
        }
        // No answer from a serving control plane, which is what a refused
        // socket is on the HTTP side. The message is kept because it is where
        // a handshake failure says which certificate it did not like.
        Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled | Code::Unknown => {
            AssignmentError::Transport {
                url: url.to_string(),
                reason: format!("{}: {}", status.code(), status.message()),
            }
        }
        code => AssignmentError::Status {
            url: url.to_string(),
            code: match code {
                Code::InvalidArgument | Code::FailedPrecondition | Code::OutOfRange => 400,
                Code::NotFound => 404,
                Code::AlreadyExists | Code::Aborted => 409,
                Code::ResourceExhausted => 429,
                Code::Unimplemented => 501,
                // Internal, DataLoss and anything a later gRPC version adds.
                _ => 500,
            },
        },
    }
}

/// One answer from the control plane.
///
/// The array on its own cannot be acted on for removal: an application that is
/// absent because it was deleted and one that is absent because this control
/// plane could not build its row look identical. `complete` is how the control
/// plane tells the two apart, and a node that removes on an incomplete answer
/// turns somebody else's problem into an outage for code that is running fine.
#[derive(Debug, Clone)]
pub struct AssignmentList {
    pub assignments: Vec<Assignment>,
    /// True only when the control plane said so. An answer with no such
    /// header - a control plane older than it - is treated as incomplete, so
    /// this node loads from it and removes nothing.
    pub complete: bool,
    /// Assignments the control plane left out, when `complete` is false.
    pub withheld: u32,
    /// The list exactly as it arrived, which is what the offline cache keeps:
    /// a node restarting without its control plane decodes it the same way.
    pub raw_json: String,
}

/// Result of one pass over the assignment list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncOutcome {
    pub assigned: usize,
    pub installed: usize,
    pub unchanged: usize,
    pub failed: usize,
    /// Environments this node was running and the control plane no longer
    /// assigns, which this pass stopped.
    pub unloaded: usize,
    /// Environments that should have been stopped and could not be. They are
    /// still serving, which is the half of a reconciliation that leaves the
    /// node further from the list than it started.
    pub unload_failed: usize,
    /// Environments this pass deliberately left alone because this node
    /// changed them after the list was asked for. The list is the older of the
    /// two instructions, so acting on it would roll a push back.
    pub superseded: usize,
    /// Environments the list did not name for the first time. Nothing was
    /// stopped for them: removal needs the same absence in the next complete
    /// answer too.
    pub removal_pending: usize,
}

impl SyncOutcome {
    /// How many environments this node is serving when the pass ends.
    ///
    /// This is the number the node publishes as its own health
    /// ([`crate::health::NodeHealth::mark_ready`]), so it has to be a count
    /// of what is *running*, not of what this pass happened to touch. Every
    /// category below leaves an environment serving:
    ///
    /// * `installed` and `unchanged` - the ordinary two.
    /// * `superseded` - the pass left the environment alone because this node
    ///   changed it after the list was asked for. It is serving; it is
    ///   serving the newer thing. Counting only the first two reported a node
    ///   that took a push mid-pass as running fewer applications than it does,
    ///   and a node that took a push for *every* environment as running none.
    /// * `removal_pending` - absent from one complete list, which is not yet
    ///   enough to stop anything. Still serving, by design.
    /// * `unload_failed` - it should have stopped and did not. Still serving,
    ///   which is exactly why the pass reports the node degraded.
    ///
    /// `failed` is deliberately not here: a version that would not load
    /// leaves either nothing running or the previous version running, and
    /// this outcome cannot tell which. `unloaded` is not here either - those
    /// are stopped.
    pub fn serving(&self) -> usize {
        self.installed
            + self.unchanged
            + self.superseded
            + self.removal_pending
            + self.unload_failed
    }
}

#[cfg(test)]
mod serving_test {
    use super::SyncOutcome;

    /// The count the node publishes is a count of running environments.
    ///
    /// Every field asserted here was, at some point, a way for a node to
    /// under-report itself: a push that landed mid-pass, an absence seen once,
    /// or an unload that failed all leave an application serving requests
    /// while the pass that saw them counted it as nothing.
    #[test]
    fn every_environment_still_serving_is_counted() {
        let outcome = SyncOutcome {
            assigned: 4,
            installed: 1,
            unchanged: 1,
            superseded: 1,
            removal_pending: 1,
            unload_failed: 1,
            ..Default::default()
        };
        assert_eq!(outcome.serving(), 5);
    }

    /// A pass in which every assignment was superseded by a push is not a
    /// node running nothing.
    #[test]
    fn a_pass_that_only_superseded_still_reports_what_it_runs() {
        let outcome = SyncOutcome {
            assigned: 3,
            superseded: 3,
            ..Default::default()
        };
        assert_eq!(
            outcome.serving(),
            3,
            "the list was the older instruction for all three; the node is serving all three"
        );
    }

    /// What has stopped, or may never have started, is not counted.
    #[test]
    fn stopped_and_failed_environments_are_not_counted() {
        let outcome = SyncOutcome {
            assigned: 2,
            installed: 1,
            failed: 1,
            unloaded: 3,
            ..Default::default()
        };
        assert_eq!(outcome.serving(), 1);
    }
}

/// Keeps this node running what the control plane says it should, one pass at
/// a time.
///
/// A pass is [`Reconciler::once`]. It is a method on a value rather than a
/// free function because one thing has to be remembered between passes and
/// cannot be recovered from the node's own state: which environments the
/// *previous* complete answer already failed to name.
///
/// That memory is what makes removal safe. `complete` on an answer means the
/// control plane dropped nothing while assembling it; it is not a check that
/// the catalog behind it was readable, so a control plane that reads an empty
/// or half-populated catalog produces a short answer that says `true`. Acting
/// on one such answer would take every application off this node at once.
/// Requiring two consecutive complete answers to agree on an absence turns a
/// transient control-plane fault into a delay instead of an outage: the second
/// answer disagrees and nothing is stopped. A real deletion is not transient,
/// so it simply arrives one reconcile interval later - and the ordinary
/// deletion is pushed to the node directly anyway; reconciliation is the
/// backstop for a node that was unreachable when that push went out.
///
/// Loading is not held back like this. Everything an answer *does* name is
/// real whatever else went wrong, so a version in the list is installed on the
/// first pass that names it.
pub struct Reconciler<'a> {
    client: &'a AssignmentClient,
    deployer: &'a DeploymentManager,
    /// Environments the previous *complete* answer did not name while this
    /// node was running them. An incomplete answer neither adds to this nor
    /// clears it: it is not evidence about absence in either direction.
    absent_before: HashSet<AppKey>,
    /// The last complete list, as it arrived, not yet handed to the offline
    /// cache. See [`Self::take_complete_list`].
    complete_list: Option<String>,
}

impl<'a> Reconciler<'a> {
    pub fn new(client: &'a AssignmentClient, deployer: &'a DeploymentManager) -> Self {
        Self {
            client,
            deployer,
            absent_before: HashSet::new(),
            complete_list: None,
        }
    }

    /// The list the last pass acted on in full, if it was complete, as it
    /// arrived. Taken once: a later pass that answers incompletely leaves
    /// nothing to take, so the offline cache keeps the last whole answer.
    pub fn take_complete_list(&mut self) -> Option<String> {
        self.complete_list.take()
    }

    /// Fetch the assignment list and make this node match it.
    ///
    /// The answer is the whole truth, not a set of additions: an environment
    /// this node runs and the list does not name is stopped here - once a
    /// second complete answer has said the same thing. Both halves matter and
    /// only one of them used to happen. An application deleted from the
    /// catalog, an environment torn down, a node taken out of an application's
    /// placement - none of those reach a node as an instruction, because the
    /// control plane's record of them is a row that no longer exists. A node
    /// that only ever added would keep serving them, with their secrets, until
    /// somebody restarted the process.
    ///
    /// It is also what makes the answer safe to act on repeatedly: running
    /// this on a node that is already correct changes nothing and says
    /// nothing.
    pub async fn once(&mut self) -> Result<SyncOutcome, AssignmentError> {
        // Both of these are read *before* the question is asked, because they
        // are what dates the answer. A push that lands while the request is in
        // flight is a later instruction about the same environment; reading
        // the node's state afterwards would show that push as drift from a
        // list that predates it, and the pass would roll it back.
        let as_of = self.deployer.change_stamp();
        let running = self.deployer.running();

        let AssignmentList {
            assignments,
            complete,
            withheld,
            raw_json,
        } = self.client.fetch().await?;

        let assigned: HashMap<AppKey, String> = assignments
            .iter()
            .map(|assignment| {
                (
                    AppKey::new(&assignment.app_id, &assignment.environment),
                    assignment.version_id.clone(),
                )
            })
            .collect();
        report_drift(&assigned, &running, complete, &self.absent_before);
        let assigned_deployments: HashSet<String> = assignments
            .iter()
            .filter_map(|assignment| assignment.deployment_id.clone())
            .collect();

        let mut outcome = SyncOutcome {
            assigned: assignments.len(),
            ..Default::default()
        };

        for assignment in assignments {
            let app_id = assignment.app_id.clone();
            let environment = assignment.environment.clone();
            let version_id = assignment.version_id.clone();
            let deployment_action = assignment.deployment_action;
            let order = match assignment.into_order() {
                Ok(order) => order,
                Err(error) => {
                    outcome.failed += 1;
                    error!(
                        app_id = %app_id,
                        environment = %environment,
                        version_id = %version_id,
                        error = %error,
                        "Refusing an assignment with only half of its canonical identity; the running version, if any, is left untouched"
                    );
                    continue;
                }
            };
            let applied = match deployment_action {
                AssignmentDeploymentAction::Prepare => {
                    self.deployer.prepare_from_assignment(order, as_of).await
                }
                AssignmentDeploymentAction::Activate => {
                    self.deployer.apply_from_assignment(order, as_of).await
                }
            };
            match applied {
                Ok(Applied::Installed { reused_bundle }) => {
                    outcome.installed += 1;
                    info!(
                        app_id = %app_id,
                        environment = %environment,
                        version_id = %version_id,
                        reused_bundle,
                        "Loaded an assigned version"
                    );
                }
                Ok(Applied::AlreadyRunning) => outcome.unchanged += 1,
                // Said in full by `apply_from_assignment`, which is the side
                // that knows what is running instead.
                Ok(Applied::Superseded { .. }) => outcome.superseded += 1,
                Err(e) => {
                    outcome.failed += 1;
                    error!(
                        app_id = %app_id,
                        environment = %environment,
                        version_id = %version_id,
                        error = %e,
                        "Could not load assigned version {} of {} environment {}; that \
                         environment is not serving on this node",
                        version_id,
                        app_id,
                        environment
                    );
                }
            }
        }

        if !complete {
            // The control plane says it could not describe every assignment it
            // holds, so absence from this list is not an instruction: an
            // application missing because the control plane failed to build its
            // row looks exactly like one that was deleted. Loading is still
            // right - everything listed is real. `absent_before` is left as it
            // was: this answer is not evidence about absence either way, so it
            // neither confirms a candidate nor clears one.
            warn!(
                url = self.client.url(),
                withheld,
                listed = outcome.assigned,
                "The control plane marked this assignment list incomplete ({} assignment(s) it \
                 could not describe), so nothing is stopped on the strength of it. This node \
                 loads what is listed and keeps everything else running; anything that should \
                 have been stopped stays up until the control plane can answer in full",
                withheld
            );
            return Ok(outcome);
        }

        // Only a complete answer bounds the secret cache: a deployment an
        // incomplete one left out may still be running here.
        self.deployer.retain_secrets(&assigned_deployments);
        self.complete_list = Some(raw_json);

        let mut absent_now: HashSet<AppKey> = HashSet::new();
        for (key, installed) in running {
            if assigned.contains_key(&key) {
                continue;
            }

            if self.deployer.changed_since(&key, as_of) {
                // Something happened to this environment on this node after
                // the list was asked for - a push, or an unload. That is the
                // newer instruction, so this list has nothing to say about it,
                // and it does not count as an absence either: the next pass
                // reads a stamp taken after the change and judges it fresh.
                outcome.superseded += 1;
                info!(
                    app_id = %key.app_id(),
                    environment = %key.environment(),
                    "Not stopping {} even though the assignment list does not name it: this node \
                     changed that environment after the list was asked for, so the list is the \
                     older instruction of the two",
                    key
                );
                continue;
            }

            if !self.absent_before.contains(&key) {
                // One complete answer is not enough to stop code that is
                // serving. See the type's own documentation for why.
                absent_now.insert(key.clone());
                outcome.removal_pending += 1;
                info!(
                    app_id = %key.app_id(),
                    environment = %key.environment(),
                    version_id = %installed.version_id,
                    "The control plane's assignment list no longer names {}, and this is the \
                     first complete answer to leave it out. It keeps serving; if the next \
                     complete answer leaves it out as well, this node stops it then",
                    key
                );
                continue;
            }

            match self
                .deployer
                .unload(key.app_id(), Some(key.environment()))
                .await
            {
                Ok(_) => {
                    outcome.unloaded += 1;
                    info!(
                        app_id = %key.app_id(),
                        environment = %key.environment(),
                        version_id = %installed.version_id,
                        "Stopped serving {} here: two complete assignment lists in a row left it \
                         out, so this node was running a version nobody asked it for. Its \
                         bundles were removed, so a restart will not bring it back",
                        key
                    );
                }
                Err(e) => {
                    outcome.unload_failed += 1;
                    // Still confirmed absent, so the next pass retries the
                    // unload rather than starting the two-answer count over.
                    absent_now.insert(key.clone());
                    error!(
                        app_id = %key.app_id(),
                        environment = %key.environment(),
                        version_id = %installed.version_id,
                        error = %e,
                        "{} is no longer assigned to this node and could not be stopped; it keeps \
                         serving, with its secrets, until this node is restarted",
                        key
                    );
                }
            }
        }
        self.absent_before = absent_now;

        Ok(outcome)
    }
}

/// Say what the control plane's answer changes, before anything acts on it.
///
/// At info, because a node that quietly loads and unloads applications on a
/// timer is a node whose behaviour nobody can reconstruct afterwards. Silence
/// when nothing changed, which is the normal case: the interesting line is the
/// one that appears.
///
/// The line says what this pass is about to do, not what the list implies:
/// an environment the list leaves out stops only once a second complete answer
/// has left it out too, so the first sighting is reported separately from the
/// stop. `absent_before` is what tells the two apart - it holds the
/// environments a previous complete answer already failed to name.
fn report_drift(
    assigned: &HashMap<AppKey, String>,
    running: &[(AppKey, InstalledVersion)],
    complete: bool,
    absent_before: &HashSet<AppKey>,
) {
    let running_versions: HashMap<&AppKey, &str> = running
        .iter()
        .map(|(key, installed)| (key, installed.version_id.as_str()))
        .collect();

    let mut added: Vec<String> = Vec::new();
    let mut changed: Vec<String> = Vec::new();
    for (key, version_id) in assigned {
        match running_versions.get(key) {
            None => added.push(format!("{key}={version_id}")),
            Some(running_version) if *running_version != version_id => {
                changed.push(format!("{key}: {running_version} -> {version_id}"))
            }
            Some(_) => {}
        }
    }

    // Only when the answer is the whole truth. On an incomplete one nothing is
    // removed and no absence is counted, so listing either would report a
    // change that is not going to happen.
    let mut removing: Vec<String> = Vec::new();
    let mut unnamed_once: Vec<String> = Vec::new();
    if complete {
        for (key, installed) in running {
            if assigned.contains_key(key) {
                continue;
            }
            let line = format!("{key}={}", installed.version_id);
            if absent_before.contains(key) {
                removing.push(line);
            } else {
                unnamed_once.push(line);
            }
        }
    }

    if added.is_empty() && changed.is_empty() && removing.is_empty() && unnamed_once.is_empty() {
        debug!(
            assigned = assigned.len(),
            "This node already runs exactly what the control plane assigns it"
        );
        return;
    }

    added.sort();
    changed.sort();
    removing.sort();
    unnamed_once.sort();

    info!(
        added = ?added,
        changed = ?changed,
        removing = ?removing,
        unnamed_once = ?unnamed_once,
        "The control plane's assignment list differs from what this node runs: {} to load, {} to \
         move to another version, {} to stop because a second complete answer in a row does not \
         name them, {} left out for the first time and therefore kept running for now",
        added.len(),
        changed.len(),
        removing.len(),
        unnamed_once.len()
    );

    let absent = removing.len() + unnamed_once.len();
    if absent > 0 && assigned.is_empty() {
        // The shape of a control plane that lost its catalog rather than of
        // somebody deleting things. Nothing stops on this answer alone, which
        // is the point - but if the next answer says the same, it will.
        warn!(
            absent,
            stopping = removing.len(),
            "The control plane assigns this node nothing at all while it runs {} environment(s). \
             {} of them stop serving here on this pass and the rest stop if the next complete \
             answer is the same. If that was not intended, the control plane's catalog is what \
             to look at",
            absent,
            removing.len()
        );
    }
}

/// Whether this node has told the control plane it exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistrationState {
    /// Not announced yet, or announced and then contact was lost. The next
    /// pass says it again.
    Pending,
    /// The control plane has it, until `refresh_at`.
    ///
    /// A registration is a lease, not a fact: deis-control drops a node that
    /// has not refreshed within its TTL, and a dropped node stops receiving
    /// deployment pushes and stops being picked for cron while still serving
    /// every request perfectly. Nothing looks wrong, which is why the deadline
    /// is carried here rather than the node registering once and assuming it
    /// is known forever.
    Done { refresh_at: Instant },
    /// This control plane has no registration endpoint, so there is nothing
    /// to keep trying. Said once and then left alone.
    NotSupported,
}

impl RegistrationState {
    /// How long the loop may sleep before this node must register again.
    ///
    /// `None` when no deadline applies: nothing to refresh, or a control plane
    /// that does not do registration at all.
    fn time_until_refresh(&self, now: Instant) -> Option<Duration> {
        match self {
            Self::Done { refresh_at } => Some(refresh_at.saturating_duration_since(now)),
            Self::Pending | Self::NotSupported => None,
        }
    }

    /// `nap`, shortened so the loop wakes in time to refresh the registration.
    ///
    /// Every sleep in the loop goes through this. The intervals it shortens
    /// are not the control plane's to know - a reconcile interval is
    /// configured here, a retry backoff is computed here - while the lease is
    /// the control plane's alone, so nothing but this stops the two from
    /// disagreeing. A node that oversleeps its lease is dropped from the fleet
    /// while still answering every request perfectly: it stops receiving
    /// deployment pushes and stops being picked for cron, and nothing about it
    /// looks wrong.
    fn cap(&self, nap: Duration) -> Duration {
        match self.time_until_refresh(Instant::now()) {
            Some(until) => nap.min(until),
            None => nap,
        }
    }
}

/// Keep this node matching what the control plane says, for as long as the
/// process runs.
///
/// Three things happen on every pass and in this order:
///
/// 1. **Register**, if the control plane has not been told about this node
///    since the last time contact was lost. A control plane that restarted
///    with an empty node list learns about this node again without anybody
///    touching either side.
/// 2. **Reconcile**: fetch the assignment list and make this node match it,
///    loading what is missing and stopping what is no longer assigned.
/// 3. **Report** readiness, so "serving what it was told to" and "serving
///    something older, or something nobody asked for" never look alike.
///
/// It returns only when there is no control plane configured at all, which is
/// the standalone case: the node loads what it has and calls itself ready,
/// because there is no list it could be missing and nobody to register with.
///
/// A pass is not the only thing that changes what this node can serve, so it
/// is not the only thing that moves what this node says about itself. Before
/// the first pass the deployment manager is handed the same readiness handle
/// ([`DeploymentManager::report_readiness_to`]), and every pushed deployment
/// and every unload moves it as it happens. Without that a node that had just
/// refused a push answered the edge `ready` for a whole reconcile interval -
/// and for the life of the process in the standalone case above, where this
/// function returns and no pass ever runs again.
///
/// A pass that cannot reach the control plane backs off and puts registration
/// back to pending: contact was lost, so nothing this node said before can be
/// assumed to still be known on the other side. A pass that *did* reach it and
/// still could not put the node right backs off on its own, separate ladder -
/// the control plane answered, so the node is not cut off, and climbing the
/// unreachable ladder for a local problem would make a node that is merely
/// degraded wait as long as one that is isolated.
///
/// No sleep in this loop ever runs past the moment the registration has to be
/// refreshed; every one of them goes through [`RegistrationState::cap`].
pub async fn run_control_loop(
    client: Option<AssignmentClient>,
    registrar: Option<NodeRegistrar>,
    deployer: Arc<DeploymentManager>,
    health: Arc<NodeHealth>,
    retry_interval: Duration,
    reconcile_interval: Duration,
    offline: Option<Arc<OfflineCache>>,
) {
    // Every event that changes what this node can serve has to move what it
    // says about itself, not only the passes below. The deployment manager is
    // where those events land - a pushed deployment taken or refused, an
    // application stopped - and this is the one place that holds both it and
    // the readiness, so this is where the two are joined.
    deployer.report_readiness_to(Arc::clone(&health));

    let Some(client) = client else {
        let recovered = deployer.recover_from_disk().await;
        info!(
            apps = recovered,
            "No control plane endpoint is configured (control.endpoint), so this node runs only \
             what is on its disk and what is pushed to it; it is ready"
        );
        health.mark_ready(recovered);
        return;
    };

    info!(url = client.url(), "Asking the control plane what to run");

    let mut recovered_locally = false;
    let mut ready_once = false;
    // Consecutive passes that could not reach the control plane.
    let mut unreachable: u32 = 0;
    // Consecutive passes that reached it and still left this node not running
    // what it was told to. Separate from `unreachable` because the two say
    // different things and have different cures.
    let mut degraded: u32 = 0;
    let mut registration = RegistrationState::Pending;
    let mut reconciler = Reconciler::new(&client, &deployer);

    // Spawn the background capability renewal loop.  It shares the same
    // machine-channel identity and endpoint, and reads the global token
    // registry that AppServices::connect() populates.
    let _renewal_metrics = client.machine_channel_client().map(|channel| {
        let on_renewed = offline.clone().map(|cache| {
            Arc::new(move || cache.record_capabilities())
                as crate::deployment::capability_renewer::OnRenewed
        });
        crate::deployment::capability_renewer::spawn(
            channel,
            client.node_id().to_string(),
            on_renewed,
        )
    });
    // Whether any pass has reached the control plane yet. The offline cache
    // is read only before that: once this process has heard from the control
    // plane, what it runs is newer than anything on disk.
    let mut synced_once = false;
    // Whether the control plane has answered this node with anything but a
    // list - a refusal above all. The offline cache is never read after
    // that: a node the control plane has refused, a revoked one for
    // instance, must not bring back on its own what it was last told to run.
    let mut answered_without_a_list = false;

    loop {
        if unreachable > 0 {
            let nap = registration.cap(retry_delay(retry_interval, unreachable));
            tokio::time::sleep(nap).await;
        }
        unreachable += 1;

        if let Some(registrar) = &registrar {
            registration = announce(registrar, registration).await;
        }

        match reconciler.once().await {
            Ok(outcome) => {
                synced_once = true;
                if let (Some(cache), Some(list)) = (&offline, reconciler.take_complete_list()) {
                    cache.record_sync(&list, deployer.secret_values());
                }
                let serving = outcome.serving();
                if outcome.failed > 0 || outcome.unload_failed > 0 {
                    // The control plane answered, so the node is not cut off;
                    // but it is not running exactly what it was told to.
                    health.mark_degraded(
                        format!(
                            "{} of {} assigned versions could not be loaded and {} could not be \
                             stopped",
                            outcome.failed, outcome.assigned, outcome.unload_failed
                        ),
                        serving,
                    );
                    // The unreachable ladder does not apply here: leaving it
                    // climbing would make a node that is merely degraded back
                    // off as though it had lost contact.
                    unreachable = 0;
                    degraded = degraded.saturating_add(1);
                    // Back round sooner than a whole reconcile interval -
                    // something is wrong and waiting that long leaves the node
                    // wrong for that long - but backing off, so an application
                    // whose bundle will never load does not produce an error
                    // every retry interval for ever. Capped, because a
                    // degraded node is still a registered one and must not
                    // sleep past its lease.
                    let nap = registration.cap(retry_delay(retry_interval, degraded));
                    tokio::time::sleep(nap).await;
                    continue;
                }

                degraded = 0;
                health.mark_ready(serving);
                if !ready_once {
                    ready_once = true;
                    info!(
                        apps = serving,
                        reconcile_interval_secs = reconcile_interval.as_secs(),
                        "This node now runs exactly what the control plane assigns it, and will \
                         re-check every {}s",
                        reconcile_interval.as_secs()
                    );
                }
                unreachable = 0;
                // Never sleep past the moment the registration has to be
                // refreshed. reconcile_interval is configurable and the
                // control plane's TTL is not this node's to choose, so a
                // 5 minute reconcile interval against a 90 second TTL would
                // otherwise drop this node out of the fleet between passes.
                tokio::time::sleep(registration.cap(reconcile_interval)).await;
            }
            Err(e) => {
                // Contact is lost, so whatever the control plane knew about
                // this node cannot be assumed to still hold.
                if matches!(registration, RegistrationState::Done { .. }) {
                    registration = RegistrationState::Pending;
                }

                let unreachable_now = e.is_unreachable();
                if !unreachable_now {
                    answered_without_a_list = true;
                    // Heard from, and refused or answered with something this
                    // node cannot act on. Nothing is restored from the
                    // offline cache or the disk on the strength of it.
                    warn!(
                        url = client.url(),
                        error = %e,
                        "The control plane answered without an assignment list. This node does \
                         not fall back to its offline cache for an answer like this - only for \
                         a control plane it cannot reach - and reports unhealthy until it is \
                         given a list"
                    );
                } else if !recovered_locally {
                    recovered_locally = true;
                    let restored = match (&offline, synced_once || answered_without_a_list) {
                        (Some(cache), false) => restore_offline(cache, &deployer).await,
                        _ => 0,
                    };
                    let recovered = restored + deployer.recover_from_disk().await;
                    warn!(
                        url = client.url(),
                        error = %e,
                        recovered,
                        "The control plane could not be reached at startup. This node is NOT \
                         running a list anyone confirmed: it loaded {} app(s) from its own disk \
                         and will keep retrying, starting at {}s and backing off. It reports \
                         unhealthy until the control plane answers",
                        recovered,
                        retry_interval.as_secs()
                    );
                } else {
                    warn!(
                        url = client.url(),
                        error = %e,
                        "Lost contact with the control plane; this node keeps serving what it \
                         has, stops picking up new versions, and reports unhealthy until the \
                         control plane answers again"
                    );
                }
                let apps = deployer.installed_app_count();
                // The class only, without `{e}`. The error carries the control
                // plane's own address, and this reason is stamped on
                // `x-deis-node-reason`, which the node answers on its public
                // data port to anyone who asks `/health` as `_system` -- so the
                // internal URL would leave the node to whoever could reach that
                // port. The address and the transport error are in the two
                // warn lines just above, which are the node's own log; the
                // header carries the class that tells an operator which of the
                // degraded causes this is, and nothing an outside caller has no
                // business reading.
                health.mark_degraded(
                    if unreachable_now {
                        "control plane unreachable"
                    } else {
                        "control plane answered without an assignment list"
                    },
                    apps,
                );
            }
        }
    }
}

/// Put back what the offline cache says this node was running, for a node
/// that starts while its control plane is unreachable.
///
/// The capabilities and the secret values go back first, so each deployment
/// is installed with its whole environment and its storage clients start out
/// holding the last capability this node was given. Each deployment is then
/// applied the way the list would apply it; its bundle has to be on this
/// node's disk already, because nothing can be downloaded. Returns how many
/// environments serve because of it.
async fn restore_offline(cache: &OfflineCache, deployer: &DeploymentManager) -> usize {
    let state = match cache.load() {
        Ok(Some(state)) => state,
        Ok(None) => return 0,
        Err(error) => {
            warn!(
                error = %error,
                "Not using this node's offline cache; it comes back only with what its bundle \
                 store records"
            );
            return 0;
        }
    };
    let capabilities = crate::deployment::capability_renewer::adopt(state.capabilities);
    deployer.restore_secret_values(
        state
            .secrets
            .into_iter()
            .map(|(deployment, values)| (deployment, values.into_iter().collect()))
            .collect(),
    );
    let assignments: Vec<Assignment> = match serde_json::from_str(&state.assignments_json) {
        Ok(assignments) => assignments,
        Err(error) => {
            warn!(
                error = %error,
                "The assignment list in this node's offline cache does not decode; nothing is \
                 restored from it"
            );
            return 0;
        }
    };

    let as_of = deployer.change_stamp();
    let mut restored = 0;
    for assignment in assignments {
        let app_id = assignment.app_id.clone();
        let environment = assignment.environment.clone();
        let version_id = assignment.version_id.clone();
        let action = assignment.deployment_action;
        let order = match assignment.into_order() {
            Ok(order) => order,
            Err(error) => {
                error!(
                    app_id = %app_id,
                    environment = %environment,
                    error = %error,
                    "Skipping an assignment in the offline cache that does not make an order"
                );
                continue;
            }
        };
        let applied = match action {
            AssignmentDeploymentAction::Prepare => {
                deployer.prepare_from_assignment(order, as_of).await
            }
            AssignmentDeploymentAction::Activate => {
                deployer.apply_from_assignment(order, as_of).await
            }
        };
        match applied {
            Ok(Applied::Installed { .. } | Applied::AlreadyRunning) => restored += 1,
            Ok(Applied::Superseded { .. }) => {}
            Err(error) => error!(
                app_id = %app_id,
                environment = %environment,
                version_id = %version_id,
                error = %error,
                "Could not restore {} from the offline cache; it is not serving here until the \
                 control plane answers",
                app_id
            ),
        }
    }
    warn!(
        restored,
        capabilities,
        written_at = state.written_at,
        "The control plane is unreachable, so this node restored {} environment(s) from its \
         offline cache, as the last sync left them, with their secrets and its last \
         capabilities. It serves them for at most 24 hours from that sync without hearing from \
         the control plane",
        restored
    );
    restored
}

/// Announce this node, unless it is already announced or this control plane
/// does not do registration.
///
/// A failure here is reported and left pending rather than propagated: the
/// assignment fetch on the very next line is what decides whether this node
/// can work at all, and a node the control plane does not know about still
/// serves the traffic it is already handling.
async fn announce(registrar: &NodeRegistrar, state: RegistrationState) -> RegistrationState {
    let now = Instant::now();
    match state {
        // Registration is a lease. Letting it lapse costs deployment pushes
        // and cron ticks silently, so the refresh is not optional and not
        // something to skip because this node registered once already.
        RegistrationState::Done { refresh_at } if now < refresh_at => return state,
        RegistrationState::NotSupported => return state,
        RegistrationState::Done { .. } | RegistrationState::Pending => {}
    }

    let first_time = !matches!(state, RegistrationState::Done { .. });

    match registrar.register().await {
        Ok(Registered::Accepted { refresh_after }) => {
            let node = registrar.node();
            if first_time {
                info!(
                    url = registrar.url(),
                    node_id = %node.node_id,
                    role = node.role,
                    version = node.version,
                    host = %node.host,
                    port = node.port,
                    refresh_after_secs = refresh_after.as_secs(),
                    "Registered this node with the control plane as {} at {}; it now receives \
                     deployment pushes and can be picked for cron, and re-registers every {}s to \
                     keep that",
                    node.node_id,
                    node.address(),
                    refresh_after.as_secs()
                );
            } else {
                debug!(
                    url = registrar.url(),
                    refresh_after_secs = refresh_after.as_secs(),
                    "Refreshed this node's registration"
                );
            }
            RegistrationState::Done {
                refresh_at: Instant::now() + refresh_after,
            }
        }
        Ok(Registered::NotSupported) => {
            warn!(
                url = registrar.url(),
                "This control plane has no node registration endpoint, so it knows about this \
                 node only if an operator wrote it into the control plane's own configuration. \
                 Everything else works; what is missing is the control plane addressing a node \
                 it was never told about - cron ticks and the dashboard. Not retried"
            );
            RegistrationState::NotSupported
        }
        Err(e) => {
            warn!(
                url = registrar.url(),
                error = %e,
                was_registered = !first_time,
                "Could not register this node with the control plane; it will try again on the \
                 next pass. Until it succeeds the control plane may not know this node exists, \
                 so it will not be picked for a cron tick and will not appear in the dashboard"
            );
            RegistrationState::Pending
        }
    }
}

/// How long to wait before retry number `attempt`.
///
/// Doubles up to a ceiling. A control plane that is down for an hour would
/// otherwise be asked 240 times and log 240 failures at the configured 15
/// second interval; backing off keeps the node trying without turning an
/// outage into a log flood or into load on a control plane that is already
/// struggling.
fn retry_delay(base: Duration, attempt: u32) -> Duration {
    /// Ceiling as a multiple of the configured interval.
    const MAX_MULTIPLIER: u32 = 20;

    let multiplier = 1u32
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(MAX_MULTIPLIER)
        .min(MAX_MULTIPLIER);
    base.saturating_mul(multiplier)
}

#[cfg(test)]
#[path = "assignments_test.rs"]
mod assignments_test;
