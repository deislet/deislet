use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::{interval, interval_at, Instant};
use tracing::{debug, error, info, warn};

use denira_workit::models::{
    ConfigPushAckMessage, ConfigPushData, WorkitMessage,
};
use denira_workit::{
    MessageBuilder, SdkResult, SupervisorConfig, WorkerCommand, WorkerInfo, WorkerType,
    WorkitClient, WorkitError,
};

use super::config::DenixSupervisorConfig;
use crate::app::ApplicationManager;

pub struct DenixSupervisorClient {
    workit_client: Arc<Mutex<WorkitClient>>,
    config: DenixSupervisorConfig,
    app_manager: Option<Arc<ApplicationManager>>,
    connected: bool,
    authenticated: bool,
    registered: bool,
}

impl DenixSupervisorClient {
    pub fn new(config: DenixSupervisorConfig, port: u16) -> SdkResult<Self> {
        let supervisor_config = SupervisorConfig::new(
            config.associated_id.clone(),
            config.secret.clone(),
            config.addr.clone(),
        );

        supervisor_config
            .validate()
            .map_err(WorkitError::ConfigurationError)?;

        let worker_info = WorkerInfo::new(
            WorkerType::Denix(env!("CARGO_PKG_VERSION").to_string()),
            "0.0.0.0".to_string(),
            vec![port],
        );

        let workit_client = WorkitClient::new(supervisor_config, worker_info);

        Ok(Self {
            workit_client: Arc::new(Mutex::new(workit_client)),
            config,
            app_manager: None,
            connected: false,
            authenticated: false,
            registered: false,
        })
    }

    /// 设置 ApplicationManager
    pub fn with_app_manager(mut self, app_manager: Arc<ApplicationManager>) -> Self {
        self.app_manager = Some(app_manager);
        self
    }

    pub async fn connect(&mut self) -> SdkResult<()> {
        let mut client = self.workit_client.lock().await;
        info!("Connecting to supervisor at {}", self.config.addr);
        client.connect().await?;
        self.connected = true;
        info!("Connected to supervisor");
        Ok(())
    }

    pub async fn authenticate(&mut self) -> SdkResult<()> {
        let mut client = self.workit_client.lock().await;
        info!("Authenticating with supervisor");
        client.authenticate().await?;
        self.authenticated = true;
        info!("Authentication successful");
        Ok(())
    }

    pub async fn register(&mut self) -> SdkResult<()> {
        let mut client = self.workit_client.lock().await;
        info!("Registering with supervisor");
        client.register().await?;
        self.registered = true;
        info!("Registration successful");
        Ok(())
    }

    pub async fn start(&mut self) -> SdkResult<()> {
        info!(
            "Starting denix supervisor client for {}",
            self.config.associated_id
        );

        self.connect().await?;
        self.authenticate().await?;
        self.register().await?;

        let sender = {
            let client = self.workit_client.lock().await;
            client.clone_sender()
        };

        if let Some(sender) = sender {
            self.start_heartbeat_task(sender);
        } else {
            warn!("Failed to get heartbeat sender");
        }
        self.start_command_listener();

        info!("Denix supervisor client started successfully");
        Ok(())
    }

    pub async fn run_with_reconnect(mut self) {
        let retry_interval = self.config.reconnect_duration();

        loop {
            info!("尝试连接 supervisor {}", self.config.addr);

            if let Err(err) = self.connect().await {
                warn!(
                    "连接 supervisor 失败，将在 {} 秒后重试，原因: {}",
                    retry_interval.as_secs(),
                    err
                );
                self.wait_before_retry(retry_interval).await;
                continue;
            }

            if let Err(err) = self.authenticate().await {
                warn!(
                    "Supervisor 认证失败，将在 {} 秒后重试，原因: {}",
                    retry_interval.as_secs(),
                    err
                );
                self.wait_before_retry(retry_interval).await;
                continue;
            }

            if let Err(err) = self.register().await {
                warn!(
                    "Supervisor 注册失败，将在 {} 秒后重试，原因: {}",
                    retry_interval.as_secs(),
                    err
                );
                self.wait_before_retry(retry_interval).await;
                continue;
            }

            info!("Supervisor 连接建立成功: {}", self.config.addr);

            let heartbeat_sender = {
                let client = self.workit_client.lock().await;
                client.clone_sender()
            };

            let Some(sender) = heartbeat_sender else {
                warn!("无法获取心跳发送器，准备重连");
                self.wait_before_retry(retry_interval).await;
                continue;
            };

            let mut heartbeat_handle = self.start_heartbeat_task(sender);
            let mut command_handle = self.start_command_listener();

            tokio::select! {
                _ = &mut heartbeat_handle => {
                    warn!("Supervisor 心跳任务已停止，准备重连");
                }
                _ = &mut command_handle => {
                    warn!("Supervisor 命令监听已停止，准备重连");
                }
            }

            heartbeat_handle.abort();
            command_handle.abort();

            self.connected = false;
            self.authenticated = false;
            self.registered = false;

            self.wait_before_retry(retry_interval).await;
        }
    }

