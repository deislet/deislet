//! Restart coordinator
//!
//! Coordinates the entire graceful restart flow.

use super::listener::ListenerManager;
use super::state::RestartState;
use super::transfer::{receive_fds, send_fds};
use super::GracefulRestartConfig;
use anyhow::{Context, Result};
use std::env;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

/// Environment variable: mark as a restarted child process
const ENV_GRACEFUL_RESTART: &str = "DEFLUX_GRACEFUL_RESTART";

/// Environment variable: Unix socket path
const ENV_SOCKET_PATH: &str = "DEFLUX_RESTART_SOCKET";

/// Restart coordinator
pub struct RestartCoordinator {
    config: GracefulRestartConfig,
    state: RestartState,
    listener_manager: Arc<tokio::sync::Mutex<ListenerManager>>,
}

impl RestartCoordinator {
    /// Create new coordinator
    pub fn new(
        config: GracefulRestartConfig,
        listener_manager: Arc<tokio::sync::Mutex<ListenerManager>>,
    ) -> Self {
        Self {
            config,
            state: RestartState::new(),
            listener_manager,
        }
    }

    /// Get state
    pub fn state(&self) -> &RestartState {
        &self.state
    }

    /// Check if it is a restarted child process
    pub fn is_graceful_restart_child() -> bool {
        env::var(ENV_GRACEFUL_RESTART).is_ok()
    }

    /// Get socket path (from environment variable)
    pub fn get_socket_path_from_env() -> Option<String> {
        env::var(ENV_SOCKET_PATH).ok()
    }

    /// Receive listeners from parent process (called by child process)
    pub async fn receive_listeners_from_parent(&self) -> Result<()> {
        info!("Child process: receiving listeners from parent");

        let socket_path = Self::get_socket_path_from_env()
            .unwrap_or_else(|| self.config.socket_path.clone());

        // Receive FDs
        let addr_to_fd = tokio::task::spawn_blocking(move || receive_fds(&socket_path))
            .await
            .context("Failed to spawn receive task")??;

        info!("Child process: received {} FDs", addr_to_fd.len());

        // Rebuild listeners from FDs
        let mut manager = self.listener_manager.lock().await;
        manager
            .rebuild_from_fds(addr_to_fd)
            .await
            .context("Failed to rebuild listeners from FDs")?;

        info!(
            "Child process: successfully rebuilt {} listeners",
            manager.count()
        );

        Ok(())
    }

    /// Initiate graceful restart (called by parent process)
    pub async fn initiate_graceful_restart(
        &self,
        shutdown_tx: broadcast::Sender<()>,
    ) -> Result<()> {
        info!("Initiating graceful restart");

        // Transition state
        self.state.transition_to_preparing_transfer().await;

        // Extract listener FDs
        let (fds, addrs) = {
            let manager = self.listener_manager.lock().await;
            if manager.count() == 0 {
                warn!("No listeners to transfer");
                return Ok(());
            }
            manager.extract_fds()
        };

        info!("Extracted {} FDs for transfer", fds.len());

        // Spawn new process
        self.spawn_new_process()
            .context("Failed to spawn new process")?;

        // Transition state
        self.state.transition_to_transferring_fds().await;

        // Send FDs (execute in blocking thread)
        let socket_path = self.config.socket_path.clone();
        let max_retry = self.config.max_retry;
        let retry_interval = Duration::from_secs(self.config.retry_interval);

        tokio::task::spawn_blocking(move || {
            send_fds(fds, addrs, &socket_path, max_retry, retry_interval)
        })
        .await
        .context("Failed to spawn send task")??;

        info!("FDs transferred successfully");

        // Wait for new process to be ready
        self.state.transition_to_waiting_for_new_process().await;
        info!(
            "Waiting {} seconds for new process to be ready",
            self.config.wait_after_transfer
        );
        tokio::time::sleep(Duration::from_secs(self.config.wait_after_transfer)).await;

        // Stop accepting new connections
        self.state.transition_to_graceful_shutdown().await;
        info!("Stopped accepting new connections, waiting for existing connections to finish");

        // Send shutdown signal
        if let Err(e) = shutdown_tx.send(()) {
            warn!("Failed to send shutdown signal: {}", e);
        }

        Ok(())
    }

    /// Spawn new process
    fn spawn_new_process(&self) -> Result<()> {
        let current_exe = env::current_exe().context("Failed to get current executable path")?;

        let args: Vec<String> = env::args().collect();

        info!("Spawning new process: {:?} {:?}", current_exe, args);

        let mut command = Command::new(&current_exe);

        // Pass all command line arguments (except the first program name)
        if args.len() > 1 {
            command.args(&args[1..]);
        }

        // Set environment variables
        command
            .env(ENV_GRACEFUL_RESTART, "1")
            .env(ENV_SOCKET_PATH, &self.config.socket_path);

        // On Unix, use exec to replace the current process (but here we need fork + exec)
        // Use standard spawn
        let child = command
            .spawn()
            .context("Failed to spawn child process")?;

        info!("Spawned new process with PID {}", child.id());

        // Do not wait for child process (let it run independently)
        std::mem::forget(child);

        Ok(())
    }

    /// Handle graceful shutdown (received SIGTERM/SIGQUIT)
    pub async fn handle_graceful_shutdown(&self, shutdown_tx: broadcast::Sender<()>) {
        info!("Graceful shutdown requested");

        self.state.transition_to_graceful_shutdown().await;

        // Send shutdown signal
        if let Err(e) = shutdown_tx.send(()) {
            error!("Failed to send shutdown signal: {}", e);
        }

        // Wait for some time to let connections complete
        tokio::time::sleep(Duration::from_secs(self.config.graceful_shutdown_timeout)).await;

        self.state.transition_to_terminated().await;
        info!("Graceful shutdown complete");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::state::RestartPhase;

    #[test]
    fn test_env_check() {
        // Should not be a child process by default
        assert!(!RestartCoordinator::is_graceful_restart_child());

        // Set environment variables
        unsafe {
            env::set_var(ENV_GRACEFUL_RESTART, "1");
        }
        assert!(RestartCoordinator::is_graceful_restart_child());

        // Clean up
        unsafe {
            env::remove_var(ENV_GRACEFUL_RESTART);
        }
    }

    #[test]
    fn test_socket_path_env() {
        let test_path = "/tmp/test-socket.sock";

        unsafe {
            env::set_var(ENV_SOCKET_PATH, test_path);
        }
        assert_eq!(
            RestartCoordinator::get_socket_path_from_env(),
            Some(test_path.to_string())
        );

        unsafe {
            env::remove_var(ENV_SOCKET_PATH);
        }
        assert_eq!(RestartCoordinator::get_socket_path_from_env(), None);
    }

    #[tokio::test]
    async fn test_state_transitions() {
        let listener_manager = Arc::new(tokio::sync::Mutex::new(ListenerManager::new()));
        let config = GracefulRestartConfig::default();
        let coordinator = RestartCoordinator::new(config, listener_manager);

        assert_eq!(
            coordinator.state().get_phase().await,
            RestartPhase::Setup
        );

        coordinator.state().transition_to_running().await;
        assert_eq!(
            coordinator.state().get_phase().await,
            RestartPhase::Running
        );
    }
}
