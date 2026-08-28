// V8 Runtime Isolate wrapper

use super::cache_ops::{create_cache_store, CacheStore};
use super::loader::DbModuleLoader;
use super::ops::{EnvStore, ResponseStore};
use super::queue_ops::MemoryQueue;
use super::rate_limit::RateLimiter;
use super::route_match::{match_routes, path_from_url};
use super::runtime_init::{build_runtime_extensions, init_lazy_extensions};
use super::snapshot::load_or_create_startup_snapshot;
use super::ws_server_ops::{create_ws_server_manager, WsServerManager};
use anyhow::Result;
use deno_ast::{MediaType, ParseParams};
use deno_core::{
    JsRuntime, OpMetricsSummary, OpMetricsSummaryTracker, PollEventLoopOptions, RuntimeOptions,
};
use deno_runtime::deno_permissions;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use sys_traits::impls::RealSys;
use tokio::sync::oneshot;

type FetchResult = (u16, HashMap<String, String>, Option<String>);

#[derive(Serialize)]
struct JsRequest {
    url: String,
    method: String,
    headers: HashMap<String, String>,
    body: Option<String>,
    /// Route placeholder values (e.g. `:id` in "/users/:id"), resolved against
    /// the app's configured route patterns. Empty when no pattern matches, or
    /// when the app has no route patterns configured yet.
    params: HashMap<String, String>,
}

#[derive(Deserialize)]
struct JsResponse {
    status: u16,
    #[serde(default)]
    headers: HashMap<String, String>,
    body: Option<String>,
}

pub struct AppRuntime {
    runtime: JsRuntime,
    app_id: String,
    module_id: Option<deno_core::ModuleId>,
    entry_specifier: deno_core::ModuleSpecifier,
    resource_table_max: Option<usize>,
    op_metrics: Rc<OpMetricsSummaryTracker>,
    snapshot_loaded: bool,
    /// Route patterns (e.g. "/users/:id") from the app's manifest, used to
    /// populate `context.params` on each request. See `super::route_match`.
    route_patterns: Vec<String>,
    /// Maximum wall-clock time (ms) a single `execute_fetch` may run before
    /// this isolate is terminated and the request fails.
    timeout_ms: u64,
    /// Configured JS-op dispatch rate limit (ops/sec) for this isolate.
    /// Kept alongside the live `RateLimiter` (captured in the op-metrics
    /// closure) purely so tests can assert the configured value reached
    /// this isolate; not read outside of `#[cfg(test)]`.
    #[cfg_attr(not(test), allow(dead_code))]
    rate_limit_ops_per_sec: f64,
}

