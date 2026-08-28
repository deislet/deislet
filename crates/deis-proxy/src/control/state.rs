//! Proxy instance lifecycle state management
//!
//! Implementation of deis-proxy instance lifecycle management based on InstanceStatus from the deis-wire library
//!
//! State Transitions: Pending -> Active -> Sunsetting -> Terminated
//! Supports restart cycle: Terminated -> Pending

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

// Directly use state definitions from the deis-wire library
pub use deis_wire::models::instance::InstanceStatus as State;

/// Proxy instance state manager
///
/// Manages the full lifecycle of a deis-proxy instance using InstanceStatus from the deis-wire library
///
/// State Transitions:
/// - Pending: Instance starting, waiting for registration and configuration delivery
/// - Active: Instance ready, processing requests normally
/// - Sunsetting: Instance preparing to close, no longer accepting new connections
/// - Terminated: Instance fully closed
pub struct ProxyStateManager {
    /// Current state
    current_state: Arc<RwLock<State>>,
    /// State transition history
    transition_history: Arc<RwLock<Vec<StateTransition>>>,
    /// Instance ID
    instance_id: String,
}

/// State transition record
#[derive(Debug, Clone)]
pub struct StateTransition {
    pub from: State,
    pub to: State,
    pub event: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl ProxyStateManager {
    /// Create a new state manager
    pub fn new(instance_id: String) -> Self {
        info!("Creating state manager for instance: {}", instance_id);

        Self {
            current_state: Arc::new(RwLock::new(State::Pending)),
            transition_history: Arc::new(RwLock::new(Vec::new())),
            instance_id,
        }
    }

    /// Internal state transition method
    async fn transition_to(&self, new_state: State, event: &str) -> Result<State, String> {
        let mut current = self.current_state.write().await;
        let old_state = current.clone();

        // Validate if the state transition is valid
        if !self.is_valid_transition(&old_state, &new_state) {
            let error_msg = format!(
                "Invalid state transition from {:?} to {:?} via event '{}'",
                old_state, new_state, event
            );
            warn!("Instance {}: {}", self.instance_id, error_msg);
            return Err(error_msg);
        }

        // Execute state transition
        *current = new_state.clone();

        // Record transition history
        let transition = StateTransition {
            from: old_state.clone(),
            to: new_state.clone(),
            event: event.to_string(),
            timestamp: chrono::Utc::now(),
        };

        let mut history = self.transition_history.write().await;
        history.push(transition);

        info!(
            "Instance {} transition: {:?} --[{}]--> {:?}",
            self.instance_id, old_state, event, new_state
        );

        Ok(new_state)
    }

    /// Validate if the state transition is valid
    fn is_valid_transition(&self, from: &State, to: &State) -> bool {
        use State::*;

        match (from, to) {
            // Normal lifecycle
            (Pending, Active) => true,        // Registration completed
            (Active, Active) => true,         // Heartbeat maintained
            (Active, Sunsetting) => true,     // Shutdown started
            (Sunsetting, Sunsetting) => true, // Heartbeat during sunsetting
            (Sunsetting, Terminated) => true, // Fully closed

            // Restart cycle
            (Terminated, Pending) => true, // Restart

            // Other transitions are invalid
            _ => false,
        }
    }

    /// Get current state
    pub async fn current_state(&self) -> State {
        self.current_state.read().await.clone()
    }

    /// Check if the instance is in active state
    pub async fn is_active(&self) -> bool {
        matches!(self.current_state().await, State::Active)
    }

    /// Check if the instance is terminated
    pub async fn is_terminated(&self) -> bool {
        matches!(self.current_state().await, State::Terminated)
    }

    /// Check if the instance is sunsetting
    pub async fn is_sunsetting(&self) -> bool {
        matches!(self.current_state().await, State::Sunsetting)
    }

    /// Check if the instance is pending
    pub async fn is_pending(&self) -> bool {
        matches!(self.current_state().await, State::Pending)
    }

    /// Get number of state transitions in history
    pub async fn history_count(&self) -> usize {
        self.transition_history.read().await.len()
    }

    /// Get state transition history
    pub async fn get_history(&self) -> Vec<StateTransition> {
        self.transition_history.read().await.clone()
    }

    /// Get instance ID
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Handle registration complete event
    ///
    /// Transition from Pending to Active state
    pub async fn handle_registration_complete(&self) -> Result<State, String> {
        info!(
            "Instance {} registration completed, transitioning to Active",
            self.instance_id
        );
        self.transition_to(State::Active, "registration_complete")
            .await
    }

    /// Handle heartbeat event
    ///
    /// Maintain activity in Active or Sunsetting state
    pub async fn handle_heartbeat(&self) -> Result<State, String> {
        let current = self.current_state().await;
        match current {
            State::Active => {
                // In Active state, heartbeat keeps the state unchanged
                self.transition_to(State::Active, "heartbeat").await
            }
            State::Sunsetting => {
                // In Sunsetting state, heartbeat also keeps the state unchanged
                self.transition_to(State::Sunsetting, "heartbeat").await
            }
            _ => {
                let error_msg = format!("Cannot send heartbeat in state: {:?}", current);
                warn!("Instance {}: {}", self.instance_id, error_msg);
                Err(error_msg)
            }
        }
    }

    /// Handle shutdown event
    ///
    /// Transition from Active to Sunsetting state
    pub async fn handle_shutdown(&self) -> Result<State, String> {
        info!(
            "Instance {} received shutdown signal, transitioning to Sunsetting",
            self.instance_id
        );
        self.transition_to(State::Sunsetting, "shutdown").await
    }

    /// Handle silenced event (all connections closed)
    ///
    /// Transition from Sunsetting to Terminated state
    pub async fn handle_silenced(&self) -> Result<State, String> {
        info!(
            "Instance {} all connections closed, transitioning to Terminated",
            self.instance_id
        );
        self.transition_to(State::Terminated, "silenced").await
    }

    /// Handle restart event
    ///
    /// Transition from Terminated to Pending state
    pub async fn handle_startup(&self) -> Result<State, String> {
        info!(
            "Instance {} restarting, transitioning to Pending",
            self.instance_id
        );
        self.transition_to(State::Pending, "startup").await
    }

    /// Handle uncontrolled termination (e.g., process killed)
    ///
    /// Attempt to report status and record it
    pub async fn handle_uncontrolled_termination(&self) -> Result<(), String> {
        let current_state = self.current_state().await;
        error!(
            "Instance {} uncontrolled termination from state: {:?}",
            self.instance_id, current_state
        );

        // Record state change history
        let history_count = self.history_count().await;
        error!(
            "Instance {} had {} state transitions before termination",
            self.instance_id, history_count
        );

        // TODO: Implement status reporting logic

        Ok(())
    }

    /// Force set state (emergency use only)
    pub async fn force_set_state(&self, new_state: State, reason: &str) -> State {
        let mut current = self.current_state.write().await;
        let old_state = current.clone();
        *current = new_state.clone();

        warn!(
            "Instance {} forced state change: {:?} -> {:?}, reason: {}",
            self.instance_id, old_state, new_state, reason
        );

        // Record forced transition
        let transition = StateTransition {
            from: old_state,
            to: new_state.clone(),
            event: format!("force_set: {}", reason),
            timestamp: chrono::Utc::now(),
        };

        let mut history = self.transition_history.write().await;
        history.push(transition);

        new_state
    }

    /// Check if a specific operation can be performed
    pub async fn can_handle_shutdown(&self) -> bool {
        matches!(self.current_state().await, State::Active)
    }

    pub async fn can_handle_heartbeat(&self) -> bool {
        matches!(
            self.current_state().await,
            State::Active | State::Sunsetting
        )
    }

    pub async fn can_handle_registration(&self) -> bool {
        matches!(self.current_state().await, State::Pending)
    }

    pub async fn can_handle_silenced(&self) -> bool {
        matches!(self.current_state().await, State::Sunsetting)
    }

    pub async fn can_handle_startup(&self) -> bool {
        matches!(self.current_state().await, State::Terminated)
    }

    /// Reset state machine (for testing only)
    #[cfg(test)]
    pub async fn reset(&self) {
        let mut current = self.current_state.write().await;
        *current = State::Pending;

        let mut history = self.transition_history.write().await;
        history.clear();

        info!("Instance {} state manager reset", self.instance_id);
    }
}

impl std::fmt::Debug for ProxyStateManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyStateManager")
            .field("instance_id", &self.instance_id)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio;

