// Runtime thread pool - multi-core support

use super::bindings::{consumer_queue_client, ServiceWiring};
use super::isolate::{AppRuntime, IsolateSpec};
use super::queue_client::LeasedMessage;
use super::queue_consumer::{
    self, BatchInvoker, BatchOutcome, ConsumerHandle, ConsumerSettings, QueuePort,
};
use crate::app_repo::AppRepo;
use anyhow::Result;
use deno_core::JsRuntime;
use deno_core::OpMetricsSummary;
use std::collections::HashMap;
use std::sync::{Arc, Once};
use std::thread;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};

/// Ensure V8 platform is initialized only once
static V8_INIT: Once = Once::new();

type FetchResult = (u16, HashMap<String, String>, Option<String>);

/// Initialize V8 platform (can only be called once per process)
fn init_v8_platform() {
    V8_INIT.call_once(|| {
        JsRuntime::init_platform(None, false);
        tracing::debug!("V8 platform initialized");
    });
}

/// Runtime message type
pub enum RuntimeMessage {
    /// Load application
    LoadApp {
        app_id: String,
        code: String,
        app_repo: Option<Arc<AppRepo>>,
        spec: IsolateSpec,
        /// Which storage services this app declared and where they live.
        wiring: ServiceWiring,
        response: oneshot::Sender<Result<()>>,
    },
    /// Unload application
    UnloadApp {
        app_id: String,
        response: oneshot::Sender<Result<()>>,
    },
    /// Execute request
    ExecuteRequest {
        app_id: String,
        url: String,
        method: String,
        headers: HashMap<String, String>,
        body: Option<String>,
        response: oneshot::Sender<Result<FetchResult>>,
    },
    /// Hand a batch of queue messages to an app's queue handler
    ExecuteQueueBatch {
        app_id: String,
        queue: String,
        messages: Vec<LeasedMessage>,
        response: oneshot::Sender<Result<BatchOutcome>>,
    },
    /// Run an app's scheduled handler for one due cron expression
    ExecuteScheduled {
        app_id: String,
        cron: String,
        /// Unix milliseconds the tick was due.
        scheduled_time_ms: i64,
        response: oneshot::Sender<Result<()>>,
    },
    /// Get application list
    #[allow(dead_code)]
    ListApps {
        response: oneshot::Sender<Vec<String>>,
    },
    /// Register WebSocket connection
    RegisterWsConnection {
        app_id: String,
        ws_id: String,
        /// Channel for JavaScript to send messages to the client
        js_to_client_tx: mpsc::UnboundedSender<super::WsServerMessage>,
        response: oneshot::Sender<Result<()>>,
    },
    /// Send WebSocket message (from Gateway to JavaScript)
    SendWsMessage {
        app_id: String,
        ws_id: String,
        message: String,
        response: oneshot::Sender<Result<()>>,
    },
    /// Unregister WebSocket connection
    UnregisterWsConnection {
        app_id: String,
        ws_id: String,
        response: oneshot::Sender<Result<()>>,
    },
    /// Get OpMetrics summary
    GetOpMetrics {
        app_id: String,
        response: oneshot::Sender<Result<OpMetricsSummary>>,
    },
    /// Connect debugger
    ConnectInspector {
        app_id: String,
        response: oneshot::Sender<
            Result<(
                mpsc::UnboundedSender<String>,
                mpsc::UnboundedReceiver<String>,
            )>,
        >,
    },
    /// Close thread
    Shutdown,
}

/// Runtime thread
struct RuntimeThread {
    #[allow(dead_code)]
    thread_id: usize,
    sender: mpsc::Sender<RuntimeMessage>,
    handle: Option<thread::JoinHandle<()>>,
}

impl RuntimeThread {
    /// Create new runtime thread
    fn new(thread_id: usize) -> Self {
        // Set channel capacity to 100, exceeding will trigger backpressure (reject requests)
        let (tx, rx) = mpsc::channel(100);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        let handle = thread::spawn(move || {
            // Create current_thread tokio runtime
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime");

            rt.block_on(async move {
                Self::run_event_loop(thread_id, rx, ready_tx).await;
            });
        });

        // Wait for thread to be ready
        let _ = ready_rx.recv();

        Self {
            thread_id,
            sender: tx,
            handle: Some(handle),
        }
    }

