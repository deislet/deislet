// Runtime 线程池 - 多核支持

use super::isolate::AppRuntime;
use anyhow::Result;
use deno_core::JsRuntime;
use std::collections::HashMap;
use std::sync::{Arc, Once};
use std::thread;
use tokio::sync::{mpsc, oneshot, RwLock};

/// 确保 V8 平台只初始化一次
static V8_INIT: Once = Once::new();

type FetchResult = (u16, HashMap<String, String>, Option<String>);

/// 初始化 V8 平台（每个进程只能调用一次）
fn init_v8_platform() {
    V8_INIT.call_once(|| {
        JsRuntime::init_platform(None, false);
        tracing::debug!("V8 platform initialized");
    });
}

/// Runtime 消息类型
pub enum RuntimeMessage {
    /// 加载应用
    LoadApp {
        app_id: String,
        code: String,
        env: HashMap<String, String>,
        memory_limit_mb: Option<usize>,
        response: oneshot::Sender<Result<()>>,
    },
    /// 卸载应用
    UnloadApp {
        app_id: String,
        response: oneshot::Sender<Result<()>>,
    },
    /// 执行请求
    ExecuteRequest {
        app_id: String,
        url: String,
        method: String,
        headers: HashMap<String, String>,
        body: Option<String>,
        response: oneshot::Sender<Result<FetchResult>>,
    },
    /// 获取应用列表
    #[allow(dead_code)]
    ListApps {
        response: oneshot::Sender<Vec<String>>,
    },
    /// 注册 WebSocket 连接
    RegisterWsConnection {
        app_id: String,
        ws_id: String,
        /// JavaScript 发送消息到客户端的通道
        js_to_client_tx: mpsc::UnboundedSender<super::WsServerMessage>,
        response: oneshot::Sender<Result<()>>,
    },
    /// 发送 WebSocket 消息 (从 Gateway 到 JavaScript)
    SendWsMessage {
        app_id: String,
        ws_id: String,
        message: String,
        response: oneshot::Sender<Result<()>>,
    },
    /// 注销 WebSocket 连接
    UnregisterWsConnection {
        app_id: String,
        ws_id: String,
        response: oneshot::Sender<Result<()>>,
    },
    /// 关闭线程
    Shutdown,
}

/// Runtime 线程
struct RuntimeThread {
    #[allow(dead_code)]
    thread_id: usize,
    sender: mpsc::UnboundedSender<RuntimeMessage>,
    handle: Option<thread::JoinHandle<()>>,
}

impl RuntimeThread {
    /// 创建新的 runtime 线程
    fn new(thread_id: usize) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        let handle = thread::spawn(move || {
            // 创建 current_thread 的 tokio runtime
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime");

            rt.block_on(async move {
                Self::run_event_loop(thread_id, rx, ready_tx).await;
            });
        });

        // 等待线程准备就绪
        let _ = ready_rx.recv();

