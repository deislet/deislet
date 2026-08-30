// V8 Runtime Isolate wrapper

use super::bindings::{AppServices, ServiceWiring};
use super::cache_ops::{create_cache_store, CacheStore};
use super::fetch_policy::FetchPolicy;
use super::loader::DbModuleLoader;
use super::metrics::{recorder, MetricsRegistry, RequestOutcome};
use super::ops::{
    AppId, EnvStore, ExecutionDeadline, PendingRequestBody, PendingResponseBody, ResponseStore,
};
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
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use sys_traits::impls::RealSys;
use tokio::sync::oneshot;

/// How long the top level of an application's module may run before the load
/// is abandoned.
///
/// Evaluating a module is not serving a request, and it does not get the
/// request's budget. It happens once per isolate, it is a deployment cost
/// rather than a per-request one, and a legitimately large bundle -- a few
/// megabytes of transpiled JavaScript building its tables and compiling its
/// patterns -- takes longer at it than any handler should be allowed to take
/// answering. Five seconds is roughly two orders of magnitude above what that
/// costs in practice, which leaves room for a cold V8 on a loaded node, and it
/// is also what a load that never finishes costs: `RuntimePool` awaits the
/// LoadApp message on the runtime thread's own message loop, so until module
/// evaluation returns that thread serves nobody. Five seconds of one thread
/// per refused deployment is a bounded loss. No ceiling at all was a permanent
/// one -- a `while (true)` at the top level took a runtime thread out of the
/// node for as long as the process lived.
///
/// Deliberately not configurable per application: an application that could
/// raise its own ceiling could raise it to forever, which is the hole this
/// closes.
const MODULE_EVALUATION_BUDGET_MS: u64 = 5_000;

/// The isolate's outbound WebSockets, as the invocation lifecycle sees them.
///
/// `runtime/ws_client_ops.rs` owns the sockets and installs the implementation
/// in the isolate's `OpState`; this trait is the whole of what this file asks
/// of them. It is declared here rather than there because the module holding
/// the sockets is assembled privately inside `runtime/runtime_init.rs` and
/// cannot be named from this one -- so the calls travel as a value instead.
///
/// Nothing installs it until the application opens its first socket, which is
/// also the first moment any of it matters: an isolate that never opened one
/// has nothing to begin, publish or close.
pub trait OutboundSockets {
    /// A new invocation is starting. Nothing opened before it belongs to it,
    /// and every op that touches a socket will say so.
    fn begin_invocation(&self);

    /// The instant the invocation running now runs out. Published on every
    /// arming, the `waitUntil` tail included: the tail runs under a budget of
    /// its own and is where a socket opened by the request may still be open.
    fn deadline_is(&self, at: Instant);

    /// Close every socket this isolate holds, and answer how many there were.
    fn close_all(&self) -> usize;
}

/// Which kind of application code a cut-off is watching.
///
/// Only used to word the warning. An operator reading "cut off" needs to know
/// what died, because the consequences differ: a request fails and the caller
/// sees it, a queue batch is redelivered, a cron tick is simply lost, and a
/// `waitUntil` tail dies behind a response that already said 200.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Watched {
    ModuleEvaluation,
    Request,
    WaitUntilTail,
    QueueBatch,
    ScheduledRun,
    WebSocketMessage,
}

/// One armed cut-off, as the watchdog thread sees it.
struct Watch {
    generation: u64,
    deadline: Instant,
    what: Watched,
    /// The queue, cron expression or socket the work belongs to, when there
    /// is one; it is what an operator needs to find the cause.
    detail: Option<String>,
    budget_ms: u64,
}

enum WatchCommand {
    Arm(Watch),
    Disarm,
    /// Sent when the isolate goes away. An explicit order rather than letting
    /// the channel close, because an [`Armed`] guard holds a sender too and
    /// the thread must not be waiting on a handle to a disposed isolate.
    Stop,
}

/// What the runtime thread and the watchdog thread agree on.
#[derive(Default)]
struct WatchState {
    /// Bumped on every arm. A cut-off that wakes late compares it and
    /// terminates nothing when the invocation it was watching is already over
    /// -- otherwise it would kill whatever the isolate went on to do next.
    generation: u64,
    armed: bool,
    /// The generation the cut-off last fired on, so the runtime thread can
    /// tell a cut-off apart from an ordinary error.
    fired_on: Option<u64>,
}

/// The cut-off behind every execution budget in this file.
///
/// # Why this is an OS thread and not a timer
///
/// The obvious way to enforce a budget here is `tokio::spawn` a sleep that
/// calls `terminate_execution`, and that is what this file used to do on five
/// of its six paths. It does not work, and the way it fails is the worst one:
/// it works in every test that awaits something and fails on exactly the code
/// an attacker writes.
///
/// Each isolate runs on its own OS thread with a `current_thread` tokio
/// runtime (see `super::pool`). A spawned timer on that runtime only runs when
/// the thread polls it, and the thread only polls when the JavaScript yields.
/// `export default function handler() { while (true) {} }` never yields, so
/// the timer never runs, and the 30-second budget the operator configured is
/// simply not enforced: measured at 75 seconds and still going, with one core
/// pinned. The same held for a `waitUntil` tail, a queue batch, a cron tick
/// and a WebSocket message.
///
/// V8 accepts `terminate_execution` from any thread, so the cut-off waits on a
/// thread that is not the one being blocked. One such thread per isolate,
/// started when the isolate is built and living exactly as long: an isolate
/// already owns a thread, and a thread per *invocation* would put a spawn and
/// a join on the hot path of every request.
struct Watchdog {
    /// Only so the one error path below can name the isolate; the thread has
    /// its own copy because it outlives borrows of this struct.
    app_id: String,
    /// Shared with [`Armed`] rather than borrowed by it. A guard that borrowed
    /// the watchdog would borrow the whole `AppRuntime`, and every one of them
    /// is held across a `&mut self` call -- which is the point: the guard has
    /// to outlive the execution it is watching.
    orders: Arc<WatchOrders>,
    thread: Option<thread::JoinHandle<()>>,
}