    /// Run event loop
    async fn run_event_loop(
        thread_id: usize,
        mut rx: mpsc::Receiver<RuntimeMessage>,
        ready_tx: std::sync::mpsc::Sender<()>,
    ) {
        tracing::info!("Runtime thread {} started", thread_id);

        // Notify main thread that this thread has finished initialization
        let _ = ready_tx.send(());

        // All application runtimes managed by this thread
        let mut runtimes: HashMap<String, AppRuntime> = HashMap::new();

        while let Some(msg) = rx.recv().await {
            match msg {
                RuntimeMessage::LoadApp {
                    app_id,
                    code,
                    app_repo,
                    spec,
                    wiring,
                    response,
                } => {
                    tracing::debug!("Thread {}: Loading app {}", thread_id, app_id);
                    let result =
                        match AppRuntime::new(app_id.clone(), code, app_repo, spec, &wiring) {
                            Ok(mut runtime) => match runtime.init_module().await {
                                Ok(()) => {
                                    runtimes.insert(app_id.clone(), runtime);
                                    Ok(())
                                }
                                Err(e) => Err(e),
                            },
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

                RuntimeMessage::ExecuteQueueBatch {
                    app_id,
                    queue,
                    messages,
                    response,
                } => {
                    tracing::debug!(
                        "Thread {}: Handling {} queue message(s) for app {} from queue {}",
                        thread_id,
                        messages.len(),
                        app_id,
                        queue
                    );
                    let result = match runtimes.get_mut(&app_id) {
                        Some(runtime) => runtime.execute_queue_batch(queue, messages).await,
                        None => Err(anyhow::anyhow!(
                            "App {app_id} is not loaded on this node; its queue batch was not \
                             handled and goes back to deis-queue"
                        )),
                    };
                    let _ = response.send(result);
                }

                RuntimeMessage::ExecuteScheduled {
                    app_id,
                    cron,
                    scheduled_time_ms,
                    response,
                } => {
                    tracing::debug!(
                        "Thread {}: Running the scheduled handler of app {} for {}",
                        thread_id,
                        app_id,
                        cron
                    );
                    let result = match runtimes.get_mut(&app_id) {
                        Some(runtime) => runtime.execute_scheduled(cron, scheduled_time_ms).await,
                        None => Err(anyhow::anyhow!(
                            "App {app_id} is not loaded on this node, so its scheduled run did                              not happen here; the control plane should try another node"
                        )),
                    };
                    let _ = response.send(result);
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
                            // Register sender to WsServerManager, so JavaScript can send messages to the client
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

                RuntimeMessage::GetOpMetrics { app_id, response } => {
                    let result = match runtimes.get(&app_id) {
                        Some(runtime) => Ok(runtime.op_metrics_summary()),
                        None => Err(anyhow::anyhow!("App not found")),
                    };
                    let _ = response.send(result);
                }

                RuntimeMessage::ConnectInspector { app_id, response } => {
                    let result = match runtimes.get_mut(&app_id) {
                        Some(runtime) => runtime.connect_inspector(),
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

        // Explicitly cleanup all runtimes to ensure they are dropped in the correct context
        tracing::debug!(
            "Cleaning up {} runtimes in thread {}",
            runtimes.len(),
            thread_id
        );
        runtimes.clear();

        tracing::info!("Runtime thread {} stopped", thread_id);
    }

    /// Send message
    fn send(&self, msg: RuntimeMessage) -> Result<()> {
        self.sender.try_send(msg).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => {
                anyhow::anyhow!("Runtime thread overloaded (queue full)")
            }
            mpsc::error::TrySendError::Closed(_) => anyhow::anyhow!("Runtime thread has stopped"),
        })
    }
}

impl Drop for RuntimeThread {
    fn drop(&mut self) {
        let _ = self.sender.try_send(RuntimeMessage::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Runtime thread pool
pub struct RuntimePool {
    threads: Vec<RuntimeThread>,
    app_assignments: Arc<RwLock<HashMap<String, usize>>>, // app_id -> thread_id
    app_repo: Option<Arc<AppRepo>>,
    /// Queue pollers, per app. Held here because they must be stopped when
    /// the app is unloaded: a poller left running would keep leasing messages
    /// for an app that is no longer on this node, and every one of them would
    /// fail and eventually be dead lettered.
    consumers: Mutex<HashMap<String, Vec<ConsumerHandle>>>,
}

impl RuntimePool {
    /// Create new thread pool
    pub fn new(num_threads: Option<usize>) -> Self {
        Self::new_with_repo(num_threads, None)
    }

    pub fn new_with_repo(num_threads: Option<usize>, app_repo: Option<Arc<AppRepo>>) -> Self {
        // Ensure V8 platform is initialized (only once per process)
        init_v8_platform();

        let num_threads = num_threads.unwrap_or_else(num_cpus::get);
        tracing::info!("Creating runtime pool with {} threads", num_threads);

        let threads = (0..num_threads).map(RuntimeThread::new).collect();

        Self {
            threads,
            app_assignments: Arc::new(RwLock::new(HashMap::new())),
            app_repo,
            consumers: Mutex::new(HashMap::new()),
        }
    }

    /// Select a thread (load balancing)
    async fn select_thread_for_new_app(&self) -> usize {
        let assignments = self.app_assignments.read().await;

        // Count number of applications in each thread
        let mut counts = vec![0; self.threads.len()];
        for thread_id in assignments.values() {
            counts[*thread_id] += 1;
        }

        // Select thread with the minimum number of applications
        counts
            .iter()
            .enumerate()
            .min_by_key(|(_, count)| *count)
            .map(|(idx, _)| idx)
            .unwrap_or(0)
    }

    /// Get the thread ID where the application is located
    async fn get_thread_id(&self, app_id: &str) -> Option<usize> {
        self.app_assignments.read().await.get(app_id).copied()
    }

    /// Load application.
    ///
    /// The platform wiring -- which storage services this app declared and
    /// where they live -- travels in `spec.env` under the reserved keys the
    /// control plane sets (see [`ServiceWiring`]). It is lifted out here, so
    /// what reaches the isolate as `ctx.env` is only the application's own
    /// variables, and the app's queue consumers are started from the same
    /// information.
    pub async fn load_app(
        &self,
        app_id: String,
        code: String,
        mut spec: IsolateSpec,
    ) -> Result<()> {
        // Check if already exists
        if self.get_thread_id(&app_id).await.is_some() {
            return Err(anyhow::anyhow!("App already exists"));
        }

        let wiring = ServiceWiring::take_from_env(&mut spec.env);
        let execution_timeout = Duration::from_millis(spec.limits.timeout_ms);

        // Select thread
        let thread_id = self.select_thread_for_new_app().await;
        tracing::info!("Assigning app {} to thread {}", app_id, thread_id);

        // Send load message
        let (tx, rx) = oneshot::channel();
        self.threads[thread_id].send(RuntimeMessage::LoadApp {
            app_id: app_id.clone(),
            code,
            app_repo: self.app_repo.clone(),
            spec,
            wiring: wiring.clone(),
            response: tx,
        })?;

        // Wait for result
        let result = rx
            .await
            .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))?;

        // Record assignment if successful
        if result.is_ok() {
            self.app_assignments
                .write()
                .await
                .insert(app_id.clone(), thread_id);
            self.start_consumers(&app_id, thread_id, &wiring, execution_timeout)
                .await;
        }

        result
    }

    /// Start one poller per queue the app consumes.
    ///
    /// Only started for an app that is actually loaded: a poller for an app
    /// whose isolate failed to build would lease messages it could never
    /// handle, and every one of them would burn a delivery attempt.
    async fn start_consumers(
        &self,
        app_id: &str,
        thread_id: usize,
        wiring: &ServiceWiring,
        execution_timeout: Duration,
    ) {
        let specs = wiring.active_consumers(app_id);
        if specs.is_empty() {
            return;
        }

        let Some(endpoint) = wiring.endpoints.queue.as_deref() else {
            return;
        };

        let client: Arc<dyn QueuePort> = match consumer_queue_client(app_id, endpoint) {
            Ok(client) => Arc::new(client),
            Err(e) => {
                tracing::error!(
                    app_id,
                    endpoint,
                    error = %e,
                    "Cannot build a queue client for app {}; none of its queues will be \
                     consumed and their messages will pile up in deis-queue",
                    app_id
                );
                return;
            }
        };

        let mut handles = Vec::with_capacity(specs.len());
        for spec in specs {
            let invoker = self.batch_invoker(app_id, thread_id);
            handles.push(queue_consumer::spawn(
                app_id.to_string(),
                spec.name,
                client.clone(),
                ConsumerSettings::for_app(spec.batch_size, execution_timeout),
                invoker,
            ));
        }

        self.consumers
            .lock()
            .await
            .insert(app_id.to_string(), handles);
    }

    /// How a poller reaches the isolate: by posting the batch to the runtime
    /// thread the app lives on, exactly like a request.
    fn batch_invoker(&self, app_id: &str, thread_id: usize) -> BatchInvoker {
        let sender = self.threads[thread_id].sender.clone();
        let app_id = app_id.to_string();
        Arc::new(move |queue: String, messages: Vec<LeasedMessage>| {
            let sender = sender.clone();
            let app_id = app_id.clone();
            Box::pin(async move {
                let (tx, rx) = oneshot::channel();
                sender
                    .send(RuntimeMessage::ExecuteQueueBatch {
                        app_id: app_id.clone(),
                        queue,
                        messages,
                        response: tx,
                    })
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "the runtime thread running app {app_id} has stopped; the batch was \
                             not handled"
                        )
                    })?;
                rx.await.map_err(|_| {
                    anyhow::anyhow!(
                        "the runtime thread running app {app_id} dropped the batch without \
                         answering"
                    )
                })?
            })
        })
    }

    /// Stop and forget an app's queue pollers, letting the batch in flight
    /// finish and be acknowledged first.
    async fn stop_consumers(&self, app_id: &str) {
        let handles = self.consumers.lock().await.remove(app_id);
        for handle in handles.into_iter().flatten() {
            handle.stop().await;
        }
    }

    /// Unload application
    pub async fn unload_app(&self, app_id: &str) -> Result<()> {
        // Get thread ID
        let thread_id = self
            .get_thread_id(app_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("App not found"))?;

        // Stop polling before the isolate goes away, so no batch is leased
        // for an app that can no longer handle it.
        self.stop_consumers(app_id).await;

        // Send unload message
        let (tx, rx) = oneshot::channel();
        self.threads[thread_id].send(RuntimeMessage::UnloadApp {
            app_id: app_id.to_string(),
            response: tx,
        })?;

        // Wait for result
        let result = rx
            .await
            .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))?;

        // Remove assignment record if successful
        if result.is_ok() {
            self.app_assignments.write().await.remove(app_id);
        }

        result
    }

    /// Execute request
    pub async fn execute_request(
        &self,
        app_id: &str,
        url: String,
        method: String,
        headers: HashMap<String, String>,
        body: Option<String>,
    ) -> Result<(u16, HashMap<String, String>, Option<String>)> {
        // Get thread ID
        let thread_id = self
            .get_thread_id(app_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("App not found"))?;

        // Send execution message
        let (tx, rx) = oneshot::channel();
        self.threads[thread_id].send(RuntimeMessage::ExecuteRequest {
            app_id: app_id.to_string(),
            url,
            method,
            headers,
            body,
            response: tx,
        })?;

        // Wait for result
        rx.await
            .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))?
    }

    /// List all applications
    #[cfg(test)]
    pub async fn list_apps(&self) -> Vec<String> {
        let assignments = self.app_assignments.read().await;
        assignments.keys().cloned().collect()
    }

    /// Get thread count
    #[cfg(test)]
    pub fn thread_count(&self) -> usize {
        self.threads.len()
    }

    /// Get statistics
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

    /// Register WebSocket connection
    ///
    /// # Parameters
    /// - app_id: Application ID
    /// - ws_id: WebSocket connection ID
    /// - js_to_client_tx: Channel for JavaScript to send messages to the client
    ///
    /// # Returns
    /// - Ok(()) Success
    /// - Err(...) Application not found or registration failed
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

    /// Send WebSocket message to JavaScript
    ///
    /// # Parameters
    /// - app_id: Application ID
    /// - ws_id: WebSocket connection ID
    /// - message: Message content
    ///
    /// # Returns
    /// - Ok(()) Success
    /// - Err(...) Application not found or send failed
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

    /// Unregister WebSocket connection
    ///
    /// # Parameters
    /// - app_id: Application ID
    /// - ws_id: WebSocket connection ID
    ///
    /// # Returns
    /// - Ok(()) Success
    /// - Err(...) Application not found or unregistration failed
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

    /// Hand a batch of queue messages to an app's queue handler.
    ///
    /// The pollers use this through [`RuntimePool::batch_invoker`]; it is
    /// public so a control-plane trigger (`Trigger/OnQueue`) can drive the
    /// same path without a poller.
    pub async fn execute_queue_batch(
        &self,
        app_id: &str,
        queue: String,
        messages: Vec<LeasedMessage>,
    ) -> Result<BatchOutcome> {
        let thread_id = self
            .get_thread_id(app_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("App not found"))?;

        let (tx, rx) = oneshot::channel();
        self.threads[thread_id].send(RuntimeMessage::ExecuteQueueBatch {
            app_id: app_id.to_string(),
            queue,
            messages,
            response: tx,
        })?;

        rx.await
            .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))?
    }

    /// Run an app's scheduled handler once, for one due cron expression.
    ///
    /// Called by `Trigger/OnScheduled`: the control plane owns the schedule
    /// and picks exactly one healthy node per tick, so this node's only job
    /// is to run the handler and say honestly whether it ran.
    pub async fn execute_scheduled(
        &self,
        app_id: &str,
        cron: String,
        scheduled_time_ms: i64,
    ) -> Result<()> {
        let thread_id = self
            .get_thread_id(app_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("App not found"))?;

        let (tx, rx) = oneshot::channel();
        self.threads[thread_id].send(RuntimeMessage::ExecuteScheduled {
            app_id: app_id.to_string(),
            cron,
            scheduled_time_ms,
            response: tx,
        })?;

        rx.await
            .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))?
    }

    /// Get aggregate application OpMetrics
    pub async fn get_op_metrics(&self, app_id: &str) -> Result<OpMetricsSummary> {
        let thread_id = self
            .get_thread_id(app_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("App not found"))?;

        let (tx, rx) = oneshot::channel();
        self.threads[thread_id].send(RuntimeMessage::GetOpMetrics {
            app_id: app_id.to_string(),
            response: tx,
        })?;

        rx.await
            .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))?
    }

    /// Connect debugger
    pub async fn connect_inspector(
        &self,
        app_id: &str,
    ) -> Result<(
        mpsc::UnboundedSender<String>,
        mpsc::UnboundedReceiver<String>,
    )> {
        let thread_id = self
            .get_thread_id(app_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("App not found"))?;

        let (tx, rx) = oneshot::channel();
        self.threads[thread_id].send(RuntimeMessage::ConnectInspector {
            app_id: app_id.to_string(),
            response: tx,
        })?;

        rx.await
            .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))?
    }
}

