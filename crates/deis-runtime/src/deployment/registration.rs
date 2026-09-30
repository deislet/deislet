//! Telling the control plane this node exists.
//!
//! Principle 8 got the node asking what to run; it did not get the control
//! plane knowing who was asking. Until now the only way a node appeared in the
//! control plane's list was for an operator to write it into the control
//! plane's configuration file, which means adding a node was an edit on the
//! other side, a node that moved kept its old entry, and the dashboard's idea
//! of the fleet was whatever the file said rather than what was running.
//!
//! So the node says so itself, on the same machine channel it already uses to
//! fetch its assignments ([`super::assignments`]).
//!
//! # One door, and why it is one
//!
//! Registering is how a node becomes a delivery target: what registers is what
//! gets deployment pushes, cron occurrences and, on its next assignment fetch,
//! its environments' decrypted secrets. So who is allowed to say it matters as
//! much as what is said.
//!
//! [`NodeRegistrar::over_machine_channel`] is the only way to say it. The
//! connection proved a name before the call was read, the control plane
//! refuses a registration whose `node_id` is not that name, and the node is
//! recorded under the name its certificate carries.
//!
//! There was a second door, and it is worth remembering what was wrong with
//! it rather than only that it is shut. A node registered over HTTP on the
//! control plane's human listener, guarded by a value the whole installation
//! held a copy of. That value could say "a machine of this installation is
//! calling" and could not say which one, so the control plane had to take the
//! caller's word for the name: any holder could register any name and be
//! handed that name's work, including that name's decrypted secrets on the
//! next fetch. Closing it was not a matter of guarding it better - no
//! comparison against a shared string could have answered the question that
//! mattered.
//!
//! The transport is decided once at startup by
//! [`NodeRegistrar::from_config`]. A node without a configured
//! `[machine_identity]` refuses to start rather than silently failing to
//! register on a path that is no longer served. What is said is the same
//! either way: the same body, with the same fields, read by the same code
//! on the other side.
//!
//! Registration is *not* a precondition for serving. A node that cannot
//! register still fetches its assignments and still serves them; what is lost
//! is the control plane's knowledge of it, which costs cron ticks and the
//! dashboard, not traffic. That is why every failure here is reported and
//! retried rather than fatal, and why a control plane that does not offer the
//! endpoint at all is said out loud once instead of every interval.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use deis_identity::{Dialler, MachineIdentity};
use deis_wire::machine::machine_channel_client::MachineChannelClient;
use deis_wire::machine::{DeregisterNodeRequest, RegisterNodeRequest};
use serde::{Deserialize, Serialize};
use tonic::transport::{Channel, Endpoint};
use tracing::{debug, error, warn};

/// What a node says about itself.
///
/// Only things the node actually knows. `host` and `port` are where this
/// process believes it can be reached: the control plane dials a node for
/// deployments and cron ticks, so an address it cannot use makes those fail
/// rather than making registration fail.
#[derive(Debug, Clone)]
pub struct NodeRegistration {
    /// The id this node answers to, the same one its assignment list is keyed
    /// by. Two processes sharing an id would ask for and report on one list.
    ///
    /// On the machine channel it is also a claim the control plane can refuse:
    /// it must equal the name the certificate proved, so a node can only ever
    /// register as itself.
    pub node_id: String,
    /// Which kind of node this is. The control plane manages deis-proxy nodes
    /// on the same fleet and does entirely different things with them.
    pub role: &'static str,
    /// This binary's version, so a fleet mid-upgrade is visible as one.
    pub version: &'static str,
    /// Hostname, as this process sees it.
    pub host: String,
    /// The port serving both HTTP traffic and the gRPC management interface.
    pub port: u16,
}

