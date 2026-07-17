//! Supervisor client implementation
//!
//! Supervisor client integrated with yasm state machine,
//! managing deflux instance lifecycle through events

use crate::supervisor::state::{DefluxStateManager, State};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use tokio::time::{Duration, interval};
use tracing::{debug, error, info, warn};

use denira_workit::SupervisorConfig;
use denira_workit::{MessageBuilder, SdkResult, WorkerCommand, WorkerInfo, WorkerType, WorkitClient, WorkitError, deflux::PipelineData};

/// Restart signal type
#[derive(Debug, Clone)]
pub enum RestartSignal {
    /// Graceful restart
    Graceful,
    /// Hard restart (stop then start)
    Hard,
}

/// Deflux supervisor client
///
/// Supervisor client with integrated state machine, responsible for:
/// - WebSocket connection with supervisor
/// - deflux instance lifecycle management
/// - Configuration delivery and command processing
/// - Status reporting and monitoring
#[derive(Debug)]
pub struct DefluxSupervisorClient {
    /// WorkIt WebSocket client
    workit_client: Arc<Mutex<WorkitClient>>,
    /// State manager
    state_manager: Arc<DefluxStateManager>,
    /// Configuration
    config: SupervisorConfig,
    /// Restart signal sender (optional)
    restart_tx: Option<broadcast::Sender<RestartSignal>>,
}

impl DefluxSupervisorClient {
    /// Create a new supervisor client
    pub fn new(config: SupervisorConfig) -> SdkResult<Self> {
        // Validate configuration
        config.validate().map_err(WorkitError::ConfigurationError)?;

        // Create workit supervisor configuration
        let supervisor_config = SupervisorConfig::new(
            config.associated_id.clone(),
            config.secret.clone(),
            config.addr.clone(),
        );

        // Validate workit configuration
        supervisor_config
            .validate()
            .map_err(WorkitError::ConfigurationError)?;

        // Create worker info
        let worker_info = WorkerInfo::new(
            WorkerType::Deflux(env!("CARGO_PKG_VERSION").to_string()),
            "0.0.0.0".to_string(),
            vec![80, 443],
        );

        // Create workit client
        let workit_client = WorkitClient::new(supervisor_config, worker_info);

        // Create state manager
        let state_manager = Arc::new(DefluxStateManager::new(config.associated_id.clone()));

        Ok(Self {
            workit_client: Arc::new(Mutex::new(workit_client)),
            state_manager,
            config,
            restart_tx: None,
        })
    }

    /// Set restart signal sender
    pub fn set_restart_channel(&mut self, restart_tx: broadcast::Sender<RestartSignal>) {
        self.restart_tx = Some(restart_tx);
    }

    /// Connect to supervisor
    pub async fn connect(&mut self) -> SdkResult<()> {
        let mut client = self.workit_client.lock().await;
        info!(
            "Connecting to supervisor at {}",
            self.config.addr
        );
        client.connect().await?;
        info!("Connected to supervisor");
        Ok(())
    }

    /// Perform authentication
    pub async fn authenticate(&mut self) -> SdkResult<()> {
        let mut client = self.workit_client.lock().await;
        info!("Authenticating with supervisor");
        client.authenticate().await?;
        info!("Authentication successful");
        Ok(())
    }

    /// Register instance
    pub async fn register(&mut self) -> SdkResult<()> {
        let mut client = self.workit_client.lock().await;
        info!("Registering with supervisor");
        client.register().await?;
        info!("Registration successful");
        Ok(())
    }

    /// Handle registration complete, transition to Active state
    pub async fn handle_registration_complete(&mut self) -> SdkResult<()> {
        self.state_manager
            .handle_registration_complete()
            .await
            .map_err(|e| {
                WorkitError::ConfigurationError(format!("State transition error: {}", e))
            })?;
        info!("Instance transitioned to Active state");
        Ok(())
    }

    /// Run client main loop
    pub async fn run(&mut self) -> SdkResult<()> {
        info!(
            "Starting deflux supervisor client main loop for instance: {}",
            self.config.associated_id
        );

        // Start background tasks
        self.start_background_tasks().await;

        // Main loop - waiting for commands and processing events
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;

            // Check connection status
            if !self.is_ready().await {
                warn!("Supervisor connection lost, attempting to reconnect...");
                match self.reconnect_with_backoff().await {
                    Ok(_) => {
                        info!("Successfully reconnected to supervisor");
                        continue;
                    }
                    Err(e) => {
                        error!("Failed to reconnect after multiple attempts: {}", e);
                        break;
                    }
                }
            }
        }

