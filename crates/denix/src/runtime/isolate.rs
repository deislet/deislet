// V8 Runtime Isolate 封装

use super::blob_ops::init_blob_extension;
use super::cache_ops::{create_cache_store, init_cache_extension, CacheStore};
use super::db_ops::init_db_extension;
// use super::http_ops::init_http_extension;
use super::ops::{init_ops_extension, EnvStore, ResponseStore};
use super::queue_ops::{init_queue_extension, MemoryQueue};
// use super::ws_ops::{create_ws_manager, /* init_ws_extension, */ WebSocketManager};
use super::ws_server_ops::{create_ws_server_manager, init_ws_server_extension, WsServerManager};
use anyhow::Result;
use deno_core::{JsRuntime, PollEventLoopOptions, RuntimeOptions};
// use deno_runtime::deno_console;
use deno_runtime::deno_crypto;
use deno_runtime::deno_fetch;
// use deno_runtime::deno_fs::RealFs;
use deno_runtime::deno_net;
use deno_runtime::deno_permissions;
// use deno_runtime::deno_tls;
// use deno_runtime::deno_url;
use deno_runtime::deno_web;
use deno_runtime::deno_webidl;
use deno_runtime::deno_websocket;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use sys_traits::impls::RealSys;
use tokio::sync::oneshot;

type FetchResult = (u16, HashMap<String, String>, Option<String>);

/// JavaScript Request 对象
#[derive(Serialize)]
struct JsRequest {
    url: String,
    method: String,
    headers: HashMap<String, String>,
    body: Option<String>,
}

/// JavaScript Response 对象
#[derive(Deserialize)]
struct JsResponse {
    status: u16,
    #[serde(default)]
    headers: HashMap<String, String>,
    body: Option<String>,
}

/// 应用 Runtime
pub struct AppRuntime {
    runtime: JsRuntime,
    app_id: String,
}

impl AppRuntime {
    /// 创建新的 Runtime
    pub fn new(
        app_id: String,
        code: String,
        env: HashMap<String, String>,
        memory_limit_mb: Option<usize>,
    ) -> Result<Self> {
        let mut create_params = deno_core::v8::CreateParams::default();
        if let Some(mb) = memory_limit_mb {
            create_params = create_params.heap_limits(0, mb * 1024 * 1024);
        }

        // 创建 Runtime
        let mut runtime = JsRuntime::new(RuntimeOptions {
            create_params: Some(create_params),
            extensions: vec![
                deno_webidl::deno_webidl::init(),
                deno_web::deno_web::lazy_init(),
                deno_fetch::deno_fetch::lazy_init(),
                deno_websocket::deno_websocket::lazy_init(),
                deno_crypto::deno_crypto::lazy_init(),
                deno_runtime::deno_tls::deno_tls::init(),
                deno_net::deno_net::lazy_init(),
                init_ops_extension(),
                init_cache_extension(),
                init_db_extension(),
                init_blob_extension(),
                init_ws_server_extension(),
                init_queue_extension(),
            ],
            ..Default::default()
        });

        // Initialize Lazy Extensions & State
        {
            let blob_store = Arc::new(deno_web::BlobStore::default());
            let broadcast_channel = deno_web::InMemoryBroadcastChannel::default();
            
            // Permissions
            let sys = RealSys;
            let parser = Arc::new(deno_permissions::RuntimePermissionDescriptorParser::new(sys));
            let permissions = deno_permissions::PermissionsContainer::allow_all(parser);

            runtime.lazy_init_extensions(vec![
                deno_web::deno_web::args(
                    blob_store,
                    Some(deno_core::url::Url::parse("file:///").unwrap()),
                    broadcast_channel,
                ),
                deno_fetch::deno_fetch::args(deno_fetch::Options {
                    user_agent: "denix/1.0".to_string(),
                    root_cert_store_provider: None,
                    ..Default::default()
                }),
                deno_websocket::deno_websocket::args(),
                deno_crypto::deno_crypto::args(None),
                deno_net::deno_net::args(None, None),
            ])?;

            // Inject State
            let op_state = runtime.op_state();
            let mut state = op_state.borrow_mut();

            state.put(permissions);

            // 环境变量
            let env_store: EnvStore = Rc::new(RefCell::new(env));
            state.put(env_store);

            // Cache Store (moka 高性能缓存)
            // 配置: 最大 10000 条目，10 分钟空闲过期
            let cache_store: CacheStore = create_cache_store(10_000, 600);
            state.put(cache_store);

            // WebSocket Server Manager (WebSocket 服务端连接管理)
            let ws_server_manager: WsServerManager = create_ws_server_manager();
            state.put(ws_server_manager);

            // Response Store (临时响应存储)
            let response_store: ResponseStore = Rc::new(RefCell::new(None));
            state.put(response_store);

            // Memory Queue (本地队列 Fallback)
            let memory_queue: MemoryQueue = Rc::new(RefCell::new(Vec::new()));
            state.put(memory_queue);
        }

        // 加载 runtime polyfills
        runtime.execute_script("<runtime>", include_str!("../../runtime_js/runtime.js"))?;

        // 转换 ES 模块语法为普通脚本
        let code = Self::transform_esm_to_script(&code);

        // 加载应用代码
        let app_name = format!("<app:{}>", app_id);
        runtime.execute_script(app_name, code)?;

        Ok(Self { runtime, app_id })
    }