/// The body of a registration, in the shape deis-control reads.
///
/// The control plane asks for one dialable address rather than a host and a
/// port, and deliberately does not infer it from the connection: the socket a
/// registration arrives on is not the socket management calls go to, and a
/// node behind a forwarder knows the address that reaches it while the
/// connection does not.
///
/// The other half is `deis_control::api::registration::RegisterRequest`. The
/// two are kept in step by the shared path in [`deis_wire::conventions`] and
/// by the tests below, which serve the control plane's real route and method
/// rather than a convenient one.
///
/// One body, both doors. The machine channel carries this same JSON in
/// `RegisterNodeRequest::registration_json` and the control plane decodes it
/// into the same type, so nothing about what a node says depends on which way
/// it said it - and a field added here reaches both sides at once instead of
/// reaching one of them a release later.
#[derive(Debug, Clone, Serialize)]
struct RegisterBody {
    address: String,
    version: &'static str,
    /// Where this node serves its management plane over mutual TLS, when it
    /// serves one.
    ///
    /// Reported beside `address` and not instead of it: they are two sockets,
    /// and the control plane dials the authenticated one only when it also has
    /// an identity of its own to prove. Omitted entirely when this node bound
    /// no such listener, because an address that nothing answers on is worse
    /// than no address - the control plane would dial it and report the node
    /// unreachable.
    ///
    /// It is reported on the machine channel too, and has to be: registering
    /// over that channel is exactly the case where the control plane can dial
    /// this node back by certificate, and this field is the only way it learns
    /// the socket to dial. A registration that dropped it there would leave
    /// both ends able to prove a name to each other and the control plane
    /// still driving this node through the plaintext door.
    #[serde(skip_serializing_if = "Option::is_none")]
    machine_address: Option<String>,
}

/// What the control plane answers a registration with.
///
/// Only the refresh interval is read. The rest is logged by the control plane
/// itself, and a node that acted on `source` or `outcome` would be deciding
/// something the control plane already decided. That is true of both doors:
/// the machine channel carries the identical JSON answer, so the same one
/// field is read from it and the rest is left where it was decided.
#[derive(Debug, Clone, Deserialize)]
struct RegisterAnswer {
    /// Seconds after which this node must register again or be dropped.
    #[serde(default)]
    refresh_after_seconds: u64,
}

impl NodeRegistration {
    /// The role string a deis-runtime node registers under.
    ///
    /// "runtime", not "worker": a worker is a Cloudflare Worker, which is the
    /// thing this node *runs*, and a node that called itself one would make
    /// every log line about the fleet ambiguous.
    pub const RUNTIME_ROLE: &'static str = "deis-runtime";

    /// Where the control plane should dial this node, as it has to send it.
    pub fn address(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// Describe this process.
    pub fn of_this_node(node_id: impl Into<String>, port: u16) -> Self {
        Self {
            node_id: node_id.into(),
            role: Self::RUNTIME_ROLE,
            version: env!("CARGO_PKG_VERSION"),
            host: hostname::get()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string(),
            port,
        }
    }

    /// Announce a routable address instead of the machine's hostname when the
    /// configuration names one.
    ///
    /// A hostname is only an address where something resolves it, and the
    /// control plane usually cannot: a node on a LAN announcing `server94`
    /// hands it a name that routes nowhere. The data port's own bind address
    /// is used when it is a specific one; a node bound to every interface
    /// (`0.0.0.0`) falls back to the address its management plane listens on,
    /// which is one of this same machine's addresses and the one the operator
    /// already chose for the control plane to reach it at. Only when neither
    /// names an address does the hostname stand.
    pub fn advertised_at(mut self, data_ip: IpAddr, management_listen: Option<&str>) -> Self {
        let routable = |ip: IpAddr| !ip.is_unspecified() && !ip.is_loopback();
        let chosen = Some(data_ip).filter(|ip| routable(*ip)).or_else(|| {
            management_listen
                .and_then(|listen| listen.parse::<SocketAddr>().ok())
                .map(|addr| addr.ip())
                .filter(|ip| routable(*ip))
        });
        if let Some(ip) = chosen {
            self.host = match ip {
                IpAddr::V4(v4) => v4.to_string(),
                IpAddr::V6(v6) => format!("[{v6}]"),
            };
        }
        self
    }
}

/// What came of one registration attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registered {
    /// The control plane took it, and wants to hear from this node again
    /// within `refresh_after`. A registration is a lease: stop refreshing it
    /// and the control plane drops this node from the fleet.
    Accepted { refresh_after: Duration },
    /// This control plane has no registration endpoint. Older than this
    /// feature, so it knows its nodes from its own configuration file and
    /// there is nothing to retry.
    NotSupported,
}