    #[tokio::test]
    async fn test_normal_lifecycle() {
        let manager = ProxyStateManager::new("test-instance".to_string());

        // Initial state should be Pending
        assert_eq!(manager.current_state().await, State::Pending);
        assert!(manager.is_pending().await);

        // Registration completed, enter Active
        let result = manager.handle_registration_complete().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), State::Active);
        assert!(manager.is_active().await);

        // Send heartbeat, maintain Active
        let result = manager.handle_heartbeat().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), State::Active);

        // Start shutdown
        let result = manager.handle_shutdown().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), State::Sunsetting);
        assert!(manager.is_sunsetting().await);

        // Heartbeat is still valid in Sunsetting state
        let result = manager.handle_heartbeat().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), State::Sunsetting);

        // All connections closed, enter Terminated
        let result = manager.handle_silenced().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), State::Terminated);
        assert!(manager.is_terminated().await);
    }

    #[tokio::test]
    async fn test_restart_cycle() {
        let manager = ProxyStateManager::new("test-instance".to_string());

        // Complete lifecycle
        manager.handle_registration_complete().await.unwrap();
        manager.handle_shutdown().await.unwrap();
        manager.handle_silenced().await.unwrap();
        assert!(manager.is_terminated().await);

        // Restart
        let result = manager.handle_startup().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), State::Pending);
        assert!(manager.is_pending().await);
    }

    #[tokio::test]
    async fn test_invalid_transitions() {
        let manager = ProxyStateManager::new("test-instance".to_string());

        // Attempt to shutdown directly from Pending state (invalid transition)
        let result = manager.handle_shutdown().await;
        assert!(result.is_err());

        // State should remain unchanged
        assert_eq!(manager.current_state().await, State::Pending);

        // Attempt to silence directly from Pending state (invalid transition)
        let result = manager.handle_silenced().await;
        assert!(result.is_err());
        assert_eq!(manager.current_state().await, State::Pending);
    }

    #[tokio::test]
    async fn test_capability_checks() {
        let manager = ProxyStateManager::new("test-instance".to_string());

        // Capabilities in Pending state
        assert!(manager.can_handle_registration().await);
        assert!(!manager.can_handle_shutdown().await);
        assert!(!manager.can_handle_heartbeat().await);

        // Transition to Active state
        manager.handle_registration_complete().await.unwrap();

        // Capabilities in Active state
        assert!(!manager.can_handle_registration().await);
        assert!(manager.can_handle_shutdown().await);
        assert!(manager.can_handle_heartbeat().await);
        assert!(!manager.can_handle_silenced().await);

        // Transition to Sunsetting state
        manager.handle_shutdown().await.unwrap();

        // Capabilities in Sunsetting state
        assert!(!manager.can_handle_shutdown().await);
        assert!(manager.can_handle_heartbeat().await);
        assert!(manager.can_handle_silenced().await);
    }

    #[tokio::test]
    async fn test_history_tracking() {
        let manager = ProxyStateManager::new("test-instance".to_string());

        assert_eq!(manager.history_count().await, 0);

        // Perform some state transitions
        manager.handle_registration_complete().await.unwrap();
        assert_eq!(manager.history_count().await, 1);

        manager.handle_heartbeat().await.unwrap();
        assert_eq!(manager.history_count().await, 2);

        manager.handle_shutdown().await.unwrap();
        assert_eq!(manager.history_count().await, 3);

        // Check history records
        let history = manager.get_history().await;
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].from, State::Pending);
        assert_eq!(history[0].to, State::Active);
        assert_eq!(history[0].event, "registration_complete");
    }

    #[tokio::test]
    async fn test_force_set_state() {
        let manager = ProxyStateManager::new("test-instance".to_string());

        // Force set state
        let new_state = manager
            .force_set_state(State::Terminated, "emergency shutdown")
            .await;
        assert_eq!(new_state, State::Terminated);
        assert!(manager.is_terminated().await);

        // Check that history records include the forced transition
        let history = manager.get_history().await;
        assert_eq!(history.len(), 1);
        assert!(history[0].event.contains("force_set"));
    }

    #[tokio::test]
    async fn test_uncontrolled_termination() {
        let manager = ProxyStateManager::new("test-instance".to_string());

        // Enter Active state
        manager.handle_registration_complete().await.unwrap();

        // Simulate uncontrolled termination
        let result = manager.handle_uncontrolled_termination().await;
        assert!(result.is_ok());

        // State should remain unchanged (since there was no actual state transition)
        assert_eq!(manager.current_state().await, State::Active);
    }
}
