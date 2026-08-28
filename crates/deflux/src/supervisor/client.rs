//! Supervisor client: registers this instance with denira, keeps the
//! heartbeat going, and applies the commands and configuration it pushes.
//!
//! Commands arrive as `CommandRequest { command, args_json }` where `command`
//! is one of denira's command type names. Process-level commands are handed
//! to [`crate::restart::ProcessControl`]; the rest are answered here.
//!
//! Configuration arrives as `ConfigUpdate { key, value_json, version }`. The
//! keys this instance understands:
//!
//! - `apps`: the complete application list; replaces everything.
//! - `app.<id>`: one application, inserted or updated.
//! - `system`: a system configuration overlay (traffic policy only, see
//!   [`SupervisorSystemConfig`]).

use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use denira_workit::pb::supervisor_message::Payload;
use denira_workit::pb::{CommandRequest, CommandResponse};
use denira_workit::{
    SdkResult, SupervisorConfig, WorkerInfo, WorkerType, WorkitClient, WorkitError,
};
use serde_json::Value as JsonValue;
use tokio::sync::broadcast;
use tokio::time::{Duration, MissedTickBehavior, interval};
use tracing::{debug, error, info, warn};

use crate::config::{
    AppConfig, AppConfigEntry, ConfigError, ConfigManager, SupervisorAppsConfig,
    SupervisorSystemConfig, Validatable,
};
use crate::restart::ProcessCommand;
use crate::supervisor::state::{DefluxStateManager, State};

/// `ConfigUpdate` key carrying the complete application list.
pub const CONFIG_KEY_APPS: &str = "apps";
/// `ConfigUpdate` key prefix for a single application (`app.<id>`).
pub const CONFIG_KEY_APP_PREFIX: &str = "app.";
/// `ConfigUpdate` key carrying a system configuration overlay.
pub const CONFIG_KEY_SYSTEM: &str = "system";

pub struct DefluxSupervisorClient {
    workit_client: WorkitClient,
    state_manager: Arc<DefluxStateManager>,
    config: SupervisorConfig,
    /// Where commands that act on the whole process are sent.
    process_tx: Option<broadcast::Sender<ProcessCommand>>,
    /// Configuration store updated by pushes. Absent in tests.
    config_manager: Option<Arc<ConfigManager>>,
    started_at: Instant,
}

enum Event {
    Heartbeat,
    Received(Result<Option<Payload>, WorkitError>),
}

impl DefluxSupervisorClient {
    /// Create a client announcing the default HTTP/HTTPS ports. Use
    /// [`with_listen_ports`](Self::with_listen_ports) to announce the real ones.
    pub fn new(config: SupervisorConfig) -> SdkResult<Self> {
        config.validate().map_err(WorkitError::ConfigurationError)?;
        let state_manager = Arc::new(DefluxStateManager::new(config.associated_id.clone()));
        let workit_client = Self::build_workit_client(&config, vec![80, 443]);
        Ok(Self {
            workit_client,
            state_manager,
            config,
            process_tx: None,
            config_manager: None,
            started_at: Instant::now(),
        })
    }

    fn build_workit_client(config: &SupervisorConfig, ports: Vec<u16>) -> WorkitClient {
        let worker_info = WorkerInfo::new(
            WorkerType::Deflux(env!("CARGO_PKG_VERSION").to_string()),
            "0.0.0.0".to_string(),
            ports,
        );
        WorkitClient::new(config.clone(), worker_info)
    }

    /// Announce the ports this instance actually listens on.
    pub fn with_listen_ports(mut self, ports: Vec<u16>) -> Self {
        self.workit_client = Self::build_workit_client(&self.config, ports);
        self
    }

    /// Attach the configuration store that pushes are applied to.
    pub fn with_config_manager(mut self, config_manager: Arc<ConfigManager>) -> Self {
        self.config_manager = Some(config_manager);
        self
    }

    /// Attach the channel that process-level commands are dispatched on.
    pub fn set_process_channel(&mut self, process_tx: broadcast::Sender<ProcessCommand>) {
        self.process_tx = Some(process_tx);
    }

    /// Connect, authenticate and register; the instance is active afterwards.
    pub async fn start(&mut self) -> SdkResult<()> {
        info!(instance = %self.config.associated_id, addr = %self.config.addr, "connecting to supervisor");
        self.connect_and_register().await?;
        info!(instance = %self.config.associated_id, "registered with supervisor");
        Ok(())
    }

    async fn connect_and_register(&mut self) -> SdkResult<()> {
        self.workit_client.connect().await?;
        self.workit_client.authenticate().await?;
        self.workit_client.register().await?;
        self.state_manager
            .handle_registration_complete()
            .await
            .map_err(|e| WorkitError::ConfigurationError(format!("state machine error: {e}")))?;
        Ok(())
    }

