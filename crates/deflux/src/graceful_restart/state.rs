//! Restart state machine
//!
//! Manages phases and state transitions of graceful restart.

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

/// Restart phase
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPhase {
    /// Initialization phase
    Setup,

    /// Normal operation
    Running,

    /// Preparing to transfer Fds
    PreparingTransfer,

    /// Transferring Fds
    TransferringFds,

    /// Waiting for new process to be ready
    WaitingForNewProcess,

    /// Graceful shutdown (no longer accepting new connections)
    GracefulShutdown,

    /// Terminated
    Terminated,
}

impl RestartPhase {
    /// Whether new connections should be accepted
    pub fn should_accept_connections(&self) -> bool {
        matches!(
            self,
            RestartPhase::Running
                | RestartPhase::PreparingTransfer
                | RestartPhase::TransferringFds
                | RestartPhase::WaitingForNewProcess
        )
    }

    /// Whether it is performing graceful shutdown
    pub fn is_shutting_down(&self) -> bool {
        matches!(
            self,
            RestartPhase::GracefulShutdown | RestartPhase::Terminated
        )
    }

    /// Whether it is restarting
    pub fn is_restarting(&self) -> bool {
        matches!(
            self,
            RestartPhase::PreparingTransfer
                | RestartPhase::TransferringFds
                | RestartPhase::WaitingForNewProcess
        )
    }
}

impl std::fmt::Display for RestartPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestartPhase::Setup => write!(f, "Setup"),
            RestartPhase::Running => write!(f, "Running"),
            RestartPhase::PreparingTransfer => write!(f, "PreparingTransfer"),
            RestartPhase::TransferringFds => write!(f, "TransferringFds"),
            RestartPhase::WaitingForNewProcess => write!(f, "WaitingForNewProcess"),
            RestartPhase::GracefulShutdown => write!(f, "GracefulShutdown"),
            RestartPhase::Terminated => write!(f, "Terminated"),
        }
    }
}

/// Restart state
pub struct RestartState {
    /// Current phase
    phase: Arc<RwLock<RestartPhase>>,
}

impl RestartState {
    /// Create new state
    pub fn new() -> Self {
        Self {
            phase: Arc::new(RwLock::new(RestartPhase::Setup)),
        }
    }

    /// Get current phase
    pub async fn get_phase(&self) -> RestartPhase {
        *self.phase.read().await
    }

    /// Set phase
    pub async fn set_phase(&self, new_phase: RestartPhase) {
        let mut phase = self.phase.write().await;
        let old_phase = *phase;

        if old_phase != new_phase {
            info!("State transition: {} -> {}", old_phase, new_phase);
            *phase = new_phase;
        } else {
            debug!("State unchanged: {}", new_phase);
        }
    }

    /// Transition to running state
    pub async fn transition_to_running(&self) {
        self.set_phase(RestartPhase::Running).await;
    }

    /// Transition to preparing transfer
    pub async fn transition_to_preparing_transfer(&self) {
        self.set_phase(RestartPhase::PreparingTransfer).await;
    }

    /// Transition to transferring Fds
    pub async fn transition_to_transferring_fds(&self) {
        self.set_phase(RestartPhase::TransferringFds).await;
    }

    /// Transition to waiting for new process
    pub async fn transition_to_waiting_for_new_process(&self) {
        self.set_phase(RestartPhase::WaitingForNewProcess).await;
    }

    /// Transition to graceful shutdown
    pub async fn transition_to_graceful_shutdown(&self) {
        self.set_phase(RestartPhase::GracefulShutdown).await;
    }

    /// Transition to terminated
    pub async fn transition_to_terminated(&self) {
        self.set_phase(RestartPhase::Terminated).await;
    }

    /// Check whether new connections should be accepted
    pub async fn should_accept_connections(&self) -> bool {
        self.get_phase().await.should_accept_connections()
    }

    /// Check whether it is shutting down
    pub async fn is_shutting_down(&self) -> bool {
        self.get_phase().await.is_shutting_down()
    }

    /// Check whether it is restarting
    pub async fn is_restarting(&self) -> bool {
        self.get_phase().await.is_restarting()
    }

    /// Clone state (sharing the same underlying state)
    pub fn clone_state(&self) -> Self {
        Self {
            phase: Arc::clone(&self.phase),
        }
    }
}

impl Default for RestartState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_phase_transitions() {
        let state = RestartState::new();

        assert_eq!(state.get_phase().await, RestartPhase::Setup);

        state.transition_to_running().await;
        assert_eq!(state.get_phase().await, RestartPhase::Running);
        assert!(state.should_accept_connections().await);

        state.transition_to_preparing_transfer().await;
        assert!(state.is_restarting().await);

        state.transition_to_graceful_shutdown().await;
        assert!(!state.should_accept_connections().await);
        assert!(state.is_shutting_down().await);
    }

    #[tokio::test]
    async fn test_state_clone() {
        let state1 = RestartState::new();
        let state2 = state1.clone_state();

        state1.transition_to_running().await;

        // state2 should see the same state
        assert_eq!(state2.get_phase().await, RestartPhase::Running);
    }
}