/// What came of telling the control plane this node is going away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deregistered {
    /// The entry is gone: no further deployment push or cron fire is aimed at
    /// this node until it registers again.
    Removed,
    /// The control plane answered and the entry stays. Two cases wear this,
    /// and a node that is stopping does the same thing about both: an id named
    /// in the control plane's own `deis_runtime_nodes` is not removed by a
    /// node asking, because the file says it exists; and an id the control
    /// plane never had is already as absent as deregistering could make it.
    Kept,
    /// This control plane has no deregistration endpoint. Nothing to retry,
    /// and nothing to do about it here: the registration lapses on its own
    /// when the lease expires, which is the pre-deregistration behaviour.
    NotSupported,
}

/// Why a registration did not land.
#[derive(Debug)]
pub enum RegistrationError {
    Transport {
        url: String,
        reason: String,
    },
    Status {
        url: String,
        code: u16,
    },
    /// This node asked to register as a machine it is not.
    ///
    /// Its own class rather than a refusal code, because it is neither a
    /// network fault nor the control plane being unwell: the connection proved
    /// one name and the call claimed another, which is a certificate that was
    /// issued for the wrong machine or a `node_id` that does not match the one
    /// it was issued for. Nothing about retrying it can help, and an operator
    /// reading a bare "permission denied" would have no way to see which two
    /// names disagreed - so both are carried here and both are in the message.
    WrongMachine {
        url: String,
        /// The name this node asked to register as: its configured `node_id`.
        asked_for: String,
        /// The name its certificate proved on the connection.
        proving: String,
        /// What the control plane said, which names both halves as it sees
        /// them.
        reason: String,
    },
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport { url, reason } => {
                write!(f, "could not reach the control plane at {url}: {reason}")
            }
            Self::Status { url, code } => {
                write!(f, "the control plane answered HTTP {code} for {url}")
            }
            Self::WrongMachine {
                url,
                asked_for,
                proving,
                reason,
            } => write!(
                f,
                "the control plane at {url} refused to register this node as {asked_for}, because \
                 the connection proved {proving}: {reason}"
            ),
        }
    }
}

impl std::error::Error for RegistrationError {}

/// How often to re-register when the control plane does not say.
///
/// Deliberately well under deis-control's default 90 second TTL: refreshing
/// too often costs one request, refreshing too slowly costs the node its place
/// in the fleet, and only one of those is recoverable without noticing.
const DEFAULT_REFRESH: Duration = Duration::from_secs(20);

/// The paths `Register` and `Deregister` are addressed at on the wire. gRPC
/// over HTTP/2 is `POST /<package>.<service>/<method>`, so these are the real
/// paths and not labels invented for the log: an operator can match them
/// against the control plane's own access log.
const REGISTER_PATH: &str = "/deis.machine.MachineChannel/Register";
const DEREGISTER_PATH: &str = "/deis.machine.MachineChannel/Deregister";

/// Registers this node with the control plane.
///
/// One statement, one door: registration proves the node's SPIFFE name
/// via mutual TLS on the machine channel. See the module documentation
/// for what the door proves and why there is no second.
pub struct NodeRegistrar {
    transport: Transport,
    /// What every log line and every error names for a registration — the
    /// machine channel endpoint followed by the RPC's path, which is
    /// literally what goes on the wire over HTTP/2, so an operator can
    /// match this against the control plane's access log.
    url: String,
    /// See [`NodeRegistrar::new`].
    machine_address: Option<String>,
    node: NodeRegistration,
}

/// How this node registers, and what it proves while registering.
enum Transport {
    /// `deis.machine.MachineChannel/Register` over mutual TLS.
    Machine {
        /// Built once, at startup, and kept for the life of the process. A
        /// node re-registers on a timer - the registration is a lease, so this
        /// call happens for as long as the process runs - and building a
        /// channel per registration would pay a fresh TCP connection and a
        /// fresh TLS handshake every lease period, on every node, for no gain.
        /// It is a channel and not a connection: it dials when it is first
        /// used and redials after a broken one, and the dialler behind it
        /// resolves the certificate per connection, so a renewed certificate
        /// reaches the wire without this being rebuilt.
        client: MachineChannelClient<Channel>,
        /// Kept so a refusal can name what this node proved, which is the
        /// other half of diagnosing one - and read through the identity rather
        /// than copied at startup, so it says what the connection actually
        /// presented after a renewal.
        identity: Arc<MachineIdentity>,
        /// The other RPC's path, for the lines and errors a deregistration
        /// produces.
        deregister_url: String,
    },
}

