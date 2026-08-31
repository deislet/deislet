//! Response bodies that are produced inside the isolate and written to the
//! client as they are produced.
//!
//! # What this is for
//!
//! Everything else in this runtime hands a response body across the
//! Rust/JavaScript boundary as one finished `Vec<u8>`: `js/runtime.js` reads
//! `await resp.arrayBuffer()` and `super::ops::PendingResponseBody` carries the
//! result out. That is the right shape for the overwhelming majority of
//! answers -- a string, a JSON document, a rendered page -- and it is left
//! exactly as it was.
//!
//! It is the wrong shape for three things, and all three were broken:
//!
//!   * **Server-sent events could not be written at all.** An SSE response body
//!     never ends; a handler that writes frames forever never returns, so the
//!     invocation hit its execution deadline and the client got nothing. That
//!     is how a progress feed, a log viewer and a streaming model proxy are all
//!     built.
//!   * **Proxying a large upstream doubled the memory.** `return fetch(url)`
//!     read the whole object into the isolate's heap, and the heap has a 128 MB
//!     ceiling by default, so the application was killed for passing through a
//!     file it never needed to hold.
//!   * **Time to first byte was the whole handler's duration**, even when the
//!     first byte had been ready from the start.
//!
//! # The shape
//!
//! A stream is a queue of at most [`QUEUED_CHUNKS`] chunks between the isolate
//! and the node's HTTP response, plus a deadline. The application's producer
//! runs in the isolate and calls [`op_deis_stream_write`], which is an *async*
//! op: it does not resolve until the queue has room. That is the whole of the
//! backpressure story -- a slow client leaves the producer parked inside its
//! own `await`, consuming no CPU and holding no chunk beyond the two the queue
//! allows.
//!
//! The receiving end is [`ResponseBody::Stream`], which `dispatcher/handlers.rs`
//! turns into an axum body. Dropping it -- which is what a client hanging up
//! does -- makes the next write fail, and `js/runtime.js` then cancels the
//! producer.
//!
//! # What a stream does *not* own
//!
//! It does not own the isolate. `runtime/pool.rs` drives open streams from the
//! same message loop that serves requests, so a second request to the same
//! application is answered while a stream is open rather than queued behind
//! it. See `AppRuntime::pump_response_streams`.

use bytes::Bytes;
use deno_core::{extension, op2, JsBuffer, OpState};
use deno_error::JsErrorBox;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Instant;
use tokio::sync::mpsc;

/// How many produced-but-unsent chunks the node holds per open stream.
///
/// This is where the memory bound is. Two rather than one so that the producer
/// may run one chunk ahead of the wire -- with a queue of one it would have to
/// wait for each chunk to be handed to the socket before starting the next,
/// which halves throughput for no safety -- and two rather than more because
/// every extra slot is a chunk of the application's choosing that the node is
/// holding on its behalf while the client is behind.
///
/// The chunk *sizes* are the application's own and are not bounded here. They
/// do not need to be: a chunk exists in the isolate's heap before it is
/// written, so it is already held to `memory_limit_mb`, and what this queue
/// adds is at most two copies of it. What is deliberately not on offer is an
/// unbounded queue, which would move the memory problem from the isolate to
/// the node rather than solving it.
const QUEUED_CHUNKS: usize = 2;

/// The body of one answered request, as it crosses from the isolate to the
/// node's HTTP entry.
///
/// The first two variants are what this boundary has always carried, named:
/// "no body at all" and "the whole body" are different answers -- a 204, a 304
/// and the answer to a HEAD carry no body, which is not the same as a body of
/// zero length -- and they used to be spelled `Option<Vec<u8>>`.
pub enum ResponseBody {
    /// The handler answered without a body: a 204, a 304, the answer to a HEAD,
    /// a WebSocket upgrade.
    None,

    /// The whole body, already in the node's memory. The simple road, and the
    /// one nearly every response takes.
    Bytes(Vec<u8>),

    /// The body is still being produced inside the isolate. Reading this to the
    /// end is what pulls the producer along; dropping it tells the producer the
    /// client is gone.
    Stream(StreamReader),
}

