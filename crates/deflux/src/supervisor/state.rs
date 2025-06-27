//! Deflux instance lifecycle state management
//!
//! 基于 workit 库的 InstanceStatus 实现 deflux 实例的生命周期管理
//!
//! 状态流转：Pending -> Active -> Sunsetting -> Terminated
//! 支持重启循环：Terminated -> Pending

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

// 直接使用 workit 库的状态定义
pub use workit::models::instance::InstanceStatus as State;

/// Deflux 实例状态管理器
///
/// 使用 workit 库的 InstanceStatus 管理 deflux 实例的完整生命周期
///
/// 状态流转：
/// - Pending: 实例启动，等待注册和配置下发
/// - Active: 实例就绪，正常处理请求
/// - Sunsetting: 实例准备关闭，不接受新连接
/// - Terminated: 实例已完全关闭
pub struct DefluxStateManager {
    /// 当前状态
    current_state: Arc<RwLock<State>>,
    /// 状态转换历史
    transition_history: Arc<RwLock<Vec<StateTransition>>>,
    /// 实例 ID
    instance_id: String,
}

/// 状态转换记录
#[derive(Debug, Clone)]
pub struct StateTransition {
    pub from: State,
    pub to: State,
    pub event: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl DefluxStateManager {
    /// 创建新的状态管理器
    pub fn new(instance_id: String) -> Self {
        info!("Creating state manager for instance: {}", instance_id);

        Self {
            current_state: Arc::new(RwLock::new(State::Pending)),
            transition_history: Arc::new(RwLock::new(Vec::new())),
            instance_id,
        }
    }

    /// 内部状态转换方法
    async fn transition_to(&self, new_state: State, event: &str) -> Result<State, String> {
        let mut current = self.current_state.write().await;
        let old_state = current.clone();

        // 验证状态转换是否有效
        if !self.is_valid_transition(&old_state, &new_state) {
            let error_msg = format!(
                "Invalid state transition from {:?} to {:?} via event '{}'",
                old_state, new_state, event
            );
            warn!("Instance {}: {}", self.instance_id, error_msg);
            return Err(error_msg);
        }

        // 执行状态转换
        *current = new_state.clone();

        // 记录转换历史
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

    /// 验证状态转换是否有效
    fn is_valid_transition(&self, from: &State, to: &State) -> bool {
        use State::*;

        match (from, to) {
            // 正常生命周期
            (Pending, Active) => true,        // 注册完成
            (Active, Active) => true,         // 心跳保持
            (Active, Sunsetting) => true,     // 开始关闭
            (Sunsetting, Sunsetting) => true, // 关闭中心跳
            (Sunsetting, Terminated) => true, // 完全关闭

            // 重启循环
            (Terminated, Pending) => true, // 重启

            // 其他转换都是无效的
            _ => false,
        }
    }

    /// 获取当前状态
    pub async fn current_state(&self) -> State {
        self.current_state.read().await.clone()
    }

    /// 检查实例是否处于活跃状态
    pub async fn is_active(&self) -> bool {
        matches!(self.current_state().await, State::Active)
    }

    /// 检查实例是否已终止
    pub async fn is_terminated(&self) -> bool {
        matches!(self.current_state().await, State::Terminated)
    }

    /// 检查实例是否在关闭中
    pub async fn is_sunsetting(&self) -> bool {
        matches!(self.current_state().await, State::Sunsetting)
    }

    /// 检查实例是否在等待状态
    pub async fn is_pending(&self) -> bool {
        matches!(self.current_state().await, State::Pending)
    }

    /// 获取状态转换历史数量
    pub async fn history_count(&self) -> usize {
        self.transition_history.read().await.len()
    }

    /// 获取状态转换历史
    pub async fn get_history(&self) -> Vec<StateTransition> {
        self.transition_history.read().await.clone()
    }

    /// 获取实例 ID
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// 处理注册完成事件
    ///
    /// 从 Pending 状态转换到 Active 状态
    pub async fn handle_registration_complete(&self) -> Result<State, String> {
        info!(
            "Instance {} registration completed, transitioning to Active",
            self.instance_id
        );
        self.transition_to(State::Active, "registration_complete")
            .await
    }

    /// 处理心跳事件
    ///
    /// 在 Active 或 Sunsetting 状态保持活跃
    pub async fn handle_heartbeat(&self) -> Result<State, String> {
        let current = self.current_state().await;
        match current {
            State::Active => {
                // 在 Active 状态，心跳保持状态不变
                self.transition_to(State::Active, "heartbeat").await
            }
            State::Sunsetting => {
                // 在 Sunsetting 状态，心跳也保持状态不变
                self.transition_to(State::Sunsetting, "heartbeat").await
            }
            _ => {
                let error_msg = format!("Cannot send heartbeat in state: {:?}", current);
                warn!("Instance {}: {}", self.instance_id, error_msg);
                Err(error_msg)
            }
        }
    }

    /// 处理关闭事件
    ///
    /// 从 Active 状态转换到 Sunsetting 状态
    pub async fn handle_shutdown(&self) -> Result<State, String> {
        info!(
            "Instance {} received shutdown signal, transitioning to Sunsetting",
            self.instance_id
        );
        self.transition_to(State::Sunsetting, "shutdown").await
    }

    /// 处理静默事件（所有连接关闭）
    ///
    /// 从 Sunsetting 状态转换到 Terminated 状态
    pub async fn handle_silenced(&self) -> Result<State, String> {
        info!(
            "Instance {} all connections closed, transitioning to Terminated",
            self.instance_id
        );
        self.transition_to(State::Terminated, "silenced").await
    }

    /// 处理重启事件
    ///
    /// 从 Terminated 状态转换到 Pending 状态
    pub async fn handle_startup(&self) -> Result<State, String> {
        info!(
            "Instance {} restarting, transitioning to Pending",
            self.instance_id
        );
        self.transition_to(State::Pending, "startup").await
    }

    /// 处理非受控终止（如进程被 kill）
    ///
    /// 尝试上报状态并记录
    pub async fn handle_uncontrolled_termination(&self) -> Result<(), String> {
        let current_state = self.current_state().await;
        error!(
            "Instance {} uncontrolled termination from state: {:?}",
            self.instance_id, current_state
        );

        // 记录状态变更历史
        let history_count = self.history_count().await;
        error!(
            "Instance {} had {} state transitions before termination",
            self.instance_id, history_count
        );

        // TODO: 实现状态上报逻辑

        Ok(())
    }

    /// 强制设置状态（仅用于紧急情况）
    pub async fn force_set_state(&self, new_state: State, reason: &str) -> State {
        let mut current = self.current_state.write().await;
        let old_state = current.clone();
        *current = new_state.clone();

        warn!(
            "Instance {} forced state change: {:?} -> {:?}, reason: {}",
            self.instance_id, old_state, new_state, reason
        );

        // 记录强制转换
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

    /// 检查是否可以执行特定操作
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

    /// 重置状态机（仅用于测试）
    #[cfg(test)]
    pub async fn reset(&self) {
        let mut current = self.current_state.write().await;
        *current = State::Pending;

        let mut history = self.transition_history.write().await;
        history.clear();

        info!("Instance {} state manager reset", self.instance_id);
    }
}

impl std::fmt::Debug for DefluxStateManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DefluxStateManager")
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
        let manager = DefluxStateManager::new("test-instance".to_string());

        // 初始状态应该是 Pending
        assert_eq!(manager.current_state().await, State::Pending);
        assert!(manager.is_pending().await);

        // 注册完成，进入 Active
        let result = manager.handle_registration_complete().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), State::Active);
        assert!(manager.is_active().await);