/// The half of the watchdog an [`Armed`] guard keeps a hold of.
struct WatchOrders {
    state: Mutex<WatchState>,
    commands: std::sync::mpsc::Sender<WatchCommand>,
}

impl Watchdog {
    /// Start the thread that watches `isolate`.
    ///
    /// Failing to start it fails the isolate. An isolate whose budgets are not
    /// enforced is the defect this type exists for, and a node that quietly
    /// served requests without them would look healthy right up to the first
    /// application that loops.
    fn start(
        isolate: deno_core::v8::IsolateHandle,
        app_id: String,
        metrics: Arc<MetricsRegistry>,
    ) -> Result<Self> {
        let (commands, orders) = std::sync::mpsc::channel::<WatchCommand>();
        let shared = Arc::new(WatchOrders {
            state: Mutex::new(WatchState::default()),
            commands,
        });
        let state_in_thread = shared.clone();
        let app_id_in_thread = app_id.clone();

        let thread = thread::Builder::new()
            .name(format!("deis-watchdog-{app_id}"))
            .spawn(move || {
                let mut pending: Option<Watch> = None;
                loop {
                    let watch = match pending.take() {
                        Some(watch) => watch,
                        // Idle. Blocks until something is armed, or until the
                        // isolate is dropped and the sender with it.
                        None => match orders.recv() {
                            Ok(WatchCommand::Arm(watch)) => watch,
                            // The disarm that follows a cut-off this thread
                            // already delivered.
                            Ok(WatchCommand::Disarm) => continue,
                            Ok(WatchCommand::Stop) | Err(_) => return,
                        },
                    };

                    let remaining = watch.deadline.saturating_duration_since(Instant::now());
                    match orders.recv_timeout(remaining) {
                        Ok(WatchCommand::Disarm) => continue,
                        // An arm with no disarm before it: the invocation that
                        // armed the previous one is over, so watch the newer.
                        Ok(WatchCommand::Arm(next)) => pending = Some(next),
                        Ok(WatchCommand::Stop) => return,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            // Held across the terminate so that a disarm
                            // arriving now either happens entirely before it
                            // -- and then the generation check below refuses
                            // to fire -- or entirely after, and reads
                            // `fired_on`. Without the lock the cut-off could
                            // land on the next invocation.
                            let mut state = state_in_thread
                                .state
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            if !state.armed || state.generation != watch.generation {
                                continue;
                            }
                            state.fired_on = Some(watch.generation);
                            metrics.record_timeout_cutoff(&app_id_in_thread);
                            warn_cut_off(&app_id_in_thread, &watch);
                            isolate.terminate_execution();
                        }
                    }
                }
            })
            .map_err(|e| {
                anyhow::anyhow!(
                    "no watchdog thread could be started for {app_id}, so the isolate is refused \
                     rather than run with budgets nothing enforces: {e}"
                )
            })?;

        Ok(Self {
            app_id,
            orders: shared,
            thread: Some(thread),
        })
    }

    /// Watch the execution that is about to start.
    ///
    /// The returned guard gives the budget back when it is dropped, so a path
    /// that returns early -- a script that fails to start, an error carried out
    /// with `?` -- cannot leave a cut-off armed on the next invocation.
    fn arm(
        &self,
        deadline: Instant,
        what: Watched,
        detail: Option<String>,
        budget_ms: u64,
    ) -> Armed {
        let generation = {
            let mut state = self
                .orders
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.generation += 1;
            state.armed = true;
            state.generation
        };

        let sent = self.orders.commands.send(WatchCommand::Arm(Watch {
            generation,
            deadline,
            what,
            detail,
            budget_ms,
        }));
        if sent.is_err() {
            // Only reachable if the thread ended, which it does only when this
            // struct is dropped. Said out loud because the consequence is that
            // this one execution runs unbounded.
            tracing::error!(
                target: "app",
                app_id = %self.app_id,
                "the watchdog thread of {} is gone; this execution runs with no cut-off",
                self.app_id
            );
        }

        Armed {
            orders: self.orders.clone(),
            generation,
            stopped: false,
        }
    }
}

impl WatchOrders {
    /// Stop watching generation `generation`, and say whether it had fired.
    fn stop(&self, generation: u64) -> bool {
        let fired = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.generation == generation {
                state.armed = false;
            }
            state.fired_on == Some(generation)
        };
        let _ = self.commands.send(WatchCommand::Disarm);
        fired
    }
}

impl Drop for Watchdog {
    /// The order wakes the thread at once, so this is not a wait. It is joined
    /// rather than detached because the thread holds an `IsolateHandle` for an
    /// isolate that is about to be dropped, and using one after disposal is
    /// not something V8 forgives.
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.orders.commands.send(WatchCommand::Stop);
            let _ = thread.join();
        }
    }
}

/// One armed cut-off, from the runtime thread's side.
struct Armed {
    orders: Arc<WatchOrders>,
    generation: u64,
    stopped: bool,
}

