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
//! # What that answer is worth, and what it is not worth yet
//!
//! A node's real readiness is `deis_runtime::health::Readiness`, and it has
//! three states: `starting` until the node has the control plane's assignment
//! list and has loaded it, `ready` while it is running what it was told to
//! run, and `degraded` when it is serving what it recovered from its own disk
//! because it cannot reach the control plane. That value is reported by the
//! node's *management* `HealthCheck` RPC, which lives on an address this proxy
//! is never given. The `/health` the data port serves is the system
//! application's own, and today it answers `200` whenever the node is serving
//! at all.
//!
//! So this probe closes the states a TCP connect cannot see -- a port sitting
//! in the kernel's backlog with nothing serving it, a dispatcher that has
//! stopped answering, a node too overloaded to answer inside
//! [`DEFAULT_HEALTH_CHECK_TIMEOUT_SECS`] -- and not yet the two that only the
//! node can name. Closing those is one change on the node's side rather than
//! on this one: the moment `_system`'s `/health` answers a non-200 while
//! `Readiness` is not `Ready`, this proxy already asks, already believes the
//! answer and already stops routing there. Nothing here has to change for
//! that, which is exactly why the asking is worth having before it happens.
//!
//! `mode = "tcp"` restores the old behaviour, for an upstream that is not a
//! deis-runtime node and has no health endpoint to ask.

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
pub const DEFAULT_HEALTH_CHECK_APP_ID: &str = "_system";

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

/// How upstreams are health checked. Fixed for the process lifetime, like the
/// balancing strategy and the check interval it runs at.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct HealthCheckConfig {
    pub mode: Option<HealthCheckMode>,
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