impl ResponseBody {
    /// The whole body when there is one in memory, for the callers that only
    /// ever deal in finished bodies -- the tests, and every path that predates
    /// streaming.
    ///
    /// A stream answers `None` rather than blocking: collecting it here would
    /// be exactly the behaviour this module exists to remove.
    pub fn into_bytes(self) -> Option<Vec<u8>> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            Self::None | Self::Stream(_) => None,
        }
    }
}

impl std::fmt::Debug for ResponseBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "ResponseBody::None"),
            Self::Bytes(bytes) => write!(f, "ResponseBody::Bytes({} bytes)", bytes.len()),
            Self::Stream(_) => write!(f, "ResponseBody::Stream"),
        }
    }
}

/// The reading end of one open stream, as `dispatcher/handlers.rs` sees it.
///
/// Two parts, because a body that ends and a body that *fails* must not look
/// the same to the client and the failure must never have to queue behind the
/// bytes. The chunks come down the channel; the failure is written beside it
/// and read once the channel has drained.
pub struct StreamReader {
    chunks: mpsc::Receiver<Bytes>,
    failure: Arc<Mutex<Option<String>>>,
}

impl StreamReader {
    /// The next chunk of the body, or `None` once the producer is done.
    ///
    /// A poll rather than an `async fn` because the caller is an HTTP body,
    /// which is polled and not awaited: `dispatcher/handlers.rs` needs to look
    /// at [`Self::failure`] at the exact moment this answers `None`, and
    /// borrowing the reader again inside its own future would not be possible.
    pub fn poll_chunk(&mut self, cx: &mut std::task::Context<'_>) -> Poll<Option<Bytes>> {
        self.chunks.poll_recv(cx)
    }

    /// Why the producer stopped, when it stopped badly.
    ///
    /// Only meaningful once [`Self::next_chunk`] has answered `None`: the
    /// producer writes this before dropping its end, and the channel reports
    /// the end only after every sender is gone, so a reader that has seen the
    /// end has seen this too.
    pub fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// One open response stream, from the node's side.
struct OpenStream {
    /// Where produced chunks go. Bounded; see [`QUEUED_CHUNKS`].
    sink: mpsc::Sender<Bytes>,

    /// Where the producer records why it stopped, when it stopped badly.
    ///
    /// Beside the queue rather than in it, so that reporting a failure can
    /// never itself have to wait for a slow client: a full queue is exactly
    /// the state a stream is in when the client has stopped reading, which is
    /// also when a producer is most likely to give up.
    failure: Arc<Mutex<Option<String>>>,

    /// When this stream runs out of time.
    ///
    /// Enforced by the node closing the stream, not by terminating the isolate:
    /// a stream shares its isolate with every other request to the same
    /// application, and killing the isolate to end one stream would take those
    /// with it. See `AppRuntime::pump_response_streams`.
    deadline: Instant,

    /// When the head went out, so the log line at the end can say how long the
    /// stream lived rather than only when it died.
    opened_at: Instant,

    /// What has actually reached the client's side of the queue, for the same
    /// log line. A stream that produced nothing and one that produced ten
    /// megabytes fail in the same words otherwise.
    chunks: u64,
    bytes: u64,
}

/// Every response stream one isolate has open.
///
/// Several at once is the ordinary case, not the exotic one: an application
/// serving server-sent events has one open per connected browser.
#[derive(Default)]
pub struct ResponseStreams {
    next_id: u32,
    open: HashMap<u32, OpenStream>,
}

/// What one finished stream cost, so the caller can say so in one line.
#[derive(Debug, Clone, Copy)]
pub struct StreamTally {
    pub chunks: u64,
    pub bytes: u64,
    pub lived: std::time::Duration,
}

impl ResponseStreams {
    /// Open a stream and answer its id and the receiving end of its body.
    ///
    /// The id is what JavaScript names this stream by; it is only ever used
    /// inside one isolate, so a `u32` that wraps after four billion streams is
    /// wide enough, and the wrap cannot collide with a live stream unless four
    /// billion are open at once.
    pub fn open(&mut self, deadline: Instant) -> (u32, StreamReader) {
        let (sink, chunks) = mpsc::channel(QUEUED_CHUNKS);
        let failure = Arc::new(Mutex::new(None));
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        self.open.insert(
            id,
            OpenStream {
                sink,
                failure: failure.clone(),
                deadline,
                opened_at: Instant::now(),
                chunks: 0,
                bytes: 0,
            },
        );
        (id, StreamReader { chunks, failure })
    }

    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    pub fn len(&self) -> usize {
        self.open.len()
    }