    /// 执行 fetch 请求
    pub async fn execute_fetch(
        &mut self,
        url: String,
        method: String,
        headers: HashMap<String, String>,
        body: Option<String>,
        response_tx: oneshot::Sender<Result<FetchResult>>,
    ) -> Result<()> {
        // 构造 Request 对象
        let request = JsRequest {
            url,
            method,
            headers,
            body,
        };

        // 序列化 request
        let request_json = serde_json::to_string(&request)?;

        // 调用 JavaScript 的 __denix_invoke 函数并通过 op 设置响应
        let code = format!(
            r#"
            (async () => {{
                const request = {};
                const response = await __denix_invoke(request);
                __denix_set_response(JSON.stringify(response));
            }})()
            "#,
            request_json
        );

        // 执行代码，返回 Promise
        let promise = self.runtime.execute_script("<fetch>", code)?;

        // 使用正确的模式：resolve + with_event_loop_promise
        let resolve_future = self.runtime.resolve(promise);
        self.runtime
            .with_event_loop_promise(resolve_future, PollEventLoopOptions::default())
            .await?;

        // 从 ResponseStore 获取响应
        let json_str = {
            let op_state = self.runtime.op_state();
            let state = op_state.borrow();
            let response_store = state.borrow::<ResponseStore>().clone();
            let response = response_store.borrow_mut().take();
            response.ok_or_else(|| anyhow::anyhow!("No response received from app"))?
        };

        // 反序列化 response
        let response: JsResponse = serde_json::from_str(&json_str)?;

        // 发送响应给客户端
        let _ = response_tx.send(Ok((response.status, response.headers, response.body)));

        // 等待 waitUntil 任务完成
        let wait_code = "__denix_wait_until_all()";
        let wait_promise = self.runtime.execute_script("<wait_until>", wait_code)?;
        let wait_resolve = self.runtime.resolve(wait_promise);
        self.runtime
            .with_event_loop_promise(wait_resolve, PollEventLoopOptions::default())
            .await?;

        Ok(())
    }


    /// 注册 WebSocket 连接
    ///
    /// 将 WebSocket 连接注册到 WsServerManager，以便 JavaScript 可以通过 op_ws_server_send 发送消息
    pub async fn register_ws_connection(
        &mut self,
        ws_id: String,
        sender: tokio::sync::mpsc::UnboundedSender<super::WsServerMessage>,
    ) -> Result<()> {
        let ws_manager = {
            let op_state = self.runtime.op_state();
            let mut state = op_state.borrow_mut();
            state
                .try_borrow_mut::<super::WsServerManager>()
                .ok_or_else(|| anyhow::anyhow!("WsServerManager not available"))?
                .clone()
        };

        let mut manager = ws_manager.write().await;
        manager.insert(ws_id.clone(), sender);

        tracing::debug!("Registered WebSocket connection: {}", ws_id);
        Ok(())
    }

    /// 分发 WebSocket 消息到 JavaScript
    ///
    /// 调用 JavaScript 的 WebSocketServer._handleMessage() 方法
    pub async fn dispatch_ws_message(&mut self, ws_id: String, message: String) -> Result<()> {
        let code = format!(
            r#"
            (async () => {{
                // 查找对应的 WebSocketServer 实例
                if (globalThis.__denix_ws_servers && globalThis.__denix_ws_servers["{}"] !== undefined) {{
                    const ws = globalThis.__denix_ws_servers["{}"];
                    if (ws && typeof ws._handleMessage === "function") {{
                        ws._handleMessage({});
                    }}
                }}
            }})()
            "#,
            ws_id,
            ws_id,
            serde_json::to_string(&message)?
        );

        let promise = self.runtime.execute_script("<ws_message>", code)?;
        let resolve_future = self.runtime.resolve(promise);
        self.runtime
            .with_event_loop_promise(resolve_future, PollEventLoopOptions::default())
            .await?;

        Ok(())
    }