impl Armed {
    /// Stop watching, and say whether the cut-off had already fired -- which
    /// is how a caller tells "the application threw" from "the platform killed
    /// it", two things that reach the runtime thread as the same error.
    fn disarm(mut self) -> bool {
        self.stopped = true;
        self.orders.stop(self.generation)
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        if !self.stopped {
            self.orders.stop(self.generation);
        }
    }
}

/// The line an operator reads when application code was killed.
///
/// One place rather than six, so the six paths cannot drift into saying the
/// same thing six ways -- and so that every one of them says what the cut-off
/// costs, which differs per path and is the part that matters.
fn warn_cut_off(app_id: &str, watch: &Watch) {
    let budget_ms = watch.budget_ms;
    let detail = watch.detail.as_deref().unwrap_or("");
    match watch.what {
        Watched::ModuleEvaluation => tracing::warn!(
            target: "app",
            app_id = %app_id,
            budget_ms,
            "The module evaluation of app {app_id} was still running after {budget_ms}ms and has \
             been cut off. The load fails and the runtime thread goes back to serving; until this \
             was bounded, top-level code that never returned held that thread for the life of the \
             process"
        ),
        Watched::Request => tracing::warn!(
            target: "app",
            app_id = %app_id,
            budget_ms,
            "App {app_id} was still running {budget_ms}ms into a request and has been cut off. \
             The request fails. A handler that never yields is only stopped here, so this line is \
             also the only sign that one exists"
        ),
        Watched::WaitUntilTail => tracing::warn!(
            target: "app",
            app_id = %app_id,
            budget_ms,
            "The waitUntil tail of app {app_id} was still running {budget_ms}ms after its \
             invocation was answered, and has been cut off. The invocation itself succeeded, so \
             this line is the only sign of it: background work this application started is being \
             killed"
        ),
        Watched::QueueBatch => tracing::warn!(
            target: "app",
            app_id = %app_id,
            queue = %detail,
            budget_ms,
            "App {app_id} took longer than {budget_ms}ms on a batch from queue {detail}. \
             Terminating the isolate; the whole batch goes back to deis-queue and will be \
             delivered again"
        ),
        Watched::ScheduledRun => tracing::warn!(
            target: "app",
            app_id = %app_id,
            cron = %detail,
            budget_ms,
            "App {app_id} took longer than {budget_ms}ms on the scheduled run for {detail}. \
             Terminating the isolate; this tick did not finish and is not retried"
        ),
        Watched::WebSocketMessage => tracing::warn!(
            target: "app",
            app_id = %app_id,
            ws_id = %detail,
            budget_ms,
            "App {app_id} took longer than {budget_ms}ms on one message from WebSocket {detail}. \
             Terminating the isolate; that message is dropped and the gateway closes the \
             connection, while the application keeps serving everything else on this thread"
        ),
    }
}

/// Status, headers and body of one answered request.
///
/// The body is bytes, not text: a handler may answer with an image, a
/// compressed frame, or anything else `fetch()` brought back from upstream,
/// and a `String` here corrupted every one of them.
type FetchResult = (u16, HashMap<String, String>, Option<Vec<u8>>);

/// The request as `js/runtime.js` sees it -- everything except the body.
///
/// This is serialised into the source text of the invocation script, so it
/// has to be text that survives being written into a program. The body does
/// not qualify: it is arbitrary bytes, and it travels beside this through
/// [`PendingRequestBody`] instead.
#[derive(Serialize)]
struct JsRequest {
    url: String,
    method: String,
    headers: HashMap<String, String>,
    /// Route placeholder values (e.g. `:id` in "/users/:id"), resolved against
    /// the app's configured route patterns. Empty when no pattern matches, or
    /// when the app has no route patterns configured yet.
    params: HashMap<String, String>,
}

/// One leased message as the JS side sees it.
#[derive(Serialize)]
struct JsQueueMessage {
    id: String,
    /// Either the body's text or its base64, tagged by which; see
    /// [`LeasedMessage::js_body`], and `js/runtime.js` for what a queue
    /// handler is then handed.
    body: super::queue_client::JsBody,
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

/// What the handler answered, minus the body: see [`PendingResponseBody`].
#[derive(Deserialize)]
struct JsResponse {
    status: u16,
    #[serde(default)]
    headers: HashMap<String, String>,
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

    /// Where this isolate's `fetch()` may go. The node's own policy: the
    /// operator's allowlist plus the platform endpoints this node knows about.
    /// `AppRuntime::new` adds the service addresses this particular deployment
    /// was wired to.
    ///
    /// The default is the tightest one -- public destinations only, no
    /// allowlist -- so an isolate built by a path that forgets to set it is
    /// closed rather than open.
    pub fetch_policy: FetchPolicy,
}

pub struct AppRuntime {
    /// The thread that enforces every budget below. Declared before `runtime`
    /// so that it is joined before the isolate it holds a handle to is
    /// dropped; see [`Watchdog`] for why the cut-off cannot be a timer.
    watchdog: Watchdog,
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
    /// Maximum wall-clock time (ms) the `waitUntil` tail of one invocation may
    /// run once the response has been sent. Its own limit, not what is left of
    /// `timeout_ms`: see [`crate::config::ExecutionLimits::wait_until_timeout_ms`].
    wait_until_timeout_ms: u64,
    /// Configured JS-op dispatch rate limit (ops/sec) for this isolate.
    /// Kept alongside the live `RateLimiter` (captured in the op-metrics
    /// closure) purely so tests can assert the configured value reached
    /// this isolate; not read outside of `#[cfg(test)]`.
    #[cfg_attr(not(test), allow(dead_code))]
    rate_limit_ops_per_sec: f64,
    /// Where this isolate's request, error, execution-time and cut-off
    /// counters go. See `super::metrics`.
    metrics: Arc<MetricsRegistry>,
    /// The instant the invocation currently running runs out of time, shared
    /// with the isolate's `OpState` so `js/bootstrap.js` can bound an outbound
    /// `fetch` by it. See [`ExecutionDeadline`].
    execution_deadline: ExecutionDeadline,
    /// Whether the timers this invocation armed have already been cleared.
    ///
    /// `js/runtime.js` clears them itself at the end of the `waitUntil` tail,
    /// which is the ordinary path; this says so, so that `end_invocation` does
    /// not run the sweep a second time on every successful request. Reset when
    /// an invocation begins.
    timers_swept: bool,
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
            fetch_policy,
        } = spec;
        let ExecutionLimits {
            timeout_ms,
            wait_until_timeout_ms,
            rate_limit_ops_per_sec,
        } = limits;

