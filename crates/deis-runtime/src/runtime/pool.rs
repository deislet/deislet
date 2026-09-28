// Runtime thread pool - multi-core support

use super::bindings::{consumer_queue_client, ServiceWiring};
use super::header_list::HeaderPairs;
use super::isolate::{AppRuntime, IsolateSpec};
use super::queue_client::LeasedMessage;
use super::queue_consumer::{
    self, BatchInvoker, BatchOutcome, ConsumerHandle, ConsumerSettings, QueuePort,
};
use super::response_stream::ResponseBody;
use crate::app_repo::AppRepo;
use anyhow::Result;
use deno_core::JsRuntime;

use super::OpMetricsSummary;
use futures_util::FutureExt;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Once};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};

/// Ensure V8 platform is initialized only once
static V8_INIT: Once = Once::new();

// Set by `RuntimeMessage::PanicInPumpForTest`, read and cleared by the next
// turn of the pump on this thread.
//
// A thread-local because that is what the thing being tested is: one runtime
// thread's own loop. Nothing in a normal build refers to it.
#[cfg(test)]
thread_local! {
    static PANIC_IN_PUMP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

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

// V8 11.6 and later require all isolate threads to share the parent thread
// that initialized the platform. Production calls `RuntimePool::new` before it
// starts any runtime thread. The Rust test harness, however, starts sibling
// worker threads before the first test body runs. Initialize V8 on the common
// pre-main thread and serialize the libtest bodies: a single body still starts
// multiple runtime threads and exercises the production concurrency model,
// while unrelated tests no longer create and destroy independent V8 pool
// families concurrently (a topology the product never uses and V8 aborts on).
#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init_v8_platform_before_parallel_tests() {
    // SAFETY: constructors run before the test harness starts any thread. No
    // other thread can read or write the process environment at this point.
    unsafe {
        std::env::set_var("RUST_TEST_THREADS", "1");
    }
    init_v8_platform();
}

/// What a caught panic said, as text.
///
/// A panic payload is `&str` for `panic!("literal")` and `String` once
/// anything is formatted into it; anything else is a payload this code never
/// produces. Without this, a log line about a dead isolate would carry the
/// word "Any" and nothing else.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "a panic payload that is neither a string nor a &str".to_string()
}

/// Why the runtime did not start work a caller asked for.
///
/// It travels back down the same reply channel an invocation's result does,
/// which is what lets a caller tell this node's own refusal apart from the
/// application failing: no isolate was entered. A caller reads it with
/// `downcast_ref::<DispatchError>()` to answer its client differently -- an
/// overload is a `503` with a `Retry-After`, not the `500` an application's
/// own error gets.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    /// The application's runtime thread is at capacity: either its queue was
    /// full when the work was posted, or the work waited past the node's
    /// admission budget before the thread reached it.
    ///
    /// The application never saw the work, so retrying is the right answer and
    /// is not a duplicate side effect.
    #[error("the runtime thread for this application is at capacity; retry after {retry_after:?}")]
    Overloaded { retry_after: Duration },
    /// The thread the application is placed on has stopped: it lost its
    /// isolate to a panic and is no longer answering for this application.
    ///
    /// Nothing is lost by retrying, because no work ran, and the pool already
    /// reports the application as not hosted so the control plane reinstalls
    /// it. Until it is reinstalled, the right answer for a caller is to try
    /// again shortly, not to fail as if the application itself had broken.
    #[error(
        "the runtime thread for this application has stopped and it is not serving on this node"
    )]
    Stopped,
    /// The application is not on this node at all: it was never placed here,
    /// or it was and its placement has since been forgotten.
    ///
    /// A scheduling answer about this node, not a statement that the
    /// application does not exist -- the control plane's placement table is
    /// the one that says that, and it may disagree with this node for a while
    /// after an uninstall or a move.
    #[error("the application is not hosted on this node")]
    NotFound,
    /// The exact deployment generation the caller selected has not been
    /// prepared on this node.
    ///
    /// The distinction from [`DispatchError::NotFound`] is the one an exact-
    /// generation caller depends on: the application's other versions may be
    /// serving here just fine, and substituting one of them would serve a
    /// different version than the routing layer selected. The answer is that
    /// this particular generation is not here yet, which is a placement or
    /// convergence fault the caller may retry while the node converges.
    #[error("the requested deployment generation has not been prepared on this node")]
    GenerationNotPrepared,
}

/// What a caller refused at admission is told to wait before retrying.
///
/// The refusal is about a thread that is busy, not an application that is
/// down, and a busy thread is usually free again within one invocation. A
/// short hint keeps the client from backing off past the moment the node could
/// have served it.
///
/// Read from the pool's shared value rather than a constant: the number is
/// the configured `overload_retry_after_seconds`, and the hint the caller
/// gets has to be the same no matter which of the refusals below produced
/// it. Floored at one second: a zero Retry-After is not a hint a client can
/// schedule on, and a value applied at 0 is refused by validation anyway.
fn overload_retry_after(secs: &AtomicU64) -> Duration {
    Duration::from_secs(secs.load(Ordering::Relaxed).max(1))
}

/// What a runtime thread does with one message it has just taken off its
/// queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// Run it.
    Run,
    /// Drop it without a word: the caller closed the reply channel.
    Abandoned,
    /// Refuse it as overloaded: it waited longer than its budget.
    Expired,
}

/// Decide what to do with work that has been waiting in the thread's queue.
///
/// The caller being gone is asked first, and the order matters: a caller that
/// has gone away is owed no answer at all, and refusing it as overloaded would
/// have this node write a reply onto a channel nobody is holding.
///
/// `wait` is passed in rather than read from the clock here, so the boundary
/// can be tested without waiting for it.
fn admission(wait: Duration, budget: Duration, caller_gone: bool) -> Admission {
    if caller_gone {
        Admission::Abandoned
    } else if wait > budget {
        Admission::Expired
    } else {
        Admission::Run
    }
}

/// Throw away work whose caller has already gone, and count it.
fn drop_abandoned(thread_id: usize, app_id: &str, what: &str) {
    tracing::debug!(
        thread_id,
        app_id = %app_id,
        kind = what,
        "Thread {}: dropping a {} whose caller has gone away before the thread reached it; the \
         application is not entered",
        thread_id,
        what
    );
    super::metrics::recorder().record_abandoned_before_start(app_id);
}

/// Answer a caller whose work outlasted its admission budget, and run nothing.
fn reply_overloaded<T>(response: oneshot::Sender<Result<T>>, retry_after: Duration) {
    let _ = response.send(Err(DispatchError::Overloaded { retry_after }.into()));
}

/// Unix seconds: the clock deis-queue stamps lease expiry with.
///
/// The same clock `queue_consumer` reads leases off. This copy exists because
/// it is the runtime thread, not the poller, that decides whether a batch is
/// still worth starting.
fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// How long the shortest-lived message in a batch is still leased to this
/// node.
///
/// The earliest expiry is the one that counts: from that instant on, that
/// message belongs to whoever pulls next. `None` when no message carries an
/// expiry, which is the shape of a test double rather than of a server.
fn remaining_lease(messages: &[LeasedMessage], now: i64) -> Option<Duration> {
    let earliest = messages
        .iter()
        .map(|message| message.lease_expires_at)
        .filter(|expiry| *expiry > 0)
        .min()?;
    Some(Duration::from_secs(
        earliest.saturating_sub(now).max(0) as u64
    ))
}

/// How long one queue batch may wait for the application's runtime thread.
///
/// The smaller of the node's admission budget and what is left of the lease,
/// because both are deadlines that have already started ticking: the admission
/// budget bounds how long a caller may wait for an answer, and the lease bounds
/// how long this node may still answer for the messages at all. Past the lease
/// the messages have been handed to another consumer, and running the handler
/// would do that work a second time.
fn batch_admission_budget(
    admission_budget: Duration,
    messages: &[LeasedMessage],
    now: i64,
) -> Duration {
    match remaining_lease(messages, now) {
        Some(lease) => admission_budget.min(lease),
        None => admission_budget,
    }
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
        /// The way back into this pool, so an isolate can be dispatched into
        /// another isolate the same pool hosts. Handed over here rather than
        /// read from a global, because the pool that owns the thread is the
        /// pool whose applications this one may call.
        router: super::service_ops::ServiceRouter,
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
        headers: HeaderPairs,
        body: Option<Vec<u8>>,
        /// When this message was posted to the thread's queue. Read when the
        /// thread takes it off: a message whose caller has gone, or that has
        /// waited past the admission budget, is not run.
        enqueued_at: Instant,
        response: oneshot::Sender<Result<FetchResult>>,
    },
    /// Hand a batch of queue messages to an app's queue handler
    ExecuteQueueBatch {
        app_id: String,
        queue: String,
        messages: Vec<LeasedMessage>,
        /// When this message was posted to the thread's queue; see
        /// [`RuntimeMessage::ExecuteRequest`].
        enqueued_at: Instant,
        response: oneshot::Sender<Result<BatchOutcome>>,
    },
    /// Hand one application-to-application call to the callee's isolate.
    ///
    /// Its own variant rather than an `ExecuteRequest` with extra headers,
    /// because the call chain has to reach the callee's isolate as data the
    /// application cannot write: an application that could put a chain in a
    /// header could name a chain it is not in, and both the cycle refusal and
    /// the depth ceiling are computed from that chain.
    ExecuteServiceCall {
        app_id: String,
        call: Box<super::service_call::ServiceCall>,
        /// When this message was posted to the thread's queue; see
        /// [`RuntimeMessage::ExecuteRequest`].
        enqueued_at: Instant,
        response: oneshot::Sender<Result<FetchResult>>,
    },
    /// Run an app's scheduled handler for one due cron expression
    ExecuteScheduled {
        app_id: String,
        cron: String,
        /// Unix milliseconds the tick was due.
        scheduled_time_ms: i64,
        /// When this message was posted to the thread's queue; see
        /// [`RuntimeMessage::ExecuteRequest`].
        enqueued_at: Instant,
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
        js_to_client_tx: super::WsClientSink,
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
    /// Attach a debugger to an app's isolate and send what V8 says back down
    /// `to_debugger`.
    AttachInspector {
        app_id: String,
        to_debugger: mpsc::UnboundedSender<String>,
        response: oneshot::Sender<Result<()>>,
    },
    /// One protocol message from an attached debugger.
    ///
    /// It arrives as a message on this queue, rather than being pumped by a
    /// task beside the isolate, because the session may only be touched on the
    /// thread that owns the isolate -- and because a debugger must not be able
    /// to reach into an isolate while that isolate is halfway through serving
    /// somebody's request. Queued here, it is handled between invocations like
    /// everything else.
    InspectorMessage { app_id: String, message: String },
    /// The debugger went away; drop its session while its isolate is still
    /// alive.
    DetachInspector { app_id: String },
    /// Panic this thread, to prove that one panic does not take an
    /// application off the node for good. Nothing outside the tests can send
    /// it.
    #[cfg(test)]
    PanicForTest { app_id: String },
    /// Arm a panic in the *next* turn of the response-stream pump, to prove
    /// the same thing about the half of this loop that is not a message.
    ///
    /// Two hooks rather than one because they panic in two different places,
    /// and the place is the whole point: a panic while handling a message and
    /// a panic while producing a body unwind through different frames of this
    /// loop, and only one of them used to be caught. Nothing outside the tests
    /// can send this either.
    #[cfg(test)]
    PanicInPumpForTest,
    /// Close thread
    Shutdown,
}

/// What one turn of a runtime thread's wait produced.
///
/// The wait has two arms and they do not answer the same thing: one takes a
/// message off the queue, the other gives the isolate's response-stream
/// producers a turn. Naming the outcome is what lets both arms sit inside one
/// `catch_unwind` -- a `continue` from inside the guarded future would leave
/// the pump outside it again.
///
/// The two variants are different sizes, which clippy notices. Boxing the
/// message to even them out would put an allocation on the path every message
/// takes, to shrink a value that lives for one `match` on the stack of a loop
/// that was going to move the message anyway.
#[allow(clippy::large_enum_variant)]
enum PumpTurn {
    /// A message arrived, or the queue closed and the thread is finished.
    Message(Option<RuntimeMessage>),
    /// The pump had its turn; go round again.
    Pumped,
}

/// A runtime thread reporting that the isolate it was hosting is gone.
///
/// Sent when handling a message panicked, or when an invocation ended on an
/// isolate its heap-limit cut-off had poisoned. The thread itself survives --
/// see [`RuntimeThread::run_event_loop`] -- but the application it was
/// hosting no longer has an isolate anywhere on this node, and the pool has
/// to stop saying that it does. Until it does, the control plane is told the
/// assignment is satisfied and never reinstalls it.
struct ThreadFault {
    thread_id: usize,
    /// The tenant whose isolate was dropped. A thread that panicked while
    /// hosting nothing has nothing to report: no placement is wrong.
    app_id: String,
    /// Why the isolate is gone, for the log line the pool writes when it
    /// acts on this.
    cause: ThreadFaultCause,
}

/// Why a runtime thread gave up the isolate it was hosting.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ThreadFaultCause {
    /// Handling a message unwound; the panic's own text travelled with the
    /// fault before this cause existed and is folded into the detail here.
    Panic(String),
    /// The isolate's V8 heap ceiling was tripped. Its heap limit was raised
    /// by the cut-off and V8 offers no way back down, so the isolate is not
    /// fit to be reused; the thread drops it and the application's next
    /// request is served on a freshly built one. Counted as
    /// `deis_runtime_isolate_recycled_total{{reason="memory"}}` before the
    /// fault is sent.
    Memory,
}

impl ThreadFaultCause {
    /// The one-line reason a log reader gets. The `Panic` detail is the
    /// panic's own message, which can be long; it is named separately where
    /// it matters.
    fn describe(&self) -> String {
        match self {
            Self::Panic(panic) => format!("panic: {panic}"),
            Self::Memory => "V8 heap limit tripped".to_string(),
        }
    }
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
    /// What a refusal this thread's queue gives the caller should tell it to
    /// wait; see the `overload_retry_after_secs` field of `RuntimePool`.
    overload_retry_after_secs: Arc<AtomicU64>,
}

