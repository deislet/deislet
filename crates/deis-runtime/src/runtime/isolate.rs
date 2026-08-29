// V8 Runtime Isolate wrapper

use super::bindings::{AppServices, ServiceWiring};
use super::cache_ops::{create_cache_store, CacheStore};
use super::loader::DbModuleLoader;
use super::metrics::{recorder, MetricsRegistry, RequestOutcome};
use super::ops::{AppId, EnvStore, ResponseStore};
use super::queue_client::LeasedMessage;
use super::queue_consumer::BatchOutcome;
use super::rate_limit::RateLimiter;
use super::route_match::{match_routes, path_from_url};
use super::runtime_init::{build_runtime_extensions, init_lazy_extensions};
use super::snapshot::load_or_create_startup_snapshot;
use super::ws_server_ops::{create_ws_server_manager, WsServerManager};
use crate::app::types::AppPermissions;
use crate::config::ExecutionLimits;
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
use std::time::Instant;
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

/// One leased message as the JS side sees it.
#[derive(Serialize)]
struct JsQueueMessage {
    id: String,
    body: String,
    /// Deliveries so far, including this one. Above 1 means the app has seen
    /// this message before and its handler has to be idempotent.
    attempts: i32,
    /// Unix second the message was enqueued.
    timestamp: i64,
}

#[derive(Serialize)]
struct JsQueueBatch {
    queue: String,
    messages: Vec<JsQueueMessage>,
}

/// What `__deis_runtime_invoke_queue` reports back.
#[derive(Deserialize)]
struct JsQueueResult {
    /// Ids the handler did not complete; everything else is acknowledged.
    #[serde(default)]
    failed: Vec<String>,
    /// Set when the whole batch failed: a handler that threw, or an app that
    /// declares a queue consumer but exports no handler.
    #[serde(default)]
    error: Option<String>,
}

/// What the runtime hands `__deis_runtime_invoke_scheduled`.
#[derive(Serialize)]
struct JsScheduledEvent {
    cron: String,
    /// Unix milliseconds the tick was due, so a handler can tell a late run
    /// from an on-time one.
    #[serde(rename = "scheduledTime")]
    scheduled_time: i64,
}

/// What `__deis_runtime_invoke_scheduled` reports back.
#[derive(Deserialize)]
struct JsScheduledResult {
    #[serde(default)]
    ok: bool,
    /// Set when the handler threw, or when the app declares a cron
    /// expression and exports no handler to run on it.
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct JsResponse {
    status: u16,
    #[serde(default)]
    headers: HashMap<String, String>,
    body: Option<String>,
}

/// Everything one isolate needs beyond its identity and its source.
///
/// These values used to travel as a run of positional arguments through
/// `RuntimePool::load_app`, `RuntimeMessage::LoadApp` and `AppRuntime::new`.
/// Several of them share a type (`Option<usize>` twice, and previously a bare
/// `u64` and `f64` for the limits), so a swapped pair compiled cleanly and only
/// showed up at runtime as an app with limits nobody configured. Naming the
/// fields makes that class of mistake a compile error, and adding a limit now
/// touches one struct instead of four signatures.
#[derive(Debug, Clone, Default)]
pub struct IsolateSpec {
    /// Module path inside the app bundle; defaults to `app.ts` when absent.
    pub entry_point: Option<String>,

    /// Values visible to the app through `Deno.env` / `ctx.env`.
    pub env: HashMap<String, String>,

    /// Net and env permissions granted to the isolate.
    pub permissions: AppPermissions,

    /// V8 heap ceiling (MB). `None` leaves V8 on its own default.
    pub memory_limit_mb: Option<usize>,

    /// Maximum live entries in the op resource table. `None` means unbounded.
    pub resource_table_max: Option<usize>,

    /// Route patterns from the app's manifest, used to fill `context.params`.
    pub route_patterns: Vec<String>,

