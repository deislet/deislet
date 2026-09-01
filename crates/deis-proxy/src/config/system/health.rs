//! What "healthy" means for an upstream, and who decides it.
//!
//! A TCP connect only proves that something holds a socket open on that port,
//! and on a deis-runtime node that is true well before the node can serve
//! anything: the data listener is bound before the node has asked the control
//! plane what to run, and it stays bound if the dispatcher behind it stops
//! answering. A pool health checked that way keeps giving such a node its
//! full share of traffic, and every request in that share is answered with an
//! error for as long as the state lasts. Two independent reviews reached this
//! the same way, one by adding a second node and one by restarting an
//! existing one.
//!
//! So the node is asked rather than its kernel. The default probe is
//! `GET `[`DEFAULT_HEALTH_CHECK_PATH`] carrying
//! `x-deis-app-id: `[`DEFAULT_HEALTH_CHECK_APP_ID`], the built-in system
//! application every node loads before it serves. Healthy means the node
//! answered `200`; anything else -- a refused connection, a timeout, a 4xx or
//! a 5xx -- takes it out of rotation until it answers `200` again. When that
//! leaves nothing in rotation the edge answers `503` itself rather than
//! forwarding, which is the honest answer and the one an operator can act on.
//!
//! # A question that can be answered no
//!
//! A `200` on its own is not evidence of much either. `_system`'s `/health`
//! used to be a handler that returned a literal, so it said "the dispatcher
//! ran an isolate for me" and nothing about whether this node is running what
//! it was told to run. A node that came up, failed to reach the control plane
//! and was serving whatever it recovered from its own disk answered `200`
//! exactly as fast as a node doing its job, and took exactly the same share of
//! the traffic.
//!
//! A node's real readiness is `deis_runtime::health::Readiness`, and it has
//! three states: `starting` until the node has the control plane's assignment
//! list and has loaded it, `ready` while it is running what it was told to
//! run, and `degraded` when it is serving what it recovered from its own disk
//! because it cannot reach the control plane. It is also reported by the
//! node's *management* `HealthCheck` RPC, which lives on an address this proxy
//! is never given - so the edge cannot ask for it there.
//!
//! So the probe asks the node to state it on the answer it already sends, in
//! [`NODE_READINESS_HEADER`]. A response header rather than a body, because
//! pingora's health check hands its validator the response *header* and never
//! the body; and a `x-deis-` name because deis-bundle refuses to let a
//! deployed application set a response header under that prefix, so the one
//! header this edge routes on is the one header a tenant cannot forge.
//!
//! The node's half of the contract is `deis_runtime::health` and
//! `dispatcher/system_app.js`, which spell the same header and the same three
//! values. Nothing here assumes the node keeps its side: a node that sends no
//! readiness at all is judged on its status code, which is what makes the
//! header safe to introduce against a fleet that does not send it yet.
//!
//! # What the edge does with each answer
//!
//! [`ReadinessPolicy`] decides, and its default is deliberately not "believe
//! only `ready`":
//!
//! - `starting` is out of rotation. The node says it cannot serve what it was
//!   assigned; forwarding there produces errors the client cannot act on.
//! - `degraded` **stays in rotation** by default, and that is the important
//!   one. A node is degraded because it cannot reach the *control plane*, and
//!   when a control plane goes down every node in the fleet goes degraded at
//!   once. Evicting them all would turn a control-plane outage - which costs
//!   nothing, because the nodes are still serving what they recovered - into a
//!   total outage caused entirely by this edge. So a degraded node keeps its
//!   share and the fact is logged.
//! - No header at all is judged on the `200` alone, so a fleet that has not
//!   been upgraded yet keeps working. Requiring the header by default would
//!   mean the first proxy of a new build evicted every node on the machine.
//!
//! `readiness = "required"` is for a fleet known to report it, and refuses a
//! node that says nothing or says `degraded`. `readiness = "ignored"` is for
//! an upstream that is not a deis-runtime node at all. `mode = "tcp"` goes
//! further still and asks nothing.