impl NodeRegistrar {
    /// Register over the machine channel, proving `identity` and refusing to
    /// talk to anything that cannot prove it is `control_identity`.
    ///
    /// No credential is carried and there is none to carry: the connection has
    /// already said which machine is registering, and the control plane
    /// refuses a registration naming anything else.
    pub fn over_machine_channel(
        machine_endpoint: &str,
        node: NodeRegistration,
        timeout: Duration,
        identity: Arc<MachineIdentity>,
        control_identity: deis_identity::SpiffeId,
        machine_address: Option<String>,
    ) -> anyhow::Result<Self> {
        let endpoint = Endpoint::from_shared(machine_endpoint.to_string())
            .map_err(|e| {
                anyhow::anyhow!(
                    "control.machine_endpoint {machine_endpoint:?} is not an address this node \
                     can dial: {e}"
                )
            })?
            // The same ceiling `[control] request_timeout_secs` puts on the
            // HTTP registration, applied to the same statement: one that hangs
            // is one this node stops waiting on and says again next pass.
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

        // Lazily, so a control plane that is not up yet is a failed
        // registration that is retried rather than a node that refuses to
        // start. This matches what the HTTP client does: neither one connects
        // while it is being built.
        let client = MachineChannelClient::new(endpoint.connect_with_connector_lazy(dialler));

        Ok(Self::machine(
            client,
            identity,
            node,
            machine_endpoint,
            machine_address,
        ))
    }

    /// The machine transport, however the channel under it was obtained.
    ///
    /// Both addresses are derived here, from one endpoint, so the two RPC
    /// paths cannot be spelled differently by two callers.
    fn machine(
        client: MachineChannelClient<Channel>,
        identity: Arc<MachineIdentity>,
        node: NodeRegistration,
        machine_endpoint: &str,
        machine_address: Option<String>,
    ) -> Self {
        let base = machine_endpoint.trim_end_matches('/');
        Self {
            transport: Transport::Machine {
                client,
                identity,
                deregister_url: format!("{base}{DEREGISTER_PATH}"),
            },
            url: format!("{base}{REGISTER_PATH}"),
            machine_address,
            node,
        }
    }

    /// The machine transport over a channel somebody else built.
    ///
    /// For the harness. A test can put a real `MachineChannel` server on a
    /// plain socket and drive this client against it, which is what covers the
    /// body, the answer and the error mapping without a certificate authority
    /// in the loop; the handshake itself is covered where it lives, against the
    /// control plane's own listener. The only difference from
    /// [`Self::over_machine_channel`] is who dialled.
    #[cfg(test)]
    pub(crate) fn over_channel(
        channel: Channel,
        node: NodeRegistration,
        identity: Arc<MachineIdentity>,
        machine_endpoint: &str,
        machine_address: Option<String>,
    ) -> Self {
        Self::machine(
            MachineChannelClient::new(channel),
            identity,
            node,
            machine_endpoint,
            machine_address,
        )
    }