    /// Effective execution limits: the app's own overrides where it set any,
    /// the platform defaults otherwise.
    pub limits: ExecutionLimits,
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
    /// Where this isolate's request, error, execution-time and cut-off
    /// counters go. See `super::metrics`.
    metrics: Arc<MetricsRegistry>,
}

impl AppRuntime {
    /// Build an isolate for one application in one environment.
    ///
    /// `app_id` is the identity this node runs the application under, which
    /// is the pair spelled as a tenant: [`crate::app::AppKey::tenant_id`],
    /// e.g. `demo.production`. It is what the storage clients are charged to,
    /// so production and a preview of one application never share a
    /// namespace, and it is the dimension every log line and metric of this
    /// isolate carries.
    ///
    /// `wiring` says which platform services the app declared and where they
    /// live; only the declared ones are constructed, and none of them dials
    /// anything until the app makes a call. Pass
    /// [`ServiceWiring::default()`] for an isolate that should reach no
    /// storage at all -- every `ctx.services` call then answers "service not
    /// available", which is what an app that declared nothing should get.
    pub fn new(
        app_id: String,
        code: String,
        app_repo: Option<Arc<crate::app_repo::AppRepo>>,
        spec: IsolateSpec,
        wiring: &ServiceWiring,
    ) -> Result<Self> {
        let IsolateSpec {
            entry_point,
            env,
            permissions: permissions_config,
            memory_limit_mb,
            resource_table_max,
            route_patterns,
            limits,
        } = spec;
        let ExecutionLimits {
            timeout_ms,
            rate_limit_ops_per_sec,
        } = limits;

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
        let metrics = recorder().clone();
        let metrics_for_limiter = metrics.clone();
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
            let metrics = metrics_for_limiter.clone();
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
                        metrics.record_rate_limit_cutoff(&app_id);
                        tracing::warn!(
                            target: "app",
                            app_id = %app_id,
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

        // Built before the runtime so a malformed address, or an identity
        // that is not a tenant, fails the load outright rather than after V8
        // has been spun up. An address that is merely unreachable is not a
        // failure here: the channels are lazy.
        let services = AppServices::connect(&app_id, wiring)?;
        if services.is_empty() && !wiring.declaration.is_empty() {
            tracing::warn!(
                app_id = %app_id,
                declared = %wiring.declaration.to_csv(),
                "App {} declared services ({}) but none could be bound; ctx.services calls will \
                 fail with \"service not available\"",
                app_id,
                wiring.declaration.to_csv()
            );
        }

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

            // The Cache API's store. Edge Canon makes it a basic feature, so
            // it is built for every isolate whether or not the application
            // declares anything, and it lives and dies with this isolate.
            // 10 000 responses, and ten minutes of freshness for a response
            // that named no `max-age` of its own.
            let cache_store: CacheStore = create_cache_store(10_000, 600);
            state.put(cache_store);

            let ws_server_manager: WsServerManager = create_ws_server_manager();
            state.put(ws_server_manager);

            let response_store: ResponseStore = Rc::new(RefCell::new(None));
            state.put(response_store);

            // Ops that emit logs need to say which tenant they are acting for;
            // see `AppId`.
            state.put(AppId(app_id.clone()));

            // The storage bindings. Only what the app declared is present;
            // an op that finds its client missing answers "service not
            // available", which is the honest answer for a service the app
            // never asked for. Nothing stands in for an absent client -- an
            // app told plainly that it has no queue is better off than one
            // handed a receipt for a message that went nowhere.
            services.install(&mut state);
        }

        if let Some(limit) = resource_table_max {
            tracing::info!(
                target: "app",
                app_id = %app_id,
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
                    app_id = %app_id_clone,
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
            metrics,
        })
    }

    /// Clear V8's termination flag, so a cut-off costs one execution and not
    /// the isolate.
    ///
    /// `terminate_execution` is not spent when the execution it killed
    /// returns. deno_core cancels the flag only long enough to build the error
    /// object and then sets it again before handing that error back
    /// (deno_core 0.380 `error.rs`), so an isolate is still marked for
    /// termination after a timeout or a rate-limit cut-off. Left that way it
    /// is finished for good: the next `execute_script` fails immediately, so
    /// every later request answers 500, every queue batch is handed back until
    /// its delivery attempts run out and the messages are dead lettered, and
    /// every cron tick fails on whichever node the control plane picks. One
    /// slow request would take the application off this node until it was
    /// redeployed or the process restarted.
    ///
    /// Cancelling unconditionally, rather than only when
    /// `is_execution_terminating()` says so, is deliberate: that predicate is
    /// true only while JS frames are still unwinding the termination. Once an
    /// execution has returned there are no frames left, so a flag that is set
    /// and waiting for the next entry into JS reads as *not* terminating --
    /// and is exactly the one that has to go. Cancelling when nothing was
    /// terminating is a no-op.
    ///
    /// Called both after an execution and before the next one: the timeout
    /// task can set the flag in the window between the execution returning and
    /// the task being aborted, and some error paths return before the first
    /// call is reached.
    ///
    /// The cut-off itself is already logged, at warn, by whichever guard fired
    /// it; this is the cleanup, and it says nothing of its own.
    fn clear_termination(&mut self) {
        self.runtime.v8_isolate().cancel_terminate_execution();
    }

