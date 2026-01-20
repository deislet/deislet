// 应用管理器

use super::types::{App, AppConfig, AppState};
use crate::runtime::RuntimePool;
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::SystemTime;

/// 应用管理器
pub struct ApplicationManager {
    apps: DashMap<String, App>,
    runtime_pool: Arc<RuntimePool>,
}

impl ApplicationManager {
    /// 创建新的应用管理器
    pub fn new() -> Self {
        Self::with_threads(None)
    }

    /// 创建新的应用管理器（指定线程数）
    pub fn with_threads(num_threads: Option<usize>) -> Self {
        Self {
            apps: DashMap::new(),
            runtime_pool: Arc::new(RuntimePool::new(num_threads)),
        }
    }

    /// 加载应用
    pub async fn load_app(&self, id: String, code: String, config: AppConfig) -> Result<()> {
        // 检查应用是否已存在
        if self.apps.contains_key(&id) {
            return Err(anyhow!("App {} already exists", id));
        }

        tracing::info!("Loading app: {}", id);

        // 创建应用信息
        let now = SystemTime::now();
        let mut app_info = App {
            id: id.clone(),
            state: AppState::Loading,
            config,
            created_at: now,
            updated_at: now,
            error: None,
        };

        // 在 runtime pool 中加载应用
        match self.runtime_pool.load_app(id.clone(), code, std::collections::HashMap::new()).await {
            Ok(()) => {
                app_info.state = AppState::Running;
                app_info.updated_at = SystemTime::now();
                self.apps.insert(id.clone(), app_info);
                tracing::info!("App {} loaded successfully", id);
                Ok(())
            }
            Err(e) => {
                app_info.state = AppState::Error;
                app_info.error = Some(e.to_string());
                app_info.updated_at = SystemTime::now();
                tracing::error!("Failed to load app {}: {}", id, e);
                Err(e)
            }
        }
    }

    /// 卸载应用
    pub async fn unload_app(&self, id: &str) -> Result<()> {
        tracing::info!("Unloading app: {}", id);

        // 从 runtime pool 中卸载
        self.runtime_pool.unload_app(id).await?;

        // 移除应用信息
        match self.apps.remove(id) {
            Some(_) => {
                tracing::info!("App {} unloaded successfully", id);
                Ok(())
            }
            None => Err(anyhow!("App {} not found", id)),
        }
    }

    /// 更新应用
    pub async fn update_app(
        &self,
        id: &str,
        code: String,
        config: Option<AppConfig>,
    ) -> Result<()> {
        tracing::info!("Updating app: {}", id);

        let entry = self
            .apps
            .get(id)
            .ok_or_else(|| anyhow!("App {} not found", id))?;

        // 获取当前配置
        let current_config = config.unwrap_or_else(|| entry.config.clone());
        drop(entry);

        // 在 runtime pool 中卸载旧版本
        self.runtime_pool.unload_app(id).await?;

        // 加载新版本
        self.runtime_pool.load_app(id.to_string(), code, std::collections::HashMap::new()).await?;

        // 更新应用信息
        if let Some(mut entry) = self.apps.get_mut(id) {
            entry.state = AppState::Running;
            entry.config = current_config;
            entry.updated_at = SystemTime::now();
            entry.error = None;
        }

        tracing::info!("App {} updated successfully", id);
        Ok(())
    }

    /// 执行应用
    pub async fn execute_app(
        &self,
        id: &str,
        url: String,
        method: String,
        headers: std::collections::HashMap<String, String>,
        body: Option<String>,
    ) -> Result<(
        u16,
        std::collections::HashMap<String, String>,
        Option<String>,
    )> {
        let entry = self
            .apps
            .get(id)
            .ok_or_else(|| anyhow!("App {} not found", id))?;

        if entry.state != AppState::Running {
            return Err(anyhow!(
                "App {} is not running (state: {:?})",
                id,
                entry.state
            ));
        }

        drop(entry);

        // 通过 runtime pool 执行请求
        self.runtime_pool
            .execute_request(id, url, method, headers, body)
            .await
    }

    /// 获取应用信息
    #[allow(dead_code)]
    pub fn get_app(&self, id: &str) -> Option<App> {
        self.apps.get(id).map(|entry| entry.clone())
    }

    /// 列出所有应用
    pub fn list_apps(&self) -> Vec<App> {
        self.apps.iter().map(|entry| entry.clone()).collect()
    }

    /// 获取统计信息
    #[allow(dead_code)]
    pub async fn get_stats(&self) -> serde_json::Value {
        let apps: Vec<App> = self.apps.iter().map(|entry| entry.clone()).collect();
        let pool_stats = self.runtime_pool.get_stats().await;

        let mut stats = serde_json::json!({
            "total": apps.len(),
            "running": apps.iter().filter(|a| a.state == AppState::Running).count(),
            "stopped": apps.iter().filter(|a| a.state == AppState::Stopped).count(),
            "error": apps.iter().filter(|a| a.state == AppState::Error).count(),
            "loading": apps.iter().filter(|a| a.state == AppState::Loading).count(),
        });

        // 添加线程池统计
        if let Some(obj) = stats.as_object_mut() {
            obj.insert(
                "pool".to_string(),
                serde_json::to_value(pool_stats).unwrap(),
            );
        }

        stats
    }

    /// 注册 WebSocket 连接
    ///
    /// 建立 Gateway 和 JavaScript WebSocketServer 之间的双向消息通道
    ///
    /// # 参数
    /// - app_id: 应用 ID
    /// - ws_id: WebSocket 连接 ID
    /// - js_to_client_tx: JavaScript 发送消息到客户端的通道
    pub async fn register_ws_connection(
        &self,
        app_id: &str,
        ws_id: String,
        js_to_client_tx: tokio::sync::mpsc::UnboundedSender<crate::runtime::WsServerMessage>,
    ) -> Result<()> {
        self.runtime_pool
            .register_ws_connection(app_id, ws_id, js_to_client_tx)
            .await
    }

    /// 发送 WebSocket 消息到 JavaScript
    ///
    /// # 参数
    /// - app_id: 应用 ID
    /// - ws_id: WebSocket 连接 ID
    /// - message: 消息内容
    pub async fn send_ws_message(
        &self,
        app_id: &str,
        ws_id: String,
        message: String,
    ) -> Result<()> {
        self.runtime_pool
            .send_ws_message(app_id, ws_id, message)
            .await
    }

    /// 注销 WebSocket 连接
    ///
    /// # 参数
    /// - app_id: 应用 ID
    /// - ws_id: WebSocket 连接 ID
    pub async fn unregister_ws_connection(&self, app_id: &str, ws_id: String) -> Result<()> {
        self.runtime_pool
            .unregister_ws_connection(app_id, ws_id)
            .await
    }
}

impl Default for ApplicationManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "manager_test.rs"]
mod manager_test;