        Self {
            thread_id,
            sender: tx,
            handle: Some(handle),
        }
    }

    /// 运行事件循环
    async fn run_event_loop(
        thread_id: usize,
        mut rx: mpsc::UnboundedReceiver<RuntimeMessage>,
        ready_tx: std::sync::mpsc::Sender<()>,
    ) {
        tracing::info!("Runtime thread {} started", thread_id);

        // 通知主线程此线程已完成初始化
        let _ = ready_tx.send(());

        // 该线程管理的所有应用 runtimes
        let mut runtimes: HashMap<String, AppRuntime> = HashMap::new();

        while let Some(msg) = rx.recv().await {
            match msg {
                RuntimeMessage::LoadApp {
                    app_id,
                    code,
                    env,
                    memory_limit_mb,
                    response,
                } => {
                    tracing::debug!("Thread {}: Loading app {}", thread_id, app_id);
                    let result = match AppRuntime::new(app_id.clone(), code, env, memory_limit_mb) {
                        Ok(runtime) => {

                            runtimes.insert(app_id.clone(), runtime);
                            Ok(())
                        }
                        Err(e) => Err(e),
                    };
                    let _ = response.send(result);
                }

                RuntimeMessage::UnloadApp { app_id, response } => {
                    tracing::debug!("Thread {}: Unloading app {}", thread_id, app_id);
                    let result = if runtimes.remove(&app_id).is_some() {
                        Ok(())
                    } else {
                        Err(anyhow::anyhow!("App not found"))
                    };
                    let _ = response.send(result);
                }

                RuntimeMessage::ExecuteRequest {
                    app_id,
                    url,
                    method,
                    headers,
                    body,
                    response,
                } => {
                    tracing::debug!("Thread {}: Executing request for app {}", thread_id, app_id);
                    match runtimes.get_mut(&app_id) {
                        Some(runtime) => {
                            if let Err(e) = runtime
                                .execute_fetch(url, method, headers, body, response)
                                .await
                            {
                                tracing::error!("Error executing fetch: {}", e);
                            }
                        }
                        None => {
                            let _ = response.send(Err(anyhow::anyhow!("App not found")));
                        }
                    };
                }

                RuntimeMessage::ListApps { response } => {
                    let apps: Vec<String> = runtimes.keys().cloned().collect();
                    let _ = response.send(apps);
                }

                RuntimeMessage::RegisterWsConnection {
                    app_id,
                    ws_id,
                    js_to_client_tx,
                    response,
                } => {
                    tracing::debug!(
                        "Thread {}: Registering WebSocket {} for app {}",
                        thread_id,
                        ws_id,
                        app_id
                    );

                    let result = match runtimes.get_mut(&app_id) {
                        Some(runtime) => {
                            // 注册 sender 到 WsServerManager，这样 JavaScript 可以发送消息到客户端
                            runtime
                                .register_ws_connection(ws_id.clone(), js_to_client_tx)
                                .await
                        }
                        None => Err(anyhow::anyhow!("App not found")),
                    };

                    let _ = response.send(result);
                }

                RuntimeMessage::SendWsMessage {
                    app_id,
                    ws_id,
                    message,
                    response,
                } => {
                    tracing::debug!(
                        "Thread {}: Sending WebSocket message to {} for app {}",
                        thread_id,
                        ws_id,
                        app_id
                    );

                    let result = match runtimes.get_mut(&app_id) {
                        Some(runtime) => runtime.dispatch_ws_message(ws_id, message).await,
                        None => Err(anyhow::anyhow!("App not found")),
                    };

                    let _ = response.send(result);
                }

                RuntimeMessage::UnregisterWsConnection {
                    app_id,
                    ws_id,
                    response,
                } => {
                    tracing::debug!(
                        "Thread {}: Unregistering WebSocket {} for app {}",
                        thread_id,
                        ws_id,
                        app_id
                    );

                    let result = match runtimes.get_mut(&app_id) {
                        Some(runtime) => {
                            runtime
                                .close_ws_connection(ws_id, 1000, "Gateway closed".to_string())
                                .await
                        }
                        None => Err(anyhow::anyhow!("App not found")),
                    };

                    let _ = response.send(result);
                }

                RuntimeMessage::Shutdown => {
                    tracing::info!("Runtime thread {} shutting down", thread_id);
                    break;
                }
            }
        }

        // 显式清理所有 runtimes，确保在正确的上下文中 drop
        tracing::debug!(
            "Cleaning up {} runtimes in thread {}",
            runtimes.len(),
            thread_id
        );
        runtimes.clear();

        tracing::info!("Runtime thread {} stopped", thread_id);
    }

    /// 发送消息
    fn send(&self, msg: RuntimeMessage) -> Result<()> {
        self.sender
            .send(msg)
            .map_err(|_| anyhow::anyhow!("Runtime thread has stopped"))
    }
}