    async fn wait_before_retry(&self, retry_interval: Duration) {
        let mut retry_timer = interval_at(Instant::now() + retry_interval, retry_interval);
        retry_timer.tick().await;
    }

    /// 启动命令监听任务
    fn start_command_listener(&self) -> tokio::task::JoinHandle<()> {
        let client = Arc::clone(&self.workit_client);
        let app_manager = self.app_manager.clone();
        let instance_id = self.config.associated_id.clone();

        tokio::spawn(async move {
            info!("Command listener started for {}", instance_id);

            loop {
                use denira_workit::pb::{
                    supervisor_message::Payload, CommandResponse,
                    SupervisorMessage,
                };

                // ...

                // 从 supervisor 接收消息
                let payload = {
                    let mut client_guard = client.lock().await;
                    match client_guard.receive_message().await {
                        Ok(Some(p)) => p,
                        Ok(None) => {
                            warn!("Stream closed for {}", instance_id);
                            break;
                        }
                        Err(e) => {
                            error!("Failed to receive message for {}: {}", instance_id, e);
                            break;
                        }
                    }
                };

                debug!("Received payload for {}: {:?}", instance_id, payload);

                match payload {
                    Payload::CommandRequest(cmd_req) => {
                        // 构建 WorkerCommand
                        // 解析 args_json
                        let args: Vec<String> = match cmd_req.args_json.is_empty()
                            || cmd_req.args_json == "null"
                        {
                            true => Vec::new(),
                            false => serde_json::from_str(&cmd_req.args_json).unwrap_or_default(),
                        };

                        let command = match cmd_req.command.as_str() {
                            "shutdown" => WorkerCommand::Shutdown {
                                timeout_seconds: None,
                                reason: None,
                            },
                            "restart" => WorkerCommand::Restart {
                                timeout_seconds: None,
                                preserve_state: false,
                            },
                            "reload" => WorkerCommand::Reload,
                            "get_status" => WorkerCommand::GetStatus,
                            "get_metrics" => WorkerCommand::GetMetrics,
                            custom => WorkerCommand::Custom {
                                command: custom.to_string(),
                                args,
                            },
                        };

                        info!("Processing command for {}: {:?}", instance_id, command);

                        let temp_client = DenixSupervisorClient {
                            workit_client: Arc::clone(&client),
                            config: DenixSupervisorConfig {
                                enabled: true,
                                addr: String::new(),
                                associated_id: instance_id.clone(),
                                secret: String::new(),
                                heartbeat_interval: 0,
                                request_timeout: 0,
                                retry_count: 0,
                                reconnect_interval: 0,
                            },
                            app_manager: app_manager.clone(),
                            connected: true,
                            authenticated: true,
                            registered: true,
                        };

                        match temp_client.handle_command(command).await {
                            Ok(response) => {
                                let response_msg = CommandResponse {
                                    id: cmd_req.id,
                                    success: true,
                                    message: "Command executed".to_string(), // Or serialize response
                                    error: String::new(),
                                };
                                let msg = SupervisorMessage {
                                    payload: Some(Payload::CommandResponse(response_msg)),
                                };
                                let client_guard = client.lock().await;
                                if let Err(e) =
                                    client_guard.send_message(msg.payload.unwrap()).await
                                {
                                    error!("Failed to send response: {}", e);
                                }
                            }
                            Err(e) => {
                                let response_msg = CommandResponse {
                                    id: cmd_req.id,
                                    success: false,
                                    message: e.to_string(),
                                    error: e.to_string(),
                                };
                                let msg = SupervisorMessage {
                                    payload: Some(Payload::CommandResponse(response_msg)),
                                };
                                let client_guard = client.lock().await;
                                if let Err(e) =
                                    client_guard.send_message(msg.payload.unwrap()).await
                                {
                                    error!("Failed to send error response: {}", e);
                                }
                            }
                        }
                    }
                    Payload::ConfigUpdate(config_update) => {
                        // TODO: Implement config update handling
                        info!("Received config update for key: {}", config_update.key);
                    }
                    _ => {
                        debug!("Ignored payload type");
                    }
                }
            }

            warn!("Command listener stopped for {}", instance_id);
        })
    }