        // The node's own configuration knows where deis-control and
        // deis-telemetry are; where deis-store and deis-queue are arrives per
        // deployment, in this app's wiring, so it can only be added now. An
        // app that declared no services still gets the node-wide half.
        let fetch_policy = fetch_policy.with_platform_endpoints(
            wiring
                .endpoints
                .store
                .iter()
                .chain(wiring.endpoints.queue.iter()),
            &app_id,
        );

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

        let execution_deadline: ExecutionDeadline = Rc::new(Cell::new(None));

        {
            let sys = RealSys;
            let parser = Arc::new(deno_permissions::RuntimePermissionDescriptorParser::new(
                sys,
            ));

            // deno reads these lists the opposite way round from how they
            // look. `Some(list)` grants exactly that list, `Some(vec![])`
            // grants everything (it is what a bare `--allow-net` means), and
            // `None` grants nothing at all.
            //
            // `allow_net` therefore has to be `Some(vec![])` for an
            // application that names no hosts, which is every application
            // that says nothing about permissions. Mapped to `None`, as it
            // was, deno refuses every outbound call inside `op_fetch` before
            // this crate's own `FetchPolicy` is ever consulted -- and the
            // refusal reads "Requires net access to \"host:port\", run again
            // with the --allow-net flag", which names a command-line flag this
            // platform does not have and an operator cannot set. It was
            // invisible only because nothing could call `fetch`.
            //
            // Where an application may go is decided by `FetchPolicy`; an
            // application that *does* list hosts is narrowing itself further
            // on top of that, and then both have to agree.
            let allow_net = if permissions_config.allow_net.is_empty()
                || permissions_config.allow_net.contains(&"*".to_string())
            {
                Some(Vec::new())
            } else {
                Some(permissions_config.allow_net)
            };

            // `allow_env` stays deny-by-default deliberately: nothing in this
            // runtime serves `Deno.env`, which is what that permission gates.
            // An application's variables arrive through `op_get_env`, which is
            // this crate's own op and is scoped to the application by
            // construction.
            let allow_env = if permissions_config.allow_env.is_empty()
                || permissions_config.allow_env.contains(&"*".to_string())
            {
                None
            } else {
                Some(permissions_config.allow_env)
            };

            // The backstop, and only the backstop. Redirects are followed in
            // `js/bootstrap.js`, which puts every hop through
            // `op_deis_fetch_guard` with the URL the connector will be given,
            // so this list no longer decides where a `Location:` header may
            // lead -- it could not say what the policy says anyway, having no
            // spelling for an IPv6 prefix and no per-port exception, which is
            // exactly why the wrapper does the following. It stays because
            // deno applies it inside `op_fetch`, underneath everything else:
            // any path that reached `op_fetch` without passing the guard would
            // still be refused a private or loopback address here. See
            // `super::fetch_policy` on redirects.
            let deny_net = fetch_policy.denied_net_ranges(&app_id);

            let permission_opts = deno_permissions::PermissionsOptions {
                allow_env,
                allow_net,
                deny_net: Some(deny_net),
                ..Default::default()
            };

            // A descriptor this crate generated that deno cannot parse would
            // be a bug here, not a bad application; failing the load says so,
            // where the `unwrap()` this replaced took the whole node down with
            // a panic on a runtime thread.
            let permissions =
                deno_permissions::Permissions::from_options(parser.as_ref(), &permission_opts)
                    .map_err(|e| {
                        anyhow::anyhow!("building the net permissions of {app_id}: {e}")
                    })?;

            let permissions_container =
                deno_permissions::PermissionsContainer::new(parser.clone(), permissions);

            init_lazy_extensions(&mut runtime, &fetch_policy, &app_id)?;

            let op_state = runtime.op_state();
            let mut state = op_state.borrow_mut();

            state.put(permissions_container);
            state.put(parser);

            let env_store: EnvStore = Rc::new(RefCell::new(env));
            state.put(env_store);

            // Empty until an invocation arms it. `terminate_execution` is what
            // enforces the timeout, and it does not reach a Rust operation that
            // is already in flight, so an outbound `fetch` reads this and
            // aborts itself instead of running on past the budget and holding
            // the runtime thread every other application on it is waiting for.
            state.put(execution_deadline.clone());

            // The same policy the HTTP client's resolver was built with, put
            // where `op_deis_fetch_guard` can read it. Both halves have to see
            // the same rules or the gate and the backstop would disagree.
            state.put(fetch_policy.clone());

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

            // Both empty until an invocation stages a body. Present from the
            // start so the two ops that use them are defined on every path
            // into this isolate, not only the request path.
            state.put(PendingRequestBody::default());
            state.put(PendingResponseBody::default());

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

        let watchdog = Watchdog::start(
            runtime.v8_isolate().thread_safe_handle(),
            app_id.clone(),
            metrics.clone(),
        )?;

        Ok(Self {
            watchdog,
            runtime,
            app_id,
            module_id: None,
            entry_specifier: specifier,
            resource_table_max,
            op_metrics,
            snapshot_loaded,
            route_patterns,
            timeout_ms,
            wait_until_timeout_ms,
            rate_limit_ops_per_sec,
            metrics,
            execution_deadline,
            timers_swept: false,
        })
    }