impl Drop for RuntimeThread {
    fn drop(&mut self) {
        let _ = self.sender.send(RuntimeMessage::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Runtime 线程池
pub struct RuntimePool {
    threads: Vec<RuntimeThread>,
    app_assignments: Arc<RwLock<HashMap<String, usize>>>, // app_id -> thread_id
}

impl RuntimePool {
    /// 创建新的线程池
    pub fn new(num_threads: Option<usize>) -> Self {
        // 确保 V8 平台已初始化（每个进程只初始化一次）
        init_v8_platform();

        let num_threads = num_threads.unwrap_or_else(num_cpus::get);
        tracing::info!("Creating runtime pool with {} threads", num_threads);

        let threads = (0..num_threads).map(RuntimeThread::new).collect();

        Self {
            threads,
            app_assignments: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 选择一个线程（负载均衡）
    async fn select_thread_for_new_app(&self) -> usize {
        let assignments = self.app_assignments.read().await;

        // 统计每个线程的应用数量
        let mut counts = vec![0; self.threads.len()];
        for thread_id in assignments.values() {
            counts[*thread_id] += 1;
        }

        // 选择应用数最少的线程
        counts
            .iter()
            .enumerate()
            .min_by_key(|(_, count)| *count)
            .map(|(idx, _)| idx)
            .unwrap_or(0)
    }

    /// 获取应用所在的线程 ID
    async fn get_thread_id(&self, app_id: &str) -> Option<usize> {
        self.app_assignments.read().await.get(app_id).copied()
    }

    /// 加载应用
    pub async fn load_app(
        &self,
        app_id: String,
        code: String,
        env: HashMap<String, String>,
        memory_limit_mb: Option<usize>,
    ) -> Result<()> {
        // 检查是否已存在
        if self.get_thread_id(&app_id).await.is_some() {
            return Err(anyhow::anyhow!("App already exists"));
        }

        // 选择线程
        let thread_id = self.select_thread_for_new_app().await;
        tracing::info!("Assigning app {} to thread {}", app_id, thread_id);

        // 发送加载消息
        let (tx, rx) = oneshot::channel();
        self.threads[thread_id].send(RuntimeMessage::LoadApp {
            app_id: app_id.clone(),
            code,
            env,
            memory_limit_mb,
            response: tx,
        })?;

        // 等待结果
        let result = rx
            .await
            .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))?;

        // 如果成功，记录分配
        if result.is_ok() {
            self.app_assignments.write().await.insert(app_id, thread_id);
        }

        result
    }

    /// 卸载应用
    pub async fn unload_app(&self, app_id: &str) -> Result<()> {
        // 获取线程 ID
        let thread_id = self
            .get_thread_id(app_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("App not found"))?;

        // 发送卸载消息
        let (tx, rx) = oneshot::channel();
        self.threads[thread_id].send(RuntimeMessage::UnloadApp {
            app_id: app_id.to_string(),
            response: tx,
        })?;

        // 等待结果
        let result = rx
            .await
            .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))?;

        // 如果成功，移除分配记录
        if result.is_ok() {
            self.app_assignments.write().await.remove(app_id);
        }

        result
    }

    /// 执行请求
    pub async fn execute_request(
        &self,
        app_id: &str,
        url: String,
        method: String,
        headers: HashMap<String, String>,
        body: Option<String>,
    ) -> Result<(u16, HashMap<String, String>, Option<String>)> {
        // 获取线程 ID
        let thread_id = self
            .get_thread_id(app_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("App not found"))?;

        // 发送执行消息
        let (tx, rx) = oneshot::channel();
        self.threads[thread_id].send(RuntimeMessage::ExecuteRequest {
            app_id: app_id.to_string(),
            url,
            method,
            headers,
            body,
            response: tx,
        })?;

        // 等待结果
        rx.await
            .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))?
    }

    /// 列出所有应用
    #[cfg(test)]
    pub async fn list_apps(&self) -> Vec<String> {
        let assignments = self.app_assignments.read().await;
        assignments.keys().cloned().collect()
    }

    /// 获取线程数
    #[cfg(test)]
    pub fn thread_count(&self) -> usize {
        self.threads.len()
    }

    /// 获取统计信息
    pub async fn get_stats(&self) -> HashMap<String, usize> {
        let assignments = self.app_assignments.read().await;

        let mut thread_apps: HashMap<String, usize> = HashMap::new();
        for (_, thread_id) in assignments.iter() {
            let key = format!("thread_{}", thread_id);
            *thread_apps.entry(key).or_insert(0) += 1;
        }

        thread_apps.insert("total_threads".to_string(), self.threads.len());
        thread_apps.insert("total_apps".to_string(), assignments.len());

        thread_apps
    }

    /// 注册 WebSocket 连接
    ///
    /// # 参数
    /// - app_id: 应用 ID
    /// - ws_id: WebSocket 连接 ID
    /// - js_to_client_tx: JavaScript 发送消息到客户端的通道
    ///
    /// # 返回
    /// - Ok(()) 成功
    /// - Err(...) 应用不存在或注册失败
    pub async fn register_ws_connection(
        &self,
        app_id: &str,
        ws_id: String,
        js_to_client_tx: mpsc::UnboundedSender<super::WsServerMessage>,
    ) -> Result<()> {
        let thread_id = self
            .get_thread_id(app_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("App not found"))?;

        let (tx, rx) = oneshot::channel();
        self.threads[thread_id].send(RuntimeMessage::RegisterWsConnection {
            app_id: app_id.to_string(),
            ws_id,
            js_to_client_tx,
            response: tx,
        })?;

        rx.await
            .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))?
    }

    /// 发送 WebSocket 消息到 JavaScript
    ///
    /// # 参数
    /// - app_id: 应用 ID
    /// - ws_id: WebSocket 连接 ID
    /// - message: 消息内容
    ///
    /// # 返回
    /// - Ok(()) 成功
    /// - Err(...) 应用不存在或发送失败
    pub async fn send_ws_message(
        &self,
        app_id: &str,
        ws_id: String,
        message: String,
    ) -> Result<()> {
        let thread_id = self
            .get_thread_id(app_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("App not found"))?;

        let (tx, rx) = oneshot::channel();
        self.threads[thread_id].send(RuntimeMessage::SendWsMessage {
            app_id: app_id.to_string(),
            ws_id,
            message,
            response: tx,
        })?;

        rx.await
            .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))?
    }

    /// 注销 WebSocket 连接
    ///
    /// # 参数
    /// - app_id: 应用 ID
    /// - ws_id: WebSocket 连接 ID
    ///
    /// # 返回
    /// - Ok(()) 成功
    /// - Err(...) 应用不存在或注销失败
    pub async fn unregister_ws_connection(&self, app_id: &str, ws_id: String) -> Result<()> {
        let thread_id = self
            .get_thread_id(app_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("App not found"))?;

        let (tx, rx) = oneshot::channel();
        self.threads[thread_id].send(RuntimeMessage::UnregisterWsConnection {
            app_id: app_id.to_string(),
            ws_id,
            response: tx,
        })?;

        rx.await
            .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::sync::Mutex;

    // 使用全局的 RuntimePool 避免多次创建/销毁导致的 V8 问题
    static TEST_POOL: Lazy<Mutex<RuntimePool>> =
        Lazy::new(|| Mutex::new(RuntimePool::new(Some(4))));

    #[tokio::test]
    async fn test_pool_creation() {
        let pool = TEST_POOL.lock().unwrap();
        assert_eq!(pool.thread_count(), 4);
    }

    #[tokio::test]
    async fn test_load_and_execute() {
        let pool = TEST_POOL.lock().unwrap();

        let code = r#"
            globalThis.default = {
                fetch(request) {
                    return new Response("Hello from pool!");
                }
            }
        "#;

        // 加载应用
        let result = pool
            .load_app(
                "test_load_exec_app".to_string(),
                code.to_string(),
                HashMap::new(),
                None,
            )
            .await;
        assert!(result.is_ok());

        // 执行请求
        let result = pool
            .execute_request(
                "test_load_exec_app",
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
            )
            .await;

        assert!(result.is_ok());
        let (status, _, body) = result.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, Some("Hello from pool!".to_string()));

        // 清理
        let _ = pool.unload_app("test_load_exec_app").await;
    }

    #[tokio::test]
    async fn test_load_balancing() {
        let pool = TEST_POOL.lock().unwrap();

        // 加载多个应用
        for i in 0..4 {
            let code = r#"globalThis.default = { fetch() { return new Response("ok"); } }"#;
            pool.load_app(
                format!("lb_app{}", i),
                code.to_string(),
                HashMap::new(),
                None,
            )
            .await
            .unwrap();
        }

        // 检查应用分配
        let stats = pool.get_stats().await;
        assert!(stats.get("total_apps").unwrap() >= &4);
        assert_eq!(stats.get("total_threads"), Some(&4));

        // 清理
        for i in 0..4 {
            let _ = pool.unload_app(&format!("lb_app{}", i)).await;
        }
    }

    #[tokio::test]
    async fn test_unload_app() {
        let pool = TEST_POOL.lock().unwrap();

        let code = r#"globalThis.default = { fetch() { return new Response("ok"); } }"#;
        pool.load_app(
            "unload_test_app".to_string(),
            code.to_string(),
            HashMap::new(),
            None,
        )
        .await
        .unwrap();

        // 卸载应用
        let result = pool.unload_app("unload_test_app").await;
        assert!(result.is_ok());

        // 验证应用已删除
        let apps = pool.list_apps().await;
        assert!(!apps.contains(&"unload_test_app".to_string()));
    }
}