        // 发送心跳，保持 Active
        let result = manager.handle_heartbeat().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), State::Active);

        // 开始关闭
        let result = manager.handle_shutdown().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), State::Sunsetting);
        assert!(manager.is_sunsetting().await);

        // 心跳在 Sunsetting 状态仍然有效
        let result = manager.handle_heartbeat().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), State::Sunsetting);

        // 所有连接关闭，进入 Terminated
        let result = manager.handle_silenced().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), State::Terminated);
        assert!(manager.is_terminated().await);
    }

    #[tokio::test]
    async fn test_restart_cycle() {
        let manager = DefluxStateManager::new("test-instance".to_string());

        // 完整生命周期
        manager.handle_registration_complete().await.unwrap();
        manager.handle_shutdown().await.unwrap();
        manager.handle_silenced().await.unwrap();
        assert!(manager.is_terminated().await);

        // 重启
        let result = manager.handle_startup().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), State::Pending);
        assert!(manager.is_pending().await);
    }

    #[tokio::test]
    async fn test_invalid_transitions() {
        let manager = DefluxStateManager::new("test-instance".to_string());

        // 尝试从 Pending 状态直接关闭（无效转换）
        let result = manager.handle_shutdown().await;
        assert!(result.is_err());

        // 状态应该保持不变
        assert_eq!(manager.current_state().await, State::Pending);

        // 尝试从 Pending 状态直接静默（无效转换）
        let result = manager.handle_silenced().await;
        assert!(result.is_err());
        assert_eq!(manager.current_state().await, State::Pending);
    }

    #[tokio::test]
    async fn test_capability_checks() {
        let manager = DefluxStateManager::new("test-instance".to_string());

        // Pending 状态的能力
        assert!(manager.can_handle_registration().await);
        assert!(!manager.can_handle_shutdown().await);
        assert!(!manager.can_handle_heartbeat().await);

        // 转换到 Active 状态
        manager.handle_registration_complete().await.unwrap();

        // Active 状态的能力
        assert!(!manager.can_handle_registration().await);
        assert!(manager.can_handle_shutdown().await);
        assert!(manager.can_handle_heartbeat().await);
        assert!(!manager.can_handle_silenced().await);

        // 转换到 Sunsetting 状态
        manager.handle_shutdown().await.unwrap();

        // Sunsetting 状态的能力
        assert!(!manager.can_handle_shutdown().await);
        assert!(manager.can_handle_heartbeat().await);
        assert!(manager.can_handle_silenced().await);
    }

    #[tokio::test]
    async fn test_history_tracking() {
        let manager = DefluxStateManager::new("test-instance".to_string());

        assert_eq!(manager.history_count().await, 0);

        // 执行一些状态转换
        manager.handle_registration_complete().await.unwrap();
        assert_eq!(manager.history_count().await, 1);

        manager.handle_heartbeat().await.unwrap();
        assert_eq!(manager.history_count().await, 2);

        manager.handle_shutdown().await.unwrap();
        assert_eq!(manager.history_count().await, 3);

        // 检查历史记录
        let history = manager.get_history().await;
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].from, State::Pending);
        assert_eq!(history[0].to, State::Active);
        assert_eq!(history[0].event, "registration_complete");
    }

    #[tokio::test]
    async fn test_force_set_state() {
        let manager = DefluxStateManager::new("test-instance".to_string());

        // 强制设置状态
        let new_state = manager
            .force_set_state(State::Terminated, "emergency shutdown")
            .await;
        assert_eq!(new_state, State::Terminated);
        assert!(manager.is_terminated().await);

        // 检查历史记录包含强制转换
        let history = manager.get_history().await;
        assert_eq!(history.len(), 1);
        assert!(history[0].event.contains("force_set"));
    }

    #[tokio::test]
    async fn test_uncontrolled_termination() {
        let manager = DefluxStateManager::new("test-instance".to_string());

        // 进入 Active 状态
        manager.handle_registration_complete().await.unwrap();

        // 模拟非受控终止
        let result = manager.handle_uncontrolled_termination().await;
        assert!(result.is_ok());

        // 状态应该保持不变（因为没有实际的状态转换）
        assert_eq!(manager.current_state().await, State::Active);
    }
}