    /// Start this invocation's clock, and publish the instant it runs out to
    /// the isolate so an outbound `fetch` can bound itself by it.
    ///
    /// Returns the deadline so the caller's cut-off task can sleep until the
    /// same instant: two clocks started a few lines apart would drift, and the
    /// one an application is told about must be the one it is actually held
    /// to.
    ///
    /// Armed *before* the script is executed, not after: a handler's first
    /// statement can be a `fetch`, and that runs synchronously inside
    /// `execute_script`, before the cut-off task below has been spawned.
    fn arm_execution_budget(&self) -> Instant {
        self.arm_budget(self.timeout_ms)
    }

    /// Start the clock again, for the `waitUntil` tail this time, once the
    /// response has gone out.
    ///
    /// A budget of its own, and a fresh one: the tail is meant to outlive the
    /// response, so what is left of the request's budget is the wrong measure
    /// -- usually nothing at all. See
    /// [`crate::config::ExecutionLimits::wait_until_timeout_ms`] for why the
    /// tail is nonetheless not allowed to run forever.
    fn arm_wait_until_budget(&self) -> Instant {
        self.arm_budget(self.wait_until_timeout_ms)
    }

    fn arm_budget(&self, budget_ms: u64) -> Instant {
        let deadline = Instant::now() + Duration::from_millis(budget_ms);
        self.execution_deadline.set(Some(deadline));
        // The open sockets are held to this too, and the tail's arming is why
        // they are told rather than handed a copy at connect time: a socket
        // opened by the request is allowed to live into the tail, under the
        // tail's budget.
        if let Some(sockets) = self.outbound_sockets() {
            sockets.deadline_is(deadline);
        }
        deadline
    }

    /// The hook `runtime/ws_client_ops.rs` installs the first time this
    /// application opens an outbound socket. Absent means none was ever
    /// opened, and there is nothing for any of these calls to do.
    fn outbound_sockets(&self) -> Option<Rc<dyn OutboundSockets>> {
        let op_state = self.runtime.op_state();
        let state = op_state.borrow();
        state.try_borrow::<Rc<dyn OutboundSockets>>().cloned()
    }

    /// Start one invocation: a request, a queue batch, a cron tick, one
    /// inbound WebSocket message, or the evaluation of the module itself.
    ///
    /// Everything an earlier invocation left behind stops belonging to this
    /// one here. Paired with [`Self::end_invocation`] on every path out.
    fn begin_invocation(&mut self) {
        self.timers_swept = false;
        if let Some(sockets) = self.outbound_sockets() {
            sockets.begin_invocation();
        }
    }

    /// Give back everything one invocation was allowed to hold.
    ///
    /// Called on every path out of one -- the answered request, the handler
    /// that threw, and the cut-off, which is the path an application chooses
    /// by deliberately overrunning its budget and therefore the one that has
    /// to work. What an invocation is allowed to hold, it holds *inside* the
    /// isolate every other request to this application will run in.
    fn end_invocation(&mut self) {
        self.disarm_execution_budget();
        // The sweep below is script, so a termination flag left by a cut-off
        // has to go first or the sweep is what it kills.
        self.clear_termination();

        if let Some(sockets) = self.outbound_sockets() {
            let closed = sockets.close_all();
            if closed > 0 {
                tracing::debug!(
                    target: "app",
                    app_id = %self.app_id,
                    closed,
                    "Closed {} outbound WebSocket(s) of app {} that were still open when the \
                     invocation ended",
                    closed,
                    self.app_id
                );
            }
        }

        self.sweep_invocation_timers();
    }

    /// Clear the timers this invocation left armed.
    ///
    /// `js/runtime.js` does this at the end of `__deis_runtime_wait_until_all`
    /// -- and that runs only when the invocation reached its tail. A cut-off
    /// returns before it, so "an interval cannot outlive the request that
    /// created it" held for every invocation except the one an application
    /// controls: 138 interval callbacks of a deliberately overrun request were
    /// observed firing inside the next one, on that request's budget. So the
    /// sweep is done from here as well, on every path.
    ///
    /// Both swept functions are this runtime's own bookkeeping, defined in
    /// `js/bootstrap.js`; neither runs a line of the application's code. A
    /// module that never finished loading has neither, which is what the
    /// `typeof` guard is for.
    ///
    /// Silent: the count is reported by `js/runtime.js` on the path that
    /// reaches it, and the paths that reach this one instead have already
    /// logged, at warn, why the invocation ended where it did.
    fn sweep_invocation_timers(&mut self) {
        if self.timers_swept {
            return;
        }
        self.timers_swept = true;

        const SOURCE: &str = r#"
            (() => {
                if (typeof globalThis.__deis_runtime_sweep_intervals !== "function") return;
                globalThis.__deis_runtime_sweep_fetch_budgets();
                globalThis.__deis_runtime_sweep_intervals();
            })()
        "#;

        match self.runtime.execute_script("<sweep>", SOURCE) {
            Ok(_) => tracing::trace!(
                target: "app",
                app_id = %self.app_id,
                "Swept the timers of app {} at the end of an invocation that did not reach its \
                 own cleanup",
                self.app_id
            ),
            Err(e) => tracing::warn!(
                target: "app",
                app_id = %self.app_id,
                error = %e,
                "The end-of-invocation timer sweep of app {} did not run: {}. An interval this \
                 invocation left armed will keep firing inside later requests to this application",
                self.app_id,
                e
            ),
        }
    }

