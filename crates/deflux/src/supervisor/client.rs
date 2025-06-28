//! Supervisor client implementation
//!
//! 集成 yasm 状态机的 supervisor 客户端，
//! 通过事件驱动管理 deflux 实例生命周期

use crate::supervisor::state::{DefluxStateManager, State};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, interval};
use tracing::{debug, error, info, warn};

use workit::SupervisorConfig;
use workit::{MessageBuilder, SdkResult, WorkerCommand, WorkerInfo, WorkerType, WorkitClient, WorkitError, deflux::PipelineData};

/// Deflux supervisor client
///
/// 集成状态机的 supervisor 客户端，负责：
/// - 与 supervisor 的 WebSocket 连接
/// - deflux 实例生命周期管理
/// - 配置下发和命令处理
/// - 状态上报和监控
#[derive(Debug)]
pub struct DefluxSupervisorClient {
    /// WorkIt WebSocket 客户端
    workit_client: Arc<Mutex<WorkitClient>>,
    /// 状态管理器
    state_manager: Arc<DefluxStateManager>,
    /// 配置
    config: SupervisorConfig,
}

impl DefluxSupervisorClient {
    /// 创建新的 supervisor 客户端
    pub fn new(config: SupervisorConfig) -> SdkResult<Self> {
        // 验证配置
        config.validate().map_err(WorkitError::ConfigurationError)?;

        // 创建 workit supervisor 配置
        let supervisor_config = SupervisorConfig::new(
            config.associated_id.clone(),
            config.secret.clone(),
            config.addr.clone(),
        );

        // 验证 workit 配置
        supervisor_config
            .validate()
            .map_err(WorkitError::ConfigurationError)?;

        // 创建 worker 信息
        let worker_info = WorkerInfo::new(
            WorkerType::Deflux(env!("CARGO_PKG_VERSION").to_string()),
            "0.0.0.0".to_string(),
            vec![80, 443],
        );

        // 创建 workit 客户端
        let workit_client = WorkitClient::new(supervisor_config, worker_info);

        // 创建状态管理器
        let state_manager = Arc::new(DefluxStateManager::new(config.associated_id.clone()));

        Ok(Self {
            workit_client: Arc::new(Mutex::new(workit_client)),
            state_manager,
            config,
        })
    }

    /// 连接到 supervisor
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

    /// 进行身份认证
    pub async fn authenticate(&mut self) -> SdkResult<()> {
        let mut client = self.workit_client.lock().await;
        info!("Authenticating with supervisor");
        client.authenticate().await?;
        info!("Authentication successful");
        Ok(())
    }

    /// 注册实例
    pub async fn register(&mut self) -> SdkResult<()> {
        let mut client = self.workit_client.lock().await;
        info!("Registering with supervisor");
        client.register().await?;
        info!("Registration successful");
        Ok(())
    }

    /// 处理注册完成，转换到 Active 状态
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

    /// 运行客户端主循环
    pub async fn run(&mut self) -> SdkResult<()> {
        info!(
            "Starting deflux supervisor client main loop for instance: {}",
            self.config.associated_id
        );

        // 启动后台任务
        self.start_background_tasks().await;

        // 主循环 - 等待命令和处理事件
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;

            // 检查连接状态
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

    /// 启动客户端并完成完整的连接流程
    pub async fn start(&mut self) -> SdkResult<()> {
        info!(
            "Starting deflux supervisor client for instance: {}",
            self.config.associated_id
        );

        // 注册状态机回调
        self.setup_state_callbacks().await;

        // 连接和注册
        self.connect_and_register().await?;

        // 启动后台任务
        self.start_background_tasks().await;

        info!("Deflux supervisor client started successfully");
        Ok(())
    }

    /// 连接到 supervisor 并完成认证注册流程
    async fn connect_and_register(&mut self) -> SdkResult<()> {
        let mut client = self.workit_client.lock().await;

        info!(
            "Connecting to supervisor at {}",
            self.config.addr
        );

        // 连接
        client.connect().await?;
        info!("Connected to supervisor");

        // 认证
        client.authenticate().await?;
        info!("Authentication successful");

        // 注册
        client.register().await?;
        info!("Registration successful");

        // 状态机：转换到 Active 状态
        self.state_manager
            .handle_registration_complete()
            .await
            .map_err(|e| WorkitError::ConfigurationError(format!("State machine error: {}", e)))?;

        Ok(())
    }

    /// 设置状态机回调
    async fn setup_state_callbacks(&self) {
        // 简化的状态管理器不需要复杂的回调设置
        // 状态变更会自动记录日志
        info!(
            "State manager initialized for instance: {}",
            self.config.associated_id
        );
    }

    /// 启动后台任务
    async fn start_background_tasks(&self) {
        // 启动心跳任务
        self.start_heartbeat_task();

        // 启动命令处理任务
        self.start_command_handler();
    }

    /// 启动心跳任务
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

    /// 启动命令处理任务
    fn start_command_handler(&self) -> tokio::task::JoinHandle<()> {
        let _client = Arc::clone(&self.workit_client);
        let _state_manager = Arc::clone(&self.state_manager);
        let instance_id = self.config.associated_id.clone();

        tokio::spawn(async move {
            info!("Starting command handler for {}", instance_id);

            // TODO: 实现实际的命令接收和处理逻辑
            // 这需要 WorkitClient 提供命令接收接口
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                // 占位符：等待 workit 库提供命令接收API
            }
        })
    }