use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, Validatable};

/// Path the probe asks a node for.
///
/// The node's built-in system application answers it; see
/// [`DEFAULT_HEALTH_CHECK_APP_ID`].
pub const DEFAULT_HEALTH_CHECK_PATH: &str = "/health";

/// Application the probe names in `x-deis-app-id`.
///
/// A node routes by that header and answers `400` to a request that carries
/// none, so the probe has to name something. `_system` is the application the
/// node loads itself, before it serves and without a deployment, which is why
/// it can be asked on a node that has been assigned nothing.
///
/// It is also the only application the edge *may* ask. The upstream pool is
/// shared by every application this edge routes, so a probe aimed at one
/// tenant's application would take the whole node away from every other
/// tenant when that one tenant's code broke. What is worth knowing about a
/// node is a fact about the node, and `_system` is where a node states one.
pub const DEFAULT_HEALTH_CHECK_APP_ID: &str = "_system";

/// Response header a deis-runtime node states its own readiness in.
///
/// The values are `deis_runtime::health::Readiness::state_name()`:
/// [`READINESS_READY`], [`READINESS_STARTING`], [`READINESS_DEGRADED`]. A node
/// that sends no such header is judged on its status code alone under the
/// default [`ReadinessPolicy`].
///
/// Under `x-deis-`, which deis-bundle refuses to let a deployed application
/// set on a response (`deis_bundle::headers::PLATFORM_PREFIX`). That is what
/// keeps it a statement by the node rather than a claim by whatever code the
/// node happens to be running.
pub const NODE_READINESS_HEADER: &str = "x-deis-node-readiness";

/// The node is running exactly what the control plane last told it to run.
pub const READINESS_READY: &str = "ready";
/// The node has not finished loading its assignment list.
pub const READINESS_STARTING: &str = "starting";
/// The node is serving, but from what it recovered from its own disk rather
/// than from an answer the control plane gave.
pub const READINESS_DEGRADED: &str = "degraded";

/// Seconds one probe gets, for the connect and again for the answer.
///
/// Deliberately shorter than [`DEFAULT_HEALTH_CHECK_INTERVAL_SECS`] (5) so a
/// probe can never still be running when the next one is due, and longer than
/// pingora's own one-second default so that a node merely busy is not evicted
/// for it. A node that cannot answer a static string in two seconds is not a
/// node this edge should be sending requests to.
///
/// [`DEFAULT_HEALTH_CHECK_INTERVAL_SECS`]: super::DEFAULT_HEALTH_CHECK_INTERVAL_SECS
pub const DEFAULT_HEALTH_CHECK_TIMEOUT_SECS: u64 = 2;

/// Consecutive passes that put a node back into rotation.
///
/// One, because a node that answers is a node that can serve, and the cost of
/// believing it early is a single request that may still fail. The cost of
/// believing it late is capacity that is up and idle while the rest of the
/// fleet carries its share.
pub const DEFAULT_HEALTHY_AFTER: usize = 1;

/// Consecutive failures that take a node out of rotation.
///
/// Two, not one: at a five-second interval one dropped packet would otherwise
/// evict a healthy node until the next probe. The price is one extra interval
/// -- about five seconds -- before a node that really has gone leaves
/// rotation, and that is the right way round: a node that is genuinely broken
/// is failing requests either way, while a node evicted by a single unlucky
/// probe takes capacity out of a fleet that was fine.
pub const DEFAULT_UNHEALTHY_AFTER: usize = 2;

/// How a node is asked whether it may be given traffic.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HealthCheckMode {
    /// Ask the node. The default, and the only one that can tell a node that
    /// is serving from a node that has merely bound its port.
    #[default]
    Http,
    /// Open a TCP connection and close it again. All an upstream that is not
    /// a deis-runtime node can be asked, and never enough for one that is.
    Tcp,
}