    /// Stop the clock. Anything that runs after this point makes outbound
    /// calls with no deadline on them, so it is only ever left off between
    /// invocations -- nothing of the application's runs there.
    fn disarm_execution_budget(&self) {
        self.execution_deadline.set(None);
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

    /// Run the `waitUntil` tail of one invocation, under a budget of its own.
    ///
    /// The tail is application code like any other and it holds the runtime
    /// thread every other application on that thread is waiting for, but by the
    /// time it runs the answer has already gone out: nothing downstream is
    /// still waiting on it, so nothing would ever report it. Before it had a
    /// budget, work handed to `waitUntil` that never finished held that thread
    /// for good -- behind a request that answered 200 and looked entirely
    /// healthy.
    ///
    /// Enforced the way the request budget is: the deadline is published so an
    /// outbound `fetch` started in the tail bounds itself by it, and a cut-off
    /// task terminates the isolate when it expires. Both are given back on
    /// every path out, including the one where the script never starts -- a
    /// deadline left armed, or a termination flag left set, is spent on the
    /// *next* invocation of this isolate rather than on this one. See
    /// `clear_termination` for what that costs.
    async fn run_wait_until_tail(&mut self, script_name: &'static str) -> Result<()> {
        let deadline = self.arm_wait_until_budget();

        // Armed before the tail starts rather than after it has been handed to
        // the event loop: `execute_script` runs the tail's first synchronous
        // stretch itself, so a cut-off armed once it returned would not cover
        // the part of the tail that runs inside it.
        let cut_off = self.watchdog.arm(
            deadline,
            Watched::WaitUntilTail,
            None,
            self.wait_until_timeout_ms,
        );

        let outcome = self
            .settle_script(script_name, "__deis_runtime_wait_until_all()".to_string())
            .await;

        cut_off.disarm();
        self.disarm_execution_budget();
        self.clear_termination();
        // The tail's last act is `js/runtime.js`'s own sweep, so on this path
        // `end_invocation` has nothing left to clear. A tail that was cut off
        // did not get there, and leaves the flag alone.
        self.timers_swept = outcome.is_ok();
        outcome
    }

    /// Start a script and drive the event loop until the promise it evaluated
    /// to settles.
    ///
    /// The two halves are one call so that a caller holding a budget gives it
    /// back on both paths: a script that never starts -- which is what a
    /// cut-off during its first synchronous stretch looks like -- leaves the
    /// same deadline armed and the same termination flag set as one that ran
    /// to the end.
    async fn settle_script(&mut self, name: &'static str, source: String) -> Result<()> {
        let promise = self.runtime.execute_script(name, source)?;
        let resolve_future = self.runtime.resolve(promise);
        self.runtime
            .with_event_loop_promise(resolve_future, PollEventLoopOptions::default())
            .await?;
        Ok(())
    }

    /// Load and evaluate the application's module.
    ///
    /// Under a budget of its own, because the top level of a module is
    /// application code like any other and this is where it runs. It used to
    /// run with nothing watching the clock at all: `while (true) {}` before
    /// the default export took the runtime thread out of the node for the life
    /// of the process, and because `RuntimePool` awaits the load on that same
    /// thread's message loop, the deployment never got an answer either.
    ///
    /// See [`MODULE_EVALUATION_BUDGET_MS`] for the ceiling and why it is not
    /// the request's, and [`Watchdog`] for why the cut-off cannot be a
    /// `tokio::spawn`ed timer the way every other one here is.
    pub async fn init_module(&mut self) -> Result<()> {
        if self.module_id.is_some() {
            return Ok(());
        }

        // A flag left set by whatever ran before would fail this evaluation
        // before a line of the module was read.
        self.clear_termination();
        self.begin_invocation();
        let deadline = self.arm_budget(MODULE_EVALUATION_BUDGET_MS);
        let watchdog = self.watchdog.arm(
            deadline,
            Watched::ModuleEvaluation,
            None,
            MODULE_EVALUATION_BUDGET_MS,
        );

        let outcome = self.evaluate_module().await;

        let cut_off = watchdog.disarm();
        // Top-level code can arm a timer and open a socket like any handler,
        // and a module that was cut off leaves both behind.
        self.end_invocation();

        match outcome {
            Ok(module_id) => {
                self.module_id = Some(module_id);
                Ok(())
            }
            Err(e) if cut_off => {
                tracing::debug!(
                    target: "app",
                    app_id = %self.app_id,
                    error = %e,
                    "Module evaluation of app {} ended in the cut-off",
                    self.app_id
                );
                Err(anyhow::anyhow!(
                    "the module evaluation of {} exceeded the {}ms ceiling and was cut off; the \
                     application is not loaded. Top-level code runs once, before any request, and \
                     has to finish -- work that takes longer belongs inside the handler, where it \
                     has the request's own budget",
                    self.app_id,
                    MODULE_EVALUATION_BUDGET_MS
                ))
            }
            Err(e) => Err(e),
        }
    }

    /// Everything the load does, with no bookkeeping of its own: the caller
    /// arms the budget around this and gives it back on both paths.
    async fn evaluate_module(&mut self) -> Result<deno_core::ModuleId> {
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

        Ok(mod_id)
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
        body: Option<Vec<u8>>,
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

        self.begin_invocation();
        let outcome = self
            .answer_one_request(started, url, method, headers, body, response_tx)
            .await;
        // On the path where the request answered, the tail below has already
        // done most of this; on the path where it was cut off, nothing has.
        self.end_invocation();
        outcome
    }

    /// One request, from the invocation script to the `waitUntil` tail.
    ///
    /// Split out so that every way of leaving it -- the answer, the handler
    /// that threw, the cut-off, the malformed response -- passes through the
    /// caller's `end_invocation`.
    async fn answer_one_request(
        &mut self,
        started: Instant,
        url: String,
        method: String,
        headers: HashMap<String, String>,
        body: Option<Vec<u8>>,
        response_tx: oneshot::Sender<Result<FetchResult>>,
    ) -> Result<()> {
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
            params,
        };

        let request_json = serde_json::to_string(&request)?;

        // The body is handed over out of band, and any body left behind by the
        // previous invocation is cleared in the same breath: an isolate is
        // reused across requests, so a stale one would be answered as this
        // request's, and a handler that returns no body would be given the
        // last one's.
        {
            let op_state = self.runtime.op_state();
            let mut state = op_state.borrow_mut();
            let bytes = body.as_ref().map(|b| b.len()).unwrap_or(0);
            state.put(PendingRequestBody(body));
            state.put(PendingResponseBody(None));
            tracing::trace!(
                target: "app",
                app_id = %self.app_id,
                bytes,
                "Request body staged for the isolate"
            );
        }

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

        // Set execution timeout: platform default, or this app's configured
        // override. Armed before the script runs; see `arm_execution_budget`.
        let deadline = self.arm_execution_budget();

        // The cut-off is armed before `execute_script`, not after it returns,
        // and that ordering is the whole of the defence. `execute_script` runs
        // the script's first synchronous stretch itself, and for
        // `export default function handler() { while (true) {} }` that stretch
        // is the entire handler -- it never returns, so a cut-off armed on the
        // next line would never be armed at all. Measured at 75 seconds
        // against a 300ms budget with the arming one line too late.
        //
        // The guard gives the budget back on every path out of this function,
        // including the `?` below, so a script that fails to start cannot
        // leave a cut-off watching the next invocation.
        let cut_off = self
            .watchdog
            .arm(deadline, Watched::Request, None, self.timeout_ms);

        let result = match self.runtime.execute_script("<fetch>", code) {
            Ok(promise) => {
                let resolve_future = self.runtime.resolve(promise);
                self.runtime
                    .with_event_loop_promise(resolve_future, PollEventLoopOptions::default())
                    .await
            }
            // A script that never started and one that was killed mid-flight
            // arrive here the same way, and both have to be answered: a
            // handler cut off inside `execute_script` used to return through
            // `?` with nothing ever sent on `response_tx`, leaving the caller
            // waiting on a channel that would never carry anything.
            Err(e) => Err(e.into()),
        };

        cut_off.disarm();
        self.disarm_execution_budget();

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

        // Absent means the handler answered without a body -- a 204, a 304,
        // the answer to a HEAD, a WebSocket upgrade -- which is not the same
        // as an empty one, so nothing is invented here.
        let response_body = {
            let op_state = self.runtime.op_state();
            let mut state = op_state.borrow_mut();
            state
                .try_borrow_mut::<PendingResponseBody>()
                .and_then(|pending| pending.0.take())
        };

        let _ = response_tx.send(Ok((response.status, response.headers, response_body)));

        // The response is gone; what the handler handed to `waitUntil` runs
        // from here, under the tail's own budget. A failure past this point
        // cannot be reported to the client and must not be counted against the
        // request, which succeeded -- it is said here instead.
        if let Err(e) = self.run_wait_until_tail("<wait_until>").await {
            tracing::warn!(
                target: "app",
                app_id = %self.app_id,
                error = %e,
                "The waitUntil work of app {} did not finish: {}. The response had already been \
                 sent, so the request is unaffected and the client saw nothing of this",
                self.app_id,
                e
            );
            return Ok(());
        }

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

        self.begin_invocation();
        let outcome = self.run_one_batch(queue, messages).await;
        self.end_invocation();
        outcome
    }