    /// The registrar this configuration calls for, or `None` when this node
    /// was told about no control plane at all and so announces itself to
    /// nobody.
    ///
    /// `identity` is what `main` loaded from `[machine_identity]`. The two
    /// halves are required together by `RuntimeConfig::validate`, so the
    /// mismatched cases below are a startup that skipped that check rather
    /// than anything an operator can write; they are refused instead of
    /// quietly falling back, because falling back would mean a node that was
    /// configured for mutual TLS going on announcing itself with the fleet
    /// key, with nothing in any log to say so. It would also be recorded by
    /// that key rather than by name, and being recorded by name is what
    /// decides whether the control plane ever dials this node by certificate.
    pub fn from_config(
        config: &crate::config::RuntimeConfig,
        node: NodeRegistration,
        identity: Option<Arc<MachineIdentity>>,
    ) -> anyhow::Result<Option<Self>> {
        let timeout = Duration::from_secs(config.control.request_timeout_secs);
        // Where this node serves its own management plane under mutual TLS,
        // when it serves one. Read from the same table as the identity because
        // it is the same decision: a node that bound that listener is a node
        // the control plane may drive by certificate.
        let machine_address = config
            .machine_identity
            .as_ref()
            .and_then(|identity| identity.management_listen.clone())
            .map(|listen| format!("https://{listen}"));

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
                    node,
                    timeout,
                    identity,
                    control_identity,
                    machine_address,
                )?))
            }
            (Some(machine_endpoint), None) => Err(anyhow::anyhow!(
                "control.machine_endpoint is {machine_endpoint:?} but this node has no machine \
                 identity loaded, so it could not complete a handshake with that listener"
            )),
            // An identity and nowhere to present it: a node that serves its
            // management plane and registers with nobody, which `validate`
            // accepts only alongside a management listener. `deis dev` runs
            // exactly that - the thing that started the node is the only
            // thing that will ever drive it, and it needs no fleet list to
            // find it.
            (None, Some(_)) => Ok(None),
            // Without an endpoint there is nobody to register with, which is
            // the standalone case: the control loop says so and reports the
            // node ready on what it finds on its own disk.
            (None, None) => Ok(None),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Where a deregistration is addressed.
    fn deregister_url(&self) -> &str {
        let Transport::Machine { deregister_url, .. } = &self.transport;
        deregister_url
    }

    pub fn node(&self) -> &NodeRegistration {
        &self.node
    }

    /// How this node registers, for the one startup line that says so.
    pub fn transport_name(&self) -> &'static str {
        "the machine channel, over mutual TLS"
    }

    /// Announce this node, or refresh what it already said.
    ///
    /// Safe to call again, and meant to be: registering is a statement of what
    /// this node is rather than a request to create something, and the control
    /// plane treats it as a lease that expires. `PUT` says exactly that, and
    /// the RPC of the same name means the same thing.
    pub async fn register(&self) -> Result<Registered, RegistrationError> {
        let body = RegisterBody {
            address: self.node.address(),
            machine_address: self.machine_address.clone(),
            version: self.node.version,
        };

        let Transport::Machine {
            client, identity, ..
        } = &self.transport;
        self.register_over_machine_channel(client, identity, &body)
            .await
    }

    async fn register_over_machine_channel(
        &self,
        client: &MachineChannelClient<Channel>,
        identity: &MachineIdentity,
        body: &RegisterBody,
    ) -> Result<Registered, RegistrationError> {
        let registration_json = self.encode(body)?;

        // tonic's generated client takes `&mut self` per call, and it is a
        // handle rather than a connection: this clone shares the one channel
        // built at startup, and with it whatever connection that channel
        // already has open.
        let mut client = client.clone();
        let answer = client
            .register(RegisterNodeRequest {
                // The claim the control plane checks against the name the
                // connection proved. It is sent as a field of its own and not
                // read out of the JSON body, because it is the one thing in
                // this call the control plane refuses rather than records.
                node_id: self.node.node_id.clone(),
                registration_json,
            })
            .await;

        let answer = match answer {
            Ok(answer) => answer.into_inner(),
            Err(status) if status.code() == tonic::Code::Unimplemented => {
                // A control plane whose machine channel predates this RPC, or
                // one built without it. The same permanent answer 404/405/501
                // is on the HTTP route, and treated the same way: said once,
                // never retried.
                return Ok(Registered::NotSupported);
            }
            Err(status) => {
                return Err(machine_failure(
                    &self.url,
                    &self.node.node_id,
                    &identity.id().to_string(),
                    &status,
                ));
            }
        };

        let refresh_after = refresh_interval(
            &self.url,
            serde_json::from_str::<RegisterAnswer>(&answer.registration_json)
                .map_err(|e| e.to_string()),
        );
        debug!(
            url = %self.url,
            refresh_after_secs = refresh_after.as_secs(),
            "The control plane accepted this node's registration"
        );
        Ok(Registered::Accepted { refresh_after })
    }

    /// Say this node is going away.
    ///
    /// Said on the door it registered on, because that is where the entry it
    /// is asking to drop was made. Without it the control plane keeps this
    /// node on the fleet list until the lease expires and spends that window
    /// pushing deployments and firing cron at a process that has gone.
    ///
    /// Not fatal, and not retried in a loop: a node that is stopping has a
    /// bounded amount of time to say so, and the lease expiring is the
    /// fallback that was there before anything said anything.
    pub async fn deregister(&self) -> Result<Deregistered, RegistrationError> {
        match &self.transport {
            Transport::Machine {
                client, identity, ..
            } => self.deregister_over_machine_channel(client, identity).await,
        }
    }

    async fn deregister_over_machine_channel(
        &self,
        client: &MachineChannelClient<Channel>,
        identity: &MachineIdentity,
    ) -> Result<Deregistered, RegistrationError> {
        let url = self.deregister_url();
        let mut client = client.clone();
        match client
            .deregister(DeregisterNodeRequest {
                node_id: self.node.node_id.clone(),
            })
            .await
        {
            Ok(answer) => {
                // `removed` is false for a node the control plane's own
                // configuration file names, which is the same thing the HTTP
                // route answers 409 for and means the same here: the entry
                // stays and pushes keep being aimed at it.
                let removed = answer.into_inner().removed;
                debug!(
                    url = %url,
                    removed,
                    "The control plane took this node's deregistration"
                );
                Ok(if removed {
                    Deregistered::Removed
                } else {
                    Deregistered::Kept
                })
            }
            Err(status) if status.code() == tonic::Code::Unimplemented => {
                Ok(Deregistered::NotSupported)
            }
            Err(status) => Err(machine_failure(
                url,
                &self.node.node_id,
                &identity.id().to_string(),
                &status,
            )),
        }
    }

    /// The body, as the RPC carries it.
    ///
    /// Encoding three owned strings cannot fail. It is still not unwrapped:
    /// this runs on a timer for the life of the process, and a panic in the
    /// registration loop would take down a node that is serving traffic
    /// perfectly well over something that costs it only its place in the fleet
    /// list. Reported as the call not having gone out, which is what it is.
    fn encode(&self, body: &RegisterBody) -> Result<String, RegistrationError> {
        serde_json::to_string(body).map_err(|e| RegistrationError::Transport {
            url: self.url.clone(),
            reason: format!("this node's own registration could not be encoded: {e}"),
        })
    }
}