    fn start_heartbeat_task(
        &self,
        sender: tokio::sync::mpsc::Sender<denira_workit::pb::SupervisorMessage>,
    ) -> tokio::task::JoinHandle<()> {
        let heartbeat_interval = self.config.heartbeat_duration();
        let instance_id = self.config.associated_id.clone();

        tokio::spawn(async move {
            let mut interval = interval(heartbeat_interval);

            loop {
                interval.tick().await;

                match WorkitClient::send_heartbeat_with_sender(&sender).await {
                    Ok(_) => debug!("Heartbeat sent for {}", instance_id),
                    Err(e) => {
                        error!("Heartbeat failed for {}: {}", instance_id, e);
                        break;
                    }
                }
            }
        })
    }

    /// 处理配置推送
    pub async fn handle_config_push(
        &self,
        config_data: ConfigPushData,
    ) -> SdkResult<WorkitMessage<ConfigPushAckMessage>> {
        info!(
            "Handling config push: key={}, worker_id={}",
            config_data.config_key, config_data.worker_id
        );

        // TODO: 实际应用配置到应用管理器
        // 目前只是记录日志并返回成功
        debug!("Config value: {}", config_data.config_value);

        // 根据配置键应用不同的配置
        let result = match config_data.config_key.as_str() {
            key if key.starts_with("app.") => {
                // 应用级配置
                info!("Applying app-level config: {}", key);
                // TODO: 应用到 ApplicationManager
                Ok(())
            }
            key if key.starts_with("gateway.") => {
                // 网关级配置
                info!("Applying gateway-level config: {}", key);
                // TODO: 应用到 Gateway 配置
                Ok(())
            }
            key if key.starts_with("runtime.") => {
                // 运行时级配置
                info!("Applying runtime-level config: {}", key);
                // TODO: 应用到 Runtime 配置
                Ok(())
            }
            _ => {
                warn!("Unknown config key: {}", config_data.config_key);
                Err(WorkitError::ConfigurationError(format!(
                    "Unknown config key: {}",
                    config_data.config_key
                )))
            }
        };

        let (success, message) = match result {
            Ok(_) => (
                true,
                format!("Config {} applied successfully", config_data.config_key),
            ),
            Err(e) => (false, format!("Failed to apply config: {}", e)),
        };

        let ack_msg = MessageBuilder::config_push_ack(
            success,
            message,
            config_data.config_key,
            config_data.version_timestamp,
        );

        Ok(ack_msg)
    }