    /// The instant the first of the open streams runs out, or `None` when
    /// none are open.
    pub fn earliest_deadline(&self) -> Option<Instant> {
        self.open.values().map(|stream| stream.deadline).min()
    }

    /// The streams whose time is up as of `now`.
    pub fn expired(&self, now: Instant) -> Vec<u32> {
        self.open
            .iter()
            .filter(|(_, stream)| now >= stream.deadline)
            .map(|(id, _)| *id)
            .collect()
    }

    /// The streams whose reader has gone -- the client hung up, or the node
    /// stopped reading the body for its own reasons.
    pub fn abandoned(&self) -> Vec<u32> {
        self.open
            .iter()
            .filter(|(_, stream)| stream.sink.is_closed())
            .map(|(id, _)| *id)
            .collect()
    }

    /// A handle on every open stream's sink, for waiting on the first client
    /// to hang up. Clones, because the wait outlives the borrow of this map.
    pub fn sinks(&self) -> Vec<mpsc::Sender<Bytes>> {
        self.open
            .values()
            .map(|stream| stream.sink.clone())
            .collect()
    }

    /// Forget one stream, having first recorded why it is ending badly.
    ///
    /// The client is reading a response that was answered 200 and is now going
    /// to stop short. Recording the reason before the entry goes is what makes
    /// its read *fail* rather than succeed on a truncated body; see
    /// [`StreamReader::failure`].
    pub fn close_with_failure(&mut self, id: u32, message: String) -> Option<StreamTally> {
        self.record_failure(id, message);
        self.close(id)
    }

    /// Forget one stream, and say what it had produced.
    ///
    /// Dropping the entry drops the last sender, which ends the client's body:
    /// closing a stream *is* removing it from this map.
    pub fn close(&mut self, id: u32) -> Option<StreamTally> {
        self.open.remove(&id).map(|stream| StreamTally {
            chunks: stream.chunks,
            bytes: stream.bytes,
            lived: stream.opened_at.elapsed(),
        })
    }

    /// Every open stream's id, oldest first is not promised -- the caller uses
    /// this to close them all when the isolate is going away.
    pub fn ids(&self) -> Vec<u32> {
        self.open.keys().copied().collect()
    }

    fn sink_of(&self, id: u32) -> Option<mpsc::Sender<Bytes>> {
        self.open.get(&id).map(|stream| stream.sink.clone())
    }

    /// Record why a stream stopped badly, so the reader can turn the end of the
    /// body into a failure rather than a clean finish.
    fn record_failure(&self, id: u32, message: String) {
        if let Some(stream) = self.open.get(&id) {
            *stream
                .failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message);
        }
    }

    fn record_written(&mut self, id: u32, bytes: usize) {
        if let Some(stream) = self.open.get_mut(&id) {
            stream.chunks += 1;
            stream.bytes += bytes as u64;
        }
    }
}

/// This isolate's open streams, created on first use.
///
/// Installed lazily rather than in the extension's state closure for the
/// reason `runtime/ws_client_ops.rs` gives about its own state: an op that
/// panicked reaching for absent state would panic inside a V8 callback, which
/// cannot unwind, and would take the node down with every application on it.
pub fn response_streams(state: &mut OpState) -> Rc<RefCell<ResponseStreams>> {
    if let Some(existing) = state.try_borrow::<Rc<RefCell<ResponseStreams>>>() {
        return existing.clone();
    }
    let created = Rc::new(RefCell::new(ResponseStreams::default()));
    state.put(created.clone());
    created
}

/// The error a producer is given when the stream it is writing to is gone.
///
/// One message for both ways that happens -- the client hung up, or the node
/// closed the stream because it ran out of time -- because from inside the
/// isolate they are the same fact: there is nobody to write to any more.
/// `js/runtime.js` catches it and stops the producer rather than letting it
/// surface as an unhandled rejection.
fn stream_closed(id: u32) -> JsErrorBox {
    JsErrorBox::generic(format!(
        "the response stream {id} is closed: the client went away, or the stream ran past this \
         node's stream_timeout_ms. Nothing more can be written to it"
    ))
}