/// How long until this node has to register again.
///
/// One reading for both doors. The answer is the identical JSON either way and
/// exactly one field of it is read, so a node cannot end up on one cadence
/// over HTTP and another over the machine channel - and the two ways of not
/// getting an interval, an answer that omits it and an answer that cannot be
/// read, land on the same floor because the consequence of both is the same:
/// never refreshing, and being dropped from the fleet without anything looking
/// wrong.
fn refresh_interval(url: &str, answer: Result<RegisterAnswer, String>) -> Duration {
    match answer {
        // The control plane names the refresh interval; it owns the TTL that
        // interval has to stay inside. A node that picked its own would be
        // guessing at when it gets dropped.
        Ok(answer) if answer.refresh_after_seconds > 0 => {
            Duration::from_secs(answer.refresh_after_seconds)
        }
        Ok(_) => {
            warn!(
                url = %url,
                fallback_secs = DEFAULT_REFRESH.as_secs(),
                "The control plane accepted this registration but did not say how often to \
                 refresh it, so this node falls back to {}s. If the control plane's registration \
                 TTL is shorter than that, this node will be dropped from the fleet between \
                 refreshes and stop receiving deployments and cron",
                DEFAULT_REFRESH.as_secs()
            );
            DEFAULT_REFRESH
        }
        Err(e) => {
            warn!(
                url = %url,
                error = %e,
                fallback_secs = DEFAULT_REFRESH.as_secs(),
                "The control plane accepted this registration but its answer could not be read, \
                 so this node falls back to refreshing every {}s. Same consequence as above if \
                 that is slower than the control plane's TTL",
                DEFAULT_REFRESH.as_secs()
            );
            DEFAULT_REFRESH
        }
    }
}