impl HealthCheckMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            HealthCheckMode::Http => "http",
            HealthCheckMode::Tcp => "tcp",
        }
    }
}

impl std::fmt::Display for HealthCheckMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How much weight the edge puts on what a node says about itself in
/// [`NODE_READINESS_HEADER`].
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReadinessPolicy {
    /// Believe the node when it reports, and fall back to the status code
    /// when it does not. The default; see the module documentation for why
    /// `degraded` keeps its share and a silent node keeps its share.
    #[default]
    Reported,
    /// Only a node that reports [`READINESS_READY`] is given traffic. For a
    /// fleet known to report, and only worth setting once every node does:
    /// under it a node that says nothing is out of rotation, so switching it
    /// on before the fleet is upgraded empties the pool.
    Required,
    /// The status code alone decides. For an upstream that is not a
    /// deis-runtime node but does serve an HTTP health endpoint.
    Ignored,
}

impl ReadinessPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReadinessPolicy::Reported => "reported",
            ReadinessPolicy::Required => "required",
            ReadinessPolicy::Ignored => "ignored",
        }
    }
}

impl std::fmt::Display for ReadinessPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the edge decided about one probe's answer, and why.
///
/// Separate from the check that produces it so the rule can be tested without
/// a node, a socket or a pingora runtime - it is a total function of (status,
/// header, policy) and there are only a dozen cases worth caring about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadinessVerdict {
    /// Give this node traffic.
    InRotation,
    /// Give this node traffic, but say why it is worth an operator's
    /// attention.
    InRotationWithWarning(String),
    /// Take this node out of rotation, naming what it said.
    OutOfRotation(String),
}

/// Judge one probe answer.
///
/// `status` is the response status; `reported` is the value of
/// [`NODE_READINESS_HEADER`] if the node sent one.
pub fn judge_readiness(
    status: u16,
    reported: Option<&str>,
    policy: ReadinessPolicy,
) -> ReadinessVerdict {
    if status != 200 {
        return ReadinessVerdict::OutOfRotation(format!("answered {status} rather than 200"));
    }
    if policy == ReadinessPolicy::Ignored {
        return ReadinessVerdict::InRotation;
    }
    match reported {
        Some(READINESS_READY) => ReadinessVerdict::InRotation,
        Some(READINESS_DEGRADED) => match policy {
            // A degraded node is serving what it recovered from its own disk.
            // It is worse than a ready one and far better than no node at
            // all, and every node goes degraded together when the control
            // plane goes away - so evicting on it would convert a
            // control-plane outage into a traffic outage.
            ReadinessPolicy::Reported => ReadinessVerdict::InRotationWithWarning(
                "the node reports it cannot reach the control plane and is serving the \
                 versions it recovered from its own disk; it keeps its share because taking \
                 every degraded node out of rotation would answer 503 to traffic the fleet \
                 can still serve"
                    .to_string(),
            ),
            _ => ReadinessVerdict::OutOfRotation(
                "the node reports degraded and health_check.readiness is \"required\"".to_string(),
            ),
        },
        Some(other) => ReadinessVerdict::OutOfRotation(format!(
            "the node reports {NODE_READINESS_HEADER}: {other}, not {READINESS_READY}"
        )),
        None => match policy {
            ReadinessPolicy::Required => ReadinessVerdict::OutOfRotation(format!(
                "the node sent no {NODE_READINESS_HEADER} and health_check.readiness is \
                 \"required\"; a deis-runtime node older than that contract never sends one"
            )),
            // The 200 alone. It proves the dispatcher ran an isolate, which
            // is what separates this from a TCP connect, and nothing more.
            _ => ReadinessVerdict::InRotation,
        },
    }
}