    pub async fn init_module(&mut self) -> Result<()> {
        if self.module_id.is_some() {
            return Ok(());
        }

        // Skip bootstrap load if snapshot is already loaded
        if !self.snapshot_loaded {
            let bootstrap_specifier =
                deno_core::ModuleSpecifier::parse("ext:bootstrap/src/runtime/js/bootstrap.js")?;
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
                if (mod.queue) {{
                    globalThis.user_module.queue = mod.queue;
                }}
                if (mod.scheduled) {{
                    globalThis.user_module.scheduled = mod.scheduled;
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
        let started = Instant::now();

        // A flag left over from the previous execution would fail this one
        // before a line of the app's code ran.
        self.clear_termination();

        if self.module_id.is_none() {
            if let Err(e) = self.init_module().await {
                // A rate-limit cut-off can fire while the module is still
                // being evaluated, and leaves the same flag set as a timeout.
                self.clear_termination();
                self.metrics
                    .record_request(&self.app_id, started.elapsed(), RequestOutcome::Error);
                let _ = response_tx.send(Err(e));
                return Ok(());
            }
        }

        let pathname = path_from_url(&url);
        let params = match_routes(&self.route_patterns, &pathname);
        if !params.is_empty() {
            tracing::debug!(
                target: "app",
                app_id = %self.app_id,
                "Matched route params for app {}: path={}, params={:?}",
                self.app_id,
                pathname,
                params
            );
        } else if !self.route_patterns.is_empty() {
            tracing::trace!(
                target: "app",
                app_id = %self.app_id,
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
        let metrics_for_timeout = self.metrics.clone();

        // Start timeout monitoring task
        let timeout_task = tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(timeout_ms)).await;
            metrics_for_timeout.record_timeout_cutoff(&app_id_for_timeout);
            tracing::warn!(
                target: "app",
                app_id = %app_id_for_timeout,
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

        // The timeout task and the rate limiter both work by terminating the
        // isolate, and that flag outlives the execution it killed.
        self.clear_termination();

        // Check results. A terminated isolate (timeout or rate-limit cut-off)
        // and a script error both land here, and both cost the request.
        if let Err(e) = &result {
            self.metrics
                .record_request(&self.app_id, started.elapsed(), RequestOutcome::Error);
            tracing::debug!(
                target: "app",
                app_id = %self.app_id,
                error = %e,
                "Isolate execution for app {} did not complete",
                self.app_id
            );
        }
        result?;

        let json_str = {
            let op_state = self.runtime.op_state();
            let state = op_state.borrow();
            let response_store = state.borrow::<ResponseStore>().clone();
            let response = response_store.borrow_mut().take();
            match response {
                Some(json) => json,
                None => {
                    self.metrics.record_request(
                        &self.app_id,
                        started.elapsed(),
                        RequestOutcome::Error,
                    );
                    anyhow::bail!("No response received from app");
                }
            }
        };

        let response: JsResponse = match serde_json::from_str::<JsResponse>(&json_str) {
            Ok(response) => response,
            Err(e) => {
                self.metrics
                    .record_request(&self.app_id, started.elapsed(), RequestOutcome::Error);
                return Err(e.into());
            }
        };

        // A 5xx is the app reporting its own failure, and it is the number an
        // operator means by "error rate", so it counts here as well as a
        // crash does.
        let outcome = if response.status >= 500 {
            RequestOutcome::Error
        } else {
            RequestOutcome::Ok
        };
        self.metrics
            .record_request(&self.app_id, started.elapsed(), outcome);

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
                    app_id = %self.app_id,
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

        let op_summary = self.op_metrics.aggregate();
        tracing::debug!(
            target: "app",
            app_id = %self.app_id,
            "OpMetrics summary: app_id={}, sync={}, async={}, fast={}, completed_async={}",
            self.app_id,
            op_summary.ops_dispatched_sync,
            op_summary.ops_dispatched_async,
            op_summary.ops_dispatched_fast,
            op_summary.ops_completed_async
        );

        Ok(())
    }

    /// Hand one batch of leased queue messages to the app's `queue` handler.
    ///
    /// Returns which message ids the handler did not complete; the poller
    /// acknowledges the rest. An error here means the batch never ran (or the
    /// isolate was terminated part-way), and the caller hands the whole batch
    /// back to deis-queue rather than silently dropping it.
    ///
    /// The same wall-clock timeout that bounds a request bounds a batch: a
    /// queue handler that never returns would otherwise pin the runtime
    /// thread its app lives on, and every request for every app on that
    /// thread would stop being served.
    pub async fn execute_queue_batch(
        &mut self,
        queue: String,
        messages: Vec<LeasedMessage>,
    ) -> Result<BatchOutcome> {
        self.clear_termination();

        if self.module_id.is_none() {
            if let Err(e) = self.init_module().await {
                self.clear_termination();
                return Err(e);
            }
        }

        let payload = JsQueueBatch {
            queue: queue.clone(),
            messages: messages
                .iter()
                .map(|message| JsQueueMessage {
                    id: message.id.clone(),
                    body: message.body.clone(),
                    attempts: message.attempts,
                    timestamp: message.created_at,
                })
                .collect(),
        };

        let code = format!(
            r#"
            (async () => {{
                const batch = {};
                const result = await __deis_runtime_invoke_queue(batch);
                __deis_runtime_set_response(JSON.stringify(result));
            }})()
            "#,
            serde_json::to_string(&payload)?
        );

        let promise = self.runtime.execute_script("<queue>", code)?;

        let timeout_ms = self.timeout_ms;
        let isolate_handle = self.runtime.v8_isolate().thread_safe_handle();
        let app_id_for_timeout = self.app_id.clone();
        let queue_for_timeout = queue.clone();
        let metrics_for_timeout = self.metrics.clone();
        let timeout_task = tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(timeout_ms)).await;
            metrics_for_timeout.record_timeout_cutoff(&app_id_for_timeout);
            tracing::warn!(
                target: "app",
                app_id = %app_id_for_timeout,
                queue = %queue_for_timeout,
                "App {} took longer than {}ms on a batch from queue {}. Terminating the isolate; \
                 the whole batch goes back to deis-queue and will be delivered again",
                app_id_for_timeout,
                timeout_ms,
                queue_for_timeout
            );
            isolate_handle.terminate_execution();
        });

        let resolve_future = self.runtime.resolve(promise);
        let result = self
            .runtime
            .with_event_loop_promise(resolve_future, PollEventLoopOptions::default())
            .await;
        timeout_task.abort();
        // Without this the batch that ran long would also be the last thing
        // this isolate ever did: every following batch would be handed back
        // and the app's messages would end up dead lettered.
        self.clear_termination();
        result?;

        let json_str = {
            let op_state = self.runtime.op_state();
            let state = op_state.borrow();
            let response_store = state.borrow::<ResponseStore>().clone();
            let response = response_store.borrow_mut().take();
            response.ok_or_else(|| {
                anyhow::anyhow!(
                    "No result received from the queue handler of app {}",
                    self.app_id
                )
            })?
        };

        let result: JsQueueResult = serde_json::from_str(&json_str)?;
        if let Some(error) = &result.error {
            tracing::warn!(
                target: "app",
                app_id = %self.app_id,
                queue = %queue,
                error = %error,
                "Queue handler of app {} rejected the whole batch from queue {}: {}",
                self.app_id,
                queue,
                error
            );
        }

        // Anything the handler awaited through waitUntil still has to finish
        // before the batch counts as settled; a message acknowledged while
        // its follow-up work is still pending would be lost if the node went
        // away right after.
        let wait_promise = self
            .runtime
            .execute_script("<queue_wait_until>", "__deis_runtime_wait_until_all()")?;
        let wait_resolve = self.runtime.resolve(wait_promise);
        self.runtime
            .with_event_loop_promise(wait_resolve, PollEventLoopOptions::default())
            .await?;

        Ok(BatchOutcome {
            failed: result.failed,
        })
    }

    /// Run the app's `scheduled` handler once, for one due cron expression.
    ///
    /// A tick is fire-and-forget by design: the control plane picks one node,
    /// calls this, and does not make up a missed run later. Anything that has
    /// to survive a failure belongs on a queue the handler fills, which is
    /// retried and dead-lettered.
    ///
    /// Bounded by the same wall-clock timeout as a request, and for the same
    /// reason: a handler that never returns would pin the runtime thread its
    /// app lives on and stop every request on that thread.
    pub async fn execute_scheduled(&mut self, cron: String, scheduled_time_ms: i64) -> Result<()> {
        self.clear_termination();

        if self.module_id.is_none() {
            if let Err(e) = self.init_module().await {
                self.clear_termination();
                return Err(e);
            }
        }

        let payload = JsScheduledEvent {
            cron: cron.clone(),
            scheduled_time: scheduled_time_ms,
        };

        let code = format!(
            r#"
            (async () => {{
                const event = {};
                const result = await __deis_runtime_invoke_scheduled(event);
                __deis_runtime_set_response(JSON.stringify(result));
            }})()
            "#,
            serde_json::to_string(&payload)?
        );

        let promise = self.runtime.execute_script("<scheduled>", code)?;

        let timeout_ms = self.timeout_ms;
        let isolate_handle = self.runtime.v8_isolate().thread_safe_handle();
        let app_id_for_timeout = self.app_id.clone();
        let cron_for_timeout = cron.clone();
        let metrics_for_timeout = self.metrics.clone();
        let timeout_task = tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(timeout_ms)).await;
            metrics_for_timeout.record_timeout_cutoff(&app_id_for_timeout);
            tracing::warn!(
                target: "app",
                app_id = %app_id_for_timeout,
                cron = %cron_for_timeout,
                "App {} took longer than {}ms on the scheduled run for {}. Terminating the                  isolate; this tick did not finish and is not retried",
                app_id_for_timeout,
                timeout_ms,
                cron_for_timeout
            );
            isolate_handle.terminate_execution();
        });

