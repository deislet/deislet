//! Whether this node is ready to be given traffic.
//!
//! A node is not healthy because its process started. It is healthy once it
//! has asked the control plane what it should be running and has loaded it
//! (docs/implementation-plan.zh.md section 4, principle 8). Between those two
//! moments it is listening - so the control plane can reach it and so an
//! operator gets an answer rather than a refused connection - but it says it
//! is not ready.
//!
//! The third state is the one that matters operationally: a node that could
//! not reach the control plane at all. It keeps serving whatever it recovered
//! from its own disk, and it reports unhealthy for as long as that is true, so
//! that "serving stale versions" never looks the same as "serving what it was
//! told to".

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;

/// What the node is currently able to say about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    /// Still working through the startup assignment list.
    Starting,
    /// Running exactly what the control plane last said to run.
    Ready { apps: usize },
    /// Serving, but not from an answer the control plane gave.
    Degraded { reason: String, apps: usize },
}

impl Readiness {
    pub fn is_healthy(&self) -> bool {
        matches!(self, Readiness::Ready { .. })
    }

    pub fn state_name(&self) -> &'static str {
        match self {
            Readiness::Starting => "starting",
            Readiness::Ready { .. } => "ready",
            Readiness::Degraded { .. } => "degraded",
        }
    }
}

/// The node's readiness, shared between the startup sync and the health RPC.
#[derive(Debug)]
pub struct NodeHealth {
    state: ArcSwap<Readiness>,
}

impl NodeHealth {
    /// A node that has not finished starting yet.
    pub fn starting() -> Arc<Self> {
        Arc::new(Self {
            state: ArcSwap::from_pointee(Readiness::Starting),
        })
    }

    pub fn snapshot(&self) -> Readiness {
        (**self.state.load()).clone()
    }

    pub fn is_healthy(&self) -> bool {
        self.state.load().is_healthy()
    }

    /// The node is running what it was told to run.
    pub fn mark_ready(&self, apps: usize) {
        let previous = self.snapshot();
        self.state.store(Arc::new(Readiness::Ready { apps }));
        match previous {
            Readiness::Ready { .. } => {}
            Readiness::Starting => tracing::info!(
                apps,
                "Node is ready: it has the control plane's assignment list and has loaded all \
                 {} of its apps",
                apps
            ),
            Readiness::Degraded { .. } => tracing::info!(
                apps,
                "Node reached the control plane again and is now running its assigned versions; \
                 it is healthy"
            ),
        }
    }

    /// The node is serving, but not from an answer the control plane gave.
    pub fn mark_degraded(&self, reason: impl Into<String>, apps: usize) {
        let reason = reason.into();
        let already =
            matches!(self.snapshot(), Readiness::Degraded { reason: ref r, .. } if *r == reason);
        self.state.store(Arc::new(Readiness::Degraded {
            reason: reason.clone(),
            apps,
        }));
        if !already {
            tracing::error!(
                apps,
                reason = %reason,
                "Node is DEGRADED and reports unhealthy: {}. It keeps serving the {} app(s) it \
                 recovered from its own disk, which may be an older version than the one it is \
                 supposed to run, and any secret those apps declare is missing",
                reason,
                apps
            );
        }
    }

    /// Key/value form for the `HealthCheck` RPC, so an operator sees *why*
    /// rather than just `healthy: false`.
    pub fn details(&self) -> HashMap<String, String> {
        let mut details = HashMap::new();
        let state = self.snapshot();
        details.insert("state".to_string(), state.state_name().to_string());
        match state {
            Readiness::Starting => {
                details.insert(
                    "reason".to_string(),
                    "still loading the assignment list from the control plane".to_string(),
                );
            }
            Readiness::Ready { apps } => {
                details.insert("apps".to_string(), apps.to_string());
            }
            Readiness::Degraded { reason, apps } => {
                details.insert("reason".to_string(), reason);
                details.insert("apps".to_string(), apps.to_string());
            }
        }
        details
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_starting_node_is_not_healthy() {
        let health = NodeHealth::starting();
        assert!(!health.is_healthy());
        assert_eq!(health.details().get("state").unwrap(), "starting");
    }

    #[test]
    fn a_node_is_healthy_only_after_it_loaded_its_assignments() {
        let health = NodeHealth::starting();
        health.mark_ready(2);
        assert!(health.is_healthy());
        assert_eq!(health.details().get("apps").unwrap(), "2");
    }

    #[test]
    fn a_node_serving_from_its_own_disk_reports_unhealthy_with_the_reason() {
        let health = NodeHealth::starting();
        health.mark_degraded("control plane unreachable", 1);
        assert!(!health.is_healthy());
        let details = health.details();
        assert_eq!(details.get("state").unwrap(), "degraded");
        assert_eq!(details.get("reason").unwrap(), "control plane unreachable");
        assert_eq!(details.get("apps").unwrap(), "1");
    }

    #[test]
    fn recovering_contact_with_the_control_plane_clears_the_degraded_state() {
        let health = NodeHealth::starting();
        health.mark_degraded("control plane unreachable", 1);
        health.mark_ready(1);
        assert_eq!(health.snapshot(), Readiness::Ready { apps: 1 });
    }
}
