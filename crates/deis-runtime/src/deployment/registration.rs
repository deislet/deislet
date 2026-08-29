//! Telling the control plane this node exists.
//!
//! Principle 8 got the node asking what to run; it did not get the control
//! plane knowing who was asking. Until now the only way a node appeared in the
//! control plane's list was for an operator to write it into the control
//! plane's configuration file, which means adding a node was an edit on the
//! other side, a node that moved kept its old entry, and the dashboard's idea
//! of the fleet was whatever the file said rather than what was running.
//!
//! So the node says so itself, on the same channel and with the same
//! credential it already uses to fetch its assignments
//! ([`super::assignments`]): the control plane's node endpoints are guarded by
//! `node_preshared_keys`, independent of the human API's `api_auth_enabled`,
//! and registering is a node action like any other.
//!
//! Registration is *not* a precondition for serving. A node that cannot
//! register still fetches its assignments and still serves them; what is lost
//! is the control plane's knowledge of it, which costs cron ticks and the
//! dashboard, not traffic. That is why every failure here is reported and
//! retried rather than fatal, and why a control plane that does not offer the
//! endpoint at all is said out loud once instead of every interval.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::credential::NodeCredential;

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
#[derive(Debug, Clone, Serialize)]
struct RegisterBody {
    address: String,
    version: &'static str,
}

/// What the control plane answers a registration with.
///
/// Only the refresh interval is read. The rest is logged by the control plane
/// itself, and a node that acted on `source` or `outcome` would be deciding
/// something the control plane already decided.
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

/// Why a registration did not land.
#[derive(Debug)]
pub enum RegistrationError {
    Transport { url: String, reason: String },
    Status { url: String, code: u16 },
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

/// Registers this node with the control plane.
pub struct NodeRegistrar {
    client: reqwest::Client,
    url: String,
    credential: Option<NodeCredential>,
    node: NodeRegistration,
}

impl NodeRegistrar {
    /// `credential` is the node key from `[control] secret`, the same one the
    /// assignment fetch presents. Without it the request goes out anonymous,
    /// which a control plane with `node_preshared_keys` configured refuses.
    pub fn new(
        endpoint: &str,
        node: NodeRegistration,
        timeout: Duration,
        credential: Option<NodeCredential>,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder().timeout(timeout).build()?;
        let url = format!(
            "{}{}",
            endpoint.trim_end_matches('/'),
            deis_wire::conventions::node_registration_path(&super::assignments::urlencode(
                &node.node_id
            ))
        );
        Ok(Self {
            client,
            url,
            credential,
            node,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn node(&self) -> &NodeRegistration {
        &self.node
    }

    /// Announce this node, or refresh what it already said.
    ///
    /// Safe to call again, and meant to be: registering is a statement of what
    /// this node is rather than a request to create something, and the control
    /// plane treats it as a lease that expires. `PUT` says exactly that.
    pub async fn register(&self) -> Result<Registered, RegistrationError> {
        let body = RegisterBody {
            address: self.node.address(),
            version: self.node.version,
        };
        let mut request = self.client.put(&self.url).json(&body);
        if let Some(credential) = &self.credential {
            request = request.header(reqwest::header::AUTHORIZATION, credential.header_value());
        }

        let response = request
            .send()
            .await
            .map_err(|e| RegistrationError::Transport {
                url: self.url.clone(),
                reason: e.to_string(),
            })?;

        let code = response.status().as_u16();
        if response.status().is_success() {
            // The control plane names the refresh interval; it owns the TTL
            // that interval has to stay inside. A node that picked its own
            // would be guessing at when it gets dropped.
            let refresh_after = match response.json::<RegisterAnswer>().await {
                Ok(answer) if answer.refresh_after_seconds > 0 => {
                    Duration::from_secs(answer.refresh_after_seconds)
                }
                Ok(_) => {
                    warn!(
                        url = %self.url,
                        fallback_secs = DEFAULT_REFRESH.as_secs(),
                        "The control plane accepted this registration but did not say how often to \
                         refresh it, so this node falls back to {}s. If the control plane's \
                         registration TTL is shorter than that, this node will be dropped from the \
                         fleet between refreshes and stop receiving deployments and cron",
                        DEFAULT_REFRESH.as_secs()
                    );
                    DEFAULT_REFRESH
                }
                Err(e) => {
                    warn!(
                        url = %self.url,
                        error = %e,
                        fallback_secs = DEFAULT_REFRESH.as_secs(),
                        "The control plane accepted this registration but its answer could not be \
                         read, so this node falls back to refreshing every {}s. Same consequence \
                         as above if that is slower than the control plane's TTL",
                        DEFAULT_REFRESH.as_secs()
                    );
                    DEFAULT_REFRESH
                }
            };
            debug!(
                url = %self.url,
                code,
                refresh_after_secs = refresh_after.as_secs(),
                "The control plane accepted this node's registration"
            );
            return Ok(Registered::Accepted { refresh_after });
        }

        // 404/405 is a control plane with no such route; 501 is one that has
        // the route and does not implement it. All three mean "this control
        // plane does not do registration", which is a permanent answer and
        // must not be retried every interval.
        if matches!(code, 404 | 405 | 501) {
            return Ok(Registered::NotSupported);
        }

        Err(RegistrationError::Status {
            url: self.url.clone(),
            code,
        })
    }
}

#[cfg(test)]
#[path = "registration_test.rs"]
mod registration_test;