    /// One batch, split out so that the cut-off path gives back what the
    /// settled path gives back; see [`Self::answer_one_request`].
    async fn run_one_batch(
        &mut self,
        queue: String,
        messages: Vec<LeasedMessage>,
    ) -> Result<BatchOutcome> {
        let payload = JsQueueBatch {
            queue: queue.clone(),
            messages: messages
                .iter()
                .map(|message| JsQueueMessage {
                    id: message.id.clone(),
                    body: message.js_body(),
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

        let deadline = self.arm_execution_budget();
        // Armed before the script starts, for the reason spelled out in
        // `execute_fetch`: the handler's first synchronous stretch runs inside
        // `execute_script`, and a handler that never yields never leaves it.
        let cut_off = self.watchdog.arm(
            deadline,
            Watched::QueueBatch,
            Some(queue.clone()),
            self.timeout_ms,
        );

        let result = match self.runtime.execute_script("<queue>", code) {
            Ok(promise) => {
                let resolve_future = self.runtime.resolve(promise);
                self.runtime
                    .with_event_loop_promise(resolve_future, PollEventLoopOptions::default())
                    .await
            }
            Err(e) => Err(e.into()),
        };
        cut_off.disarm();
        self.disarm_execution_budget();
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
        // away right after. A tail that runs past its own budget is cut off,
        // and the batch goes back to deis-queue for the same reason: its
        // follow-up work did not finish either.
        self.run_wait_until_tail("<queue_wait_until>").await?;

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

        self.begin_invocation();
        let outcome = self.run_one_tick(cron, scheduled_time_ms).await;
        self.end_invocation();
        outcome
    }

    /// One tick, split out for the same reason as [`Self::run_one_batch`].
    async fn run_one_tick(&mut self, cron: String, scheduled_time_ms: i64) -> Result<()> {
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

        let deadline = self.arm_execution_budget();
        // Armed before the script starts; see `execute_fetch`.
        let cut_off = self.watchdog.arm(
            deadline,
            Watched::ScheduledRun,
            Some(cron.clone()),
            self.timeout_ms,
        );

        let result = match self.runtime.execute_script("<scheduled>", code) {
            Ok(promise) => {
                let resolve_future = self.runtime.resolve(promise);
                self.runtime
                    .with_event_loop_promise(resolve_future, PollEventLoopOptions::default())
                    .await
            }
            Err(e) => Err(e.into()),
        };
        cut_off.disarm();
        self.disarm_execution_budget();
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
        // called done, or it would be cut off when the isolate goes idle. It
        // gets the tail's budget to do that in, and a tick whose tail is cut
        // off is reported as a failed tick.
        self.run_wait_until_tail("<scheduled_wait_until>").await?;

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

    /// Hand one message from a connected client to the application's socket.
    ///
    /// Bounded by the same wall-clock budget a request is, and for the same
    /// reason: this is the application's own `onmessage` running on the runtime
    /// thread every other application on that thread shares. A socket is open
    /// for as long as the client keeps it open and carries as many messages as
    /// the client sends, so an unbounded one is a request path that anybody can
    /// reach without ever making a request.
    pub async fn dispatch_ws_message(&mut self, ws_id: String, message: String) -> Result<()> {
        // A flag left over from a previous cut-off would fail this message
        // before a line of the application's code ran.
        self.clear_termination();
        self.begin_invocation();

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

        // Armed before the script runs; see `arm_execution_budget`.
        let deadline = self.arm_execution_budget();

        // Armed before the dispatch, unlike the request path's: a message
        // handler is called synchronously by the script below, so all of it
        // runs inside `execute_script` and a cut-off armed afterwards would
        // never cover any of it.
        let cut_off = self.watchdog.arm(
            deadline,
            Watched::WebSocketMessage,
            Some(ws_id.clone()),
            self.timeout_ms,
        );

        let outcome = self.settle_script("<ws_message>", code).await;

        cut_off.disarm();
        // A budget armed and never given back is spent by whatever runs next in
        // this isolate -- the next message, or a request -- which would be told
        // its time was already gone. Same trap as `execute_fetch`. The rest of
        // what this message may have left armed goes back with it: a message
        // handler can set an interval and open a socket exactly as a request
        // handler can, and there is no `waitUntil` tail here to clean up after
        // it.
        self.end_invocation();
        outcome
    }

    /// Tell the application one of its WebSocket clients has gone.
    ///
    /// This runs the application's `close` listener, so it is application code
    /// and it is bounded like any other -- with the same cut-off, for the same
    /// reason. It is not a request, nothing downstream is waiting on it and
    /// the socket is already gone, so a `close` handler that loops would
    /// otherwise take the runtime thread with nothing to show for it: no
    /// request timing out, no client noticing, just a thread that stops
    /// answering for every other application on it.
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

        self.begin_invocation();
        let deadline = self.arm_execution_budget();
        let cut_off = self.watchdog.arm(
            deadline,
            Watched::WebSocketMessage,
            Some(ws_id.clone()),
            self.timeout_ms,
        );

        let outcome = self.settle_script("<ws_close>", js_code).await;

        cut_off.disarm();
        self.disarm_execution_budget();
        self.clear_termination();
        // A close listener can arm a timer and open a socket exactly as a
        // message listener can, and there is no tail here to sweep after it.
        self.end_invocation();
        outcome?;

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
    /// The watchdog and the rate limiter both end an execution by calling
    /// `terminate_execution` on this isolate's handle; this is the same call.
    /// Tests use it for the rate-limit case, which is not reproducible on its
    /// own terms: the limiter would keep firing on the very request that is
    /// supposed to prove recovery. The timeout no longer needs it -- see
    /// `a_handler_that_never_yields_is_still_cut_off`, which drives the real
    /// cut-off end to end.
    #[cfg(test)]
    pub fn terminate_like_a_cut_off(&mut self) {
        self.runtime
            .v8_isolate()
            .thread_safe_handle()
            .terminate_execution();
    }

    /// Exposes the effective execution timeout for this isolate, so a test can
    /// assert the configured value arrived here without also waiting it out.
    ///
    /// This used to carry a paragraph saying the timeout could not be tested
    /// behaviourally, because it was a `tokio::spawn`ed timer that a handler
    /// blocking the thread never let run. That was true, and it was a
    /// description of the defect rather than of the mechanism: the budget an
    /// operator configured was simply not enforced against a handler that did
    /// not yield. [`Watchdog`] is the mechanism now, and the behaviour is
    /// tested directly.
    #[cfg(test)]
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    /// Exposes the effective `waitUntil` budget for this isolate, so a test
    /// can assert the configured value arrived here. It is set from a
    /// different config key than `timeout_ms`, and an isolate handed the
    /// request's value in this slot behaves correctly on every path except a
    /// tail -- which is the path nothing downstream is waiting on.
    #[cfg(test)]
    pub fn wait_until_timeout_ms(&self) -> u64 {
        self.wait_until_timeout_ms
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