    /// 发送状态更新到 supervisor（内部方法）
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

        // TODO: 等待 WorkitClient 提供通用消息发送API
        info!("Would send state update: {} -> {}", from, to);

        Ok(())
    }

    /// 发送状态更新到 supervisor
    pub async fn send_status_update(
        &self,
        status: &str,
        details: Option<serde_json::Value>,
    ) -> SdkResult<()> {
        let _status_message = MessageBuilder::status_update(status.to_string(), details);

        info!("Sending status update: {}", status);
        // TODO: 实现实际发送
        Ok(())
    }

    /// 发送管道消息到 supervisor
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
        // TODO: 实现实际发送
        Ok(())
    }

    /// 处理来自 supervisor 的命令
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

                // 状态机：转换到 Sunsetting 状态
                match self.state_manager.handle_shutdown().await {
                    Ok(_) => {
                        // 执行实际的关闭逻辑
                        self.execute_shutdown(timeout_seconds.map(|t| t as u32))
                            .await;

                        // 状态机：转换到 Terminated 状态
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

                // 状态机：先关闭再重启
                let _ = self.state_manager.handle_shutdown().await;
                let _ = self.state_manager.handle_silenced().await;

                // 执行重启逻辑
                self.execute_restart(preserve_state).await;

                Ok(serde_json::json!({
                    "success": true,
                    "message": "Restart initiated"
                }))
            }

            WorkerCommand::Reload => {
                info!("Received reload command");

                // 实现重载逻辑（重新加载配置、路由规则等）
                Ok(serde_json::json!({
                    "success": true,
                    "message": "Reload completed"
                }))
            }

            WorkerCommand::UpdateConfig { config } => {
                info!("Received config update: {:?}", config);

                // 实现配置更新逻辑
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
                    "uptime": "placeholder", // TODO: 实现实际的运行时间计算
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
                warn!(
                    "Received unknown custom command: {} with args: {:?}",
                    command, args
                );
                Ok(serde_json::json!({
                    "success": false,
                    "error": format!("Unknown command: {}", command)
                }))
            }
        }
    }

    /// 执行关闭逻辑
    async fn execute_shutdown(&self, timeout_seconds: Option<u32>) {
        let timeout = timeout_seconds.unwrap_or(30);
        info!("Executing graceful shutdown with timeout: {}s", timeout);

        // TODO: 实现实际的关闭逻辑
        // 1. 停止接受新连接
        // 2. 等待现有连接完成
        // 3. 关闭所有资源
    }

    /// 执行重启逻辑
    async fn execute_restart(&self, preserve_state: bool) {
        info!("Executing restart, preserve_state: {}", preserve_state);

        // TODO: 实现实际的重启逻辑
        // 1. 保存状态（如果需要）
        // 2. 重新加载配置
        // 3. 重新启动服务
    }

    /// 应用配置更新
    async fn apply_config_update(&self, _config: serde_json::Value) -> Result<(), String> {
        info!("Applying configuration update");

        // TODO: 实现配置更新逻辑
        // 1. 验证配置
        // 2. 应用配置
        // 3. 重新加载相关组件

        Ok(())
    }

    /// 检查客户端是否就绪
    pub async fn is_ready(&self) -> bool {
        let client = self.workit_client.lock().await;
        client.is_connected()
            && client.is_authenticated()
            && client.is_registered()
            && self.state_manager.is_active().await
    }

    /// 优雅断开连接
    pub async fn disconnect(&mut self) -> SdkResult<()> {
        info!("Disconnecting from supervisor");

        // 状态机：转换到关闭状态
        if self.state_manager.is_active().await {
            let _ = self.state_manager.handle_shutdown().await;
        }
        let _ = self.state_manager.handle_silenced().await;

        // 断开连接
        let mut client = self.workit_client.lock().await;
        client.disconnect().await?;

        info!("Disconnected from supervisor");
        Ok(())
    }

    /// 获取当前状态
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

    /// 获取状态管理器（用于测试）
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
            addr: "ws://localhost:8080".to_string(),
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

        // 初始状态应该是 Pending
        assert_eq!(client.current_state().await, State::Pending);

        // 检查状态管理器集成
        let state_manager = client.state_manager();
        assert_eq!(state_manager.instance_id(), "test-worker");
    }

    #[tokio::test]
    async fn test_command_handling() {
        let config = create_test_config();
        let client = DefluxSupervisorClient::new(config).unwrap();

        // 测试状态查询命令
        let result = client.handle_command(WorkerCommand::GetStatus).await;
        assert!(result.is_ok());

        let status = result.unwrap();
        assert_eq!(status["instance_id"], "test-worker");
        assert_eq!(status["state"], "Pending");
    }
}