/// How upstreams are health checked. Fixed for the process lifetime, like the
/// balancing strategy and the check interval it runs at.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct HealthCheckConfig {
    pub mode: Option<HealthCheckMode>,
    /// How much the edge believes what a node says about itself. Ignored in
    /// `tcp` mode, which asks nothing at all.
    pub readiness: Option<ReadinessPolicy>,
    /// Path the HTTP probe asks for. Ignored in `tcp` mode.
    pub path: Option<String>,
    /// Value of the `x-deis-app-id` header the HTTP probe carries. Empty
    /// sends no such header, which is what an upstream that is not a
    /// deis-runtime node wants.
    pub app_id: Option<String>,
    /// Seconds one probe gets to connect, and again to be answered.
    pub timeout_secs: Option<u64>,
    /// Consecutive passes before a node is given traffic again.
    pub healthy_after: Option<usize>,
    /// Consecutive failures before a node stops being given traffic.
    pub unhealthy_after: Option<usize>,
}

impl HealthCheckConfig {
    pub fn mode(&self) -> HealthCheckMode {
        self.mode.unwrap_or_default()
    }

    pub fn readiness(&self) -> ReadinessPolicy {
        self.readiness.unwrap_or_default()
    }

    pub fn path(&self) -> &str {
        self.path.as_deref().unwrap_or(DEFAULT_HEALTH_CHECK_PATH)
    }

    pub fn app_id(&self) -> &str {
        self.app_id
            .as_deref()
            .unwrap_or(DEFAULT_HEALTH_CHECK_APP_ID)
    }

    pub fn timeout_secs(&self) -> u64 {
        self.timeout_secs
            .unwrap_or(DEFAULT_HEALTH_CHECK_TIMEOUT_SECS)
    }

    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs())
    }

    pub fn healthy_after(&self) -> usize {
        self.healthy_after.unwrap_or(DEFAULT_HEALTHY_AFTER)
    }

    pub fn unhealthy_after(&self) -> usize {
        self.unhealthy_after.unwrap_or(DEFAULT_UNHEALTHY_AFTER)
    }
}