/// Internal op: write the next chunk of a streaming response body.
///
/// Async, and that is the whole of the backpressure: the returned promise does
/// not settle until the node's queue for this stream has room, which means
/// until the client has taken what is already there. A producer awaiting this
/// op is parked -- it holds no CPU, and the only budget it spends is the
/// stream's own wall clock.
///
/// Rejecting means the stream is gone and the producer must stop. It is not a
/// failure of the response: the head went out long ago and the client either
/// has what it wanted or has hung up.
#[op2]
async fn op_deis_stream_write(
    state: Rc<RefCell<OpState>>,
    id: u32,
    #[buffer] chunk: JsBuffer,
) -> Result<(), JsErrorBox> {
    let bytes = Bytes::from(chunk.to_vec());
    let written = bytes.len();

    let sink = {
        let mut op_state = state.borrow_mut();
        response_streams(&mut op_state).borrow().sink_of(id)
    };
    let Some(sink) = sink else {
        return Err(stream_closed(id));
    };

    // The borrow above is released before this await: an op that held
    // `OpState` across one would refuse every other op dispatched while this
    // producer was parked, which -- with a slow client -- is most of the
    // stream's life.
    sink.send(bytes).await.map_err(|_| stream_closed(id))?;

    let mut op_state = state.borrow_mut();
    response_streams(&mut op_state)
        .borrow_mut()
        .record_written(id, written);
    Ok(())
}

/// Internal op: the producer is finished; end the body cleanly.
///
/// Dropping the sink is what writes the terminating chunk, so a client reading
/// this body sees a complete response.
#[op2(fast)]
fn op_deis_stream_close(state: &mut OpState, id: u32) {
    let streams = response_streams(state);
    let tally = streams.borrow_mut().close(id);
    if let Some(tally) = tally {
        tracing::debug!(
            target: "app",
            stream_id = id,
            chunks = tally.chunks,
            bytes = tally.bytes,
            lived_ms = tally.lived.as_millis() as u64,
            "A response stream ended: {} chunks and {} bytes over {}ms",
            tally.chunks,
            tally.bytes,
            tally.lived.as_millis()
        );
    }
}

/// Internal op: the producer threw part-way through.
///
/// The status line went out with the head, so there is no 500 left to send.
/// What reaches the client instead is a body that stops without its terminating
/// chunk, which every HTTP client reports as a failed transfer rather than as a
/// short but successful one. That is the only honest ending available this late,
/// and it is why the reason is recorded beside the queue rather than pushed
/// into it: a producer usually gives up precisely when the client has stopped
/// reading, which is when the queue is full and nothing more can be pushed.
#[op2(fast)]
fn op_deis_stream_fail(state: &mut OpState, id: u32, #[string] message: String) {
    let streams = response_streams(state);
    let mut streams = streams.borrow_mut();
    streams.record_failure(id, message.clone());
    // Dropping the entry drops the node's end of the queue, which is what ends
    // the body; the failure recorded a line above is what the reader finds
    // when it gets there.
    let tally = streams.close(id);
    let bytes = tally.map(|t| t.bytes).unwrap_or(0);
    tracing::error!(
        target: "app",
        stream_id = id,
        chunks = tally.map(|t| t.chunks).unwrap_or(0),
        bytes,
        "A response stream failed after its head had already been sent: {}. The client was \
         answered 200 and has been given {} bytes; its read of the rest fails, because the body \
         is deliberately left without its terminating chunk",
        message,
        bytes
    );
}

extension!(
    deis_runtime_response_stream_ops,
    ops = [
        op_deis_stream_write,
        op_deis_stream_close,
        op_deis_stream_fail
    ],
);

/// Initialize the response-stream ops extension.
pub fn init_response_stream_extension() -> deno_core::Extension {
    deis_runtime_response_stream_ops::init()
}

#[cfg(test)]
#[path = "response_stream_test.rs"]
mod response_stream_test;