#[cfg(test)]
// Holding the std MutexGuard across await points is intentional here: the global
// TEST_POOL mutex exists precisely to serialize these tests against a shared
// singleton RuntimePool (V8 cannot be created/destroyed repeatedly). Each test is
// an independent tokio runtime, so no deadlock is possible, and dropping the guard
// early would defeat the purpose.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use std::sync::Mutex;

    // Use global RuntimePool to avoid V8 issues from multiple creation/destruction
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
            export default function handler(context) {
                return new Response("Hello from pool!");
            }
        "#;

        // Load application
        let result = pool
            .load_app(
                "test_load_exec_app".to_string(),
                code.to_string(),
                IsolateSpec::default(),
            )
            .await;
        assert!(result.is_ok());

        // Execute request
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

        // Cleanup
        let _ = pool.unload_app("test_load_exec_app").await;
    }

    #[tokio::test]
    async fn test_load_balancing() {
        let pool = TEST_POOL.lock().unwrap();

        // Load multiple applications
        for i in 0..4 {
            let code = r#"export default function handler(context) { return new Response("ok"); }"#;
            pool.load_app(
                format!("lb_app{}", i),
                code.to_string(),
                IsolateSpec::default(),
            )
            .await
            .unwrap();
        }

        // Check application assignments
        let stats = pool.get_stats().await;
        assert!(stats.get("total_apps").unwrap() >= &4);
        assert_eq!(stats.get("total_threads"), Some(&4));

        // Cleanup
        for i in 0..4 {
            let _ = pool.unload_app(&format!("lb_app{}", i)).await;
        }
    }

    #[tokio::test]
    async fn the_platform_wiring_never_reaches_the_app_s_env() {
        // The control plane delivers the store address in the same env map as
        // the app's own variables. The app must see its variables and nothing
        // else: platform wiring is not application configuration, and an app
        // variable spelled like one of the reserved keys must not be able to
        // point a storage client somewhere else.
        let pool = TEST_POOL.lock().unwrap();

        let code = r#"
            export default async function handler(ctx) {
                let kv = "no call";
                try {
                    await ctx.services.kv.get("k");
                } catch (e) {
                    kv = String(e.message || e);
                }
                return new Response(JSON.stringify({ env: ctx.env, kv }));
            }
        "#;

        let env: HashMap<String, String> = [
            ("DEIS_SERVICES", "kv"),
            // Nothing listens on port 1: enough to prove a client was built.
            ("DEIS_STORE_ENDPOINT", "http://127.0.0.1:1"),
            ("LOG_LEVEL", "info"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        pool.load_app(
            "env_wiring_app".to_string(),
            code.to_string(),
            IsolateSpec {
                env,
                ..IsolateSpec::default()
            },
        )
        .await
        .expect("the app loads");

        let (status, _, body) = pool
            .execute_request(
                "env_wiring_app",
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
            )
            .await
            .expect("the request runs");
        let _ = pool.unload_app("env_wiring_app").await;

        assert_eq!(status, 200);
        let body = body.unwrap_or_default();
        assert!(body.contains("LOG_LEVEL"), "{body}");
        assert!(
            !body.contains("DEIS_STORE_ENDPOINT") && !body.contains("DEIS_SERVICES"),
            "platform wiring leaked into ctx.env: {body}"
        );
        assert!(
            !body.contains("not available to this app"),
            "the declared kv service must be bound, not missing: {body}"
        );
    }

    #[tokio::test]
    async fn an_app_declaring_queues_without_a_queue_address_still_loads() {
        // Refusing to load would take the whole app down over a consumer that
        // cannot run; the app serves requests and the missing address is a
        // warning, not an outage.
        let pool = TEST_POOL.lock().unwrap();

        let code = r#"export default function handler() { return new Response("ok"); }"#;
        let env: HashMap<String, String> = [
            ("DEIS_SERVICES", "queue"),
            ("DEIS_QUEUE_CONSUMERS", "jobs:5"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        pool.load_app(
            "queueless_app".to_string(),
            code.to_string(),
            IsolateSpec {
                env,
                ..IsolateSpec::default()
            },
        )
        .await
        .expect("the app loads without a queue address");

        let (status, _, _) = pool
            .execute_request(
                "queueless_app",
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
            )
            .await
            .expect("the request runs");
        assert_eq!(status, 200);

        // Unloading an app with no running poller is still clean.
        pool.unload_app("queueless_app").await.expect("unload");
    }

    #[tokio::test]
    async fn test_unload_app() {
        let pool = TEST_POOL.lock().unwrap();

        let code = r#"export default function handler(context) { return new Response("ok"); }"#;
        pool.load_app(
            "unload_test_app".to_string(),
            code.to_string(),
            IsolateSpec::default(),
        )
        .await
        .unwrap();

        // Unload application
        let result = pool.unload_app("unload_test_app").await;
        assert!(result.is_ok());

        // Verify application deleted
        let apps = pool.list_apps().await;
        assert!(!apps.contains(&"unload_test_app".to_string()));
    }
}