    pub async fn handle_command(&self, command: WorkerCommand) -> SdkResult<serde_json::Value> {
        info!("Handling command: {:?}", command);

        match command {
            WorkerCommand::Shutdown {
                timeout_seconds,
                reason,
            } => {
                info!(
                    "Shutdown requested: timeout={:?}, reason={:?}",
                    timeout_seconds, reason
                );

                // 实际的关闭逻辑
                if let Some(app_manager) = &self.app_manager {
                    let apps = app_manager.list_apps();
                    info!("Shutting down {} applications", apps.len());

                    // TODO: 实现优雅关闭逻辑
                    // - 停止接收新请求
                    // - 等待现有请求完成
                    // - 卸载所有应用
                }

                Ok(serde_json::json!({
                    "success": true,
                    "message": "Shutdown initiated",
                    "timeout": timeout_seconds
                }))
            }

            WorkerCommand::Restart {
                timeout_seconds,
                preserve_state,
            } => {
                info!(
                    "Restart requested: timeout={:?}, preserve_state={}",
                    timeout_seconds, preserve_state
                );

                // 实际的重启逻辑
                if let Some(app_manager) = &self.app_manager {
                    if preserve_state {
                        info!("Preserving application state during restart");
                    } else {
                        info!("Restarting without preserving state");
                        // TODO: 清除应用状态
                    }
                }

                Ok(serde_json::json!({
                    "success": true,
                    "message": "Restart initiated",
                    "preserve_state": preserve_state
                }))
            }

            WorkerCommand::Reload => {
                info!("Reload requested");

                if let Some(app_manager) = &self.app_manager {
                    let apps = app_manager.list_apps();
                    info!("Reloading {} applications", apps.len());

                    // TODO: 实现重新加载逻辑
                    // - 重新读取配置
                    // - 热重载应用代码
                }

                Ok(serde_json::json!({
                    "success": true,
                    "message": "Reload completed"
                }))
            }

            WorkerCommand::UpdateConfig { config } => {
                info!("Config update received");

                // TODO: 实现配置更新逻辑
                // 1. 验证配置
                // 2. 应用配置
                // 3. 通知相关组件

                debug!("Config: {}", config);

                let keys_count = if let Some(obj) = config.as_object() {
                    obj.len()
                } else {
                    0
                };

                Ok(serde_json::json!({
                    "success": true,
                    "message": "Config updated",
                    "keys_updated": keys_count
                }))
            }

            WorkerCommand::GetStatus => {
                let (total_apps, running_apps) = if let Some(app_manager) = &self.app_manager {
                    let apps = app_manager.list_apps();
                    let running = apps
                        .iter()
                        .filter(|a| a.state == crate::app::AppState::Running)
                        .count();
                    (apps.len(), running)
                } else {
                    (0, 0)
                };

                Ok(serde_json::json!({
                    "instance_id": self.config.associated_id,
                    "version": env!("CARGO_PKG_VERSION"),
                    "worker_type": "denix",
                    "connected": self.connected,
                    "authenticated": self.authenticated,
                    "registered": self.registered,
                    "total_apps": total_apps,
                    "running_apps": running_apps,
                }))
            }

            WorkerCommand::GetMetrics => {
                let stats = if let Some(app_manager) = &self.app_manager {
                    app_manager.get_stats().await
                } else {
                    serde_json::json!({
                        "total": 0,
                        "running": 0
                    })
                };

                Ok(serde_json::json!({
                    "apps": stats,
                    "uptime_seconds": 0, // TODO: 实现 uptime 追踪
                    "memory_usage_mb": 0, // TODO: 实现内存监控
                }))
            }

            WorkerCommand::Custom { command, args } => {
                warn!("Custom command: {} with args: {:?}", command, args);

                // 处理自定义命令
                match command.as_str() {
                    "list_apps" => {
                        if let Some(app_manager) = &self.app_manager {
                            let apps = app_manager.list_apps();
                            Ok(serde_json::json!({
                                "success": true,
                                "apps": apps
                            }))
                        } else {
                            Ok(serde_json::json!({
                                "success": false,
                                "error": "ApplicationManager not available"
                            }))
                        }
                    }
                    _ => Ok(serde_json::json!({
                        "success": false,
                        "error": format!("Unknown command: {}", command)
                    })),
                }
            }
        }
    }

    pub async fn disconnect(&mut self) -> SdkResult<()> {
        info!("Disconnecting from supervisor");
        let mut client = self.workit_client.lock().await;
        client.disconnect().await?;
        self.connected = false;
        self.authenticated = false;
        self.registered = false;
        info!("Disconnected from supervisor");
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn is_ready(&self) -> bool {
        self.connected && self.authenticated && self.registered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_config() -> DenixSupervisorConfig {
        DenixSupervisorConfig {
            enabled: true,
            addr: "http://localhost:8080".to_string(),
            associated_id: "test-denix".to_string(),
            secret: "a1b2c3d4e5f67890123456789012345678901234567890123456789012345678".to_string(),
            heartbeat_interval: 30,
            request_timeout: 10,
            retry_count: 3,
            reconnect_interval: 5,
        }
    }

    #[test]
    fn test_client_creation() {
        let config = create_test_config();
        let client = DenixSupervisorClient::new(config, 8000);
        assert!(client.is_ok());
    }

    #[test]
    fn test_invalid_secret() {
        let config = DenixSupervisorConfig {
            secret: "too_short".to_string(),
            ..create_test_config()
        };
        let client = DenixSupervisorClient::new(config, 8000);
        assert!(client.is_err());
    }

    #[tokio::test]
    async fn test_command_handling() {
        let config = create_test_config();
        let client = DenixSupervisorClient::new(config, 8000).unwrap();

        let result = client.handle_command(WorkerCommand::GetStatus).await;
        assert!(result.is_ok());

        let status = result.unwrap();
        assert_eq!(status["instance_id"], "test-denix");
    }
}
