//! Proxy instance lifecycle state management
//!
//! The lifecycle itself is not defined here. It is `InstanceFSM` in
//! deis-wire (`deis_wire::models::instance::instance_fsm`), and everything
//! below asks that table what may happen next instead of answering for
//! itself.
//!
//! It used to answer for itself, and that is worth recording because it is
//! the failure this module exists to not repeat. A hand-written
//! `is_valid_transition(from, to)` sat here, matching on the *state pair*
//! alone. yasm's table is state **plus input**, so the pair-only copy was
//! strictly weaker: `(Active, Active)` was legal, which meant a caller could
//! ask to move to `Active` naming the event `"shutdown"`, be told yes, and
//! leave a history entry saying an instance shut down while it went on
//! serving. The event reached the log and never the decision.
//!
//! So the event *is* the decision now. Every entry point below names an
//! [`Input`] and the table names the destination. The five `can_handle_*`
//! predicates, which were a third copy of the same rules, ask
//! [`InstanceFSM::valid_inputs`].
//!
//! State transitions: Pending -> Active -> Sunsetting -> Terminated,
//! plus the restart edge Terminated -> Pending.

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use yasm::StateMachine;

// The lifecycle definition, taken whole from the deis-wire library.
pub use deis_wire::models::instance::InstanceStatus as State;
use deis_wire::models::instance::instance_fsm::{Input, InstanceFSM};