        info!("Deflux supervisor client main loop ended");
        Ok(())
    }

    /// Start client and complete the full connection flow
    pub async fn start(&mut self) -> SdkResult<()> {
        info!(
            "Starting deflux supervisor client for instance: {}",
            self.config.associated_id
        );

        // Register state machine callbacks
        self.setup_state_callbacks().await;

        // Connect and register
        self.connect_and_register().await?;

        // Start background tasks
        self.start_background_tasks().await;

        info!("Deflux supervisor client started successfully");
        Ok(())
    }

    /// Connect to supervisor and complete authentication and registration flow
    async fn connect_and_register(&mut self) -> SdkResult<()> {
        let mut client = self.workit_client.lock().await;

        info!(
            "Connecting to supervisor at {}",
            self.config.addr
        );

        // Connect
        client.connect().await?;
        info!("Connected to supervisor");

        // Authenticate
        client.authenticate().await?;
        info!("Authentication successful");

        // Register
        client.register().await?;
        info!("Registration successful");

        // State machine: transition to Active state
        self.state_manager
            .handle_registration_complete()
            .await
            .map_err(|e| WorkitError::ConfigurationError(format!("State machine error: {}", e)))?;

        Ok(())
    }

    /// Set state machine callbacks
    async fn setup_state_callbacks(&self) {
        // Simplified state manager does not require complex callback setup
        // State changes are automatically logged
        info!(
            "State manager initialized for instance: {}",
            self.config.associated_id
        );
    }

    /// Start background tasks
    async fn start_background_tasks(&self) {
        // Start heartbeat task
        self.start_heartbeat_task();

        // Start command handling task
        self.start_command_handler();
    }

    /// Start heartbeat task
    fn start_heartbeat_task(&self) -> tokio::task::JoinHandle<()> {
        let client = Arc::clone(&self.workit_client);
        let heartbeat_interval = self.config.heartbeat_duration();
        let instance_id = self.config.associated_id.clone();

        tokio::spawn(async move {
            let mut interval = interval(heartbeat_interval);

            loop {
                interval.tick().await;

                let mut client_guard = client.lock().await;
                match client_guard.send_heartbeat().await {
                    Ok(_) => debug!("Heartbeat sent successfully for {}", instance_id),
                    Err(e) => {
                        error!("Failed to send heartbeat for {}: {}", instance_id, e);
                        // Heartbeat failure indicates connection issue
                        // The main loop will detect this and trigger reconnection
                        break;
                    }
                }
            }
        })
    }

    /// Start command handling task
    fn start_command_handler(&self) -> tokio::task::JoinHandle<()> {
        let _client = Arc::clone(&self.workit_client);
        let _state_manager = Arc::clone(&self.state_manager);
        let instance_id = self.config.associated_id.clone();

        tokio::spawn(async move {
            info!("Starting command handler for {}", instance_id);

            // TODO: Implement actual command reception and handling logic
            // This requires WorkitClient to provide a command reception interface
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                // Placeholder: waiting for workit library to provide command reception API
            }
        })
    }

    /// Send state update to supervisor (internal method)
    #[allow(dead_code)]
    async fn send_state_update_internal(
        _client: Arc<Mutex<WorkitClient>>,
        instance_id: &str,
        from: &str,
        input: &str,
        to: &str,
    ) -> SdkResult<()> {
        let status_data = serde_json::json!({
            "instance_id": instance_id,
            "transition": {
                "from": from,
                "input": input,
                "to": to,
            },
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });

        let _status_message = MessageBuilder::status_update(to.to_string(), Some(status_data));

        // TODO: Wait for WorkitClient to provide general message sending API
        info!("Would send state update: {} -> {}", from, to);

        Ok(())
    }

    /// Send status update to supervisor
    pub async fn send_status_update(
        &self,
        status: &str,
        details: Option<serde_json::Value>,
    ) -> SdkResult<()> {
        let _status_message = MessageBuilder::status_update(status.to_string(), details);

        info!("Sending status update: {}", status);
        // TODO: Implement actual sending
        Ok(())
    }

    /// Send pipeline message to supervisor
    pub async fn send_pipeline_message(&self, pipeline_data: PipelineData) -> SdkResult<()> {
        let _pipeline_message = MessageBuilder::deflux_pipeline(
            pipeline_data.pipeline_id.clone(),
            pipeline_data.description.clone(),
            pipeline_data.stages,
        );

        info!(
            "Sending pipeline message for: {}",
            pipeline_data.pipeline_id
        );
        // TODO: Implement actual sending
        Ok(())
    }

    /// Handle command from supervisor
    pub async fn handle_command(&self, command: WorkerCommand) -> SdkResult<serde_json::Value> {
        info!("Handling command: {:?}", command);

        match command {
            WorkerCommand::Shutdown {
                timeout_seconds,
                reason,
            } => {
                info!(
                    "Received shutdown command, timeout: {:?}, reason: {:?}",
                    timeout_seconds, reason
                );

                // State machine: transition to Sunsetting state
                match self.state_manager.handle_shutdown().await {
                    Ok(_) => {
                        // Execute actual shutdown logic
                        self.execute_shutdown(timeout_seconds.map(|t| t as u32))
                            .await;

                        // State machine: transition to Terminated state
                        let _ = self.state_manager.handle_silenced().await;

                        Ok(serde_json::json!({
                            "success": true,
                            "message": "Shutdown initiated"
                        }))
                    }
                    Err(e) => {
                        error!("Failed to transition to shutdown state: {}", e);
                        Ok(serde_json::json!({
                            "success": false,
                            "error": e
                        }))
                    }
                }
            }

            WorkerCommand::Restart {
                timeout_seconds,
                preserve_state,
            } => {
                info!(
                    "Received restart command, timeout: {:?}, preserve_state: {}",
                    timeout_seconds, preserve_state
                );

                // State machine: sunset then restart
                let _ = self.state_manager.handle_shutdown().await;
                let _ = self.state_manager.handle_silenced().await;

                // Execute restart logic
                self.execute_restart(preserve_state).await;

                Ok(serde_json::json!({
                    "success": true,
                    "message": "Restart initiated"
                }))
            }

            WorkerCommand::Reload => {
                info!("Received reload command");

                // Implement reload logic (reload config, routing rules, etc.)
                Ok(serde_json::json!({
                    "success": true,
                    "message": "Reload completed"
                }))
            }

            WorkerCommand::UpdateConfig { config } => {
                info!("Received config update: {:?}", config);

                // Implement configuration update logic
                match self.apply_config_update(config).await {
                    Ok(_) => Ok(serde_json::json!({
                        "success": true,
                        "message": "Configuration updated"
                    })),
                    Err(e) => Ok(serde_json::json!({
                        "success": false,
                        "error": e
                    })),
                }
            }

            WorkerCommand::GetStatus => {
                info!("Received status request");

                let current_state = self.state_manager.current_state().await;
                let history_count = self.state_manager.history_count().await;

                Ok(serde_json::json!({
                    "instance_id": self.config.associated_id,
                    "version": env!("CARGO_PKG_VERSION"),
                    "state": format!("{:?}", current_state),
                    "is_active": self.state_manager.is_active().await,
                    "is_terminated": self.state_manager.is_terminated().await,
                    "transition_count": history_count,
                    "uptime": "placeholder", // TODO: Implement actual uptime calculation
                }))
            }

            WorkerCommand::GetMetrics => {
                info!("Received metrics request");

                Ok(serde_json::json!({
                    "requests_per_second": 0,
                    "response_time_avg": 0.0,
                    "error_rate": 0.0,
                    "active_connections": 0,
                    "memory_usage": "placeholder",
                }))
            }

            WorkerCommand::Custom { command, args } => {
                info!(
                    "Received custom command: {} with args: {:?}",
                    command, args
                );

                match command.as_str() {
                    "graceful_restart" => {
                        info!("Initiating graceful restart via supervisor command");

                        if let Some(restart_tx) = &self.restart_tx {
                            match restart_tx.send(RestartSignal::Graceful) {
                                Ok(_) => {
                                    info!("Graceful restart signal sent successfully");
                                    Ok(serde_json::json!({
                                        "success": true,
                                        "message": "Graceful restart initiated"
                                    }))
                                }
                                Err(e) => {
                                    error!("Failed to send graceful restart signal: {}", e);
                                    Ok(serde_json::json!({
                                        "success": false,
                                        "error": format!("Failed to send restart signal: {}", e)
                                    }))
                                }
                            }
                        } else {
                            warn!("Restart channel not configured");
                            Ok(serde_json::json!({
                                "success": false,
                                "error": "Restart channel not configured"
                            }))
                        }
                    }
                    _ => {
                        warn!("Unknown custom command: {}", command);
                        Ok(serde_json::json!({
                            "success": false,
                            "error": format!("Unknown command: {}", command)
                        }))
                    }
                }
            }
        }
    }

    /// Execute shutdown logic
    async fn execute_shutdown(&self, timeout_seconds: Option<u32>) {
        let timeout = timeout_seconds.unwrap_or(30);
        info!("Executing graceful shutdown with timeout: {}s", timeout);

        // TODO: Implement actual shutdown logic
        // 1. Stop accepting new connections
        // 2. Wait for existing connections to complete
        // 3. Close all resources
    }

    /// Execute restart logic
    async fn execute_restart(&self, preserve_state: bool) {
        info!("Executing restart, preserve_state: {}", preserve_state);

        // TODO: Implement actual restart logic
        // 1. Save state (if needed)
        // 2. Reload configuration
        // 3. Restart service
    }

    /// Apply configuration update
    async fn apply_config_update(&self, _config: serde_json::Value) -> Result<(), String> {
        info!("Applying configuration update");

        // TODO: Implement configuration update logic
        // 1. Validate configuration
        // 2. Apply configuration
        // 3. Reload related components

        Ok(())
    }

    /// Check if client is ready
    pub async fn is_ready(&self) -> bool {
        let client = self.workit_client.lock().await;
        client.is_connected()
            && client.is_authenticated()
            && client.is_registered()
            && self.state_manager.is_active().await
    }

    /// Gracefully disconnect
    pub async fn disconnect(&mut self) -> SdkResult<()> {
        info!("Disconnecting from supervisor");

        // State machine: transition to closed state
        if self.state_manager.is_active().await {
            let _ = self.state_manager.handle_shutdown().await;
        }
        let _ = self.state_manager.handle_silenced().await;

        // Disconnect
        let mut client = self.workit_client.lock().await;
        client.disconnect().await?;

        info!("Disconnected from supervisor");
        Ok(())
    }

    /// Get current state
    pub async fn current_state(&self) -> State {
        self.state_manager.current_state().await
    }

    /// Reconnect with exponential backoff
    async fn reconnect_with_backoff(&mut self) -> SdkResult<()> {
        const MAX_RETRIES: u32 = 5;
        const BASE_DELAY_MS: u64 = 1000;
        const MAX_DELAY_MS: u64 = 30000;

        for attempt in 1..=MAX_RETRIES {
            let delay_ms = std::cmp::min(
                BASE_DELAY_MS * 2_u64.pow(attempt - 1),
                MAX_DELAY_MS
            );

            info!("Reconnection attempt {} of {} (delay: {}ms)", attempt, MAX_RETRIES, delay_ms);
            
            // Wait before attempting reconnection
            if attempt > 1 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }

            // Try to reconnect
            match self.connect_and_register().await {
                Ok(_) => {
                    info!("Reconnection successful on attempt {}", attempt);
                    return Ok(());
                }
                Err(e) => {
                    warn!("Reconnection attempt {} failed: {}", attempt, e);
                    if attempt == MAX_RETRIES {
                        return Err(e);
                    }
                }
            }
        }

        Err(WorkitError::ConfigurationError(
            "Maximum reconnection attempts exceeded".to_string()
        ))
    }

    /// Force disconnect and reconnect
    pub async fn force_reconnect(&mut self) -> SdkResult<()> {
        info!("Forcing reconnection to supervisor");
        
        // Try to disconnect gracefully first
        let _ = self.disconnect().await;
        
        // Wait a bit before reconnecting
        tokio::time::sleep(Duration::from_millis(2000)).await;
        
        // Reconnect with backoff
        self.reconnect_with_backoff().await
    }

    /// Get state manager (for testing)
    #[cfg(test)]
    pub fn state_manager(&self) -> &Arc<DefluxStateManager> {
        &self.state_manager
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_config() -> SupervisorConfig {
        SupervisorConfig {
            associated_id: "test-worker".to_string(),
            secret: "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678".to_string(),
            addr: "http://localhost:8080".to_string(),
            heartbeat_interval: 30,
            request_timeout: 10,
            retry_count: 3,
        }
    }

    #[test]
    fn test_client_creation() {
        let config = create_test_config();
        let client = DefluxSupervisorClient::new(config);
        assert!(client.is_ok());
    }

    #[test]
    fn test_invalid_config() {
        let mut config = create_test_config();
        config.secret = "invalid".to_string();

        let client = DefluxSupervisorClient::new(config);
        assert!(client.is_err());
    }

    #[tokio::test]
    async fn test_state_integration() {
        let config = create_test_config();
        let client = DefluxSupervisorClient::new(config).unwrap();

        // Initial state should be Pending
        assert_eq!(client.current_state().await, State::Pending);

        // Check state manager integration
        let state_manager = client.state_manager();
        assert_eq!(state_manager.instance_id(), "test-worker");
    }

    #[tokio::test]
    async fn test_command_handling() {
        let config = create_test_config();
        let client = DefluxSupervisorClient::new(config).unwrap();

        // Test status query command
        let result = client.handle_command(WorkerCommand::GetStatus).await;
        assert!(result.is_ok());

        let status = result.unwrap();
        assert_eq!(status["instance_id"], "test-worker");
        assert_eq!(status["state"], "Pending");
    }
}