    /// Serve heartbeats and incoming messages until the connection is lost
    /// beyond recovery.
    pub async fn run(&mut self) -> SdkResult<()> {
        let mut heartbeat = interval(self.config.heartbeat_duration());
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            let event = tokio::select! {
                _ = heartbeat.tick() => Event::Heartbeat,
                received = self.workit_client.receive_message() => Event::Received(received),
            };

            match event {
                Event::Heartbeat => {
                    if let Err(e) = self.workit_client.send_heartbeat().await {
                        warn!(error = %e, "heartbeat failed; reconnecting");
                        self.reconnect_with_backoff().await?;
                    } else {
                        debug!(instance = %self.config.associated_id, "heartbeat sent");
                    }
                }
                Event::Received(Ok(Some(payload))) => self.handle_payload(payload).await,
                Event::Received(Ok(None)) => debug!("empty payload from supervisor ignored"),
                Event::Received(Err(e)) => {
                    warn!(error = %e, "supervisor stream failed; reconnecting");
                    self.reconnect_with_backoff().await?;
                }
            }
        }
    }

    async fn handle_payload(&mut self, payload: Payload) {
        match payload {
            Payload::CommandRequest(request) => {
                let response = self.handle_command_request(&request).await;
                if let Err(e) = self
                    .workit_client
                    .send_message(Payload::CommandResponse(response))
                    .await
                {
                    warn!(command = %request.command, id = %request.id, error = %e, "command response not delivered");
                }
            }
            Payload::ConfigUpdate(update) => {
                self.apply_config_update(&update.key, &update.value_json, &update.version)
                    .await
            }
            Payload::AppConfigs(apps) => {
                self.apply_config_update(CONFIG_KEY_APPS, &apps.configs_json, &apps.version)
                    .await
            }
            Payload::SystemConfig(system) => {
                self.apply_config_update(CONFIG_KEY_SYSTEM, &system.config_json, &system.version)
                    .await
            }
            Payload::AuthResponse(_) | Payload::RegisterResponse(_) => {
                debug!("late handshake response ignored")
            }
            other => debug!(?other, "unexpected payload from supervisor ignored"),
        }
    }

    /// Execute a command and build the response to send back.
    pub async fn handle_command_request(&self, request: &CommandRequest) -> CommandResponse {
        info!(command = %request.command, id = %request.id, "command received");
        let outcome: Result<String, String> = match request.command.as_str() {
            "shutdown" => self
                .dispatch(ProcessCommand::Shutdown)
                .await
                .map(|_| "graceful shutdown initiated".to_string()),
            "restart" | "reload" | "graceful_restart" => self
                .dispatch(ProcessCommand::Upgrade)
                .await
                .map(|_| "zero-downtime restart initiated".to_string()),
            "get_status" => Ok(self.status().await.to_string()),
            "get_metrics" => Err("metrics are not implemented".to_string()),
            other => Err(format!("unknown command: {other}")),
        };

        match outcome {
            Ok(message) => CommandResponse {
                id: request.id.clone(),
                success: true,
                message,
                error: String::new(),
            },
            Err(error) => {
                warn!(command = %request.command, id = %request.id, %error, "command rejected");
                CommandResponse {
                    id: request.id.clone(),
                    success: false,
                    message: String::new(),
                    error,
                }
            }
        }
    }

    async fn dispatch(&self, command: ProcessCommand) -> Result<(), String> {
        let Some(process_tx) = &self.process_tx else {
            return Err("process control is not attached".to_string());
        };
        if command == ProcessCommand::Shutdown {
            self.state_manager.handle_shutdown().await?;
        }
        process_tx
            .send(command)
            .map(|_| ())
            .map_err(|e| format!("process control is not listening: {e}"))
    }

    /// Status report answered to `get_status`.
    pub async fn status(&self) -> JsonValue {
        serde_json::json!({
            "instance_id": self.config.associated_id,
            "version": env!("CARGO_PKG_VERSION"),
            "state": format!("{:?}", self.state_manager.current_state().await),
            "is_active": self.state_manager.is_active().await,
            "uptime_secs": self.started_at.elapsed().as_secs(),
            "transition_count": self.state_manager.history_count().await,
        })
    }

    async fn apply_config_update(&self, key: &str, value_json: &str, version: &str) {
        let Some(config_manager) = &self.config_manager else {
            warn!(
                key,
                "configuration push ignored: no configuration store attached"
            );
            return;
        };
        let version = DateTime::parse_from_rfc3339(version)
            .map(|v| v.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());

        let outcome = if key == CONFIG_KEY_APPS {
            match SupervisorAppsConfig::from_json(value_json) {
                Ok(apps) => {
                    config_manager
                        .replace_app_configs(apps.get_apps(), version)
                        .await
                }
                Err(e) => Err(e),
            }
        } else if let Some(app_id) = key.strip_prefix(CONFIG_KEY_APP_PREFIX) {
            match serde_json::from_str::<AppConfig>(value_json)
                .map_err(ConfigError::from)
                .and_then(|app| app.validate().map(|_| app))
            {
                Ok(app) => {
                    if app.id != app_id {
                        warn!(key, id = %app.id, "app id in payload differs from key; using the key");
                    }
                    match serde_json::to_value(&app) {
                        Ok(config) => config_manager
                            .update_app_configs(vec![AppConfigEntry {
                                app_id: app_id.to_string(),
                                config,
                                version_timestamp: version,
                            }])
                            .await
                            .map(|_| ()),
                        Err(e) => Err(ConfigError::from(e)),
                    }
                }
                Err(e) => Err(e),
            }
        } else if key == CONFIG_KEY_SYSTEM {
            SupervisorSystemConfig::from_json(value_json)
                .and_then(|overlay| config_manager.apply_system_overlay(overlay))
        } else {
            Err(ConfigError::SupervisorError(format!(
                "unknown configuration key: {key}"
            )))
        };

        match outcome {
            Ok(()) => info!(key, %version, "configuration applied"),
            Err(e) => error!(key, %version, error = %e, "configuration push rejected"),
        }
    }

    pub async fn is_ready(&self) -> bool {
        self.workit_client.is_connected()
            && self.workit_client.is_authenticated()
            && self.workit_client.is_registered()
            && self.state_manager.is_active().await
    }

    pub async fn disconnect(&mut self) -> SdkResult<()> {
        if self.state_manager.is_active().await {
            let _ = self.state_manager.handle_shutdown().await;
        }
        let _ = self.state_manager.handle_silenced().await;
        self.workit_client.disconnect().await?;
        info!(instance = %self.config.associated_id, "disconnected from supervisor");
        Ok(())
    }

    pub async fn current_state(&self) -> State {
        self.state_manager.current_state().await
    }

    /// Reconnect with exponential backoff between attempts.
    async fn reconnect_with_backoff(&mut self) -> SdkResult<()> {
        const MAX_RETRIES: u32 = 5;
        const BASE_DELAY_MS: u64 = 1000;
        const MAX_DELAY_MS: u64 = 30000;

        let _ = self.workit_client.disconnect().await;
        for attempt in 1..=MAX_RETRIES {
            if attempt > 1 {
                let delay = Duration::from_millis(
                    (BASE_DELAY_MS * 2_u64.pow(attempt - 2)).min(MAX_DELAY_MS),
                );
                info!(attempt, ?delay, "waiting before reconnecting to supervisor");
                tokio::time::sleep(delay).await;
            }
            match self.connect_and_register().await {
                Ok(()) => {
                    info!(attempt, "reconnected to supervisor");
                    return Ok(());
                }
                Err(e) => warn!(attempt, error = %e, "reconnection attempt failed"),
            }
        }
        error!(
            retries = MAX_RETRIES,
            "giving up on the supervisor connection"
        );
        Err(WorkitError::ConnectionError(
            "maximum reconnection attempts exceeded".to_string(),
        ))
    }

    #[cfg(test)]
    pub fn state_manager(&self) -> &Arc<DefluxStateManager> {
        &self.state_manager
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> SupervisorConfig {
        SupervisorConfig {
            associated_id: "test-worker".to_string(),
            secret: "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678".to_string(),
            addr: "http://localhost:8080".to_string(),
            heartbeat_interval: 30,
            request_timeout: 10,
            retry_count: 3,
        }
    }

    fn request(command: &str) -> CommandRequest {
        CommandRequest {
            id: "cmd-1".to_string(),
            command: command.to_string(),
            args_json: "null".to_string(),
        }
    }

    #[test]
    fn creates_client() {
        assert!(DefluxSupervisorClient::new(test_config()).is_ok());
    }

    #[test]
    fn rejects_invalid_secret() {
        let mut config = test_config();
        config.secret = "invalid".to_string();
        assert!(DefluxSupervisorClient::new(config).is_err());
    }

    #[tokio::test]
    async fn starts_pending() {
        let client = DefluxSupervisorClient::new(test_config()).unwrap();
        assert_eq!(client.current_state().await, State::Pending);
        assert_eq!(client.state_manager().instance_id(), "test-worker");
        assert!(!client.is_ready().await);
    }

    #[tokio::test]
    async fn answers_status() {
        let client = DefluxSupervisorClient::new(test_config()).unwrap();
        let response = client.handle_command_request(&request("get_status")).await;
        assert!(response.success);
        let status: JsonValue = serde_json::from_str(&response.message).unwrap();
        assert_eq!(status["instance_id"], "test-worker");
        assert_eq!(status["state"], "Pending");
    }

    #[tokio::test]
    async fn rejects_unknown_and_unimplemented_commands() {
        let client = DefluxSupervisorClient::new(test_config()).unwrap();
        let response = client.handle_command_request(&request("get_metrics")).await;
        assert!(!response.success);
        let response = client.handle_command_request(&request("dance")).await;
        assert!(!response.success);
        assert!(response.error.contains("unknown command"));
    }

    #[tokio::test]
    async fn process_commands_need_a_channel() {
        let mut client = DefluxSupervisorClient::new(test_config()).unwrap();
        let response = client.handle_command_request(&request("restart")).await;
        assert!(!response.success);

        let (tx, mut rx) = broadcast::channel(2);
        client.set_process_channel(tx);
        let response = client.handle_command_request(&request("reload")).await;
        assert!(response.success, "{}", response.error);
        assert_eq!(rx.recv().await.unwrap(), ProcessCommand::Upgrade);
    }
}