    /// 关闭 WebSocket 连接
    ///
    /// 调用 JavaScript 的 WebSocketServer._handleClose() 方法并清理连接
    pub async fn close_ws_connection(
        &mut self,
        ws_id: String,
        code: u16,
        reason: String,
    ) -> Result<()> {
        // 调用 JavaScript 的 _handleClose
        let js_code = format!(
            r#"
            (async () => {{
                if (globalThis.__denix_ws_servers && globalThis.__denix_ws_servers["{}"] !== undefined) {{
                    const ws = globalThis.__denix_ws_servers["{}"];
                    if (ws && typeof ws._handleClose === "function") {{
                        ws._handleClose({}, {});
                    }}
                    delete globalThis.__denix_ws_servers["{}"];
                }}
            }})()
            "#,
            ws_id,
            ws_id,
            code,
            serde_json::to_string(&reason)?,
            ws_id
        );

        let promise = self.runtime.execute_script("<ws_close>", js_code)?;
        let resolve_future = self.runtime.resolve(promise);
        self.runtime
            .with_event_loop_promise(resolve_future, PollEventLoopOptions::default())
            .await?;

        // 从 WsServerManager 中移除
        let ws_manager_opt = {
            let op_state = self.runtime.op_state();
            let mut state = op_state.borrow_mut();
            state.try_borrow_mut::<super::WsServerManager>().cloned()
        };

        if let Some(ws_manager) = ws_manager_opt {
            let mut manager = ws_manager.write().await;
            manager.remove(&ws_id);
            tracing::debug!("Closed WebSocket connection: {}", ws_id);
        }

        Ok(())
    }

    /// 获取应用 ID
    #[cfg(test)]
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    fn transform_esm_to_script(code: &str) -> String {
        use regex::Regex;

        // TypeScript: 移除 interface 定义块
        let re_interface = Regex::new(r"interface\s+\w+\s*\{[^}]*\}\s*").unwrap();
        let transformed = re_interface.replace_all(code, "");

        // TypeScript: 移除 type 别名定义
        let re_type_alias = Regex::new(r"type\s+\w+\s*=\s*[^;]+;\s*").unwrap();
        let transformed = re_type_alias.replace_all(&transformed, "");

        // export default { ... } -> globalThis.user_module.default = { ... }
        let re_export_default = Regex::new(r"export\s+default\s+").unwrap();
        let transformed =
            re_export_default.replace_all(&transformed, "globalThis.user_module.default = ");

        // export const/let/var name = ... -> globalThis.user_module.name = ...
        let re_export_decl = Regex::new(r"export\s+(const|let|var)\s+(\w+)\s*=").unwrap();
        let transformed = re_export_decl.replace_all(&transformed, "globalThis.user_module.$2 =");

        // export function name(...) -> globalThis.user_module.name = function(...)
        let re_export_func = Regex::new(r"export\s+function\s+(\w+)\s*\(").unwrap();
        let transformed =
            re_export_func.replace_all(&transformed, "globalThis.user_module.$1 = function(");

        // export async function name(...) -> globalThis.user_module.name = async function(...)
        let re_export_async_func = Regex::new(r"export\s+async\s+function\s+(\w+)\s*\(").unwrap();
        let transformed = re_export_async_func
            .replace_all(&transformed, "globalThis.user_module.$1 = async function(");

        // import ... from "..." -> 移除 (简化处理，假设依赖已内置)
        let re_import = Regex::new(r#"import\s+.*?\s+from\s+["'][^"']+["'];?\s*"#).unwrap();
        let transformed = re_import.replace_all(&transformed, "");

        // TypeScript: 移除函数返回类型 ): Promise<Response> { -> ) {
        let re_return_type = Regex::new(r"\)\s*:\s*[A-Za-z_][\w<>,\s\[\]|&]*\s*\{").unwrap();
        let transformed = re_return_type.replace_all(&transformed, ") {");

        // TypeScript: 移除箭头函数返回类型 ): Type => -> ) =>
        let re_arrow_return = Regex::new(r"\)\s*:\s*[A-Za-z_][\w<>,\s\[\]|&]*\s*=>").unwrap();
        let transformed = re_arrow_return.replace_all(&transformed, ") =>");

        // TypeScript: 移除函数签名中的参数类型标注
        // 匹配函数签名: name( ... ) 或 async name( ... )，在括号内移除类型标注
        let re_func_sig =
            Regex::new(r"((?:async\s+)?(?:function\s+)?\w+\s*\()([^)]*)(\))").unwrap();
        let re_param_type_inner =
            Regex::new(r"(\w+)\s*:\s*[A-Za-z_]\w*(?:<[^>]*>)?(?:\[\])?").unwrap();
        let transformed = re_func_sig.replace_all(&transformed, |caps: &regex::Captures| {
            let prefix = &caps[1];
            let params = &caps[2];
            let suffix = &caps[3];
            let clean_params = re_param_type_inner.replace_all(params, "$1");
            format!("{}{}{}", prefix, clean_params, suffix)
        });

        // TypeScript: 移除变量类型标注 const/let/var name: Type = -> const/let/var name =
        let re_var_type =
            Regex::new(r"(const|let|var)\s+(\w+)\s*:\s*[A-Za-z_][\w<>,\s\[\]|&]*\s*=").unwrap();
        let transformed = re_var_type.replace_all(&transformed, "$1 $2 =");

        transformed.to_string()
    }
}

impl Drop for AppRuntime {
    fn drop(&mut self) {
        tracing::debug!("Dropping runtime for app: {}", self.app_id);
    }
}

#[cfg(test)]
#[path = "isolate_test.rs"]
mod isolate_test;