/// Put a failed RPC into the taxonomy an operator already diagnoses this node
/// from.
///
/// A fleet that will not form is diagnosed from these messages, so the classes
/// are kept apart rather than flattened into one transport failure:
///
/// * `PERMISSION_DENIED` is [`RegistrationError::WrongMachine`]: the
///   connection proved one name and the registration claimed another, or the
///   node it proved is revoked. Nothing is wrong with the network or the
///   control plane, and retrying cannot help until a certificate or a
///   `node_id` changes or the revocation is lifted, so it says both names and
///   the control plane's reason.
/// * `ALREADY_EXISTS` is the id-held-by-another-machine case, which the HTTP
///   route answers 409 for. It is carried as that same 409 so that one
///   condition reads as one thing in a log whichever door it arrived at: two
///   machines are configured with one node id, and one of them has to be given
///   another.
/// * a call that never reached a control plane willing to answer - a refused
///   connection, a handshake that failed, a deadline, a cancelled call - is
///   `Transport`, which is the same thing the HTTP client reports when the
///   socket does not open.
/// * anything else the control plane answered with a status is `Status`,
///   carrying the HTTP code gRPC's own specification maps that code onto, so
///   that "refused this node's credential" reads as 401 whichever door it came
///   through.
///
/// `UNIMPLEMENTED` never reaches here: it means this control plane does not do
/// registration at all, which is an answer rather than a failure, and both
/// callers turn it into one before asking for a mapping.
fn machine_failure(
    url: &str,
    asked_for: &str,
    proving: &str,
    status: &tonic::Status,
) -> RegistrationError {
    use tonic::Code;

    match status.code() {
        Code::PermissionDenied => {
            error!(
                url = %url,
                asked_for = %asked_for,
                proving = %proving,
                message = status.message(),
                "The control plane refused to register this node under a name it did not prove: \
                 the connection proved one machine and the registration claimed another. This \
                 node is not on the fleet list, so it is handed no deployment push and no cron \
                 occurrence, though it still serves what it already has. Either node_id is wrong \
                 for the certificate in [machine_identity], the certificate is the wrong \
                 machine's, or an operator has revoked this node on the control plane - the \
                 message says which - and it stays off the fleet until that is lifted"
            );
            RegistrationError::WrongMachine {
                url: url.to_string(),
                asked_for: asked_for.to_string(),
                proving: proving.to_string(),
                reason: status.message().to_string(),
            }
        }
        Code::Unauthenticated => {
            error!(
                url = %url,
                proving = %proving,
                message = status.message(),
                "The control plane would not authenticate this node on the machine channel, so \
                 this node never joins the fleet list and is handed no deployment push and no \
                 cron occurrence. Check that the certificate in [machine_identity] is one this \
                 fleet's trust root signed and that it has not expired"
            );
            RegistrationError::Status {
                url: url.to_string(),
                code: 401,
            }
        }
        Code::AlreadyExists => {
            error!(
                url = %url,
                asked_for = %asked_for,
                message = status.message(),
                "Another machine already holds this node id on the control plane, so this node \
                 stays off the fleet list and is handed no deployment push and no cron \
                 occurrence. Two machines are configured with one node_id; one of them has to be \
                 given another"
            );
            RegistrationError::Status {
                url: url.to_string(),
                code: 409,
            }
        }
        // No answer from a serving control plane, which is what a refused
        // socket is on the HTTP side. The message is kept because it is where
        // a handshake failure says which certificate it did not like.
        Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled | Code::Unknown => {
            RegistrationError::Transport {
                url: url.to_string(),
                reason: format!("{}: {}", status.code(), status.message()),
            }
        }
        code => RegistrationError::Status {
            url: url.to_string(),
            code: match code {
                // A registration the control plane could not read, or an
                // address it will not accept: the same 400 the HTTP route
                // answers for both.
                Code::InvalidArgument | Code::FailedPrecondition | Code::OutOfRange => 400,
                // Not "no such route" - gRPC says that with UNIMPLEMENTED,
                // which is handled before this is reached.
                Code::NotFound => 404,
                Code::Aborted => 409,
                Code::ResourceExhausted => 429,
                Code::Unimplemented => 501,
                // Internal, DataLoss and anything a later gRPC version adds.
                _ => 500,
            },
        },
    }
}

#[cfg(test)]
#[path = "registration_test.rs"]
mod registration_test;
