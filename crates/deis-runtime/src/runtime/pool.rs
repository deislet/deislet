// Runtime thread pool - multi-core support

use super::bindings::{consumer_queue_client, ServiceWiring};
use super::isolate::{AppRuntime, IsolateSpec};
use super::queue_client::LeasedMessage;
use super::queue_consumer::{
    self, BatchInvoker, BatchOutcome, ConsumerHandle, ConsumerSettings, QueuePort,
};
use super::response_headers::HeaderPairs;
use super::response_stream::ResponseBody;
use crate::app_repo::AppRepo;
use anyhow::Result;
use deno_core::JsRuntime;

use super::OpMetricsSummary;
use std::collections::HashMap;
use std::sync::{Arc, Once};
use std::thread;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};

/// Ensure V8 platform is initialized only once
static V8_INIT: Once = Once::new();

/// Status, headers and body of one answered request; the same triple
/// `super::isolate` produces. The body may be bytes or a body still being
/// produced -- see [`ResponseBody`] and its definition there. The headers are
/// a list rather than a map because one name may appear twice; by the time one
/// of these exists the isolate has already decided, from the application's
/// compatibility date, whether the repeats survive.
type FetchResult = (u16, HeaderPairs, ResponseBody);

/// Initialize V8 platform (can only be called once per process)
fn init_v8_platform() {
    V8_INIT.call_once(|| {
        JsRuntime::init_platform(None);
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
        /// Boxed because this is by far the largest thing any of these
        /// variants carries, and every message sent down a thread's channel is
        /// as big as the biggest variant. One indirection on the load path,
        /// which happens once per deployment, keeps every request message
        /// small.
        spec: Box<IsolateSpec>,
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
        body: Option<Vec<u8>>,
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

/// The one application a runtime thread hosts.
///
/// One isolate per OS thread is a V8 requirement, not a scheduling
/// preference. `v8::OwnedIsolate` enters its isolate when it is created and
/// exits when it is dropped, so a second isolate built on the same thread
/// shadows the first: every later use of the first one panics with
/// "...and Context do not belong to the same Isolate", which kills the thread
/// and with it every application that thread was hosting, and dropping the two
/// out of creation order aborts the process outright.
///
/// A map here would let a future edit put two isolates on one thread again
/// and get that silently. An `Option` cannot.
struct HostedApp {
    app_id: String,
    runtime: AppRuntime,
}

/// The hosted runtime, if this thread is hosting the application asked for.
fn hosted_mut<'a>(slot: &'a mut Option<HostedApp>, app_id: &str) -> Option<&'a mut AppRuntime> {
    slot.as_mut()
        .filter(|hosted| hosted.app_id == app_id)
        .map(|hosted| &mut hosted.runtime)
}

/// Runtime thread.
///
/// Hosts at most one application at a time; see [`HostedApp`]. A thread whose
/// application has been unloaded stays alive and is handed to the next
/// application that loads, so a deployment does not pay for starting one.
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

        // The one application runtime this thread hosts, if any.
        let mut hosted: Option<HostedApp> = None;

        loop {
            // An isolate with a response body still being produced has work of
            // its own, and it has to get a turn: nothing else in this process
            // drives that producer. It gets one *between* messages rather than
            // instead of them -- an arriving request drops the pump wherever it
            // is and is served at once, and the pump resumes afterwards -- so a
            // client reading one stream slowly cannot delay another request to
            // the same application by more than the time one chunk takes to
            // produce.
            let msg = match hosted
                .as_mut()
                .filter(|hosted| hosted.runtime.has_open_response_streams())
            {
                Some(hosted) => {
                    tokio::select! {
                        msg = rx.recv() => msg,
                        () = hosted.runtime.pump_response_streams() => continue,
                    }
                }
                None => rx.recv().await,
            };
            let Some(msg) = msg else {
                break;
            };
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
                    // Drop whatever this thread hosted before building the new
                    // isolate. Two live isolates on one thread break each other
                    // (see `HostedApp`), and the previous one must be dropped
                    // here, on the thread that created it.
                    if let Some(previous) = hosted.take() {
                        tracing::warn!(
                            thread_id,
                            previous = %previous.app_id,
                            app_id = %app_id,
                            "Runtime thread {} was still hosting {} when {} was assigned to it; \
                             the previous isolate is being dropped, so that application stops \
                             serving on this node until it is loaded again",
                            thread_id,
                            previous.app_id,
                            app_id
                        );
                    }
                    let result =
                        match AppRuntime::new(app_id.clone(), code, app_repo, *spec, &wiring) {
                            Ok(mut runtime) => match runtime.init_module().await {
                                Ok(()) => {
                                    hosted = Some(HostedApp {
                                        app_id: app_id.clone(),
                                        runtime,
                                    });
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
                    let result = if hosted.as_ref().is_some_and(|h| h.app_id == app_id) {
                        // Dropped here rather than handed back to the caller:
                        // an isolate must be dropped on the thread that
                        // created it.
                        hosted = None;
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
                    match hosted_mut(&mut hosted, &app_id) {
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
                    let result = match hosted_mut(&mut hosted, &app_id) {
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
                    let result = match hosted_mut(&mut hosted, &app_id) {
                        Some(runtime) => runtime.execute_scheduled(cron, scheduled_time_ms).await,
                        None => Err(anyhow::anyhow!(
                            "App {app_id} is not loaded on this node, so its scheduled run did                              not happen here; the control plane should try another node"
                        )),
                    };
                    let _ = response.send(result);
                }

                RuntimeMessage::ListApps { response } => {
                    let apps: Vec<String> = hosted.iter().map(|h| h.app_id.clone()).collect();
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

                    let result = match hosted_mut(&mut hosted, &app_id) {
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

                    let result = match hosted_mut(&mut hosted, &app_id) {
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

                    let result = match hosted_mut(&mut hosted, &app_id) {
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
                    let result = match hosted_mut(&mut hosted, &app_id) {
                        Some(runtime) => Ok(runtime.op_metrics_summary()),
                        None => Err(anyhow::anyhow!("App not found")),
                    };
                    let _ = response.send(result);
                }

                RuntimeMessage::ConnectInspector { app_id, response } => {
                    let result = match hosted_mut(&mut hosted, &app_id) {
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

        // Dropped here, on the thread that created it: an isolate cannot be
        // dropped anywhere else.
        if let Some(hosted) = hosted.take() {
            tracing::debug!(
                "Dropping the runtime of {} on thread {}",
                hosted.app_id,
                thread_id
            );
        }

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

/// Runtime thread pool.
///
/// # One application per thread
///
/// A thread hosts at most one application, because a thread hosts at most one
/// V8 isolate; see [`HostedApp`] for why that is not negotiable. `num_threads`
/// is therefore how many threads are started up front rather than a ceiling:
/// loading an application takes an idle thread if there is one and starts
/// another if there is not, and unloading leaves the thread idle for the next
/// load rather than stopping it.
///
/// # What an application is keyed by here
///
/// By the pair (application, environment), spelled the way the storage
/// services read it: [`crate::app::AppKey::tenant_id`], e.g.
/// `demo.production`. Every `app_id: String` below is that string. Keying by
/// the application alone would put a preview and production in one slot, so
/// the second deployment to arrive would replace the first -- its code, its
/// secrets and its storage tenant -- with nothing in the logs to say so.
///
/// Log lines and metrics keep the field name `app_id` because that is the one
/// tenant dimension deis-telemetry stores and every query filters on; the
/// value in it is the pair.
pub struct RuntimePool {
    placement: RwLock<Placement>,
    app_repo: Option<Arc<AppRepo>>,
    /// Queue pollers, per app. Held here because they must be stopped when
    /// the app is unloaded: a poller left running would keep leasing messages
    /// for an app that is no longer on this node, and every one of them would
    /// fail and eventually be dead lettered.
    consumers: Mutex<HashMap<String, Vec<ConsumerHandle>>>,
}

/// Which thread each loaded application sits on, and the threads themselves.
///
/// Kept behind one lock rather than two so that claiming a free thread for a
/// new application is a single atomic step: with the two apart, two
/// deployments arriving together could both read "thread 3 is free" and both
/// be sent there, and the second isolate would break the first.
///
/// A thread is spoken for if it appears in *any* of the three collections
/// below, and [`Self::idle_thread`] is the one place that decides. Each names
/// a different moment in a thread's life, and a thread in any of them holds a
/// live isolate:
///
/// * `of_app` - serving. Requests are dispatched here.
/// * `staging` - building a replacement that is not serving yet.
/// * `retiring` - swapped out, its isolate not dropped yet.
struct Placement {
    threads: Vec<RuntimeThread>,
    /// `<app>.<environment>` tenant -> index into `threads`.
    of_app: HashMap<String, usize>,
    /// Tenant -> the thread building a replacement isolate for it. The entry
    /// exists from the moment the thread is claimed, so a replacement being
    /// built cannot be handed the thread the application is still serving on,
    /// and two replacements cannot be built at once.
    staging: HashMap<String, usize>,
    /// Threads whose isolate has been swapped out and is being dropped. They
    /// are not idle yet: handing one out while its old isolate is alive would
    /// put two isolates on it.
    retiring: std::collections::HashSet<usize>,
}

impl Placement {
    /// A thread with no application on it, if there is one.
    fn idle_thread(&self) -> Option<usize> {
        let mut taken: std::collections::HashSet<usize> = self.of_app.values().copied().collect();
        taken.extend(self.staging.values().copied());
        taken.extend(self.retiring.iter().copied());
        (0..self.threads.len()).find(|id| !taken.contains(id))
    }
}

/// A replacement isolate that is built and warm but is not serving yet.
///
/// It holds a runtime thread of its own from the moment it is staged, which is
/// what lets the version it replaces keep answering requests the whole time it
/// is being built. Hand it to [`RuntimePool::activate`] to make it the one
/// that serves, or to [`RuntimePool::discard`] to throw it away; dropping the
/// value itself does neither, and would strand the thread.
#[must_use = "a staged version holds a runtime thread until it is activated or discarded"]
pub struct StagedVersion {
    tenant: String,
    thread_id: usize,
    sender: mpsc::Sender<RuntimeMessage>,
    /// Which storage services this version declared and where they live; the
    /// queue pollers are started from it at activation, not before, so a
    /// version that never activates never leases a message.
    wiring: ServiceWiring,
    execution_timeout: Duration,
}

impl StagedVersion {
    /// The `<app>.<environment>` this replacement is for.
    pub fn tenant(&self) -> &str {
        &self.tenant
    }
}

impl std::fmt::Debug for StagedVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedVersion")
            .field("tenant", &self.tenant)
            .field("thread_id", &self.thread_id)
            .finish()
    }
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
        tracing::info!(
            "Creating runtime pool with {} threads started up front; one application runs per \
             thread, so more threads are started if more applications are loaded than that",
            num_threads
        );

        let threads = (0..num_threads).map(RuntimeThread::new).collect();

        Self {
            placement: RwLock::new(Placement {
                threads,
                of_app: HashMap::new(),
                staging: HashMap::new(),
                retiring: std::collections::HashSet::new(),
            }),
            app_repo,
            consumers: Mutex::new(HashMap::new()),
        }
    }

    /// Reserve a thread to build an isolate on, starting one if every existing
    /// thread is already spoken for.
    ///
    /// The reservation goes into `staging` before the isolate is built, so a
    /// second load arriving at the same moment cannot be sent to the same
    /// thread, and - the part that matters for a version change - the thread
    /// the application is still *serving* on is never the one the replacement
    /// is built on. [`Self::release_staging`] undoes it if the build fails.
    ///
    /// `replacing` says whether the tenant may already be serving here. False
    /// is the plain load: an application that is already placed is a caller
    /// mistake and is refused rather than silently swapped.
    ///
    /// Returns the thread's index (for logs) and a handle to post to it.
    async fn claim_thread(
        &self,
        tenant: &str,
        replacing: bool,
    ) -> Result<(usize, mpsc::Sender<RuntimeMessage>)> {
        let mut placement = self.placement.write().await;
        if !replacing && placement.of_app.contains_key(tenant) {
            return Err(anyhow::anyhow!("App already exists"));
        }
        if placement.staging.contains_key(tenant) {
            return Err(anyhow::anyhow!(
                "a replacement isolate for {tenant} is already being built; the version change \
                 that started it has to finish or be discarded first"
            ));
        }

        let id = match placement.idle_thread() {
            Some(id) => id,
            None => {
                // Starting a thread waits for it to report ready, which holds
                // this lock for that moment. It happens once per application
                // load, never on a request, and the alternative - deciding on
                // a thread outside the lock - is what lets two loads pick the
                // same one.
                let id = placement.threads.len();
                placement.threads.push(RuntimeThread::new(id));
                tracing::info!(
                    thread_id = id,
                    app_id = tenant,
                    "Started runtime thread {} for {}: every existing thread already hosts an \
                     application, and two isolates cannot share one thread",
                    id,
                    tenant
                );
                id
            }
        };

        placement.staging.insert(tenant.to_string(), id);
        Ok((id, placement.threads[id].sender.clone()))
    }

    /// Give up a staging reservation whose isolate was never built.
    async fn release_staging(&self, tenant: &str) {
        self.placement.write().await.staging.remove(tenant);
    }

    /// Give up an application's thread, leaving it idle for the next load.
    async fn release(&self, tenant: &str) {
        self.placement.write().await.of_app.remove(tenant);
    }

    /// Drop the isolate a retiring thread still holds and give the thread back.
    ///
    /// The thread stays out of circulation until this returns, so nothing can
    /// build a second isolate on it while the first is alive. If the unload
    /// cannot be delivered the thread is released anyway with the reason
    /// logged: a load that lands on it later drops the stale isolate itself,
    /// whereas keeping the thread reserved forever would leak one per failure.
    async fn retire_thread(
        &self,
        tenant: &str,
        thread_id: usize,
        sender: mpsc::Sender<RuntimeMessage>,
    ) {
        let (tx, rx) = oneshot::channel();
        let outcome = match sender
            .send(RuntimeMessage::UnloadApp {
                app_id: tenant.to_string(),
                response: tx,
            })
            .await
        {
            Ok(()) => rx
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("the runtime thread dropped the request"))),
            Err(_) => Err(anyhow::anyhow!("the runtime thread has stopped")),
        };

        if let Err(e) = outcome {
            tracing::warn!(
                thread_id,
                app_id = tenant,
                error = %e,
                "The previous isolate of {} could not be retired from thread {}; it is left in \
                 place and the thread is put back in circulation, so the next application \
                 loaded there drops it. Nothing is serving from it in the meantime",
                tenant,
                thread_id
            );
        }

        self.placement.write().await.retiring.remove(&thread_id);
    }

    /// Post a message to the thread hosting `app_id`.
    async fn dispatch(&self, app_id: &str, message: RuntimeMessage) -> Result<()> {
        let placement = self.placement.read().await;
        let id = *placement
            .of_app
            .get(app_id)
            .ok_or_else(|| anyhow::anyhow!("App not found"))?;
        placement.threads[id].send(message)
    }

    /// Load one environment of one application.
    ///
    /// `tenant` identifies it: [`crate::app::AppKey::tenant_id`], e.g.
    /// `demo.production`. It is the key in this pool, the tenant every
    /// storage call of this isolate is charged to, and the dimension its logs
    /// and metrics carry -- so a preview's data, requests and errors never
    /// land in production's.
    ///
    /// The platform wiring -- which storage services this app declared and
    /// where they live -- travels in `spec.env` under the reserved keys the
    /// control plane sets (see [`ServiceWiring`]). It is lifted out here, so
    /// what reaches the isolate as `ctx.env` is only the application's own
    /// variables, and the app's queue consumers are started from the same
    /// information.
    pub async fn load_app(&self, tenant: String, code: String, spec: IsolateSpec) -> Result<()> {
        // `replacing: false` - an application that is already placed here is a
        // caller mistake, not a version change. Changing versions goes through
        // [`Self::stage_app`], which keeps the old one answering.
        let staged = self.build(tenant, code, spec, false).await?;
        self.activate(staged).await
    }

    /// Build a replacement isolate for an application that is already serving,
    /// without touching the one that is.
    ///
    /// This is the first half of a version change. The new isolate is built on
    /// a thread of its own and is not reachable: [`Self::dispatch`] still sends
    /// every request to the version that is serving, and no queue poller is
    /// started for the new one. Building can take as long as it takes -- a
    /// module that has to be transpiled and evaluated is the slow part of a
    /// deployment -- and the application answers normally throughout.
    ///
    /// Hand the result to [`Self::activate`] to swap, or to [`Self::discard`]
    /// to throw it away. Until one of those happens the application holds two
    /// runtime threads and two isolates, which is the memory cost of not
    /// having a gap.
    ///
    /// A build that fails leaves the running version untouched and returns the
    /// error: the caller has taken nothing away, so there is nothing to put
    /// back.
    pub async fn stage_app(
        &self,
        tenant: String,
        code: String,
        spec: IsolateSpec,
    ) -> Result<StagedVersion> {
        self.build(tenant, code, spec, true).await
    }

    /// Build an isolate on a thread of its own and hand back the claim on it.
    async fn build(
        &self,
        tenant: String,
        code: String,
        mut spec: IsolateSpec,
        replacing: bool,
    ) -> Result<StagedVersion> {
        let wiring = ServiceWiring::take_from_env(&mut spec.env);
        let execution_timeout = Duration::from_millis(spec.limits.timeout_ms);

        // Reserved before the isolate is built, so no second load can be sent
        // to the same thread while this one is starting.
        let (thread_id, sender) = self.claim_thread(&tenant, replacing).await?;
        tracing::info!(
            thread_id,
            app_id = %tenant,
            replacing,
            "Building the isolate of {} on thread {}",
            tenant,
            thread_id
        );

        let (tx, rx) = oneshot::channel();
        let sent = sender.try_send(RuntimeMessage::LoadApp {
            app_id: tenant.clone(),
            code,
            app_repo: self.app_repo.clone(),
            spec: Box::new(spec),
            wiring: wiring.clone(),
            response: tx,
        });

        let result = match sent {
            Ok(()) => rx
                .await
                .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))
                .and_then(|inner| inner),
            Err(e) => Err(match e {
                mpsc::error::TrySendError::Full(_) => {
                    anyhow::anyhow!("Runtime thread overloaded (queue full)")
                }
                mpsc::error::TrySendError::Closed(_) => {
                    anyhow::anyhow!("Runtime thread has stopped")
                }
            }),
        };

        match result {
            Ok(()) => Ok(StagedVersion {
                tenant,
                thread_id,
                sender,
                wiring,
                execution_timeout,
            }),
            Err(e) => {
                // The thread is free again; leaving it reserved would strand
                // it, and the next application would start a thread for
                // nothing.
                self.release_staging(&tenant).await;
                Err(e)
            }
        }
    }

    /// Make a staged isolate the one that serves.
    ///
    /// The swap is one write under the placement lock, so a request either
    /// reaches the version that was serving or the one that now is; there is
    /// no moment in between where the application is not placed. A request
    /// already handed to the old thread finishes there -- a runtime thread
    /// runs one message to completion before the next -- so the retirement
    /// posted afterwards cannot cut it short.
    ///
    /// The old version's queue pollers are stopped, and the new one's started,
    /// around the swap: the stop waits for a batch in flight to finish and be
    /// acknowledged, so no message is leased for an isolate that is about to
    /// go away.
    ///
    /// It then waits for the old isolate to be dropped before returning, which
    /// means it waits for whatever that isolate was in the middle of - up to
    /// one execution timeout. Traffic is not waiting: the new version has been
    /// answering since the swap. What waits is the deployment call, and the
    /// alternative is handing the thread out while a live isolate is still on
    /// it, which is the one thing this pool must never do.
    ///
    /// Fails only if the staged version is no longer this application's: the
    /// application was unloaded while it was being built, which already
    /// discarded it.
    pub async fn activate(&self, staged: StagedVersion) -> Result<()> {
        let StagedVersion {
            tenant,
            thread_id,
            sender,
            wiring,
            execution_timeout,
        } = staged;

        let previous = {
            let mut placement = self.placement.write().await;
            match placement.staging.get(&tenant) {
                Some(id) if *id == thread_id => {
                    placement.staging.remove(&tenant);
                }
                _ => {
                    return Err(anyhow::anyhow!(
                        "the staged isolate of {tenant} on thread {thread_id} is no longer this \
                         application's; it was unloaded while the new version was being built, \
                         and the new version was discarded with it"
                    ))
                }
            }
            let previous = placement.of_app.insert(tenant.clone(), thread_id);
            if let Some(previous_id) = previous {
                // Out of circulation until its isolate is gone.
                placement.retiring.insert(previous_id);
            }
            previous.map(|id| (id, placement.threads[id].sender.clone()))
        };

        // Stop before the old isolate goes away, so a batch already leased is
        // handed to the isolate that can still handle it.
        self.stop_consumers(&tenant).await;
        self.start_consumers(&tenant, sender, &wiring, execution_timeout)
            .await;

        match previous {
            Some((previous_id, previous_sender)) => {
                tracing::info!(
                    app_id = %tenant,
                    from_thread = previous_id,
                    to_thread = thread_id,
                    "{} now serves from the isolate on thread {}; the one on thread {} answered \
                     every request until this moment and is being retired",
                    tenant,
                    thread_id,
                    previous_id
                );
                self.retire_thread(&tenant, previous_id, previous_sender)
                    .await;
            }
            None => tracing::info!(
                app_id = %tenant,
                thread_id,
                "Assigning app {} to thread {}",
                tenant,
                thread_id
            ),
        }

        Ok(())
    }

    /// Throw away a staged isolate that will not be activated.
    ///
    /// The version that is serving is not touched -- it never stopped -- and
    /// the thread the replacement was built on goes back into circulation.
    pub async fn discard(&self, staged: StagedVersion) {
        let StagedVersion {
            tenant,
            thread_id,
            sender,
            ..
        } = staged;

        {
            let mut placement = self.placement.write().await;
            if placement.staging.get(&tenant) == Some(&thread_id) {
                placement.staging.remove(&tenant);
            }
            placement.retiring.insert(thread_id);
        }

        tracing::info!(
            app_id = %tenant,
            thread_id,
            "Discarding the staged isolate of {} on thread {}; whatever was serving keeps \
             serving",
            tenant,
            thread_id
        );
        self.retire_thread(&tenant, thread_id, sender).await;
    }

    /// Throw away a replacement that is still being built for `tenant`, if
    /// there is one.
    ///
    /// Called when the application is unloaded: the staged isolate is for an
    /// application that will not be there to receive it, and leaving it would
    /// hold a runtime thread for the life of the process.
    async fn discard_staged_for(&self, tenant: &str) {
        let staged = {
            let mut placement = self.placement.write().await;
            placement.staging.remove(tenant).map(|id| {
                placement.retiring.insert(id);
                (id, placement.threads[id].sender.clone())
            })
        };

        if let Some((thread_id, sender)) = staged {
            tracing::info!(
                app_id = tenant,
                thread_id,
                "{} is being unloaded while a replacement version was still being built on \
                 thread {}; that replacement is thrown away",
                tenant,
                thread_id
            );
            self.retire_thread(tenant, thread_id, sender).await;
        }
    }

    /// Start one poller per queue the app consumes.
    ///
    /// Only started for an app that is actually loaded: a poller for an app
    /// whose isolate failed to build would lease messages it could never
    /// handle, and every one of them would burn a delivery attempt.
    async fn start_consumers(
        &self,
        tenant: &str,
        sender: mpsc::Sender<RuntimeMessage>,
        wiring: &ServiceWiring,
        execution_timeout: Duration,
    ) {
        let specs = wiring.active_consumers(tenant);
        if specs.is_empty() {
            return;
        }

        let Some(endpoint) = wiring.endpoints.queue.as_deref() else {
            return;
        };

        // Charged to the same tenant as the app's own queue client, and
        // carrying the same capability token, so a preview leases only its own
        // messages and a deis-queue that checks the token answers the poller
        // instead of refusing it.
        let client: Arc<dyn QueuePort> =
            match consumer_queue_client(tenant, endpoint, wiring.storage_token.as_deref()) {
                Ok(client) => Arc::new(client),
                Err(e) => {
                    tracing::error!(
                        app_id = tenant,
                        endpoint,
                        error = %e,
                        "Cannot build a queue client for {}; none of its queues will be \
                         consumed and their messages will pile up in deis-queue",
                        tenant
                    );
                    return;
                }
            };

        let mut handles = Vec::with_capacity(specs.len());
        for spec in specs {
            let invoker = Self::batch_invoker(tenant, sender.clone());
            handles.push(queue_consumer::spawn(
                tenant.to_string(),
                spec.name,
                client.clone(),
                ConsumerSettings::for_app(spec.batch_size, execution_timeout),
                invoker,
            ));
        }

        self.consumers
            .lock()
            .await
            .insert(tenant.to_string(), handles);
    }

    /// How a poller reaches the isolate: by posting the batch to the runtime
    /// thread the app lives on, exactly like a request.
    fn batch_invoker(app_id: &str, sender: mpsc::Sender<RuntimeMessage>) -> BatchInvoker {
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
        // Stop polling before the isolate goes away, so no batch is leased
        // for an app that can no longer handle it.
        self.stop_consumers(app_id).await;

        // A replacement that was still being built is for an application that
        // is going away; it would otherwise hold a runtime thread until the
        // process ends.
        self.discard_staged_for(app_id).await;

        let (tx, rx) = oneshot::channel();
        self.dispatch(
            app_id,
            RuntimeMessage::UnloadApp {
                app_id: app_id.to_string(),
                response: tx,
            },
        )
        .await?;

        let result = rx
            .await
            .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))?;

        // The thread is left running and idle: it costs one stack, and the
        // next application to load takes it instead of paying to start one.
        if result.is_ok() {
            self.release(app_id).await;
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
        body: Option<Vec<u8>>,
    ) -> Result<FetchResult> {
        let (tx, rx) = oneshot::channel();
        self.dispatch(
            app_id,
            RuntimeMessage::ExecuteRequest {
                app_id: app_id.to_string(),
                url,
                method,
                headers,
                body,
                response: tx,
            },
        )
        .await?;

        rx.await
            .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))?
    }

    /// List all applications
    #[cfg(test)]
    pub async fn list_apps(&self) -> Vec<String> {
        self.placement.read().await.of_app.keys().cloned().collect()
    }

    /// Get thread count
    #[cfg(test)]
    pub async fn thread_count(&self) -> usize {
        self.placement.read().await.threads.len()
    }

    /// Get statistics
    pub async fn get_stats(&self) -> HashMap<String, usize> {
        let placement = self.placement.read().await;

        let mut thread_apps: HashMap<String, usize> = HashMap::new();
        for thread_id in placement.of_app.values() {
            let key = format!("thread_{}", thread_id);
            *thread_apps.entry(key).or_insert(0) += 1;
        }

        thread_apps.insert("total_threads".to_string(), placement.threads.len());
        thread_apps.insert("total_apps".to_string(), placement.of_app.len());
        // A non-zero count here means a version change is in flight: the
        // application is holding two threads, one serving and one being built
        // or dropped. Standing at non-zero is the signal that a swap wedged.
        thread_apps.insert("staging_apps".to_string(), placement.staging.len());
        thread_apps.insert("retiring_threads".to_string(), placement.retiring.len());

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
        let (tx, rx) = oneshot::channel();
        self.dispatch(
            app_id,
            RuntimeMessage::RegisterWsConnection {
                app_id: app_id.to_string(),
                ws_id,
                js_to_client_tx,
                response: tx,
            },
        )
        .await?;

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
        let (tx, rx) = oneshot::channel();
        self.dispatch(
            app_id,
            RuntimeMessage::SendWsMessage {
                app_id: app_id.to_string(),
                ws_id,
                message,
                response: tx,
            },
        )
        .await?;

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
        let (tx, rx) = oneshot::channel();
        self.dispatch(
            app_id,
            RuntimeMessage::UnregisterWsConnection {
                app_id: app_id.to_string(),
                ws_id,
                response: tx,
            },
        )
        .await?;

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
        let (tx, rx) = oneshot::channel();
        self.dispatch(
            app_id,
            RuntimeMessage::ExecuteQueueBatch {
                app_id: app_id.to_string(),
                queue,
                messages,
                response: tx,
            },
        )
        .await?;

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
        let (tx, rx) = oneshot::channel();
        self.dispatch(
            app_id,
            RuntimeMessage::ExecuteScheduled {
                app_id: app_id.to_string(),
                cron,
                scheduled_time_ms,
                response: tx,
            },
        )
        .await?;

        rx.await
            .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))?
    }

    /// Get aggregate application OpMetrics
    pub async fn get_op_metrics(&self, app_id: &str) -> Result<OpMetricsSummary> {
        let (tx, rx) = oneshot::channel();
        self.dispatch(
            app_id,
            RuntimeMessage::GetOpMetrics {
                app_id: app_id.to_string(),
                response: tx,
            },
        )
        .await?;

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
        let (tx, rx) = oneshot::channel();
        self.dispatch(
            app_id,
            RuntimeMessage::ConnectInspector {
                app_id: app_id.to_string(),
                response: tx,
            },
        )
        .await?;

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

    /// Take the shared pool for one test.
    ///
    /// A poisoned lock is taken anyway: poisoning only means an earlier test
    /// panicked, and refusing every later test would report that one failure
    /// as several and hide which one was real.
    fn pool() -> std::sync::MutexGuard<'static, RuntimePool> {
        TEST_POOL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[tokio::test]
    async fn test_pool_creation() {
        let pool = pool();
        // Not an equality: the pool starts these four up front and adds one
        // whenever more applications are loaded than there are threads, and
        // this pool is shared with every other test in this module.
        assert!(pool.thread_count().await >= 4);
    }

    #[tokio::test]
    async fn test_load_and_execute() {
        let pool = pool();

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
        assert_eq!(body.into_bytes().as_deref(), Some(&b"Hello from pool!"[..]));

        // Cleanup
        let _ = pool.unload_app("test_load_exec_app").await;
    }

    #[tokio::test]
    async fn a_module_whose_top_level_never_returns_fails_the_load_and_frees_the_thread() {
        // Top-level code is application code, and this is where it runs. The
        // LoadApp message is awaited on the runtime thread's own message loop,
        // so before module evaluation had a ceiling a bundle like this one
        // took a thread out of the node for the life of the process *and* left
        // the deployment waiting forever -- the response was never sent
        // because the loop that would have sent it never came back. Observed:
        // still spinning eight seconds into a 500ms request budget.
        //
        // The spin is synchronous on purpose. A top-level `await` that never
        // settles already failed cleanly in a few milliseconds; a loop that
        // never yields is the one a timer spawned on the same thread can never
        // interrupt, which is why the cut-off waits on a thread of its own.
        let pool = pool();

        let spinning = r#"
            let n = 0;
            while (true) { n++; }
            export default async function handler() { return new Response("never"); }
        "#;

        let started = std::time::Instant::now();
        let refused = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            pool.load_app(
                "spin_app.production".to_string(),
                spinning.to_string(),
                IsolateSpec::default(),
            ),
        )
        .await
        .expect(
            "the load never answered: the runtime thread is still inside module evaluation and \
             will not process another message",
        )
        .expect_err("a module whose top level never returns must not load");

        let took = started.elapsed();
        let message = refused.to_string();
        assert!(
            message.contains("spin_app.production"),
            "the refusal must name the application: {message}"
        );
        assert!(
            message.contains("module evaluation"),
            "the refusal must say what was cut off: {message}"
        );
        assert!(
            took < std::time::Duration::from_secs(20),
            "the load was refused after {took:?}, which is not the ceiling doing it"
        );

        // And the pool is serving again. A thread still spinning inside the
        // refused load would be one thread short, and the isolate left behind
        // would answer nothing.
        let good = r#"export default function handler() { return new Response("ok"); }"#;
        pool.load_app(
            "after_spin_app.production".to_string(),
            good.to_string(),
            IsolateSpec::default(),
        )
        .await
        .expect("an application must still load after a refused one");

        let (status, _headers, body) = pool
            .execute_request(
                "after_spin_app.production",
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
            )
            .await
            .expect("and it must serve");
        assert_eq!(status, 200);
        assert_eq!(body.into_bytes().as_deref(), Some(&b"ok"[..]));

        pool.unload_app("after_spin_app.production").await.unwrap();
    }

    #[tokio::test]
    async fn each_app_gets_a_thread_to_itself() {
        let pool = pool();

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

        let stats = pool.get_stats().await;
        assert_eq!(stats.get("total_apps"), Some(&4));
        // The per-thread counts are the point: two applications sharing a
        // thread means two isolates sharing one, which breaks both.
        for (thread, apps) in stats.iter().filter(|(k, _)| k.starts_with("thread_")) {
            assert_eq!(*apps, 1, "{thread} hosts {apps} applications");
        }

        for i in 0..4 {
            let _ = pool.unload_app(&format!("lb_app{}", i)).await;
        }
    }

    #[tokio::test]
    async fn more_apps_than_threads_all_keep_serving() {
        // Two isolates on one OS thread break each other: the second one
        // entered shadows the first, so the first application's next request
        // panics the runtime thread and takes every application on it down.
        // The pool has four threads, so loading six is what used to double up.
        let pool = pool();

        let code = r#"export default function handler() { return new Response("ok"); }"#;
        let names: Vec<String> = (0..6)
            .map(|i| format!("crowded_app{i}.production"))
            .collect();
        for name in &names {
            pool.load_app(name.clone(), code.to_string(), IsolateSpec::default())
                .await
                .unwrap_or_else(|e| panic!("loading {name}: {e}"));
        }

        // Every one of them, oldest first: the oldest is the one an isolate
        // built later on the same thread would have broken.
        for name in &names {
            let (status, _, body) = pool
                .execute_request(
                    name,
                    "http://localhost/".to_string(),
                    "GET".to_string(),
                    HashMap::new(),
                    None,
                )
                .await
                .unwrap_or_else(|e| panic!("{name} stopped serving: {e}"));
            assert_eq!(status, 200, "{name}");
            assert_eq!(body.into_bytes().as_deref(), Some(&b"ok"[..]), "{name}");
        }

        assert!(
            pool.thread_count().await >= names.len(),
            "the pool must have started a thread per application rather than doubling up"
        );

        for name in &names {
            pool.unload_app(name).await.expect("unload");
        }
    }

    #[tokio::test]
    async fn an_unloaded_apps_thread_is_reused() {
        // A thread costs a stack and a tokio runtime; a node that redeploys
        // all day must not accumulate one per deployment.
        let pool = pool();

        let code = r#"export default function handler() { return new Response("ok"); }"#;
        pool.load_app(
            "recycled_app.production".to_string(),
            code.to_string(),
            IsolateSpec::default(),
        )
        .await
        .expect("load");
        let before = pool.thread_count().await;
        pool.unload_app("recycled_app.production")
            .await
            .expect("unload");

        pool.load_app(
            "recycled_app.preview".to_string(),
            code.to_string(),
            IsolateSpec::default(),
        )
        .await
        .expect("load");
        assert_eq!(
            pool.thread_count().await,
            before,
            "the idle thread should have been taken instead of a new one started"
        );
        pool.unload_app("recycled_app.preview")
            .await
            .expect("unload");
    }

    #[tokio::test]
    async fn the_platform_wiring_never_reaches_the_app_s_env() {
        // The control plane delivers the store address in the same env map as
        // the app's own variables. The app must see its variables and nothing
        // else: platform wiring is not application configuration, and an app
        // variable spelled like one of the reserved keys must not be able to
        // point a storage client somewhere else.
        let pool = pool();

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
            "env_wiring_app.production".to_string(),
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
                "env_wiring_app.production",
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
            )
            .await
            .expect("the request runs");
        let _ = pool.unload_app("env_wiring_app.production").await;

        assert_eq!(status, 200);
        let body = String::from_utf8(body.into_bytes().unwrap_or_default())
            .expect("the handler answers JSON");
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
        let pool = pool();

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

    /// The version-change property, stated as the thing that used to be
    /// false: while the replacement is being built, the application is still
    /// answering, and with the *old* code.
    #[tokio::test]
    async fn the_running_version_answers_while_its_replacement_is_built() {
        let pool = pool();

        let v1 = r#"export default function handler() { return new Response("v1"); }"#;
        let v2 = r#"export default function handler() { return new Response("v2"); }"#;
        let app = "swap_app.production";

        pool.load_app(app.to_string(), v1.to_string(), IsolateSpec::default())
            .await
            .expect("load v1");

        let staged = pool
            .stage_app(app.to_string(), v2.to_string(), IsolateSpec::default())
            .await
            .expect("stage v2");

        // v2 exists, is warm, and is not reachable: before staging, the pool
        // could only unload then load, so this is the moment where the app
        // used to be gone.
        let (status, _, body) = pool
            .execute_request(
                app,
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
            )
            .await
            .expect("the app is still serving while its replacement is built");
        assert_eq!(status, 200);
        assert_eq!(body.into_bytes().as_deref(), Some(&b"v1"[..]));

        let stats = pool.get_stats().await;
        assert_eq!(
            stats.get("staging_apps"),
            Some(&1),
            "a staged replacement must be visible as one"
        );

        pool.activate(staged).await.expect("activate v2");

        let (_, _, body) = pool
            .execute_request(
                app,
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
            )
            .await
            .expect("the app serves after the swap");
        assert_eq!(
            body.into_bytes().as_deref(),
            Some(&b"v2"[..]),
            "the swap did not make the new version the one that serves"
        );

        let stats = pool.get_stats().await;
        assert_eq!(stats.get("staging_apps"), Some(&0));
        assert_eq!(
            stats.get("retiring_threads"),
            Some(&0),
            "the thread the old isolate was on was never given back"
        );

        pool.unload_app(app).await.expect("unload");
    }

    #[tokio::test]
    async fn a_replacement_that_will_not_build_leaves_the_old_one_serving() {
        let pool = pool();

        let good = r#"export default function handler() { return new Response("good"); }"#;
        let broken = "this is not JavaScript ((((";
        let app = "broken_swap_app.production";

        pool.load_app(app.to_string(), good.to_string(), IsolateSpec::default())
            .await
            .expect("load the working version");

        let error = pool
            .stage_app(app.to_string(), broken.to_string(), IsolateSpec::default())
            .await
            .expect_err("code that does not parse cannot be staged");

        let (status, _, body) = pool
            .execute_request(
                app,
                "http://localhost/".to_string(),
                "GET".to_string(),
                HashMap::new(),
                None,
            )
            .await
            .unwrap_or_else(|e| panic!("the working version stopped serving after {error}: {e}"));
        assert_eq!(status, 200);
        assert_eq!(body.into_bytes().as_deref(), Some(&b"good"[..]));

        // The thread the failed build reserved is back in circulation.
        assert_eq!(pool.get_stats().await.get("staging_apps"), Some(&0));

        pool.unload_app(app).await.expect("unload");
    }

    #[tokio::test]
    async fn two_replacements_cannot_be_built_for_one_app_at_once() {
        // Both would end up activating, and the second swap would retire an
        // isolate the first had already retired.
        let pool = pool();

        let code = r#"export default function handler() { return new Response("ok"); }"#;
        let app = "double_stage_app.production";

        pool.load_app(app.to_string(), code.to_string(), IsolateSpec::default())
            .await
            .expect("load");
        let staged = pool
            .stage_app(app.to_string(), code.to_string(), IsolateSpec::default())
            .await
            .expect("stage the first replacement");

        let error = pool
            .stage_app(app.to_string(), code.to_string(), IsolateSpec::default())
            .await
            .expect_err("a second replacement is refused while the first is in flight");
        assert!(error.to_string().contains("already being built"), "{error}");

        pool.discard(staged).await;
        pool.unload_app(app).await.expect("unload");
    }

    #[tokio::test]
    async fn unloading_an_app_throws_away_a_replacement_still_being_built() {
        // Otherwise the staged isolate holds a runtime thread for the life of
        // the process, for an application that is no longer on this node.
        let pool = pool();

        let code = r#"export default function handler() { return new Response("ok"); }"#;
        let app = "abandoned_stage_app.production";

        pool.load_app(app.to_string(), code.to_string(), IsolateSpec::default())
            .await
            .expect("load");
        let staged = pool
            .stage_app(app.to_string(), code.to_string(), IsolateSpec::default())
            .await
            .expect("stage a replacement");

        pool.unload_app(app).await.expect("unload");

        let stats = pool.get_stats().await;
        assert_eq!(stats.get("staging_apps"), Some(&0));
        assert_eq!(stats.get("retiring_threads"), Some(&0));

        // Activating it now is refused rather than resurrecting an app the
        // node was told to stop running.
        let error = pool
            .activate(staged)
            .await
            .expect_err("the staged version went with the application");
        assert!(error.to_string().contains("no longer"), "{error}");
        assert!(!pool.list_apps().await.contains(&app.to_string()));
    }

    #[tokio::test]
    async fn test_unload_app() {
        let pool = pool();

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