        let resolve_future = self.runtime.resolve(promise);
        let result = self
            .runtime
            .with_event_loop_promise(resolve_future, PollEventLoopOptions::default())
            .await;
        timeout_task.abort();
        self.clear_termination();
        result?;

        let json_str = {
            let op_state = self.runtime.op_state();
            let state = op_state.borrow();
            let response_store = state.borrow::<ResponseStore>().clone();
            let response = response_store.borrow_mut().take();
            response.ok_or_else(|| {
                anyhow::anyhow!(
                    "No result received from the scheduled handler of app {}",
                    self.app_id
                )
            })?
        };

        let outcome: JsScheduledResult = serde_json::from_str(&json_str)?;

        // Follow-up work the handler started must finish before the tick is
        // called done, or it would be cut off when the isolate goes idle.
        let wait_promise = self
            .runtime
            .execute_script("<scheduled_wait_until>", "__deis_runtime_wait_until_all()")?;
        let wait_resolve = self.runtime.resolve(wait_promise);
        self.runtime
            .with_event_loop_promise(wait_resolve, PollEventLoopOptions::default())
            .await?;

        if outcome.ok {
            return Ok(());
        }

        Err(anyhow::anyhow!(
            "{}",
            outcome
                .error
                .unwrap_or_else(|| "the scheduled handler did not report why it failed".to_string())
        ))
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

    /// Put this isolate in exactly the state a cut-off leaves it in.
    ///
    /// The timeout task and the rate limiter both end an execution by calling
    /// `terminate_execution` on this isolate's handle; this is the same call.
    /// Tests use it because neither of the real triggers is reproducible here:
    /// the timeout is a `tokio::spawn`ed timer that a handler blocking the
    /// thread never lets run, and the rate limiter would keep firing on the
    /// request that is supposed to prove recovery.
    #[cfg(test)]
    pub fn terminate_like_a_cut_off(&mut self) {
        self.runtime
            .v8_isolate()
            .thread_safe_handle()
            .terminate_execution();
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