impl RuntimeThread {
    /// Create new runtime thread
    fn new(
        thread_id: usize,
        faults: mpsc::UnboundedSender<ThreadFault>,
        admission_wait_ms: Arc<AtomicU64>,
        overload_retry_after_secs: Arc<AtomicU64>,
    ) -> Self {
        // Set channel capacity to 100, exceeding will trigger backpressure (reject requests)
        let (tx, rx) = mpsc::channel(100);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        // Cloned for the struct: the field is how `send` prices a full-queue
        // refusal from the pool's side of the queue, and the thread keeps its
        // own copy for the refusals it answers from the other side.
        let thread_retry_after = overload_retry_after_secs.clone();

        let handle = thread::spawn(move || {
            // Create current_thread tokio runtime
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime");

            rt.block_on(async move {
                Self::run_event_loop(
                    thread_id,
                    rx,
                    ready_tx,
                    faults,
                    admission_wait_ms.clone(),
                    overload_retry_after_secs,
                )
                .await;
            });
        });

        // Wait for thread to be ready
        let _ = ready_rx.recv();

        Self {
            thread_id,
            sender: tx,
            handle: Some(handle),
            overload_retry_after_secs: thread_retry_after,
        }
    }

    /// Run event loop
    async fn run_event_loop(
        thread_id: usize,
        mut rx: mpsc::Receiver<RuntimeMessage>,
        ready_tx: std::sync::mpsc::Sender<()>,
        faults: mpsc::UnboundedSender<ThreadFault>,
        admission_wait_ms: Arc<AtomicU64>,
        overload_retry_after_secs: Arc<AtomicU64>,
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
                Some(app) => {
                    // The guard is around the *whole* wait, not around the
                    // message half of it. Producing a body is application
                    // code running on this thread exactly as handling a
                    // message is -- it is the same isolate, the same ops and
                    // the same V8 -- but it runs from this arm rather than
                    // from `handle_message`, so the `catch_unwind` below used
                    // to be somewhere the unwind never passed through. A
                    // handler that panicked mid-body took the thread with it
                    // for good, and every later request, queue batch and cron
                    // tick for that application answered "Runtime thread has
                    // stopped" -- which is the failure that guard was added to
                    // remove, arriving by the one door it did not cover.
                    let turn = AssertUnwindSafe(async {
                        tokio::select! {
                            msg = rx.recv() => PumpTurn::Message(msg),
                            () = Self::pump(&mut app.runtime) => PumpTurn::Pumped,
                        }
                    })
                    .catch_unwind()
                    .await;
                    match turn {
                        Ok(PumpTurn::Message(msg)) => msg,
                        Ok(PumpTurn::Pumped) => continue,
                        Err(panic) => {
                            // The same recovery a panicking message gets, and
                            // deliberately the same code: an isolate that
                            // unwound is not one to keep serving from,
                            // whichever half of the loop it unwound out of,
                            // and the clients reading the bodies it was
                            // producing have their transfers fail as the
                            // isolate is dropped.
                            Self::lose_isolate(thread_id, &mut hosted, panic, &faults);
                            continue;
                        }
                    }
                }
                None => rx.recv().await,
            };
            let Some(msg) = msg else {
                break;
            };
            // Read per message rather than once, so a budget applied after this
            // thread started is the one in force.
            let admission_budget = Duration::from_millis(admission_wait_ms.load(Ordering::Relaxed));
            // The hint a refusal of this message would carry, read beside the
            // budget for the same reason.
            let retry_after = overload_retry_after(&overload_retry_after_secs);
            // One panic on a runtime thread used to end it. The thread is
            // this application's only isolate on this node, so every later
            // request, queue batch and cron tick for it answered "Runtime
            // thread has stopped" -- for good, because nothing revived it and
            // the node went on reporting the application as running. So the
            // panic is caught at the boundary of one message; see
            // [`Self::lose_isolate`] for what is thrown away and what is kept.
            let handled = AssertUnwindSafe(Self::handle_message(
                thread_id,
                &mut hosted,
                msg,
                admission_budget,
                retry_after,
                &faults,
            ))
            .catch_unwind()
            .await;
            match handled {
                Ok(ControlFlow::Continue(())) => {}
                Ok(ControlFlow::Break(())) => break,
                Err(panic) => Self::lose_isolate(thread_id, &mut hosted, panic, &faults),
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

    /// Drive the open response streams one turn.
    ///
    /// A wrapper rather than a direct call so the test hook has somewhere to
    /// live; in a normal build it compiles to the call itself.
    async fn pump(runtime: &mut AppRuntime) {
        #[cfg(test)]
        if PANIC_IN_PUMP.with(std::cell::Cell::take) {
            panic!("deliberate test panic while producing a response stream");
        }
        runtime.pump_response_streams().await
    }

    /// Give up the isolate this thread was hosting, having caught a panic out
    /// of it, and keep the thread.
    ///
    /// One function and not two copies, because the two places a panic is
    /// caught -- handling a message, and producing a response body -- have to
    /// recover identically: the difference between them is only where the
    /// unwind came from, and a second copy would be a second chance to forget
    /// one of the three things below.
    ///
    /// What is thrown away is the isolate, not the thread. A panic can unwind
    /// from anywhere, including through V8, and an isolate that was halfway
    /// through something when it happened is not one to keep serving from.
    /// Dropping it here is also the only place it *may* be dropped -- an
    /// isolate belongs to the thread that built it -- and it leaves the thread
    /// idle and ready for the next application, rather than leaking a thread
    /// slot nothing can ever use again.
    ///
    /// Whatever was in flight is lost: a request's reply channel went with the
    /// unwind and its caller sees the call dropped, and any body this isolate
    /// was still producing ends without its terminating chunk, so those
    /// clients' transfers fail rather than appearing to complete.
    fn lose_isolate(
        thread_id: usize,
        hosted: &mut Option<HostedApp>,
        panic: Box<dyn std::any::Any + Send>,
        faults: &mpsc::UnboundedSender<ThreadFault>,
    ) {
        let detail = panic_message(panic.as_ref());
        Self::lose_hosted_isolate(thread_id, hosted, ThreadFaultCause::Panic(detail), faults);
    }

    /// Drop the isolate this thread hosts because a finished invocation left
    /// it poisoned by a heap-limit cut-off, and keep the thread.
    ///
    /// The same recovery a panic gets, and deliberately the same code -- see
    /// [`Self::lose_hosted_isolate`] for what is thrown away and what is
    /// kept. The difference is only the register the event is written in: a
    /// panic is a fault in or around the application, while a heap cut-off is
    /// the platform's own ceiling doing what it was configured to do, so the
    /// recycle is counted (reason `memory`) and logged at warn, not error.
    /// The caller passes the tenant the finished invocation belonged to; a
    /// thread that was hosting nobody when it was asked has nothing to
    /// recycle, and the placement that moved the application elsewhere in the
    /// meantime is handled by the pool, not by this thread.
    fn recycle_poisoned_isolate(
        thread_id: usize,
        hosted: &mut Option<HostedApp>,
        app_id: &str,
        faults: &mpsc::UnboundedSender<ThreadFault>,
    ) {
        let poisoned = hosted
            .as_ref()
            .is_some_and(|h| h.app_id == app_id && h.runtime.heap_poisoned());
        if !poisoned {
            return;
        }
        super::metrics::recorder()
            .record_isolate_recycled(app_id, super::metrics::RECYCLE_REASON_MEMORY);
        Self::lose_hosted_isolate(thread_id, hosted, ThreadFaultCause::Memory, faults);
    }

    /// The one way a thread gives up an isolate and keeps itself, whichever
    /// of the two events asked for it.
    ///
    /// One function and not a copy per caller, because the three things below
    /// all have to happen, in this order, for either cause: the isolate is
    /// dropped here, on the thread that built it; the log line says what was
    /// lost; and the pool is told to stop claiming the application is placed
    /// here, without which the control plane never reinstalls it.
    fn lose_hosted_isolate(
        thread_id: usize,
        hosted: &mut Option<HostedApp>,
        cause: ThreadFaultCause,
        faults: &mpsc::UnboundedSender<ThreadFault>,
    ) {
        // Taken before the log line so the isolate is dropped whether or not
        // anything was hosted, and so the fault names the application that
        // lost it.
        match hosted.take() {
            Some(lost) => {
                match &cause {
                    ThreadFaultCause::Panic(panic) => tracing::error!(
                        thread_id,
                        app_id = %lost.app_id,
                        panic = %panic,
                        "Runtime thread {} panicked while serving {}: {}. Its isolate is being \
                         dropped and the application has nothing running on this node until it is \
                         loaded again; the thread itself stays and is free for the next application",
                        thread_id,
                        lost.app_id,
                        panic
                    ),
                    ThreadFaultCause::Memory => tracing::warn!(
                        thread_id,
                        app_id = %lost.app_id,
                        "Runtime thread {} is recycling the isolate of {}: its V8 heap limit was \
                         tripped. The isolate is being dropped and rebuilt fresh on this thread's \
                         next load of the application, which is what keeps its heap ceiling equal \
                         to the configured one; the thread itself stays and the application is \
                         reinstalled without operator action",
                        thread_id,
                        lost.app_id
                    ),
                }
                let app_id = lost.app_id.clone();
                // Here, explicitly, and before the fault is sent: the isolate
                // may only be dropped on the thread that built it, and the
                // pool must not be told the thread is free while it still
                // holds one.
                drop(lost);
                // Tells the pool to stop claiming this application is placed
                // here. Without it the control plane keeps being told the
                // assignment is satisfied and never reinstalls it.
                let _ = faults.send(ThreadFault {
                    thread_id,
                    app_id,
                    cause,
                });
            }
            None => tracing::error!(
                thread_id,
                detail = %cause.describe(),
                "Runtime thread {} lost its isolate ({}), but it was hosting nothing; the thread \
                 stays and is free for the next application",
                thread_id,
                cause.describe()
            ),
        }
    }

    /// Handle one message, and say whether the thread should carry on.
    ///
    /// A function of its own, and not the body of the loop, so that the loop
    /// can catch a panic in it: catching one needs a future to wrap, and
    /// wrapping the whole loop would mean losing the thread anyway. Everything
    /// it touches is reachable from `hosted`, so a panic leaves nothing behind
    /// that the loop cannot drop.
    async fn handle_message(
        thread_id: usize,
        hosted: &mut Option<HostedApp>,
        msg: RuntimeMessage,
        admission_budget: Duration,
        retry_after: Duration,
        faults: &mpsc::UnboundedSender<ThreadFault>,
    ) -> ControlFlow<()> {
        match msg {
            RuntimeMessage::LoadApp {
                app_id,
                code,
                app_repo,
                spec,
                wiring,
                router,
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
                    match AppRuntime::new(app_id.clone(), code, app_repo, *spec, &wiring, router) {
                        Ok(mut runtime) => match runtime.init_module().await {
                            Ok(()) => {
                                *hosted = Some(HostedApp {
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
                    *hosted = None;
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
                enqueued_at,
            } => {
                match admission(
                    enqueued_at.elapsed(),
                    admission_budget,
                    response.is_closed(),
                ) {
                    Admission::Abandoned => drop_abandoned(thread_id, &app_id, "request"),
                    Admission::Expired => reply_overloaded(response, retry_after),
                    Admission::Run => {
                        tracing::debug!(
                            "Thread {}: Executing request for app {}",
                            thread_id,
                            app_id
                        );
                        match hosted_mut(hosted, &app_id) {
                            Some(runtime) => {
                                if let Err(e) = runtime
                                    .execute_fetch(url, method, headers, body, response)
                                    .await
                                {
                                    tracing::error!("Error executing fetch: {}", e);
                                }
                            }
                            None => {
                                // The thread was reassigned or the app's
                                // placement was forgotten between the dispatch
                                // reading it and this message being taken: the
                                // caller gets the same named answer as if the
                                // lookup itself had failed.
                                let _ = response.send(Err(DispatchError::NotFound.into()));
                            }
                        };
                        // A heap-limit cut-off during the invocation leaves the
                        // isolate poisoned: its raised ceiling will not come
                        // back down, so it is dropped and the application's
                        // next request is served on a freshly built one. See
                        // `recycle_poisoned_isolate`.
                        Self::recycle_poisoned_isolate(thread_id, hosted, &app_id, faults);
                    }
                }
            }

            RuntimeMessage::ExecuteServiceCall {
                app_id,
                call,
                response,
                enqueued_at,
            } => {
                match admission(
                    enqueued_at.elapsed(),
                    admission_budget,
                    response.is_closed(),
                ) {
                    Admission::Abandoned => drop_abandoned(thread_id, &app_id, "service call"),
                    Admission::Expired => reply_overloaded(response, retry_after),
                    Admission::Run => {
                        tracing::debug!(
                            thread_id,
                            app_id = %app_id,
                            binding = %call.binding,
                            caller = %call.caller_tenant(),
                            "Thread {}: {} is calling {} through binding {}",
                            thread_id,
                            call.caller_tenant(),
                            app_id,
                            call.binding
                        );
                        match hosted_mut(hosted, &app_id) {
                            Some(runtime) => {
                                if let Err(e) = runtime.execute_service_call(*call, response).await
                                {
                                    tracing::error!(
                                        app_id = %app_id,
                                        error = %e,
                                        "A service call into {} could not be run",
                                        app_id
                                    );
                                }
                            }
                            None => {
                                // The thread was reassigned between the placement
                                // read and this message being taken. The caller
                                // gets the same answer as if the callee had never
                                // been here, which is what it now is.
                                let _ = response.send(Err(anyhow::anyhow!(
                                    "{app_id} is no longer loaded on this node"
                                )));
                            }
                        }
                        Self::recycle_poisoned_isolate(thread_id, hosted, &app_id, faults);
                    }
                }
            }

            RuntimeMessage::ExecuteQueueBatch {
                app_id,
                queue,
                messages,
                response,
                enqueued_at,
            } => {
                // A batch is held to the shorter of the node's admission budget
                // and what is left of its lease; past the lease the messages
                // already belong to another consumer.
                let budget = batch_admission_budget(admission_budget, &messages, now_unix_secs());
                match admission(enqueued_at.elapsed(), budget, response.is_closed()) {
                    Admission::Abandoned => drop_abandoned(thread_id, &app_id, "queue batch"),
                    Admission::Expired => reply_overloaded(response, retry_after),
                    Admission::Run => {
                        tracing::debug!(
                            "Thread {}: Handling {} queue message(s) for app {} from queue {}",
                            thread_id,
                            messages.len(),
                            app_id,
                            queue
                        );
                        let result = match hosted_mut(hosted, &app_id) {
                            Some(runtime) => runtime.execute_queue_batch(queue, messages).await,
                            None => Err(anyhow::anyhow!(
                                "App {app_id} is not loaded on this node; its queue batch was not \
                                 handled and goes back to deis-queue"
                            )),
                        };
                        let _ = response.send(result);
                        Self::recycle_poisoned_isolate(thread_id, hosted, &app_id, faults);
                    }
                }
            }

            RuntimeMessage::ExecuteScheduled {
                app_id,
                cron,
                scheduled_time_ms,
                response,
                enqueued_at,
            } => {
                match admission(
                    enqueued_at.elapsed(),
                    admission_budget,
                    response.is_closed(),
                ) {
                    Admission::Abandoned => drop_abandoned(thread_id, &app_id, "scheduled run"),
                    Admission::Expired => reply_overloaded(response, retry_after),
                    Admission::Run => {
                        tracing::debug!(
                            "Thread {}: Running the scheduled handler of app {} for {}",
                            thread_id,
                            app_id,
                            cron
                        );
                        let result = match hosted_mut(hosted, &app_id) {
                            Some(runtime) => {
                                runtime.execute_scheduled(cron, scheduled_time_ms).await
                            }
                            None => Err(anyhow::anyhow!(
                                "App {app_id} is not loaded on this node, so its scheduled run did \
                                 not happen here; the control plane should try another node"
                            )),
                        };
                        let _ = response.send(result);
                        Self::recycle_poisoned_isolate(thread_id, hosted, &app_id, faults);
                    }
                }
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

                let result = match hosted_mut(hosted, &app_id) {
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

                let result = match hosted_mut(hosted, &app_id) {
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

                let result = match hosted_mut(hosted, &app_id) {
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
                let result = match hosted_mut(hosted, &app_id) {
                    Some(runtime) => Ok(runtime.op_metrics_summary()),
                    None => Err(anyhow::anyhow!("App not found")),
                };
                let _ = response.send(result);
            }

            RuntimeMessage::AttachInspector {
                app_id,
                to_debugger,
                response,
            } => {
                let result = match hosted_mut(hosted, &app_id) {
                    Some(runtime) => runtime.attach_inspector(to_debugger),
                    None => Err(anyhow::anyhow!("App not found")),
                };
                let _ = response.send(result);
            }

            RuntimeMessage::InspectorMessage { app_id, message } => {
                match hosted_mut(hosted, &app_id) {
                    Some(runtime) => runtime.dispatch_inspector_message(message),
                    // The isolate was replaced or unloaded between the
                    // debugger writing this and the thread reaching it. There
                    // is nobody to answer, and the session goes away with the
                    // isolate.
                    None => tracing::debug!(
                        thread_id,
                        app_id = %app_id,
                        "Thread {}: dropping a debugger message for {}, which is not hosted \
                         here any more",
                        thread_id,
                        app_id
                    ),
                }
            }

            RuntimeMessage::DetachInspector { app_id } => {
                if let Some(runtime) = hosted_mut(hosted, &app_id) {
                    runtime.detach_inspector();
                }
            }

            #[cfg(test)]
            RuntimeMessage::PanicForTest { app_id } => {
                panic!("deliberate test panic on thread {thread_id} while hosting {app_id}");
            }

            #[cfg(test)]
            RuntimeMessage::PanicInPumpForTest => {
                PANIC_IN_PUMP.with(|armed| armed.set(true));
            }

            RuntimeMessage::Shutdown => {
                tracing::info!("Runtime thread {} shutting down", thread_id);
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    /// Send message
    fn send(&self, msg: RuntimeMessage) -> Result<()> {
        match self.sender.try_send(msg) {
            Ok(()) => Ok(()),
            // The same two refusals the admission check makes, from the other
            // side of the queue: a full queue is a thread at capacity and a
            // closed one is a thread that is gone. A caller reads both back
            // as a [`DispatchError`], not as a sentence it has to parse.
            Err(mpsc::error::TrySendError::Full(_)) => Err(DispatchError::Overloaded {
                retry_after: overload_retry_after(&self.overload_retry_after_secs),
            }
            .into()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(DispatchError::Stopped.into()),
        }
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
    /// What the version currently serving each tenant declared about
    /// bindings: who may call it, and which namespaces it reaches.
    ///
    /// Here rather than anywhere else because this is where the two halves of
    /// a service call meet: the pool is what knows whether a callee is on this
    /// node at all, and consent is the second question about the same callee.
    /// Keeping it beside the placement means the answer is taken from the
    /// version that is *serving right now* - a callee that withdraws consent
    /// stops being callable at its own next deployment, with nothing to
    /// redeliver to whoever named it.
    serving: RwLock<HashMap<String, ServingVersion>>,
    /// Handed to every runtime thread so it can report that it lost its
    /// isolate to a panic. Kept here as well because threads are started after
    /// the pool is, when more applications are loaded than there are threads.
    faults_tx: mpsc::UnboundedSender<ThreadFault>,
    /// The other end of it, drained by [`Self::apply_thread_faults`].
    ///
    /// Read where the pool is about to act on its placement rather than by a
    /// task of its own: a background task would have to be spawned, which ties
    /// building a pool to being inside a tokio runtime, and it would make
    /// "when does the node notice" depend on a scheduler rather than on the
    /// next thing that asks. A `std` mutex and not a `tokio` one because it is
    /// only ever held for a `try_recv` and never across an await.
    faults: std::sync::Mutex<mpsc::UnboundedReceiver<ThreadFault>>,
    /// A handle back to this pool, for the isolates it hosts.
    ///
    /// A service call is dispatched by an op running inside one isolate into
    /// another isolate this same pool hosts, so the op needs the pool. `Weak`
    /// and not `Arc`: the isolates are owned by the threads the pool owns, so
    /// an `Arc` here would be a cycle that never drops and would keep every
    /// runtime thread alive after the node had finished with them.
    /// How long work may wait in a runtime thread's queue before that thread
    /// refuses it, in milliseconds.
    ///
    /// Shared with every thread rather than copied into each as it starts: the
    /// value is applied once at startup, and a thread started before that (or
    /// after it, for a newly loaded application) has to read the one in force.
    /// Read on the runtime thread every time it takes a message off its queue.
    admission_wait_ms: Arc<AtomicU64>,
    /// What an overloaded refusal tells its caller to wait before retrying,
    /// in whole seconds.
    ///
    /// Shared with every thread the way the admission budget is, for the
    /// same reason: the value is applied once at startup from
    /// `RuntimeConfig::overload_retry_after_seconds`, and a thread started
    /// before that has to give the caller the hint in force, not the default
    /// it was born with. This is the single source of the `Retry-After` a
    /// client sees on an overloaded refusal, and of the number the node logs
    /// beside it -- nothing else may invent one.
    overload_retry_after_secs: Arc<AtomicU64>,
    self_ref: std::sync::Weak<RuntimePool>,
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
    /// Immutable deployment slots addressable by the routing generation the
    /// trusted edge selected. Active, prepared and retained rollback slots all
    /// live here; `of_app` remains the selector for triggers and legacy calls.
    generations: HashMap<(String, String), GenerationSlot>,
    active_generation: HashMap<String, String>,
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

#[derive(Clone)]
struct GenerationSlot {
    storage_tenant: String,
    thread_id: usize,
    sender: mpsc::Sender<RuntimeMessage>,
    wiring: ServiceWiring,
    execution_timeout: Duration,
}

impl Placement {
    /// A thread with no application on it, if there is one.
    fn idle_thread(&self) -> Option<usize> {
        let mut taken: std::collections::HashSet<usize> = self.of_app.values().copied().collect();
        taken.extend(self.generations.values().map(|slot| slot.thread_id));
        taken.extend(self.staging.values().copied());
        taken.extend(self.retiring.iter().copied());
        (0..self.threads.len()).find(|id| !taken.contains(id))
    }
}

/// What the version currently serving one tenant declared about bindings.
///
/// Two facts about the same version, kept together because both are only true
/// of the version that is serving and both stop being true the moment another
/// one does.
struct ServingVersion {
    /// Who this version agrees to be called by, verbatim from its manifest.
    callable_by: Vec<String>,
    /// The namespaces and applications it reaches by name, so the next
    /// version's table can be compared against it.
    bindings: deis_bundle::DeclaredBindings,
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
    /// Storage authority of this version. It can change at a fenced cutover
    /// without changing the runtime placement key or creating a second live
    /// copy of the application.
    storage_tenant: String,
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
    /// Build a pool.
    ///
    /// Answers an `Arc` rather than a bare value because the pool has to be
    /// able to hand its isolates a way back to itself: a service call is one
    /// isolate this pool hosts being dispatched into another. `Arc::new_cyclic`
    /// is what lets the `Weak` exist before the pool does, so nothing has to
    /// remember to attach it afterwards - a step that could be forgotten would
    /// leave every service binding on the node quietly answering "not
    /// available".
    pub fn new(num_threads: Option<usize>) -> Arc<Self> {
        Self::new_with_repo(num_threads, None)
    }

    pub fn new_with_repo(num_threads: Option<usize>, app_repo: Option<Arc<AppRepo>>) -> Arc<Self> {
        // Ensure V8 platform is initialized (only once per process)
        init_v8_platform();

        let num_threads = num_threads.unwrap_or_else(num_cpus::get);
        tracing::info!(
            "Creating runtime pool with {} threads started up front; one application runs per \
             thread, so more threads are started if more applications are loaded than that",
            num_threads
        );

        // Unbounded, and read without ever blocking a thread that writes to
        // it: a runtime thread reports a fault from inside its own panic
        // handling, where it has nowhere to wait, and there are at most as
        // many unread faults as there are threads.
        let (faults_tx, faults_rx) = mpsc::unbounded_channel();

        // Built before the threads, so each one shares it, and replaced once at
        // startup with the configured value.
        let admission_wait_ms =
            Arc::new(AtomicU64::new(crate::config::default_admission_wait_ms()));
        let overload_retry_after_secs = Arc::new(AtomicU64::new(
            crate::config::default_overload_retry_after_seconds(),
        ));

        Arc::new_cyclic(|self_ref| {
            let threads = (0..num_threads)
                .map(|id| {
                    RuntimeThread::new(
                        id,
                        faults_tx.clone(),
                        admission_wait_ms.clone(),
                        overload_retry_after_secs.clone(),
                    )
                })
                .collect();
            Self {
                placement: RwLock::new(Placement {
                    threads,
                    of_app: HashMap::new(),
                    generations: HashMap::new(),
                    active_generation: HashMap::new(),
                    staging: HashMap::new(),
                    retiring: std::collections::HashSet::new(),
                }),
                app_repo,
                consumers: Mutex::new(HashMap::new()),
                serving: RwLock::new(HashMap::new()),
                faults_tx,
                faults: std::sync::Mutex::new(faults_rx),
                admission_wait_ms,
                overload_retry_after_secs,
                self_ref: self_ref.clone(),
            }
        })
    }

    /// A handle an isolate can use to reach this pool, for service calls.
    pub fn router(&self) -> super::service_ops::ServiceRouter {
        super::service_ops::ServiceRouter::new(self.self_ref.clone())
    }

    /// Hold queued work to this admission budget before a runtime thread
    /// refuses it.
    ///
    /// Called once at startup with the configured `admission_wait_ms`. The
    /// threads read it live, so a value applied after they started is the one
    /// they use; nothing else about a running pool changes.
    pub fn set_admission_wait_ms(&self, admission_wait_ms: u64) {
        self.admission_wait_ms
            .store(admission_wait_ms, Ordering::Relaxed);
    }

    /// What an overloaded refusal tells its caller to retry in, in whole
    /// seconds.
    ///
    /// Called once at startup with the configured
    /// `overload_retry_after_seconds`, which validation has already held
    /// above zero. The threads read it live, so a value applied after they
    /// started is the one their next refusal carries; nothing else about a
    /// running pool changes.
    pub fn set_overload_retry_after_secs(&self, overload_retry_after_secs: u64) {
        self.overload_retry_after_secs
            .store(overload_retry_after_secs, Ordering::Relaxed);
    }

    /// Record what the version now serving one tenant declared, and say out
    /// loud what the version it replaced declared and this one does not.
    ///
    /// Called wherever a version becomes the one that serves. The record is
    /// written even when it is empty, because "this application consents to
    /// nobody" and "this application is not loaded" are different answers and
    /// a caller is told them apart.
    ///
    /// The line about dropped bindings is the only moment this is worth
    /// saying. A binding name *is* its namespace, so a version that stops
    /// declaring one has not renamed anything - the data written through the
    /// old name is still in the store under the old name and is now
    /// unreachable from this application. Nothing here can move it, and
    /// nothing later will notice, so it is said at the one moment somebody is
    /// deploying.
    async fn record_serving(&self, tenant: &str, wiring: &ServiceWiring) {
        let mut serving = self.serving.write().await;
        if let Some(previous) = serving.get(tenant) {
            if let Some(dropped) = wiring.describe_dropped(&previous.bindings) {
                tracing::warn!(
                    app_id = tenant,
                    dropped = %dropped,
                    "The version now serving {} no longer declares these storage bindings: {}. A \
                     binding name is its namespace, so this is not a rename: everything written \
                     through those names is still in the store under them and is no longer \
                     reachable from this application. Declare the name again to reach it",
                    tenant,
                    dropped
                );
            }
        }
        serving.insert(
            tenant.to_string(),
            ServingVersion {
                callable_by: wiring.callable_by.clone(),
                bindings: wiring.bindings.clone(),
            },
        );
    }

    /// Whether `caller_app` may call `callee_tenant`, and whether the callee is
    /// on this node at all.
    ///
    /// `None` means the callee is not loaded here. `Some(false)` means it is
    /// and does not consent. The two are kept apart all the way to the
    /// application, because one is a scheduling question for an operator and
    /// the other is a line somebody has to add to a manifest.
    async fn consents_to(&self, callee_tenant: &str, caller_app: &str) -> Option<bool> {
        let serving = self.serving.read().await;
        serving
            .get(callee_tenant)
            .map(|version| deis_bundle::callable_by_allows(&version.callable_by, caller_app))
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
        // Before the lock: a thread that lost its isolate to a panic still
        // looks occupied until its fault is applied, so without this a load
        // would start a new thread beside a perfectly idle one -- and a
        // reinstall of the application that panicked would be refused with
        // "App already exists".
        self.apply_thread_faults().await;
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
                placement.threads.push(RuntimeThread::new(
                    id,
                    self.faults_tx.clone(),
                    self.admission_wait_ms.clone(),
                    self.overload_retry_after_secs.clone(),
                ));
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
        let mut placement = self.placement.write().await;
        placement.of_app.remove(tenant);
        placement.active_generation.remove(tenant);
        drop(placement);
        // Its record goes with it. Left behind, its consent would answer for
        // an application that is no longer here, and the caller would be told
        // "not consented" for something that is really "not deployed" - the
        // one distinction a caller needs in order to know whose problem it is.
        self.serving.write().await.remove(tenant);
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

    /// Dispatch one application-to-application call into the callee's isolate.
    ///
    /// Everything that can be answered without entering the callee is answered
    /// here, in this order, and each answer is a different variant so the
    /// caller can tell them apart:
    ///
    /// 1. **is it here** - a callee this node does not host cannot be reached
    ///    at all, because a service call never leaves the process;
    /// 2. **does it consent** - read from the callee's own current bundle;
    /// 3. **the call itself**, bounded by what the caller has left.
    ///
    /// Consent is checked before the message is posted rather than inside the
    /// callee's isolate, so a refused call costs the callee nothing: no
    /// isolate is entered, no invocation is counted against it, and nothing
    /// appears in its logs. A caller must not be able to make a callee do work
    /// simply by naming it.
    pub async fn call_service(
        &self,
        callee_tenant: &str,
        call: super::service_call::ServiceCall,
        allowance: Option<Duration>,
    ) -> Result<FetchResult, super::service_call::ServiceCallError> {
        use super::service_call::ServiceCallError;

        // So that a callee whose isolate was lost to a panic answers "not
        // deployed here", which is what it now is, rather than the caller
        // waiting on a thread that will never answer.
        self.apply_thread_faults().await;

        let binding = call.binding.clone();
        let caller_app = call.caller_app().to_string();

        match self.consents_to(callee_tenant, &caller_app).await {
            None => {
                return Err(ServiceCallError::NotDeployedHere {
                    binding,
                    callee: callee_tenant.to_string(),
                });
            }
            Some(false) => {
                return Err(ServiceCallError::NotConsented {
                    binding,
                    callee: callee_tenant.to_string(),
                    caller: caller_app,
                });
            }
            Some(true) => {}
        }

        let (tx, rx) = oneshot::channel();
        let posted = {
            let placement = self.placement.read().await;
            match placement.of_app.get(callee_tenant) {
                // Loaded a moment ago and gone now: the callee was unloaded
                // between the consent read and this one. "Not deployed here"
                // is what it now is.
                None => {
                    return Err(ServiceCallError::NotDeployedHere {
                        binding,
                        callee: callee_tenant.to_string(),
                    });
                }
                Some(id) => placement.threads[*id].send(RuntimeMessage::ExecuteServiceCall {
                    app_id: callee_tenant.to_string(),
                    call: Box::new(call),
                    enqueued_at: Instant::now(),
                    response: tx,
                }),
            }
        };
        if let Err(e) = posted {
            // The callee's thread queue is full or its thread has stopped.
            // That is the callee failing to take the call, not the caller
            // running out of time, and it has not entered the callee's code.
            return Err(ServiceCallError::CalleeFailed {
                binding,
                callee: callee_tenant.to_string(),
                reason: e.to_string(),
            });
        }

        // Bounded by what the *caller* has left, never by the callee's own
        // timeout: the callee is held to its own budget by its own watchdog,
        // and the caller must not be left waiting past the point where it can
        // still answer the request it is itself serving. Which of the two ran
        // out is the difference between these two variants.
        let answered = match allowance {
            Some(allowance) => match tokio::time::timeout(allowance, rx).await {
                Ok(answered) => answered,
                Err(_) => {
                    return Err(ServiceCallError::CallerBudgetSpent {
                        binding,
                        callee: callee_tenant.to_string(),
                    });
                }
            },
            None => rx.await,
        };

        match answered {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(e)) => {
                // The caller is another tenant. What the callee's machinery
                // said - a module path, a cut-off with its ceiling, an
                // exception's text - is the callee's to read in its own logs,
                // under its own app id, and not the caller's to receive. The
                // caller gets the fact and an incident id that finds the line.
                let incident = uuid::Uuid::new_v4().simple().to_string()[..16].to_string();
                tracing::warn!(
                    app_id = %callee_tenant,
                    incident = %incident,
                    error = %format!("{e:#}"),
                    "service call failed inside the callee"
                );
                Err(ServiceCallError::CalleeFailed {
                    binding,
                    callee: callee_tenant.to_string(),
                    reason: format!(
                        "its invocation did not answer (incident {incident}); the detail is in \
                         {callee_tenant}'s own logs, not in the caller's"
                    ),
                })
            }
            Err(_) => Err(ServiceCallError::CalleeFailed {
                binding,
                callee: callee_tenant.to_string(),
                reason: "its runtime thread dropped the call without answering".to_string(),
            }),
        }
    }

    /// Act on every runtime thread that lost its isolate to a panic since the
    /// last time anybody looked.
    ///
    /// A thread that panicked drops the isolate it was hosting and carries on
    /// (see [`RuntimeThread::run_event_loop`]). What it cannot do is edit this
    /// pool, so until this runs the pool still says the application is placed
    /// on that thread -- and that claim is what the node answers with when the
    /// control plane asks whether the assignment is satisfied. Left standing,
    /// the application is never reinstalled and never serves again.
    ///
    /// Called wherever the pool is about to answer a question about placement
    /// or act on it, so the answer is never older than the last panic.
    async fn apply_thread_faults(&self) {
        // Never blocks: if another caller is already draining, that caller
        // handles these faults, and this one is not made to wait on the
        // request path for something that is being done anyway.
        let faults: Vec<ThreadFault> = {
            let Ok(mut receiver) = self.faults.try_lock() else {
                return;
            };
            let mut faults = Vec::new();
            while let Ok(fault) = receiver.try_recv() {
                faults.push(fault);
            }
            faults
        };

        for ThreadFault {
            thread_id,
            app_id,
            cause,
        } in faults
        {
            // Only if it is still placed *there*. A version change that moved
            // the application to another thread between the panic and now has
            // already given it a live isolate, and forgetting it here would
            // take a working application off the node.
            let (placed_here, lost_generations) = {
                let mut placement = self.placement.write().await;
                let placed_here = placement
                    .of_app
                    .get(&app_id)
                    .is_some_and(|id| *id == thread_id);
                let lost: Vec<(String, String)> = placement
                    .generations
                    .iter()
                    .filter(|((tenant, _), slot)| tenant == &app_id && slot.thread_id == thread_id)
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in &lost {
                    placement.generations.remove(key);
                }
                if placed_here {
                    placement.of_app.remove(&app_id);
                    placement.active_generation.remove(&app_id);
                }
                (placed_here, lost)
            };
            if !placed_here && lost_generations.is_empty() {
                tracing::info!(
                    thread_id,
                    app_id = %app_id,
                    cause = %cause.describe(),
                    "Runtime thread {} lost the isolate of {} ({}), which has since moved to \
                     another thread; nothing to forget",
                    thread_id,
                    app_id,
                    cause.describe()
                );
                continue;
            }

            if !placed_here {
                tracing::error!(
                    thread_id,
                    app_id = %app_id,
                    generations = ?lost_generations.iter().map(|(_, generation)| generation).collect::<Vec<_>>(),
                    cause = %cause.describe(),
                    "A prepared or retained deployment generation lost its isolate before activation/drain; it is no longer addressable and the control plane must prepare it again"
                );
                continue;
            }

            tracing::error!(
                thread_id,
                app_id = %app_id,
                cause = %cause.describe(),
                "Forgetting the placement of {}: the isolate it was serving from on thread {} \
                 was lost ({}). This node now answers requests for it with \
                 \"App not found\" instead of a stopped thread, and reports it as not running, \
                 so the control plane reinstalls it on the next reconcile",
                app_id,
                thread_id,
                cause.describe()
            );

            // Before the placement goes, so no batch is leased for an
            // application that has no isolate to hand it to.
            self.stop_consumers(&app_id).await;
            self.serving.write().await.remove(&app_id);
        }
    }

    /// Whether an application has an isolate on this node right now.
    ///
    /// The honest answer to "is it running here", as opposed to "was it
    /// installed here": a version record survives its isolate, and the two
    /// disagree exactly when something went wrong.
    pub async fn is_hosted(&self, app_id: &str) -> bool {
        self.apply_thread_faults().await;
        self.placement.read().await.of_app.contains_key(app_id)
    }

    /// Post a message to the thread hosting `app_id`.
    async fn dispatch(&self, app_id: &str, message: RuntimeMessage) -> Result<()> {
        self.apply_thread_faults().await;
        let placement = self.placement.read().await;
        let id = *placement
            .of_app
            .get(app_id)
            .ok_or(DispatchError::NotFound)?;
        placement.threads[id].send(message)
    }

    async fn dispatch_generation(
        &self,
        app_id: &str,
        generation: &str,
        message: RuntimeMessage,
    ) -> Result<()> {
        self.apply_thread_faults().await;
        let placement = self.placement.read().await;
        let slot = placement
            .generations
            .get(&(app_id.to_string(), generation.to_string()))
            .ok_or(DispatchError::GenerationNotPrepared)?;
        // The generation's thread is a raw sender rather than a
        // [`RuntimeThread`], so the two refusals get the same answers here
        // that [`RuntimeThread::send`] gives them on the current path.
        match slot.sender.try_send(message) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(DispatchError::Overloaded {
                retry_after: overload_retry_after(&self.overload_retry_after_secs),
            }
            .into()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(DispatchError::Stopped.into()),
        }
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
        // Empty when the version names no canonical tenant: fine for an
        // application that reaches no storage service, refused when a
        // storage client is built for one that does.
        let storage_tenant = spec.storage_tenant.clone().unwrap_or_default();
        // Tests name tenants by label; see the same step in `AppRuntime::new`.
        #[cfg(test)]
        let storage_tenant = crate::runtime::tenant::test_tenant(if storage_tenant.is_empty() {
            &tenant
        } else {
            &storage_tenant
        });
        #[cfg(test)]
        {
            spec.storage_tenant = Some(storage_tenant.clone());
        }
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
            router: self.router(),
            response: tx,
        });

        let result = match sent {
            Ok(()) => rx
                .await
                .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))
                .and_then(|inner| inner),
            // The same mapping `RuntimeThread::send` applies: the claim on
            // the thread is released below when this returns an error, so a
            // caller retrying the stage finds it free again.
            Err(e) => Err(match e {
                mpsc::error::TrySendError::Full(_) => DispatchError::Overloaded {
                    retry_after: overload_retry_after(&self.overload_retry_after_secs),
                }
                .into(),
                mpsc::error::TrySendError::Closed(_) => DispatchError::Stopped.into(),
            }),
        };

        match result {
            Ok(()) => Ok(StagedVersion {
                tenant,
                storage_tenant,
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

    /// Publish a warm staged isolate under an immutable generation without
    /// moving the tenant's production selector or starting asynchronous
    /// consumers. HTTP addressed to this generation can now be verified and
    /// served explicitly while legacy/current traffic keeps using `of_app`.
    pub async fn prepare_generation(
        &self,
        staged: StagedVersion,
        generation: String,
    ) -> Result<()> {
        let StagedVersion {
            tenant,
            storage_tenant,
            thread_id,
            sender,
            wiring,
            execution_timeout,
        } = staged;

        let mut placement = self.placement.write().await;
        match placement.staging.get(&tenant) {
            Some(id) if *id == thread_id => {
                placement.staging.remove(&tenant);
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "the staged isolate of {tenant} on thread {thread_id} is no longer reserved"
                ));
            }
        }
        let key = (tenant.clone(), generation.clone());
        if placement.generations.contains_key(&key) {
            // This should have been caught by ApplicationManager before a
            // second isolate was built. Do not overwrite the authoritative
            // slot and strand its thread.
            placement.retiring.insert(thread_id);
            drop(placement);
            self.retire_thread(&tenant, thread_id, sender).await;
            return Err(anyhow::anyhow!(
                "deployment generation {generation} of {tenant} is already prepared"
            ));
        }
        placement.generations.insert(
            key,
            GenerationSlot {
                storage_tenant,
                thread_id,
                sender,
                wiring,
                execution_timeout,
            },
        );
        tracing::info!(
            app_id = %tenant,
            routing_generation = %generation,
            thread_id,
            "Prepared an immutable deployment generation without moving production traffic"
        );
        Ok(())
    }

    /// Whether a prepared/active/retained generation still owns a live slot.
    pub async fn is_generation_hosted(&self, tenant: &str, generation: &str) -> bool {
        self.apply_thread_faults().await;
        self.placement
            .read()
            .await
            .generations
            .contains_key(&(tenant.to_string(), generation.to_string()))
    }

    /// Move the current selector to an already prepared generation. Old
    /// generation-addressed slots stay reachable for proxies that still hold
    /// the previous routing snapshot and for an exact rollback.
    pub async fn activate_generation(&self, tenant: &str, generation: &str) -> Result<()> {
        self.apply_thread_faults().await;
        let (slot, previous) = {
            let mut placement = self.placement.write().await;
            let slot = placement
                .generations
                .get(&(tenant.to_string(), generation.to_string()))
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "deployment generation {generation} of {tenant} is not prepared"
                    )
                })?;
            if placement.of_app.get(tenant) == Some(&slot.thread_id)
                && placement.active_generation.get(tenant).map(String::as_str) == Some(generation)
            {
                return Ok(());
            }
            let previous_id = placement.of_app.insert(tenant.to_string(), slot.thread_id);
            placement
                .active_generation
                .insert(tenant.to_string(), generation.to_string());
            let previous = previous_id.filter(|id| *id != slot.thread_id).map(|id| {
                let retained = placement
                    .generations
                    .values()
                    .any(|candidate| candidate.thread_id == id);
                if !retained {
                    placement.retiring.insert(id);
                }
                (id, placement.threads[id].sender.clone(), retained)
            });
            (slot, previous)
        };

        self.record_serving(tenant, &slot.wiring).await;
        self.stop_consumers(tenant).await;
        self.start_consumers(
            tenant,
            &slot.storage_tenant,
            slot.sender.clone(),
            &slot.wiring,
            slot.execution_timeout,
            Some(generation),
        )
        .await;

        if let Some((previous_id, previous_sender, retained)) = previous {
            if retained {
                tracing::info!(
                    app_id = tenant,
                    routing_generation = generation,
                    from_thread = previous_id,
                    to_thread = slot.thread_id,
                    "Activated the prepared generation; the previous generation remains addressable while routing converges"
                );
            } else {
                self.retire_thread(tenant, previous_id, previous_sender)
                    .await;
            }
        }
        Ok(())
    }

    /// Discard one generation that is not the current trigger/legacy selector.
    pub async fn discard_generation(&self, tenant: &str, generation: &str) -> Result<()> {
        let slot = {
            let mut placement = self.placement.write().await;
            if placement.active_generation.get(tenant).map(String::as_str) == Some(generation) {
                return Err(anyhow::anyhow!(
                    "deployment generation {generation} of {tenant} is active and must be replaced before it can drain"
                ));
            }
            let slot = placement
                .generations
                .remove(&(tenant.to_string(), generation.to_string()))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "deployment generation {generation} of {tenant} is not prepared"
                    )
                })?;
            placement.retiring.insert(slot.thread_id);
            slot
        };
        self.retire_thread(tenant, slot.thread_id, slot.sender)
            .await;
        Ok(())
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
            storage_tenant,
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
                    ));
                }
            }
            let previous = placement.of_app.insert(tenant.clone(), thread_id);
            if let Some(previous_id) = previous {
                // Out of circulation until its isolate is gone.
                placement.retiring.insert(previous_id);
            }
            previous.map(|id| (id, placement.threads[id].sender.clone()))
        };

        // Recorded at the swap, beside the consumers, because both are things
        // that must be true of the version that is *serving*: from this moment
        // a service call into this tenant is answered against the new
        // version's `callableBy`, and a version that withdrew consent stops
        // being callable here without anything being redelivered to whoever
        // named it.
        self.record_serving(&tenant, &wiring).await;

        // Stop before the old isolate goes away, so a batch already leased is
        // handed to the isolate that can still handle it.
        self.stop_consumers(&tenant).await;
        self.start_consumers(
            &tenant,
            &storage_tenant,
            sender,
            &wiring,
            execution_timeout,
            None,
        )
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
        storage_tenant: &str,
        sender: mpsc::Sender<RuntimeMessage>,
        wiring: &ServiceWiring,
        execution_timeout: Duration,
        generation: Option<&str>,
    ) {
        let specs = wiring.active_consumers(tenant);
        if specs.is_empty() {
            return;
        }

        let Some(endpoint) = wiring.endpoints.queue.as_deref() else {
            return;
        };

        // Charged to the same tenant as the app's own queue client, and
        // carrying the same queue capability, so a preview leases only its
        // own messages and deis-queue answers the poller instead of refusing
        // it. Not the storage token: deis-queue accepts only its own audience.
        let client: Arc<dyn QueuePort> = match consumer_queue_client(
            storage_tenant,
            endpoint,
            wiring.queue_token.as_deref(),
            generation,
        ) {
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
            let invoker = Self::batch_invoker(
                tenant,
                sender.clone(),
                self.overload_retry_after_secs.clone(),
            );
            handles.push(queue_consumer::spawn(
                tenant.to_string(),
                spec.name,
                client.clone(),
                ConsumerSettings::for_app(spec.batch_size, spec.max_concurrency, execution_timeout),
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
    ///
    /// The post is a `try_send` and not the blocking `send`: the batch is
    /// already out on its lease from the moment it was pulled, and a post
    /// that waits on a busy thread holds that lease while it waits -- past
    /// the lease the same messages go to another node and the work runs
    /// twice. A full queue is answered as overloaded instead, and the
    /// consumer refunds the attempt the refusal burned; a closed one is a
    /// thread that is gone, and the batch goes back without a refund.
    fn batch_invoker(
        app_id: &str,
        sender: mpsc::Sender<RuntimeMessage>,
        overload_retry_after_secs: Arc<AtomicU64>,
    ) -> BatchInvoker {
        let app_id = app_id.to_string();
        Arc::new(move |queue: String, messages: Vec<LeasedMessage>| {
            let sender = sender.clone();
            let app_id = app_id.clone();
            let overload_retry_after_secs = overload_retry_after_secs.clone();
            Box::pin(async move {
                let (tx, rx) = oneshot::channel();
                let posted = sender.try_send(RuntimeMessage::ExecuteQueueBatch {
                    app_id: app_id.clone(),
                    queue,
                    messages,
                    enqueued_at: Instant::now(),
                    response: tx,
                });
                if let Err(error) = posted {
                    return Err(match error {
                        mpsc::error::TrySendError::Full(_) => DispatchError::Overloaded {
                            retry_after: overload_retry_after(&overload_retry_after_secs),
                        }
                        .into(),
                        mpsc::error::TrySendError::Closed(_) => DispatchError::Stopped.into(),
                    });
                }
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

        // Remove every prepared/retained generation from the addressable set
        // in the same lock acquisition. The current thread is unloaded by the
        // existing path below; every other generation is retired afterwards.
        let retained = {
            let mut placement = self.placement.write().await;
            let active = placement.of_app.get(app_id).copied();
            let keys: Vec<(String, String)> = placement
                .generations
                .keys()
                .filter(|(tenant, _)| tenant == app_id)
                .cloned()
                .collect();
            let mut retained = Vec::new();
            for key in keys {
                if let Some(slot) = placement.generations.remove(&key) {
                    if Some(slot.thread_id) != active {
                        placement.retiring.insert(slot.thread_id);
                        retained.push(slot);
                    }
                }
            }
            placement.active_generation.remove(app_id);
            retained
        };

        let (tx, rx) = oneshot::channel();
        let dispatched = self
            .dispatch(
                app_id,
                RuntimeMessage::UnloadApp {
                    app_id: app_id.to_string(),
                    response: tx,
                },
            )
            .await;

        for slot in retained {
            self.retire_thread(app_id, slot.thread_id, slot.sender)
                .await;
        }
        dispatched?;

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
        headers: HeaderPairs,
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
                enqueued_at: Instant::now(),
                response: tx,
            },
        )
        .await?;

        rx.await.map_err(|_| {
            // Reached only when the runtime thread ended without answering --
            // it panicked mid-request, or the isolate was dropped underneath
            // it. Every ordinary failure of the invocation now travels down
            // this channel as itself; see `AppRuntime::execute_fetch`, where
            // three paths used to return through `?` with the channel dropped,
            // so a handler that threw was reported to the client as this
            // sentence rather than as its own error.
            anyhow::anyhow!(
                "the runtime thread serving app {app_id} ended this request without answering it; \
                 the node's log has what happened to it"
            )
        })?
    }

    /// Execute against the exact generation selected by a trusted routing
    /// table. There is deliberately no fallback to [`Self::execute_request`].
    pub async fn execute_request_generation(
        &self,
        app_id: &str,
        generation: &str,
        url: String,
        method: String,
        headers: HeaderPairs,
        body: Option<Vec<u8>>,
    ) -> Result<FetchResult> {
        let (tx, rx) = oneshot::channel();
        self.dispatch_generation(
            app_id,
            generation,
            RuntimeMessage::ExecuteRequest {
                app_id: app_id.to_string(),
                url,
                method,
                headers,
                body,
                enqueued_at: Instant::now(),
                response: tx,
            },
        )
        .await?;
        rx.await.map_err(|_| {
            anyhow::anyhow!(
                "the runtime thread serving generation {generation} of {app_id} ended this request without answering it"
            )
        })?
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
        self.apply_thread_faults().await;
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
        js_to_client_tx: super::WsClientSink,
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

    pub async fn register_ws_connection_generation(
        &self,
        app_id: &str,
        generation: &str,
        ws_id: String,
        js_to_client_tx: super::WsClientSink,
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.dispatch_generation(
            app_id,
            generation,
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

    pub async fn send_ws_message_generation(
        &self,
        app_id: &str,
        generation: &str,
        ws_id: String,
        message: String,
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.dispatch_generation(
            app_id,
            generation,
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

    pub async fn unregister_ws_connection_generation(
        &self,
        app_id: &str,
        generation: &str,
        ws_id: String,
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.dispatch_generation(
            app_id,
            generation,
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
                enqueued_at: Instant::now(),
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
                enqueued_at: Instant::now(),
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

    /// Attach a debugger to an application's isolate.
    ///
    /// Answers the two ends of one CDP session: write the debugger's messages
    /// into the sender, read V8's out of the receiver. Dropping the sender is
    /// how a session ends -- the isolate is told to drop its half, which it
    /// must do while it is still alive.
    ///
    /// The debugger's messages are relayed by a task here rather than being
    /// pumped beside the isolate, because the session may only be touched on
    /// the thread that owns the isolate, and that thread's only inbox is its
    /// message queue. Relaying them onto that queue also means a debugger
    /// cannot reach into an isolate that is halfway through serving a request:
    /// its messages are handled between invocations, like every other message.
    ///
    /// See [`super::isolate::AppRuntime::attach_inspector`] for who is allowed
    /// to do this and why that is the line.
    pub async fn connect_inspector(
        &self,
        app_id: &str,
    ) -> Result<(
        mpsc::UnboundedSender<String>,
        mpsc::UnboundedReceiver<String>,
    )> {
        let (to_debugger, from_v8) = mpsc::unbounded_channel();
        let (to_v8, mut from_debugger) = mpsc::unbounded_channel::<String>();

        let (tx, rx) = oneshot::channel();
        self.dispatch(
            app_id,
            RuntimeMessage::AttachInspector {
                app_id: app_id.to_string(),
                to_debugger,
                response: tx,
            },
        )
        .await?;
        rx.await
            .map_err(|_| anyhow::anyhow!("Runtime thread dropped"))??;

        // Taken once, here: the relay must keep posting to the thread the
        // session was created on. If the application moves to another thread,
        // this sender starts failing and the relay stops, which is right --
        // the session belongs to an isolate that no longer exists.
        let sender = {
            let placement = self.placement.read().await;
            match placement.of_app.get(app_id) {
                Some(id) => placement.threads[*id].sender.clone(),
                None => return Err(anyhow::anyhow!("App not found")),
            }
        };

        let tenant = app_id.to_string();
        tokio::spawn(async move {
            while let Some(message) = from_debugger.recv().await {
                if sender
                    .send(RuntimeMessage::InspectorMessage {
                        app_id: tenant.clone(),
                        message,
                    })
                    .await
                    .is_err()
                {
                    tracing::info!(
                        app_id = %tenant,
                        "The runtime thread of {} stopped taking debugger messages; the session \
                         ends here",
                        tenant
                    );
                    return;
                }
            }
            // The debugger's end went away. The isolate has to be told, and on
            // its own thread: the session it holds owns V8 objects that must
            // be torn down before the isolate is.
            let _ = sender
                .send(RuntimeMessage::DetachInspector { app_id: tenant })
                .await;
        });

        Ok((to_v8, from_v8))
    }
}

#[cfg(test)]
impl RuntimePool {
    /// Test hook: post requests at the thread that serves `tenant` -- or, when
    /// `generation` is given, at that generation's slot -- until one is
    /// refused, which is the moment the queue is full.
    ///
    /// The caller is left to hold the thread on something else first: an idle
    /// thread drains a queue of 100 faster than a test can fill it, and what
    /// a test after this one wants is the refusal that only a full queue
    /// gives. The posts carry reply channels nobody holds, so whatever the
    /// thread takes off the queue afterwards it finds its caller gone and
    /// throws away rather than runs: this fills the queue, not the
    /// application.
    pub async fn post_requests_until_refused(
        &self,
        tenant: &str,
        generation: Option<&str>,
    ) -> Result<()> {
        self.apply_thread_faults().await;
        let placement = self.placement.read().await;
        let sender = match generation {
            Some(generation) => placement
                .generations
                .get(&(tenant.to_string(), generation.to_string()))
                .ok_or_else(|| {
                    anyhow::anyhow!("generation {generation} of {tenant} is not prepared")
                })?
                .sender
                .clone(),
            None => placement.threads[*placement
                .of_app
                .get(tenant)
                .ok_or_else(|| anyhow::anyhow!("tenant {tenant} is not placed"))?]
            .sender
            .clone(),
        };
        for _ in 0..1000 {
            let (tx, _rx) = oneshot::channel();
            if sender
                .try_send(RuntimeMessage::ExecuteRequest {
                    app_id: tenant.to_string(),
                    url: "http://localhost/".to_string(),
                    method: "GET".to_string(),
                    headers: Vec::new(),
                    body: None,
                    enqueued_at: Instant::now(),
                    response: tx,
                })
                .is_err()
            {
                return Ok(());
            }
        }
        anyhow::bail!(
            "a queue of 100 never filled with 1000 posts: the thread is draining it \
             faster than the test can fill it"
        )
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
    use crate::runtime::metrics::{recorder, ABANDONED_BEFORE_START_TOTAL, REQUESTS_TOTAL};
    use once_cell::sync::Lazy;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    // Use global RuntimePool to avoid V8 issues from multiple creation/destruction
    static TEST_POOL: Lazy<Mutex<Arc<RuntimePool>>> =
        Lazy::new(|| Mutex::new(RuntimePool::new(Some(4))));

    /// Take the shared pool for one test.
    ///
    /// A poisoned lock is taken anyway: poisoning only means an earlier test
    /// panicked, and refusing every later test would report that one failure
    /// as several and hide which one was real.
    fn pool() -> std::sync::MutexGuard<'static, Arc<RuntimePool>> {
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
                Vec::new(),
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

    /// Load one application with a binding table and a consent list, the way a
    /// deployment does: through the reserved keys in the env map, which is the
    /// only channel the control plane has into a node.
    async fn load_with_wiring(
        pool: &RuntimePool,
        tenant: &str,
        code: &str,
        wiring: ServiceWiring,
    ) -> Result<()> {
        let mut spec = IsolateSpec::default();
        wiring.write_to_env(&mut spec.env);
        pool.load_app(tenant.to_string(), code.to_string(), spec)
            .await
    }

    /// A caller that answers with whatever the binding named in `?binding=`
    /// answered, or with the message of whatever it threw. Both halves matter:
    /// the refusals are the whole point of the feature, and a refusal that
    /// reached the handler as a `TypeError` is what an author can catch.
    const CALLER_CODE: &str = r#"
        export default async function handler(context) {
            const url = new URL(context.request.url);
            const name = url.searchParams.get("binding");
            try {
                const answer = await context.bindings[name].fetch("/hello?from=caller");
                return new Response(await answer.text(), { status: answer.status });
            } catch (e) {
                return new Response("threw: " + e.message, { status: 599 });
            }
        }
    "#;

    /// A callee that says who called it, out of the header the platform
    /// stamps rather than out of anything the caller wrote.
    const CALLEE_CODE: &str = r#"
        export default async function handler(context) {
            const caller = context.request.headers.get("x-deis-caller");
            const url = new URL(context.request.url);
            return new Response(
                "callee saw " + caller + " asking for " + url.pathname + url.search,
                { status: 201 }
            );
        }
    "#;

    fn service_wiring(bindings: &[(&str, &str)], callable_by: &[&str]) -> ServiceWiring {
        let mut wiring = ServiceWiring::default();
        for (name, target) in bindings {
            let (app, environment) = match target.split_once('/') {
                Some((app, environment)) => (app, Some(environment.to_string())),
                None => (*target, None),
            };
            wiring.bindings.insert(
                *name,
                deis_bundle::Binding {
                    kind: deis_bundle::BindingKind::Service,
                    app: Some(app.to_string()),
                    environment,
                },
            );
        }
        wiring.callable_by = callable_by.iter().map(|name| name.to_string()).collect();
        wiring
    }

    async fn call_through(pool: &RuntimePool, caller: &str, binding: &str) -> (u16, String) {
        let (status, _, body) = pool
            .execute_request(
                caller,
                format!("http://localhost/?binding={binding}"),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await
            .expect("the caller itself must answer");
        (
            status,
            String::from_utf8(body.into_bytes().unwrap_or_default())
                .expect("the test handlers answer with text"),
        )
    }

    #[tokio::test]
    async fn one_application_calls_another_on_the_same_node() {
        let pool = pool();

        load_with_wiring(
            &pool,
            "sc-callee.production",
            CALLEE_CODE,
            service_wiring(&[], &["sc-caller"]),
        )
        .await
        .expect("the callee loads");
        load_with_wiring(
            &pool,
            "sc-caller.production",
            CALLER_CODE,
            service_wiring(&[("AUTH", "sc-callee")], &[]),
        )
        .await
        .expect("the caller loads");

        let (status, body) = call_through(&pool, "sc-caller.production", "AUTH").await;

        // The callee's own status comes back, not a status this node invented,
        // and the caller header is the platform's statement about who called.
        assert_eq!(status, 201, "{body}");
        assert_eq!(
            body,
            "callee saw sc-caller.production asking for /hello?from=caller"
        );

        let _ = pool.unload_app("sc-caller.production").await;
        let _ = pool.unload_app("sc-callee.production").await;
    }

    #[tokio::test]
    async fn a_callee_that_did_not_consent_is_not_entered() {
        // Being named by a caller is not consent. The callee here lists
        // somebody else, so the call is refused before its isolate is touched
        // -- which is what stops a caller making a callee do work simply by
        // naming it.
        let pool = pool();

        load_with_wiring(
            &pool,
            "nc-callee.production",
            CALLEE_CODE,
            service_wiring(&[], &["somebody-else"]),
        )
        .await
        .expect("the callee loads");
        load_with_wiring(
            &pool,
            "nc-caller.production",
            CALLER_CODE,
            service_wiring(&[("AUTH", "nc-callee")], &[]),
        )
        .await
        .expect("the caller loads");

        let (status, body) = call_through(&pool, "nc-caller.production", "AUTH").await;
        assert_eq!(status, 599, "the caller must catch a TypeError: {body}");
        assert!(body.contains("callableBy"), "{body}");
        assert!(body.contains("nc-caller"), "{body}");

        let _ = pool.unload_app("nc-caller.production").await;
        let _ = pool.unload_app("nc-callee.production").await;
    }

    #[tokio::test]
    async fn a_callee_that_is_not_on_this_node_says_so_rather_than_failing_vaguely() {
        // The distinction an operator needs: this is a scheduling answer, not
        // a manifest one, and it must not read like a refusal by the callee.
        let pool = pool();

        load_with_wiring(
            &pool,
            "nd-caller.production",
            CALLER_CODE,
            service_wiring(&[("AUTH", "never-deployed")], &[]),
        )
        .await
        .expect("the caller loads");

        let (status, body) = call_through(&pool, "nd-caller.production", "AUTH").await;
        assert_eq!(status, 599, "{body}");
        assert!(body.contains("not deployed on this node"), "{body}");
        assert!(
            body.contains("never-deployed.production"),
            "the refusal must name the environment it looked for: {body}"
        );

        let _ = pool.unload_app("nd-caller.production").await;
    }

    #[tokio::test]
    async fn a_binding_follows_the_callers_own_environment_unless_it_names_one() {
        // The data-crossing case. A preview of one application must reach the
        // preview of the other, never production, unless the manifest says
        // production in so many words.
        let pool = pool();

        load_with_wiring(
            &pool,
            "env-callee.preview",
            CALLEE_CODE,
            service_wiring(&[], &["env-caller"]),
        )
        .await
        .expect("the preview of the callee loads");
        load_with_wiring(
            &pool,
            "env-caller.preview",
            CALLER_CODE,
            service_wiring(
                &[("SAME", "env-callee"), ("PROD", "env-callee/production")],
                &[],
            ),
        )
        .await
        .expect("the preview of the caller loads");

        // The binding that names no environment reaches the preview, because
        // that is where the caller is.
        let (status, body) = call_through(&pool, "env-caller.preview", "SAME").await;
        assert_eq!(status, 201, "{body}");

        // The one that names production reaches production - which is not
        // deployed here, and says so. That it did not silently answer out of
        // the preview is the whole assertion.
        let (status, body) = call_through(&pool, "env-caller.preview", "PROD").await;
        assert_eq!(status, 599, "{body}");
        assert!(body.contains("env-callee.production"), "{body}");

        let _ = pool.unload_app("env-caller.preview").await;
        let _ = pool.unload_app("env-callee.preview").await;
    }

    #[tokio::test]
    async fn a_cycle_is_refused_rather_than_left_to_deadlock() {
        // A -> B -> A. Without the chain check, A's runtime thread would be
        // waiting inside its own op for a message it cannot read, and the only
        // way out would be two budgets expiring. The refusal happens at the
        // second hop, inside B, and B answers it as an ordinary failure.
        let pool = pool();

        let bounce = r#"
            export default async function handler(context) {
                try {
                    const answer = await context.bindings.BACK.fetch("/back");
                    return new Response("unexpectedly reached: " + await answer.text(), { status: 200 });
                } catch (e) {
                    return new Response("threw: " + e.message, { status: 598 });
                }
            }
        "#;

        load_with_wiring(
            &pool,
            "cy-b.production",
            bounce,
            service_wiring(&[("BACK", "cy-a")], &["cy-a"]),
        )
        .await
        .expect("b loads");
        load_with_wiring(
            &pool,
            "cy-a.production",
            CALLER_CODE,
            service_wiring(&[("AUTH", "cy-b")], &["cy-b"]),
        )
        .await
        .expect("a loads");

        let started = std::time::Instant::now();
        let (status, body) = call_through(&pool, "cy-a.production", "AUTH").await;
        // B answered 598 with the refusal in its body, and A passed that body
        // through with B's status.
        assert_eq!(status, 598, "{body}");
        assert!(body.contains("is a cycle"), "{body}");
        assert!(body.contains("cy-a.production"), "{body}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "a cycle must be refused structurally, not by waiting for a budget to run out; \
             this one took {:?}",
            started.elapsed()
        );

        let _ = pool.unload_app("cy-a.production").await;
        let _ = pool.unload_app("cy-b.production").await;
    }

    #[tokio::test]
    async fn a_chain_of_service_calls_is_bounded() {
        // Five applications in a line. The first four hops are inside the
        // ceiling; the fifth is refused, because every frame in a chain is
        // another runtime thread held for as long as the chain lasts, and a
        // request that could pass through any number of them is a way for one
        // caller to occupy the node.
        let pool = pool();

        // Each link calls the next and answers with what it got, so the
        // refusal at the far end travels back through every hop.
        let link = r#"
            export default async function handler(context) {
                try {
                    const answer = await context.bindings.NEXT.fetch("/on");
                    return new Response(await answer.text(), { status: answer.status });
                } catch (e) {
                    return new Response("threw: " + e.message, { status: 596 });
                }
            }
        "#;
        let last = r#"
            export default async function handler(context) {
                return new Response("reached the end", { status: 200 });
            }
        "#;

        let names = ["dp-a", "dp-b", "dp-c", "dp-d", "dp-e"];
        // Loaded from the far end back, so a link is never loaded before the
        // one it calls; nothing depends on the order, but it keeps the test
        // from proving "not deployed here" by accident.
        load_with_wiring(
            &pool,
            "dp-e.production",
            last,
            service_wiring(&[], &["dp-d"]),
        )
        .await
        .expect("the last link loads");
        for pair in names.windows(2).rev() {
            let (here, next) = (pair[0], pair[1]);
            let callers: Vec<&str> = names
                .iter()
                .copied()
                .filter(|candidate| *candidate != here)
                .collect();
            load_with_wiring(
                &pool,
                &format!("{here}.production"),
                link,
                service_wiring(&[("NEXT", next)], &callers),
            )
            .await
            .unwrap_or_else(|e| panic!("{here} loads: {e}"));
        }

        let (status, body) = pool
            .execute_request(
                "dp-a.production",
                "http://localhost/".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await
            .map(|(status, _, body)| {
                (
                    status,
                    String::from_utf8(body.into_bytes().unwrap_or_default()).unwrap_or_default(),
                )
            })
            .expect("the first link answers");

        assert_eq!(status, 596, "the fifth hop must be refused: {body}");
        assert!(body.contains("applications deep"), "{body}");
        assert!(
            body.contains(
                "dp-a.production -> dp-b.production -> dp-c.production -> dp-d.production"
            ),
            "the refusal must print the chain it would have extended: {body}"
        );

        for name in names {
            let _ = pool.unload_app(&format!("{name}.production")).await;
        }
    }

    #[tokio::test]
    async fn a_service_call_spends_the_invocations_subrequest_allowance() {
        // A service call is fan-out and costs a runtime thread, so it comes
        // out of the same allowance an outbound socket does. With a ceiling of
        // one, the second call in an invocation is refused with nothing
        // dispatched.
        let pool = pool();

        let twice = r#"
            export default async function handler(context) {
                const first = await context.bindings.AUTH.fetch("/one");
                try {
                    await context.bindings.AUTH.fetch("/two");
                    return new Response("both went through", { status: 200 });
                } catch (e) {
                    return new Response(
                        "first=" + first.status + " second threw: " + e.message,
                        { status: 597 }
                    );
                }
            }
        "#;

        load_with_wiring(
            &pool,
            "sq-callee.production",
            CALLEE_CODE,
            service_wiring(&[], &["sq-caller"]),
        )
        .await
        .expect("the callee loads");

        let mut spec = IsolateSpec::default();
        spec.limits.max_subrequests = 1;
        service_wiring(&[("AUTH", "sq-callee")], &[]).write_to_env(&mut spec.env);
        pool.load_app("sq-caller.production".to_string(), twice.to_string(), spec)
            .await
            .expect("the caller loads");

        let (status, body) = pool
            .execute_request(
                "sq-caller.production",
                "http://localhost/".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await
            .map(|(status, _, body)| {
                (
                    status,
                    String::from_utf8(body.into_bytes().unwrap_or_default()).unwrap_or_default(),
                )
            })
            .expect("the caller answers");

        assert_eq!(status, 597, "{body}");
        assert!(body.contains("first=201"), "{body}");
        assert!(body.contains("ceiling for this application"), "{body}");

        let _ = pool.unload_app("sq-caller.production").await;
        let _ = pool.unload_app("sq-callee.production").await;
    }

    #[tokio::test]
    async fn withdrawing_consent_takes_effect_at_the_callees_next_deployment() {
        // Consent is read from the version that is serving, so a callee that
        // stops listing a caller stops being callable without anything being
        // redelivered to the caller - which is why it is checked here and not
        // resolved into the caller's deployment.
        let pool = pool();

        load_with_wiring(
            &pool,
            "wd-callee.production",
            CALLEE_CODE,
            service_wiring(&[], &["wd-caller"]),
        )
        .await
        .expect("the callee loads");
        load_with_wiring(
            &pool,
            "wd-caller.production",
            CALLER_CODE,
            service_wiring(&[("AUTH", "wd-callee")], &[]),
        )
        .await
        .expect("the caller loads");

        let (status, _) = call_through(&pool, "wd-caller.production", "AUTH").await;
        assert_eq!(status, 201);

        // The callee ships a version that consents to nobody. The caller is
        // untouched: same bundle, same deployment, same binding.
        let mut spec = IsolateSpec::default();
        service_wiring(&[], &[]).write_to_env(&mut spec.env);
        let staged = pool
            .stage_app(
                "wd-callee.production".to_string(),
                CALLEE_CODE.to_string(),
                spec,
            )
            .await
            .expect("the new version of the callee stages");
        pool.activate(staged).await.expect("it activates");

        let (status, body) = call_through(&pool, "wd-caller.production", "AUTH").await;
        assert_eq!(status, 599, "{body}");
        assert!(body.contains("callableBy"), "{body}");

        let _ = pool.unload_app("wd-caller.production").await;
        let _ = pool.unload_app("wd-callee.production").await;
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
                Vec::new(),
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
                    Vec::new(),
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
                Vec::new(),
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
                Vec::new(),
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
                Vec::new(),
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
                Vec::new(),
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
                Vec::new(),
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

    #[tokio::test]
    async fn a_debugger_attaches_to_a_running_isolate_and_it_keeps_serving() {
        // The whole feature, end to end. It had never run: the isolate was
        // built with no inspector, so the one call this path makes was an
        // `unwrap()` on `None` inside deno_core -- it panicked on the runtime
        // thread, and that thread was the application's only isolate on this
        // node.
        let pool = pool();

        let code = r#"export default function handler() { return new Response("serving"); }"#;
        let app = "inspected_app.production";
        pool.load_app(app.to_string(), code.to_string(), IsolateSpec::default())
            .await
            .expect("load");

        let (to_v8, mut from_v8) = pool
            .connect_inspector(app)
            .await
            .expect("a debugger can attach");

        to_v8
            .send(
                r#"{"id":1,"method":"Runtime.evaluate","params":{"expression":"6*7","returnByValue":true}}"#
                    .to_string(),
            )
            .expect("the session takes the message");

        // Read until the answer to this call arrives: a session may send
        // notifications of its own, and which of them appear is V8's business,
        // not this test's.
        let reply = loop {
            let message = tokio::time::timeout(Duration::from_secs(10), from_v8.recv())
                .await
                .expect("V8 answers the debugger")
                .expect("the session is still open");
            if message.contains("\"id\":1") {
                break message;
            }
        };
        assert!(
            reply.contains("\"value\":42"),
            "the debugger must be talking to the real isolate: {reply}"
        );

        // And the application it is attached to is still an application.
        let (status, _, body) = pool
            .execute_request(
                app,
                "http://localhost/".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await
            .expect("the application still serves while a debugger is attached");
        assert_eq!(status, 200);
        assert_eq!(body.into_bytes().as_deref(), Some(&b"serving"[..]));

        // Dropping the debugger's end is what ends a session; the isolate is
        // told, and must still be alive to be told.
        drop(to_v8);
        let (status, _, _) = pool
            .execute_request(
                app,
                "http://localhost/".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await
            .expect("the application still serves after the debugger leaves");
        assert_eq!(status, 200);

        let _ = pool.unload_app(app).await;
    }

    #[tokio::test]
    async fn a_panic_on_a_runtime_thread_does_not_take_the_application_off_the_node() {
        // What used to happen: the thread unwound and was gone, its channel
        // closed, and every later request, queue batch and cron tick for that
        // application answered "Runtime thread has stopped" -- indefinitely,
        // because the node went on reporting the application as running and so
        // nothing reinstalled it.
        let pool = pool();

        let code = r#"export default function handler() { return new Response("alive"); }"#;
        let app = "panicked_thread.production";
        pool.load_app(app.to_string(), code.to_string(), IsolateSpec::default())
            .await
            .expect("load");
        let (status, _, _) = pool
            .execute_request(
                app,
                "http://localhost/".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await
            .expect("it serves before the panic");
        assert_eq!(status, 200);

        pool.dispatch(
            app,
            RuntimeMessage::PanicForTest {
                app_id: app.to_string(),
            },
        )
        .await
        .expect("the thread takes the message");

        // No polling and no waiting: a runtime thread runs its messages in
        // order, so by the time this one is answered the panic before it has
        // been caught and reported.
        let error = pool
            .execute_request(
                app,
                "http://localhost/".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await
            .expect_err("the isolate that panicked is gone");
        assert!(
            matches!(
                error.downcast_ref::<DispatchError>(),
                Some(DispatchError::NotFound)
            ),
            "a lost isolate must read as an application that is not here, not as a stopped \
             thread: {error}"
        );

        // The node no longer claims to be running it, which is what makes the
        // control plane reinstall it instead of leaving it dead.
        assert!(!pool.is_hosted(app).await);

        // And the thread came back into circulation: loading the application
        // again works, and it serves.
        pool.load_app(app.to_string(), code.to_string(), IsolateSpec::default())
            .await
            .expect("the application can be loaded again after its thread panicked");
        let (status, _, body) = pool
            .execute_request(
                app,
                "http://localhost/".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await
            .expect("it serves again");
        assert_eq!(status, 200);
        assert_eq!(body.into_bytes().as_deref(), Some(&b"alive"[..]));

        let _ = pool.unload_app(app).await;
    }

    /// One inbound request that passes through two applications is one
    /// invocation to everyone outside the platform, and has to be one trace.
    ///
    /// The callee's isolate is on another runtime thread with its own log
    /// stream and its own trace records, so the id has to travel with the
    /// call. It travels as data on the message, beside the chain and for the
    /// same reason: an application that could write it could file its lines
    /// under somebody else's request.
    ///
    /// Asserted on the trace records rather than on the log, because these
    /// two isolates are on threads of their own and a thread-local subscriber
    /// would see neither. The records are process-wide, which is what makes
    /// them the right witness here.
    #[tokio::test]
    async fn a_service_call_continues_the_callers_trace_rather_than_starting_one() {
        let pool = pool();

        load_with_wiring(
            &pool,
            "trace-callee.production",
            CALLEE_CODE,
            service_wiring(&[], &["trace-caller"]),
        )
        .await
        .expect("the callee loads");
        load_with_wiring(
            &pool,
            "trace-caller.production",
            CALLER_CODE,
            service_wiring(&[("AUTH", "trace-callee")], &[]),
        )
        .await
        .expect("the caller loads");

        let (status, body) = call_through(&pool, "trace-caller.production", "AUTH").await;
        assert_eq!(status, 201, "{body}");

        let caller = one_trace_of("trace-caller.production");
        let callee = one_trace_of("trace-callee.production");
        assert!(
            !caller.trace_id.is_empty(),
            "the request that arrived from outside has to be given an id"
        );
        assert_eq!(
            callee.trace_id, caller.trace_id,
            "a callee that started its own trace would leave half of one request unfindable: \
             `deis logs --trace-id` on the id `deis trace` printed would stop at the first hop"
        );

        let _ = pool.unload_app("trace-caller.production").await;
        let _ = pool.unload_app("trace-callee.production").await;
    }

    /// The one invocation a tenant recorded, taken out of the process-wide
    /// recorder with everything else put back.
    ///
    /// These tests share the recorder, so a test that swallowed another's
    /// records would make that other test's failure look like this one's.
    fn one_trace_of(tenant: &str) -> super::super::metrics::InvocationTrace {
        let recorded = super::super::metrics::recorder().traces().drain(usize::MAX);
        let (mut mine, others): (Vec<_>, Vec<_>) = recorded
            .into_iter()
            .partition(|trace| trace.tenant == tenant);
        if !others.is_empty() {
            super::super::metrics::recorder().traces().requeue(others);
        }
        assert_eq!(
            mine.len(),
            1,
            "{tenant} was invoked once and must have one record"
        );
        mine.remove(0)
    }

    /// The other half of the same guarantee, on the other half of the loop.
    ///
    /// A runtime thread waits on two things: a message, and the producers of
    /// any response body the isolate is still writing. The panic guard was put
    /// around the first and not the second, so a handler that panicked
    /// mid-body still ended the thread for good -- and the application it was
    /// hosting answered "Runtime thread has stopped" to every later request,
    /// queue batch and cron tick, exactly the failure the guard was added to
    /// remove.
    ///
    /// The panic is injected rather than provoked out of V8, because the shape
    /// under test is where the unwind passes through and not what raised it: a
    /// panicking op reaches this loop through the same arm of the same
    /// `select!`. What the test asserts is what an operator would see -- the
    /// application comes back, and the thread is still there to put it on.
    #[tokio::test]
    async fn a_panic_while_producing_a_response_body_does_not_take_the_thread_with_it() {
        let pool = pool();

        // Endless on purpose: the stream has to still be open when the panic
        // happens, or the thread would be waiting on messages like any other
        // and the arm under test would never be entered. The producer parks on
        // a full queue after two chunks, which is what leaves the thread free
        // to take the message below.
        let code = r#"
            export default function handler() {
              const stream = new ReadableStream({
                pull(controller) { controller.enqueue("x".repeat(8)); },
              });
              return new Response(stream);
            }
        "#;
        let app = "panicked_pump.production";
        pool.load_app(app.to_string(), code.to_string(), IsolateSpec::default())
            .await
            .expect("load");

        let (status, _, body) = pool
            .execute_request(
                app,
                "http://localhost/stream".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await
            .expect("the handler answers with a body it is still producing");
        assert_eq!(status, 200);
        let ResponseBody::Stream(mut reader) = body else {
            panic!("the handler answered with a stream");
        };

        pool.dispatch(app, RuntimeMessage::PanicInPumpForTest)
            .await
            .expect("the thread takes the message");

        // The client's read is what witnesses the panic, and it is a real
        // condition rather than a wait: the body can only end here because the
        // isolate producing it was dropped, and nothing else in this test
        // closes it. Reading to the end also orders everything below after the
        // panic has been caught and reported.
        while std::future::poll_fn(|cx| reader.poll_chunk(cx))
            .await
            .is_some()
        {}
        assert!(
            reader.failure().is_some(),
            "a body cut off by a panic must fail its client's read rather than look complete"
        );

        // The distinguishing assertion. Before the fix this said "Runtime
        // thread has stopped": the thread had unwound and its queue was
        // closed, so nothing could ever be dispatched to it again.
        let error = pool
            .execute_request(
                app,
                "http://localhost/".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await
            .expect_err("the isolate that panicked is gone");
        assert!(
            matches!(
                error.downcast_ref::<DispatchError>(),
                Some(DispatchError::NotFound)
            ),
            "a lost isolate must read as an application that is not here, not as a stopped \
             thread: {error}"
        );
        assert!(!pool.is_hosted(app).await);

        // And the thread came back into circulation.
        let plain = r#"export default function handler() { return new Response("alive"); }"#;
        pool.load_app(app.to_string(), plain.to_string(), IsolateSpec::default())
            .await
            .expect("the application can be loaded again after its pump panicked");
        let (status, _, body) = pool
            .execute_request(
                app,
                "http://localhost/".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await
            .expect("it serves again");
        assert_eq!(status, 200);
        assert_eq!(body.into_bytes().as_deref(), Some(&b"alive"[..]));

        let _ = pool.unload_app(app).await;
    }

    #[tokio::test]
    async fn a_caller_cannot_dress_a_service_call_in_the_platform_s_own_headers() {
        // `x-deis-caller` was written over the caller's value and the rest of
        // the namespace was not, so a caller could hand its callee
        // `x-deis-tenant` and `x-deis-storage-token` -- the pair that names
        // and authorises a storage tenant one hop further in -- and they
        // arrived looking exactly like something this node had stamped.
        let pool = pool();

        let mirror = r#"
            export default function handler(context) {
                const seen = [];
                for (const [name, value] of context.request.headers.entries()) {
                    const lower = name.toLowerCase();
                    if (lower.startsWith("x-deis-") || lower.startsWith("x-shop-")) {
                        seen.push(lower + "=" + value);
                    }
                }
                seen.sort();
                return new Response(seen.join("\n"), { status: 200 });
            }
        "#;
        let forger = r#"
            export default async function handler(context) {
                const answer = await context.bindings.AUTH.fetch("/probe", {
                    headers: {
                        "x-deis-caller": "admin-app.production",
                        "x-deis-tenant": "victim.production",
                        "x-deis-storage-token": "forged",
                        "x-deis-not-invented-yet": "whatever",
                        "X-Deis-Caller-Mixed-Case": "second-try",
                        "x-shop-request-id": "abc123",
                    },
                });
                return new Response(await answer.text(), { status: answer.status });
            }
        "#;

        load_with_wiring(
            &pool,
            "hdr-callee.production",
            mirror,
            service_wiring(&[], &["hdr-caller"]),
        )
        .await
        .expect("the callee loads");
        load_with_wiring(
            &pool,
            "hdr-caller.production",
            forger,
            service_wiring(&[("AUTH", "hdr-callee")], &[]),
        )
        .await
        .expect("the caller loads");

        let (status, body) = call_through(&pool, "hdr-caller.production", "AUTH").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body, "x-deis-caller=hdr-caller.production\nx-shop-request-id=abc123",
            "the only platform header a callee may see is the one this node wrote, and a \
             header of the application's own must still cross"
        );

        let _ = pool.unload_app("hdr-caller.production").await;
        let _ = pool.unload_app("hdr-callee.production").await;
    }

    #[tokio::test]
    async fn a_service_call_carries_every_value_of_a_repeated_header() {
        // The shape a caller actually writes is
        // `bindings.AUTH.fetch(context.request)`: the call is a hop of the
        // request it is serving and carries that request's headers. Those
        // crossed as a map, so a caller forwarding a request with two `Cookie`
        // lines delivered one of them -- the same defect as on the inbound
        // path, one hop further in. See `runtime/header_list.rs`.
        //
        // The request forwarded here is a GET with no body, which is also what
        // pins the other half: reading a body off it yields a zero-length
        // buffer, and a callee handed one used to fail before it ran with
        // "Request with GET/HEAD method cannot have body".
        let pool = pool();

        let mirror = r#"
            export default function handler(context) {
                return new Response(JSON.stringify({
                    cookie: context.request.headers.get("cookie"),
                    accept: context.request.headers.get("accept"),
                    auth: context.request.headers.get("authorization"),
                }), { status: 200 });
            }
        "#;
        // The `authorization` here is the init overriding the request's own,
        // which is what `fetch` does: one credential reaches the callee, not
        // the caller's and the init's joined together.
        let caller = r#"
            export default async function handler(context) {
                const outbound = new Request("https://ignored.invalid/probe", {
                    headers: [
                        ["cookie", "session=abc"],
                        ["cookie", "csrf=xyz"],
                        ["accept", "text/html"],
                        ["accept", "application/json"],
                        ["authorization", "Bearer caller"],
                    ],
                });
                const answer = await context.bindings.AUTH.fetch(outbound, {
                    headers: { authorization: "Bearer init" },
                });
                return new Response(await answer.text(), { status: answer.status });
            }
        "#;

        load_with_wiring(
            &pool,
            "repeat-callee.production",
            mirror,
            service_wiring(&[], &["repeat-caller"]),
        )
        .await
        .expect("the callee loads");
        load_with_wiring(
            &pool,
            "repeat-caller.production",
            caller,
            service_wiring(&[("AUTH", "repeat-callee")], &[]),
        )
        .await
        .expect("the caller loads");

        let (status, body) = call_through(&pool, "repeat-caller.production", "AUTH").await;
        assert_eq!(status, 200, "{body}");
        let seen: serde_json::Value = serde_json::from_str(&body).expect("the callee answers JSON");
        assert_eq!(
            seen["cookie"], "session=abc, csrf=xyz",
            "a cookie was dropped on the way to the callee"
        );
        assert_eq!(seen["accept"], "text/html, application/json");
        assert_eq!(
            seen["auth"], "Bearer init",
            "an init header replaces the request's rather than being added beside it"
        );

        let _ = pool.unload_app("repeat-caller.production").await;
        let _ = pool.unload_app("repeat-callee.production").await;
    }

    /// Post a request straight to the thread hosting `app`, the way
    /// [`RuntimePool::execute_request`] does, and answer the caller's half of
    /// the reply channel.
    ///
    /// Going around the public entry point is what lets a test say when the
    /// message was posted: [`RuntimeMessage::ExecuteRequest`] carries that
    /// instant, and a test that needs a message to have waited a long time
    /// cannot spend it.
    async fn post_request(
        pool: &RuntimePool,
        app: &str,
        enqueued_at: Instant,
    ) -> oneshot::Receiver<Result<FetchResult>> {
        let (tx, rx) = oneshot::channel();
        let placement = pool.placement.read().await;
        let id = *placement
            .of_app
            .get(app)
            .expect("the application is loaded on some thread");
        placement.threads[id]
            .send(RuntimeMessage::ExecuteRequest {
                app_id: app.to_string(),
                url: "http://localhost/".to_string(),
                method: "GET".to_string(),
                headers: Vec::new(),
                body: None,
                enqueued_at,
                response: tx,
            })
            .expect("the thread takes the message");
        rx
    }

    /// A request whose caller has gone must not be run.
    ///
    /// This is what a client hanging up, or the edge's own read timeout
    /// firing, leaves behind: the reply channel is dropped while the request
    /// waits behind whatever the application's runtime thread is already
    /// running, and the thread has to find that out before it enters the
    /// isolate.
    #[tokio::test]
    async fn a_request_whose_caller_has_gone_is_not_run() {
        let pool = pool();
        let app = "r4-abandoned.production";
        let code = r#"
            export default function handler() {
                return new Response("ran");
            }
        "#;
        pool.load_app(app.to_string(), code.to_string(), IsolateSpec::default())
            .await
            .expect("the application loads");

        let requests_before = recorder().value(REQUESTS_TOTAL, app);
        let abandoned_before = recorder().value(ABANDONED_BEFORE_START_TOTAL, app);

        // Posted, then abandoned before the thread reaches it.
        drop(post_request(&pool, app, Instant::now()).await);

        // The same thread handles this one after it, so once this answers, the
        // first has already been decided.
        let answer = post_request(&pool, app, Instant::now()).await;
        let (status, _, _) = answer
            .await
            .expect("the thread answers the second request")
            .expect("the second request runs");
        assert_eq!(status, 200);

        assert_eq!(
            recorder().value(REQUESTS_TOTAL, app),
            requests_before + 1.0,
            "only the second request may have entered the isolate"
        );
        assert_eq!(
            recorder().value(ABANDONED_BEFORE_START_TOTAL, app),
            abandoned_before + 1.0,
            "the abandoned request must be counted as dropped before it started"
        );

        let _ = pool.unload_app(app).await;
    }

    /// A request that waited past the admission budget is refused as
    /// overloaded rather than started.
    #[tokio::test]
    async fn a_request_that_waited_past_the_admission_budget_is_refused_as_overloaded() {
        let pool = pool();
        let app = "r4-expired.production";
        let code = r#"
            export default function handler() {
                return new Response("ran");
            }
        "#;
        pool.load_app(app.to_string(), code.to_string(), IsolateSpec::default())
            .await
            .expect("the application loads");

        let requests_before = recorder().value(REQUESTS_TOTAL, app);

        pool.set_admission_wait_ms(50);
        // Posted as though it had been waiting half a second, which is the
        // whole subject of the check and the one thing a test cannot spend.
        let answer = post_request(&pool, app, Instant::now() - Duration::from_millis(500)).await;
        let answered = answer
            .await
            .expect("the thread answers rather than dropping the message");
        // Put the budget back before asserting, so a failure here cannot leak
        // into the next test of this module.
        pool.set_admission_wait_ms(crate::config::default_admission_wait_ms());

        let error = answered.expect_err("a request past the budget must be refused");
        assert!(
            matches!(
                error.downcast_ref::<DispatchError>(),
                Some(DispatchError::Overloaded { .. })
            ),
            "expected an overloaded refusal, got: {error}"
        );
        assert_eq!(
            recorder().value(REQUESTS_TOTAL, app),
            requests_before,
            "a refused request must not have entered the isolate"
        );

        let _ = pool.unload_app(app).await;
    }

    /// A full queue is the same refusal as a spent admission budget, from
    /// the other side of the queue: the work is turned away at the moment it
    /// is posted rather than when the thread reaches it, and the caller gets
    /// the same named answer either way.
    #[tokio::test]
    async fn a_request_posted_to_a_full_queue_is_refused_as_overloaded() {
        let pool = pool();
        let app = "r4-fullqueue.production";
        // Holds the thread for as long as the flood below takes, so the
        // queue it fills cannot drain in the middle of the test.
        let code = r#"
            export default function handler() {
                return new Promise((resolve) =>
                    setTimeout(() => resolve(new Response("ran")), 200));
            }
        "#;
        pool.load_app(app.to_string(), code.to_string(), IsolateSpec::default())
            .await
            .expect("the application loads");

        // Put the thread to work; from here on it cannot take another
        // message off its queue until this request finishes.
        let first = post_request(&pool, app, Instant::now()).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Fill the queue. The reply channel of every message posted is
        // dropped, so what the thread reaches afterwards is work whose
        // caller has gone and it throws it away rather than running it.
        let placement = pool.placement.read().await;
        let id = *placement
            .of_app
            .get(app)
            .expect("the application is placed");
        let mut refused = None;
        for _ in 0..200 {
            let (tx, _rx) = oneshot::channel();
            match placement.threads[id].send(RuntimeMessage::ExecuteRequest {
                app_id: app.to_string(),
                url: "http://localhost/".to_string(),
                method: "GET".to_string(),
                headers: Vec::new(),
                body: None,
                enqueued_at: Instant::now(),
                response: tx,
            }) {
                Ok(()) => {}
                Err(e) => {
                    refused = Some(e);
                    break;
                }
            }
        }
        drop(placement);

        let error = refused.expect("a queue of 100 cannot hold 200 more requests");
        assert!(
            matches!(
                error.downcast_ref::<DispatchError>(),
                Some(DispatchError::Overloaded { .. })
            ),
            "expected an overloaded refusal, got: {error}"
        );

        // The work the thread actually took still ran to the end.
        let (status, _, _) = first
            .await
            .expect("the thread answers the request it took")
            .expect("the first request runs");
        assert_eq!(status, 200);

        // The queue is still full, so the UnloadApp message cannot get in
        // yet: in front of it sit the dropped posts. The probe below holds
        // its reply channel, so it is run rather than dropped, and it is
        // answered only once everything ahead of it is gone -- after which
        // the unload message fits.
        let probe = 'probe: loop {
            let (tx, rx) = oneshot::channel();
            let placement = pool.placement.read().await;
            let id = *placement
                .of_app
                .get(app)
                .expect("the application is still placed");
            let accepted = placement.threads[id]
                .send(RuntimeMessage::ExecuteRequest {
                    app_id: app.to_string(),
                    url: "http://localhost/".to_string(),
                    method: "GET".to_string(),
                    headers: Vec::new(),
                    body: None,
                    enqueued_at: Instant::now(),
                    response: tx,
                })
                .is_ok();
            drop(placement);
            if accepted {
                break 'probe rx;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let (status, _, _) = probe
            .await
            .expect("the thread answers the probe")
            .expect("the probe runs");
        assert_eq!(status, 200);

        pool.unload_app(app).await.expect("the application unloads");
    }

    /// A generation's slot holds a bare sender rather than a runtime thread,
    /// so the same full-queue refusal is given by the dispatch rather than by
    /// the thread, and must give the same answer.
    #[tokio::test]
    async fn a_generation_whose_queue_is_full_is_refused_as_overloaded() {
        let pool = pool();
        let app = "r4-gen-full.production";
        let code = r#"
            export default function handler() {
                return new Promise((resolve) =>
                    setTimeout(() => resolve(new Response("ran")), 200));
            }
        "#;
        pool.load_app(app.to_string(), code.to_string(), IsolateSpec::default())
            .await
            .expect("the application loads");
        let staged = pool
            .stage_app(app.to_string(), code.to_string(), IsolateSpec::default())
            .await
            .expect("the replacement stages");
        pool.prepare_generation(staged, "gen-1".to_string())
            .await
            .expect("the generation is prepared");

        // Put the generation's thread to work, through the slot's own
        // sender: that is the channel the dispatch below will fill.
        let (tx, first) = oneshot::channel();
        {
            let placement = pool.placement.read().await;
            let slot = placement
                .generations
                .get(&(app.to_string(), "gen-1".to_string()))
                .expect("the generation is prepared")
                .clone();
            slot.sender
                .try_send(RuntimeMessage::ExecuteRequest {
                    app_id: app.to_string(),
                    url: "http://localhost/".to_string(),
                    method: "GET".to_string(),
                    headers: Vec::new(),
                    body: None,
                    enqueued_at: Instant::now(),
                    response: tx,
                })
                .expect("the generation's thread takes the request");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut refused = None;
        for _ in 0..200 {
            let (tx, _rx) = oneshot::channel();
            match pool
                .dispatch_generation(
                    app,
                    "gen-1",
                    RuntimeMessage::ExecuteRequest {
                        app_id: app.to_string(),
                        url: "http://localhost/".to_string(),
                        method: "GET".to_string(),
                        headers: Vec::new(),
                        body: None,
                        enqueued_at: Instant::now(),
                        response: tx,
                    },
                )
                .await
            {
                Ok(()) => {}
                Err(e) => {
                    refused = Some(e);
                    break;
                }
            }
        }

        let error = refused.expect("a queue of 100 cannot hold 200 more requests");
        assert!(
            matches!(
                error.downcast_ref::<DispatchError>(),
                Some(DispatchError::Overloaded { .. })
            ),
            "expected an overloaded refusal, got: {error}"
        );

        let (status, _, _) = first
            .await
            .expect("the thread answers the request it took")
            .expect("the first request runs");
        assert_eq!(status, 200);

        pool.discard_generation(app, "gen-1")
            .await
            .expect("the generation is discarded");
        let _ = pool.unload_app(app).await;
    }

    /// The two placement refusals that never reach a queue: the application
    /// is not on this node, and the generation asked for is not prepared
    /// here. Each is a named error rather than a sentence to parse, and
    /// downcasts to it from the `Result` the dispatch hands back.
    #[tokio::test]
    async fn placement_refusals_are_named_errors() {
        let pool = pool();

        let error = pool
            .execute_request(
                "r4-ghost.production",
                "http://localhost/".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await
            .expect_err("the application is not here");
        assert!(
            matches!(
                error.downcast_ref::<DispatchError>(),
                Some(DispatchError::NotFound)
            ),
            "expected an application that is not hosted here, got: {error}"
        );

        let code = r#"export default function handler() { return new Response("ok"); }"#;
        let app = "r4-gen-missing.production";
        pool.load_app(app.to_string(), code.to_string(), IsolateSpec::default())
            .await
            .expect("the application loads");
        let error = pool
            .execute_request_generation(
                app,
                "a-generation-never-prepared",
                "http://localhost/".to_string(),
                "GET".to_string(),
                Vec::new(),
                None,
            )
            .await
            .expect_err("the generation is not prepared");
        assert!(
            matches!(
                error.downcast_ref::<DispatchError>(),
                Some(DispatchError::GenerationNotPrepared)
            ),
            "expected a generation that is not prepared here, got: {error}"
        );

        let _ = pool.unload_app(app).await;
    }

    /// The refusal sentences are read by callers and operators alike, so a
    /// line wrapped without its continuation once broke one: the error
    /// carried a run of spaces inside the sentence, and nothing said so. A
    /// refusal that cannot be quoted verbatim cannot be matched against,
    /// so the refusal the code produces is checked for it, not the one a
    /// test happens to type.
    #[test]
    fn dispatch_refusal_messages_carry_no_accidental_spaces_in_their_middle() {
        for message in [
            DispatchError::Overloaded {
                retry_after: Duration::from_secs(1),
            }
            .to_string(),
            DispatchError::Stopped.to_string(),
            DispatchError::NotFound.to_string(),
            DispatchError::GenerationNotPrepared.to_string(),
        ] {
            assert!(
                !message.contains("  "),
                "a refusal message carries a run of spaces in its middle: {message:?}"
            );
        }
    }

    /// Where the admission boundary is, and which question is asked first.
    #[test]
    fn the_admission_budget_refuses_only_what_waited_longer_than_it() {
        let budget = Duration::from_millis(100);
        assert_eq!(
            admission(Duration::from_millis(99), budget, false),
            Admission::Run
        );
        assert_eq!(
            admission(budget, budget, false),
            Admission::Run,
            "exactly the budget is still inside it"
        );
        assert_eq!(
            admission(Duration::from_millis(101), budget, false),
            Admission::Expired
        );
        // A caller that has gone is owed no answer, however long it waited:
        // asking the budget question first would write a reply onto a channel
        // nobody is holding.
        assert_eq!(
            admission(Duration::from_secs(60), Duration::from_millis(1), true),
            Admission::Abandoned
        );
    }

    /// One message leased until `lease_expires_at`.
    fn message_leased_until(id: &str, lease_expires_at: i64) -> LeasedMessage {
        LeasedMessage {
            id: id.to_string(),
            body: Vec::new(),
            attempts: 1,
            lease_expires_at,
            created_at: 0,
            lease_token: format!("lease-of-{id}"),
        }
    }

    /// A queue batch is held to the shorter of the admission budget and what
    /// is left of its lease.
    #[test]
    fn a_queue_batch_waits_no_longer_than_its_lease_or_the_admission_budget() {
        let now = 1_000;
        let admission_budget = Duration::from_secs(10);

        // Plenty of lease left: the admission budget is the binding one.
        let long_lease = [message_leased_until("m1", now + 60)];
        assert_eq!(
            batch_admission_budget(admission_budget, &long_lease, now),
            admission_budget
        );

        // The lease now runs out sooner than the admission budget would allow.
        let short_lease = [message_leased_until("m2", now + 3)];
        assert_eq!(
            batch_admission_budget(admission_budget, &short_lease, now),
            Duration::from_secs(3)
        );

        // The earliest expiry in the batch is the one that counts.
        let mixed = [
            message_leased_until("m3", now + 60),
            message_leased_until("m4", now + 2),
        ];
        assert_eq!(
            batch_admission_budget(admission_budget, &mixed, now),
            Duration::from_secs(2)
        );

        // Already lapsed: no budget of its own, so nothing may start.
        let lapsed = [message_leased_until("m5", now - 5)];
        assert_eq!(
            batch_admission_budget(admission_budget, &lapsed, now),
            Duration::ZERO
        );

        // No expiry at all is a test double's shape, not a server's; the
        // admission budget is then all there is to go on.
        let no_expiry = [message_leased_until("m6", 0)];
        assert_eq!(
            batch_admission_budget(admission_budget, &no_expiry, now),
            admission_budget
        );
    }
}