impl AppRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        app_id: String,
        code: String,
        entry_point: Option<String>,
        app_repo: Option<Arc<crate::app_repo::AppRepo>>,
        env: HashMap<String, String>,
        permissions_config: super::super::app::types::AppPermissions,
        memory_limit_mb: Option<usize>,
        resource_table_max: Option<usize>,
        route_patterns: Vec<String>,
        timeout_ms: u64,
        rate_limit_ops_per_sec: f64,
    ) -> Result<Self> {
        let mut create_params = deno_core::v8::CreateParams::default();
        if let Some(mb) = memory_limit_mb {
            create_params = create_params.heap_limits(0, mb * 1024 * 1024);
        }

        let entry_path = entry_point
            .unwrap_or_else(|| "app.ts".to_string())
            .trim_start_matches('/')
            .to_string();
        let specifier =
            deno_core::ModuleSpecifier::parse(&format!("deis:///apps/{}/{}", app_id, entry_path))?;
        deno_ast::parse_module(ParseParams {
            specifier: specifier.clone(),
            text: code.clone().into(),
            media_type: MediaType::TypeScript,
            capture_tokens: false,
            scope_analysis: false,
            maybe_syntax: None,
        })
        .map_err(|e| anyhow::anyhow!(e))?;

        let module_loader = Rc::new(DbModuleLoader::new(
            app_id.clone(),
            specifier.clone(),
            code.into_bytes(),
            app_repo,
        )?);

        let op_metrics = Rc::new(OpMetricsSummaryTracker::default());
        let op_metrics_tracker = op_metrics.clone();

        // Rate limiter: platform default, or this app's configured override.
        let limiter = Arc::new(Mutex::new(RateLimiter::new(rate_limit_ops_per_sec)));
        let isolate_handle_cell = Arc::new(OnceCell::<deno_core::v8::IsolateHandle>::new());
        let isolate_handle_cell_clone = isolate_handle_cell.clone();
        let app_id_clone = app_id.clone();

        let op_metrics_factory = Box::new(move |id, count, decl: &deno_core::OpDecl| {
            let tracker_factory = op_metrics_tracker
                .clone()
                .op_metrics_factory_fn(|d| d.name.starts_with("op_"));
            let tracker_fn = tracker_factory(id, count, decl);

            let limiter = limiter.clone();
            let handle_cell = isolate_handle_cell_clone.clone();
            let app_id = app_id_clone.clone();
            let op_name = decl.name.to_string();

            let callback = move |ctx: &_,
                                 event: deno_core::OpMetricsEvent,
                                 source: deno_core::OpMetricsSource| {
                // Call the original tracker first
                if let Some(ref f) = tracker_fn {
                    f(ctx, event, source);
                }

                // Only check Dispatched events
                if event == deno_core::OpMetricsEvent::Dispatched {
                    let mut limiter = limiter.lock().unwrap();
                    if !limiter.check() {
                        tracing::warn!(
                            target: "app",
                            "Rate limit exceeded for app {}, op: {}. Terminating isolate execution; the in-flight request will fail.",
                            app_id,
                            op_name
                        );
                        if let Some(handle) = handle_cell.get() {
                            handle.terminate_execution();
                        }
                    }
                }
            };

            Some(Rc::new(callback) as deno_core::OpMetricsFn)
        });

        let startup_snapshot = load_or_create_startup_snapshot()?;
        let snapshot_loaded = startup_snapshot.is_some();
        let mut runtime = JsRuntime::new(RuntimeOptions {
            create_params: Some(create_params),
            module_loader: Some(module_loader),
            op_metrics_factory_fn: Some(op_metrics_factory),
            startup_snapshot,
            extensions: build_runtime_extensions(),
            ..Default::default()
        });

        // Initialize Handle
        let _ = isolate_handle_cell.set(runtime.v8_isolate().thread_safe_handle());

        {
            let sys = RealSys;
            let parser = Arc::new(deno_permissions::RuntimePermissionDescriptorParser::new(
                sys,
            ));

            let permission_opts = deno_permissions::PermissionsOptions {
                allow_env: if permissions_config.allow_env.is_empty()
                    || permissions_config.allow_env.contains(&"*".to_string())
                {
                    None
                } else {
                    Some(permissions_config.allow_env)
                },
                allow_net: if permissions_config.allow_net.is_empty()
                    || permissions_config.allow_net.contains(&"*".to_string())
                {
                    None
                } else {
                    Some(permissions_config.allow_net)
                },
                ..Default::default()
            };

            let permissions =
                deno_permissions::Permissions::from_options(parser.as_ref(), &permission_opts)
                    .unwrap();

            let permissions_container =
                deno_permissions::PermissionsContainer::new(parser.clone(), permissions);

            init_lazy_extensions(&mut runtime)?;

            let op_state = runtime.op_state();
            let mut state = op_state.borrow_mut();

            state.put(permissions_container);
            state.put(parser);

            let env_store: EnvStore = Rc::new(RefCell::new(env));
            state.put(env_store);

            let cache_store: CacheStore = create_cache_store(10_000, 600);
            state.put(cache_store);

            let ws_server_manager: WsServerManager = create_ws_server_manager();
            state.put(ws_server_manager);

            let response_store: ResponseStore = Rc::new(RefCell::new(None));
            state.put(response_store);

            let memory_queue: MemoryQueue = Rc::new(RefCell::new(Vec::new()));
            state.put(memory_queue);
        }

        if let Some(limit) = resource_table_max {
            tracing::info!(
                target: "app",
                "ResourceTable quota enabled: app_id={}, limit={}",
                app_id,
                limit
            );
        }

        // Configure OOM handling: terminate execution when heap memory is near limit instead of panicking
        if memory_limit_mb.is_some() {
            let handle = runtime.v8_isolate().thread_safe_handle();
            let app_id_clone = app_id.clone();

            runtime.add_near_heap_limit_callback(move |current, initial| {
                tracing::error!(
                    target: "app",
                    "OOM detected for app {}: current={} bytes, limit={} bytes. Terminating.",
                    app_id_clone,
                    current,
                    initial
                );

                // Terminate execution
                handle.terminate_execution();

                // Return current value + 10MB to give V8 space to complete termination and avoid immediate crash
                current + 10 * 1024 * 1024
            });
        }

        Ok(Self {
            runtime,
            app_id,
            module_id: None,
            entry_specifier: specifier,
            resource_table_max,
            op_metrics,
            snapshot_loaded,
            route_patterns,
            timeout_ms,
            rate_limit_ops_per_sec,
        })
    }

    pub async fn init_module(&mut self) -> Result<()> {
        if self.module_id.is_some() {
            return Ok(());
        }

        // Skip bootstrap load if snapshot is already loaded
        if !self.snapshot_loaded {
            let bootstrap_specifier = deno_core::ModuleSpecifier::parse(
                "ext:bootstrap/src/runtime/js/bootstrap.js",
            )?;
            let bootstrap_id = self
                .runtime
                .load_side_es_module(&bootstrap_specifier)
                .await?;
            let bootstrap_eval = self.runtime.mod_evaluate(bootstrap_id);
            self.runtime
                .with_event_loop_promise(bootstrap_eval, PollEventLoopOptions::default())
                .await?;
        }

        self.runtime
            .execute_script("<runtime>", include_str!("js/runtime.js"))?;

        let mod_id = self
            .runtime
            .load_main_es_module(&self.entry_specifier)
            .await?;

        let evaluation = self.runtime.mod_evaluate(mod_id);

        let result = self
            .runtime
            .with_event_loop_promise(evaluation, PollEventLoopOptions::default())
            .await;

        if let Err(e) = result {
            return Err(anyhow::anyhow!("Module evaluation failed: {}", e));
        }

        let glue_code = format!(
            r#"
            (async () => {{
                const mod = await import("{}");
                globalThis.user_module = {{ default: mod.default }};
                if (mod.fetch) {{
                    globalThis.user_module.fetch = mod.fetch;
                }}
            }})();
        "#,
            self.entry_specifier.as_str()
        );

        let promise = self.runtime.execute_script("<glue>", glue_code)?;
        let resolve_future = self.runtime.resolve(promise);
        self.runtime
            .with_event_loop_promise(resolve_future, PollEventLoopOptions::default())
            .await?;

        self.module_id = Some(mod_id);

        Ok(())
    }

    /// Connect debugger
    pub fn connect_inspector(
        &mut self,
    ) -> Result<(
        tokio::sync::mpsc::UnboundedSender<String>,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    )> {
        let inspector = self.runtime.inspector();
        let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
        let (in_tx, mut in_rx) = tokio::sync::mpsc::unbounded_channel();

        // V8 -> Frontend
        let callback = Box::new(move |msg: deno_core::InspectorMsg| {
            let _ = out_tx.send(msg.content);
        });

        let mut session = deno_core::JsRuntimeInspector::create_local_session(
            inspector,
            callback,
            deno_core::InspectorSessionKind::NonBlocking {
                wait_for_disconnect: false,
            },
        );

        // Frontend -> V8
        tokio::task::spawn_local(async move {
            while let Some(msg) = in_rx.recv().await {
                session.dispatch(msg);
            }
        });

        Ok((in_tx, out_rx))
    }

    pub async fn execute_fetch(
        &mut self,
        url: String,
        method: String,
        headers: HashMap<String, String>,
        body: Option<String>,
        response_tx: oneshot::Sender<Result<FetchResult>>,
    ) -> Result<()> {
        if self.module_id.is_none() {
            if let Err(e) = self.init_module().await {
                let _ = response_tx.send(Err(e));
                return Ok(());
            }
        }

        let pathname = path_from_url(&url);
        let params = match_routes(&self.route_patterns, &pathname);
        if !params.is_empty() {
            tracing::debug!(
                target: "app",
                "Matched route params for app {}: path={}, params={:?}",
                self.app_id,
                pathname,
                params
            );
        } else if !self.route_patterns.is_empty() {
            tracing::trace!(
                target: "app",
                "No configured route pattern matched: app={}, path={}",
                self.app_id,
                pathname
            );
        }

        let request = JsRequest {
            url,
            method,
            headers,
            body,
            params,
        };

        let request_json = serde_json::to_string(&request)?;

        let code = format!(
            r#"
            (async () => {{
                const request = {};
                const response = await __deis_runtime_invoke(request);
                __deis_runtime_set_response(JSON.stringify(response));
            }})()
            "#,
            request_json
        );

        let promise = self.runtime.execute_script("<fetch>", code)?;

        // Set execution timeout: platform default, or this app's configured override.
        let timeout_ms = self.timeout_ms;
        let isolate_handle = self.runtime.v8_isolate().thread_safe_handle();
        let app_id_for_timeout = self.app_id.clone();

        // Start timeout monitoring task
        let timeout_task = tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(timeout_ms)).await;
            tracing::warn!(
                target: "app",
                "App {} execution timed out after {}ms. Terminating isolate execution; the request will fail.",
                app_id_for_timeout,
                timeout_ms
            );
            isolate_handle.terminate_execution();
        });

        let resolve_future = self.runtime.resolve(promise);
        let result = self
            .runtime
            .with_event_loop_promise(resolve_future, PollEventLoopOptions::default())
            .await;

        // Cancel timeout task
        timeout_task.abort();

        // Check results
        result?;

        let json_str = {
            let op_state = self.runtime.op_state();
            let state = op_state.borrow();
            let response_store = state.borrow::<ResponseStore>().clone();
            let response = response_store.borrow_mut().take();
            response.ok_or_else(|| anyhow::anyhow!("No response received from app"))?
        };

        let response: JsResponse = serde_json::from_str(&json_str)?;

        let _ = response_tx.send(Ok((response.status, response.headers, response.body)));

        let wait_code = "__deis_runtime_wait_until_all()";
        let wait_promise = self.runtime.execute_script("<wait_until>", wait_code)?;
        let wait_resolve = self.runtime.resolve(wait_promise);
        self.runtime
            .with_event_loop_promise(wait_resolve, PollEventLoopOptions::default())
            .await?;

        let resource_count = {
            let op_state = self.runtime.op_state();
            let state = op_state.borrow();
            state.resource_table.len()
        };

        if let Some(limit) = self.resource_table_max {
            if resource_count > limit {
                tracing::warn!(
                    target: "app",
                    "ResourceTable limit exceeded: app_id={}, limit={}, current={}",
                    self.app_id,
                    limit,
                    resource_count
                );
                return Err(anyhow::anyhow!(
                    "ResourceTable limit exceeded: current={}, limit={}",
                    resource_count,
                    limit
                ));
            }
        }

        let metrics = self.op_metrics.aggregate();
        tracing::debug!(
            target: "app",
            "OpMetrics summary: app_id={}, sync={}, async={}, fast={}, completed_async={}",
            self.app_id,
            metrics.ops_dispatched_sync,
            metrics.ops_dispatched_async,
            metrics.ops_dispatched_fast,
            metrics.ops_completed_async
        );

        Ok(())
    }

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

    pub async fn dispatch_ws_message(&mut self, ws_id: String, message: String) -> Result<()> {
        let code = format!(
            r#"
            (async () => {{
                if (globalThis.__deis_runtime_ws_servers && globalThis.__deis_runtime_ws_servers["{}"] !== undefined) {{
                    const ws = globalThis.__deis_runtime_ws_servers["{}"];
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

    pub async fn close_ws_connection(
        &mut self,
        ws_id: String,
        code: u16,
        reason: String,
    ) -> Result<()> {
        let js_code = format!(
            r#"
            (async () => {{
                if (globalThis.__deis_runtime_ws_servers && globalThis.__deis_runtime_ws_servers["{}"] !== undefined) {{
                    const ws = globalThis.__deis_runtime_ws_servers["{}"];
                    if (ws && typeof ws._handleClose === "function") {{
                        ws._handleClose({}, {});
                    }}
                    delete globalThis.__deis_runtime_ws_servers["{}"];
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

    #[cfg(test)]
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// Exposes the effective execution timeout for this isolate. Behavioral
    /// termination tests are unreliable here: `execute_fetch`'s timeout is
    /// enforced by a `tokio::spawn`ed sleep-and-terminate task, which on the
    /// `current_thread` runtime this isolate runs on only gets scheduled once
    /// the polling task yields -- something a JS handler that never performs
    /// a genuine async Rust op (only synchronous work or microtasks) may
    /// never do, no matter how long it runs. That is a pre-existing
    /// characteristic of the timeout mechanism itself, not something this
    /// accessor works around; it exists so tests can assert the *value*
    /// reached the isolate without depending on that timing.
    #[cfg(test)]
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    /// Exposes the effective JS-op rate limit for this isolate, for the same
    /// reason as `timeout_ms` above.
    #[cfg(test)]
    pub fn rate_limit_ops_per_sec(&self) -> f64 {
        self.rate_limit_ops_per_sec
    }

    pub fn op_metrics_summary(&self) -> OpMetricsSummary {
        self.op_metrics.aggregate()
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