/// The name an event is recorded under in [`StateTransition::event`].
///
/// A function of the input rather than a parameter beside it: these strings
/// go into the transition history an operator reads afterwards, and a
/// free-form string next to the transition is a string that can describe a
/// different event from the one that actually fired. There is now no way to
/// write "shutdown" into the history of a transition that was not a shutdown.
fn event_name(input: &Input) -> &'static str {
    match input {
        Input::Registered => "registration_complete",
        Input::_Heartbeat => "heartbeat",
        Input::Shutdown => "shutdown",
        Input::Silenced => "silenced",
        Input::Startup => "startup",
    }
}

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
    ///
    /// Kept by hand rather than read out of a `yasm::StateMachineInstance`
    /// because what an operator needs from it is the timestamp and the event
    /// name, and yasm's own history is a bounded ring of `(state, input)`
    /// pairs with no clock. The *rule* comes from yasm; the record does not.
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
            current_state: Arc::new(RwLock::new(InstanceFSM::initial_state())),
            transition_history: Arc::new(RwLock::new(Vec::new())),
            instance_id,
        }
    }

    /// Apply one lifecycle event.
    ///
    /// The caller says what happened. Where that leaves the instance is
    /// `InstanceFSM`'s answer, not the caller's, and an event the current
    /// state has no row for changes nothing at all.
    async fn apply(&self, input: Input) -> Result<State, String> {
        let mut current = self.current_state.write().await;
        let old_state = current.clone();

        let Some(new_state) = InstanceFSM::next_state(&old_state, &input) else {
            let error_msg = format!(
                "Invalid state transition from {:?} via event '{}'",
                old_state,
                event_name(&input)
            );
            warn!("Instance {}: {}", self.instance_id, error_msg);
            return Err(error_msg);
        };

        // Execute state transition
        *current = new_state.clone();

        // Record transition history
        let transition = StateTransition {
            from: old_state.clone(),
            to: new_state.clone(),
            event: event_name(&input).to_string(),
            timestamp: chrono::Utc::now(),
        };

        let mut history = self.transition_history.write().await;
        history.push(transition);

        info!(
            "Instance {} transition: {:?} --[{}]--> {:?}",
            self.instance_id,
            old_state,
            event_name(&input),
            new_state
        );

        Ok(new_state)
    }

    /// Whether the current state has a row for this event.
    async fn accepts(&self, input: &Input) -> bool {
        InstanceFSM::valid_inputs(&self.current_state().await).contains(input)
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
    /// Activates a pending instance. Accepted again on an instance that is
    /// already active, which is what a reconnect after a control plane
    /// outage looks like: the instance never stopped serving, so it stays
    /// where it is.
    pub async fn handle_registration_complete(&self) -> Result<State, String> {
        info!(
            "Instance {} registration completed, transitioning to Active",
            self.instance_id
        );
        self.apply(Input::Registered).await
    }

    /// Handle heartbeat event
    ///
    /// Maintain activity in Active or Sunsetting state
    pub async fn handle_heartbeat(&self) -> Result<State, String> {
        // Which states a heartbeat is legal in, and what it leaves them in,
        // is `InstanceFSM`'s answer. This used to be a `match` on the
        // current state naming `Active` and `Sunsetting` by hand.
        self.apply(Input::_Heartbeat).await
    }

    /// Handle shutdown event
    ///
    /// Transition from Active to Sunsetting state
    pub async fn handle_shutdown(&self) -> Result<State, String> {
        info!(
            "Instance {} received shutdown signal, transitioning to Sunsetting",
            self.instance_id
        );
        self.apply(Input::Shutdown).await
    }

    /// Handle silenced event (all connections closed)
    ///
    /// Transition from Sunsetting to Terminated state
    pub async fn handle_silenced(&self) -> Result<State, String> {
        info!(
            "Instance {} all connections closed, transitioning to Terminated",
            self.instance_id
        );
        self.apply(Input::Silenced).await
    }

    /// Handle restart event
    ///
    /// Transition from Terminated to Pending state
    pub async fn handle_startup(&self) -> Result<State, String> {
        info!(
            "Instance {} restarting, transitioning to Pending",
            self.instance_id
        );
        self.apply(Input::Startup).await
    }

    /// Record that this instance is going down without having passed through
    /// `handle_shutdown`.
    ///
    /// It reports nothing to the control plane, and deliberately so: by the
    /// time this is reachable the process is on its way out, and a report that
    /// may or may not arrive is worse evidence than none. The control plane
    /// notices the same fact on its own, by the node's registration lease
    /// lapsing. What this leaves behind is a pair of error lines - the state
    /// the instance died in, and how many transitions it had made - which is
    /// what an operator reading the log afterwards actually needs.
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

        Ok(())
    }

    /// Force set state (emergency use only)
    ///
    /// The one door around the table, kept because an operator dealing with
    /// an instance wedged in a state its events cannot leave needs one. It
    /// names no event, which is why its history entry is prefixed
    /// `force_set:` - so that reading the history back, a state change nobody
    /// can account for is never mistaken for one the lifecycle allowed.
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
        self.accepts(&Input::Shutdown).await
    }

    pub async fn can_handle_heartbeat(&self) -> bool {
        self.accepts(&Input::_Heartbeat).await
    }

    pub async fn can_handle_registration(&self) -> bool {
        self.accepts(&Input::Registered).await
    }

    pub async fn can_handle_silenced(&self) -> bool {
        self.accepts(&Input::Silenced).await
    }

    pub async fn can_handle_startup(&self) -> bool {
        self.accepts(&Input::Startup).await
    }

    /// Reset state machine (for testing only)
    #[cfg(test)]
    pub async fn reset(&self) {
        let mut current = self.current_state.write().await;
        *current = InstanceFSM::initial_state();

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

    /// The failure the hand-written pair-only table could not see.
    ///
    /// It said `(Active, Active) => true`, so a caller could reach `Active`
    /// from `Active` under *any* event name and leave a history entry
    /// claiming that event happened. Here `Shutdown` from `Active` has one
    /// destination, `Sunsetting`, so a recorded shutdown always is one.
    ///
    /// Before this change the assertions below read the other way: the
    /// instance was still `Active` and the history said `shutdown`.
    #[tokio::test]
    async fn a_shutdown_event_can_no_longer_be_recorded_without_sunsetting() {
        let manager = ProxyStateManager::new("test-instance".to_string());
        manager.handle_registration_complete().await.unwrap();

        manager.handle_shutdown().await.unwrap();

        assert_eq!(manager.current_state().await, State::Sunsetting);
        let history = manager.get_history().await;
        let shutdowns: Vec<&StateTransition> =
            history.iter().filter(|t| t.event == "shutdown").collect();
        assert_eq!(shutdowns.len(), 1);
        assert_eq!(shutdowns[0].from, State::Active);
        assert_eq!(shutdowns[0].to, State::Sunsetting);
    }

    /// `Sunsetting` used to accept anything that left it in `Sunsetting`,
    /// including a second shutdown. Only a heartbeat holds it there now.
    #[tokio::test]
    async fn a_second_shutdown_while_sunsetting_is_refused() {
        let manager = ProxyStateManager::new("test-instance".to_string());
        manager.handle_registration_complete().await.unwrap();
        manager.handle_shutdown().await.unwrap();
        let before = manager.history_count().await;

        assert!(manager.handle_shutdown().await.is_err());

        assert_eq!(manager.current_state().await, State::Sunsetting);
        assert_eq!(
            manager.history_count().await,
            before,
            "a refused event must not leave a history entry"
        );
    }

    /// A proxy that lost its control connection registers again while still
    /// serving. That has to be accepted, and has to leave it active.
    ///
    /// The second assertion is where the two hand-written copies used to
    /// contradict each other: `can_handle_registration` matched only
    /// `Pending` and answered no, while `is_valid_transition` matched the
    /// pair `(Active, Active)` and let the very same registration through.
    /// One table cannot disagree with itself.
    #[tokio::test]
    async fn re_registration_while_active_is_accepted() {
        let manager = ProxyStateManager::new("test-instance".to_string());
        manager.handle_registration_complete().await.unwrap();

        let result = manager.handle_registration_complete().await;

        assert_eq!(result.unwrap(), State::Active);
        assert!(manager.is_active().await);
        assert!(manager.can_handle_registration().await);
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

    /// The five `can_handle_*` predicates were a hand-written copy of the
    /// table beside the hand-written copy in `is_valid_transition`. They now
    /// read the table, so this asserts they cannot disagree with it.
    #[tokio::test]
    async fn capability_checks_agree_with_the_table() {
        let manager = ProxyStateManager::new("test-instance".to_string());

        for state in InstanceFSM::states() {
            manager.force_set_state(state.clone(), "table check").await;
            let valid = InstanceFSM::valid_inputs(&state);

            assert_eq!(
                manager.can_handle_registration().await,
                valid.contains(&Input::Registered),
                "registration in {state:?}"
            );
            assert_eq!(
                manager.can_handle_heartbeat().await,
                valid.contains(&Input::_Heartbeat),
                "heartbeat in {state:?}"
            );
            assert_eq!(
                manager.can_handle_shutdown().await,
                valid.contains(&Input::Shutdown),
                "shutdown in {state:?}"
            );
            assert_eq!(
                manager.can_handle_silenced().await,
                valid.contains(&Input::Silenced),
                "silenced in {state:?}"
            );
            assert_eq!(
                manager.can_handle_startup().await,
                valid.contains(&Input::Startup),
                "startup in {state:?}"
            );
        }
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