impl Validatable for HealthCheckConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if let Some(path) = &self.path
            && !path.starts_with('/')
        {
            return Err(ConfigError::ValidationError {
                message: format!(
                    "health_check.path must start with '/', got {path:?}; it is the request \
                     target of an HTTP probe, not a URL"
                ),
            });
        }
        if self.timeout_secs == Some(0) {
            return Err(ConfigError::ValidationError {
                message: "health_check.timeout_secs must be at least 1; 0 would fail every \
                          probe and take the whole fleet out of rotation"
                    .to_string(),
            });
        }
        for (name, value) in [
            ("health_check.healthy_after", self.healthy_after),
            ("health_check.unhealthy_after", self.unhealthy_after),
        ] {
            if value == Some(0) {
                return Err(ConfigError::ValidationError {
                    message: format!(
                        "{name} must be at least 1; 0 is not a number of probes that can ever \
                         be reached, so the state it guards would never be entered"
                    ),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asking_the_node_is_the_default() {
        let settings = HealthCheckConfig::default();
        assert_eq!(settings.mode(), HealthCheckMode::Http);
        assert_eq!(settings.path(), "/health");
        assert_eq!(settings.app_id(), "_system");
        assert_eq!(settings.readiness(), ReadinessPolicy::Reported);
    }

    #[test]
    fn a_node_that_says_nothing_is_still_judged_on_its_status() {
        // The compatibility half. Every deis-runtime node in existence when
        // this contract was written answers 200 and sends no readiness
        // header; if that read as unhealthy, the first proxy carrying this
        // change would empty its own pool and answer 503 to everything.
        assert_eq!(
            judge_readiness(200, None, ReadinessPolicy::Reported),
            ReadinessVerdict::InRotation
        );
        assert!(matches!(
            judge_readiness(503, None, ReadinessPolicy::Reported),
            ReadinessVerdict::OutOfRotation(_)
        ));
    }

    #[test]
    fn a_node_that_says_it_is_starting_is_taken_out_of_rotation() {
        // The point of the whole contract: a node that is up, listening and
        // answering, and has not loaded what it was told to run. Before the
        // header there was nothing in the answer that could say so.
        let out = judge_readiness(200, Some(READINESS_STARTING), ReadinessPolicy::Reported);
        match out {
            ReadinessVerdict::OutOfRotation(why) => assert!(
                why.contains(READINESS_STARTING),
                "the reason has to name what the node said: {why}"
            ),
            other => panic!("a starting node was left in rotation: {other:?}"),
        }
    }

    #[test]
    fn a_degraded_node_keeps_its_share_unless_the_operator_says_otherwise() {
        // Every node goes degraded at once when the control plane goes away,
        // and they are all still serving. Evicting them would be this edge
        // turning somebody else's outage into its own.
        match judge_readiness(200, Some(READINESS_DEGRADED), ReadinessPolicy::Reported) {
            ReadinessVerdict::InRotationWithWarning(why) => {
                assert!(why.contains("control plane"), "{why}")
            }
            other => panic!("a degraded node was not kept in rotation: {other:?}"),
        }
        assert!(matches!(
            judge_readiness(200, Some(READINESS_DEGRADED), ReadinessPolicy::Required),
            ReadinessVerdict::OutOfRotation(_)
        ));
    }

    #[test]
    fn required_refuses_a_node_that_states_nothing() {
        assert!(matches!(
            judge_readiness(200, None, ReadinessPolicy::Required),
            ReadinessVerdict::OutOfRotation(_)
        ));
        assert_eq!(
            judge_readiness(200, Some(READINESS_READY), ReadinessPolicy::Required),
            ReadinessVerdict::InRotation
        );
    }

    #[test]
    fn ignored_looks_only_at_the_status() {
        // For an upstream that is not a node at all: whatever it puts in that
        // header means nothing here, and a 200 is all there is to go on.
        assert_eq!(
            judge_readiness(200, Some(READINESS_STARTING), ReadinessPolicy::Ignored),
            ReadinessVerdict::InRotation
        );
        assert!(matches!(
            judge_readiness(500, Some(READINESS_READY), ReadinessPolicy::Ignored),
            ReadinessVerdict::OutOfRotation(_)
        ));
    }

    #[test]
    fn an_unknown_state_is_not_read_as_ready() {
        // A node from a future build naming a state this proxy has never
        // heard of. Guessing that it means "fine" is how a real state gets
        // silently ignored; the safe reading is that this edge does not know
        // what it means and will not route on it.
        assert!(matches!(
            judge_readiness(200, Some("quiescing"), ReadinessPolicy::Reported),
            ReadinessVerdict::OutOfRotation(_)
        ));
    }

    #[test]
    fn the_policy_is_spelled_the_way_it_is_configured() {
        let settings: HealthCheckConfig =
            toml::from_str("readiness = \"required\"\n").expect("the policy parses from TOML");
        assert_eq!(settings.readiness(), ReadinessPolicy::Required);
    }

    #[test]
    fn a_path_that_is_not_a_request_target_is_refused() {
        let settings = HealthCheckConfig {
            path: Some("health".to_string()),
            ..Default::default()
        };
        let message = settings.validate().unwrap_err().to_string();
        assert!(
            message.contains("must start with '/'"),
            "the error should name the rule, got: {message}"
        );
    }

    #[test]
    fn a_zero_timeout_is_refused_because_it_would_evict_every_node() {
        let settings = HealthCheckConfig {
            timeout_secs: Some(0),
            ..Default::default()
        };
        assert!(settings.validate().is_err());
    }

    #[test]
    fn zero_probes_is_refused_for_both_directions() {
        for settings in [
            HealthCheckConfig {
                healthy_after: Some(0),
                ..Default::default()
            },
            HealthCheckConfig {
                unhealthy_after: Some(0),
                ..Default::default()
            },
        ] {
            assert!(settings.validate().is_err());
        }
    }
}
