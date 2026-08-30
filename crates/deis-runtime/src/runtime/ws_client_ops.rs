//! The outbound `WebSocket`, dialled through the same decision point an
//! outbound `fetch` goes through.
//!
//! # Why this is not `deno_websocket`'s client
//!
//! `deno_websocket` is already loaded -- `js/bootstrap.js` imports its
//! JavaScript so that the *inbound* half of a WebSocket exists -- and binding
//! its `WebSocket` class onto the global object would be one line. That line
//! would be a hole. Its client resolves the host and opens the socket with a
//! connector of its own, which never reaches
//! [`crate::runtime::fetch_policy`]: an application could open a socket to
//! deis-control, to the cloud instance's metadata service, or to any address
//! on the node's own network, and none of it would be judged or logged. That
//! is exactly the class of bypass the outbound fetch policy exists to close,
//! and a second way out of the isolate that does not pass it makes the first
//! one decorative.
//!
//! So the client is this crate's own, and the dial is arranged so that the
//! policy is unavoidable.
//!
//! # The address, not the name
//!
//! [`FetchPolicy::check`] judges a destination *after* resolving it, and this
//! op then connects to the address that was checked rather than handing the
//! name to a connector that would resolve it a second time. The gap between
//! the two lookups is a rebinding window, and closing it is why the dial is
//! spelled out here instead of being left to `tokio_tungstenite::connect_async`.
//!
//! The policy is asked twice, and both questions are needed:
//!
//! * once with the destination **as the application wrote it**, which is the
//!   same call `op_deis_fetch_guard` makes. That is what applies the rules
//!   that are about the *name*: the platform's own service endpoints, and the
//!   operator's `[fetch] allow` list;
//! * once with the **address about to be dialled**, written as a literal, so
//!   the socket can only be opened to an address the policy accepts in its own
//!   right. A literal involves no DNS, so this verdict cannot go stale between
//!   being given and being acted on.
//!
//! The second question makes this client deliberately narrower than `fetch` in
//! one case: an operator allowlist entry that is a *name* resolving to a
//! non-public address (`[fetch] allow = ["internal-api.svc"]` where that name
//! is on `10.0.0.0/8`) opens the destination for `fetch` and not for a
//! WebSocket. `FetchPolicy`'s public surface answers "may this destination be
//! reached", not "was this name allowlisted", and inferring the latter from
//! the former means guessing -- the guess being wrong in the direction that
//! matters is precisely a successful rebind. An allowlist entry that names the
//! address (`10.1.2.3:9000`) works for both.
//!
//! # A socket belongs to one invocation
//!
//! An isolate is reused across requests, so a socket that outlives the request
//! that opened it becomes a socket the *next* request finds already open --
//! charged to the node, attached to no one, and reachable from a handler
//! serving somebody else's traffic. Two things prevent that, and neither
//! depends on the application being well behaved:
//!
//! * a socket may only be opened while an invocation is in flight, and it is
//!   force-closed no later than that invocation's execution deadline. The
//!   pump task below owns the connection and drops it at that instant, which
//!   is the same instant the isolate itself would be cut off;
//! * every op that touches a socket checks that the invocation which opened it
//!   is still the one running. A handler that stashed a socket on `globalThis`
//!   and reached for it during a later request gets an error saying so, and
//!   the socket is closed rather than reused.
//!
//! # A cap on how many
//!
//! Each open socket is a file descriptor and a live TCP connection on a
//! runtime thread that other applications share, held for as long as a request
//! may run. [`MAX_OPEN_SOCKETS`] is what one isolate may hold at once; past it
//! the connection is refused with a message that names the limit, rather than
//! being queued behind sockets that may never close.

use std::cell::RefCell;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use deno_core::url::{Host, Url};
use deno_core::{extension, op2, JsBuffer, OpState, ToJsBuffer};
use deno_error::JsErrorBox;
use deno_runtime::deno_tls::rustls::pki_types::ServerName;
use deno_runtime::deno_tls::rustls::{ClientConfig, ClientConnection};
use deno_runtime::deno_tls::{create_client_config, SocketUse, TlsClientConfigOptions, TlsStream};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::runtime::fetch_policy::{loggable, FetchPolicy};
use crate::runtime::ops::{AppId, ExecutionDeadline};

/// How many outbound sockets one isolate may hold open at once.
///
/// Eight is generous for the shapes an edge handler actually has -- a fan-out
/// to a handful of upstreams, a single long-lived relay -- and small enough
/// that one application cannot take a runtime thread's descriptors away from
/// the others sharing it.
const MAX_OPEN_SOCKETS: usize = 8;

/// The largest message this client will accept from a peer, and the largest
/// single frame inside one.
///
/// tungstenite's own defaults are 64 MiB and 16 MiB. Those are sized for a
/// process that serves one application; here a peer the application chose can
/// make this node allocate that much inside an isolate that shares its heap
/// ceiling and its thread with other tenants, so the limits are brought down
/// to something a request-shaped workload still fits inside.
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// How many bytes a handler may have queued for sending but not yet written.
///
/// `send()` hands the frame to the pump task and returns, which is what the
/// specification says it does -- so without a ceiling a loop that sends faster
/// than the socket drains is an unbounded allocation in the node process. Past
/// this the send is refused with a message naming the limit; `bufferedAmount`
/// is what an application watches to stay under it.
const MAX_BUFFERED_BYTES: usize = 2 * 1024 * 1024;

/// Anything that can carry a WebSocket: a plain TCP stream, or a TLS session
/// on top of one. Boxed so the pump task has one type to hold either way.
trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

type ClientSocket = WebSocketStream<Box<dyn Duplex>>;

/// What JavaScript asked the pump task to do.
enum Outgoing {
    Text(String),
    Binary(Vec<u8>),
    Close { code: u16, reason: String },
}

impl Outgoing {
    /// What this costs against [`MAX_BUFFERED_BYTES`] while it waits.
    fn queued_bytes(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Binary(bytes) => bytes.len(),
            Self::Close { .. } => 0,
        }
    }
}

/// What the pump task has for JavaScript.
enum Incoming {
    Text(String),
    Binary(Vec<u8>),
    Close { code: u16, reason: String },
    Error(String),
}

impl Incoming {
    /// Whether this is the last thing this socket will ever say.
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Close { .. } | Self::Error(_))
    }
}

/// One event, in the shape `js/ws_client.js` turns into a DOM event.
///
/// A tagged record rather than four ops: the read side is a loop awaiting
/// whatever comes next, and which kind of frame that is cannot be known before
/// it arrives. The payload of a binary frame crosses as a buffer -- a
/// `Uint8Array` on the JavaScript side -- because a WebSocket's binary frame
/// is arbitrary bytes and JSON has no way to carry those.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IncomingJs {
    kind: &'static str,
    text: Option<String>,
    data: Option<ToJsBuffer>,
    code: Option<u16>,
}

impl From<Incoming> for IncomingJs {
    fn from(event: Incoming) -> Self {
        match event {
            Incoming::Text(text) => Self {
                kind: "text",
                text: Some(text),
                data: None,
                code: None,
            },
            Incoming::Binary(bytes) => Self {
                kind: "binary",
                text: None,
                data: Some(bytes.into()),
                code: None,
            },
            Incoming::Close { code, reason } => Self {
                kind: "close",
                text: Some(reason),
                data: None,
                code: Some(code),
            },
            Incoming::Error(message) => Self {
                kind: "error",
                text: Some(message),
                data: None,
                code: None,
            },
        }
    }
}

/// What `op_deis_ws_connect` hands back once the handshake is done.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Opened {
    rid: u32,
    /// The subprotocol the server picked, or the empty string.
    protocol: String,
    /// The extensions the server accepted, or the empty string.
    extensions: String,
}

/// One live socket, from the op layer's side of it.
struct OpenSocket {
    /// Dropping this ends the pump task, which closes the connection. That is
    /// how a socket is closed: removing it from the registry is enough.
    outgoing: mpsc::UnboundedSender<Outgoing>,
    /// Behind a mutex because `op_deis_ws_next_event` holds the receiver
    /// across an await, and the read loop in `js/ws_client.js` is the only
    /// caller -- so the lock is never actually contended, it is what lets the
    /// receiver be borrowed mutably from an op at all.
    incoming: Rc<Mutex<mpsc::UnboundedReceiver<Incoming>>>,
    /// Bytes handed to the pump task and not yet written. Shared with it:
    /// this op layer adds, the pump subtracts once a frame is on the wire.
    buffered: Arc<AtomicUsize>,
    /// The execution deadline that was in force when this socket was opened.
    /// It identifies the invocation: a different deadline means a different
    /// request, and this socket does not belong to it.
    opened_under: Instant,
    /// The destination, with any credentials stripped, for the log line when
    /// this socket is closed out from under the application.
    destination: String,
}

/// Every outbound socket one isolate holds.
#[derive(Default)]
struct WsClients {
    next_rid: u32,
    open: HashMap<u32, OpenSocket>,
}

impl WsClients {
    fn insert(&mut self, socket: OpenSocket) -> u32 {
        self.next_rid = self.next_rid.wrapping_add(1);
        let rid = self.next_rid;
        self.open.insert(rid, socket);
        rid
    }

    /// Drop the entries whose pump task has already finished.
    ///
    /// A socket the peer closed while the application was not reading stays in
    /// the map until something looks at it, and would otherwise count against
    /// [`MAX_OPEN_SOCKETS`] forever. The pump task dropping its end of the
    /// command channel is what says it has finished.
    fn forget_closed(&mut self) {
        self.open.retain(|_, socket| !socket.outgoing.is_closed());
    }
}

/// The registry for this isolate, created on first use.
///
/// Installed lazily rather than in the extension's state closure so that no op
/// can ever find it missing: an op that panicked reaching for absent state
/// would panic inside a V8 callback, which cannot unwind, and would take the
/// whole node process down with every other application on it.
fn clients(state: &mut OpState) -> Rc<RefCell<WsClients>> {
    if let Some(existing) = state.try_borrow::<Rc<RefCell<WsClients>>>() {
        return existing.clone();
    }
    let created = Rc::new(RefCell::new(WsClients::default()));
    state.put(created.clone());
    created
}

/// Whether the invocation that opened a socket is still the one running.
///
/// `None` -- no deadline armed -- is read as "still the same invocation": that
/// is the `waitUntil` tail, which runs with the clock stopped and is the same
/// request finishing its work. A *different* deadline is a different request.
fn still_the_same_invocation(state: &OpState, opened_under: Instant) -> bool {
    match state
        .try_borrow::<ExecutionDeadline>()
        .and_then(|slot| slot.get())
    {
        None => true,
        Some(current) => current == opened_under,
    }
}

/// Why one outbound socket was not opened.
enum DialFailure {
    /// The policy said no. The application is told, and the node logs it at
    /// `warn` with the destination and the reason.
    Refused(String),
    /// The destination was allowed and the connection did not come up. That is
    /// the upstream's business, not a policy event.
    Unreachable(String),
}

impl DialFailure {
    fn message(&self) -> &str {
        match self {
            Self::Refused(message) | Self::Unreachable(message) => message,
        }
    }
}

/// The `http(s)` URL that stands for a `ws(s)` one, for the policy to judge.
///
/// [`FetchPolicy::check`] is written in terms of the schemes an outbound fetch
/// may use, and refuses everything else -- so a `ws:` URL handed to it
/// straight would be refused for its scheme rather than judged on its
/// destination. The mapping is the one the WebSocket specification itself
/// makes: `ws` is `http` and `wss` is `https`, with the same default ports, so
/// what the policy judges is the same host and the same port the socket will
/// be opened to.
fn policy_url(parsed: &Url) -> Result<Url, String> {
    let mapped = match parsed.scheme() {
        "ws" => "http",
        "wss" => "https",
        other => {
            return Err(format!(
                "the {other}: scheme is not a WebSocket URL; a WebSocket is opened with ws: or wss:"
            ))
        }
    };
    let mut probe = parsed.clone();
    probe.set_scheme(mapped).map_err(|_| {
        format!(
            "{} could not be read as a destination",
            loggable(parsed.as_str())
        )
    })?;
    Ok(probe)
}

/// How the policy is asked about one address it is about to be dialled at.
fn literal_url(secure: bool, addr: SocketAddr) -> String {
    let scheme = if secure { "https" } else { "http" };
    match addr.ip() {
        IpAddr::V4(v4) => format!("{scheme}://{v4}:{}/", addr.port()),
        IpAddr::V6(v6) => format!("{scheme}://[{v6}]:{}/", addr.port()),
    }
}

/// The TLS settings every `wss:` connection from this node uses.
///
/// Built once: it carries the webpki root store, which is a few hundred
/// certificates, and nothing about it varies per application. ALPN says
/// `http/1.1` because a WebSocket handshake *is* an HTTP/1.1 request, and a
/// server behind an ALPN-aware front end will refuse the connection outright
/// without it.
fn tls_config() -> Result<Arc<ClientConfig>, String> {
    static CONFIG: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            create_client_config(TlsClientConfigOptions {
                socket_use: SocketUse::Http1Only,
                ..Default::default()
            })
            .map(Arc::new)
            .map_err(|e| e.to_string())
        })
        .clone()
}

/// Judge the destination, then dial the address that was judged.
///
/// Every refusal in here is a [`DialFailure::Refused`], and every one of them
/// happens before a socket is opened.
async fn dial(
    policy: &FetchPolicy,
    app_id: &str,
    url: &str,
    protocols: &[String],
) -> Result<(ClientSocket, String, String), DialFailure> {
    let parsed = Url::parse(url)
        .map_err(|e| DialFailure::Refused(format!("{} is not a URL: {e}", loggable(url))))?;
    let probe = policy_url(&parsed).map_err(DialFailure::Refused)?;
    let secure = probe.scheme() == "https";

    // The same question `op_deis_fetch_guard` asks, about the destination as
    // the application wrote it: the platform's own endpoints, the operator's
    // allowlist, and the address ranges, all applied to the resolved host.
    policy.check(probe.as_str()).await.map_err(|refusal| {
        DialFailure::Refused(format!(
            "WebSocket to {} was refused: {refusal}",
            loggable(url)
        ))
    })?;

    let port = probe
        .port_or_known_default()
        .unwrap_or(if secure { 443 } else { 80 });
    let host = probe
        .host()
        .ok_or_else(|| DialFailure::Refused(format!("{} names no host", loggable(url))))?;

    // An address the application wrote as a literal never went through a
    // resolver, so the check above already judged the exact address this will
    // dial and there is no second lookup to guard against.
    let addr = match host {
        Host::Ipv4(v4) => SocketAddr::new(IpAddr::V4(v4), port),
        Host::Ipv6(v6) => SocketAddr::new(IpAddr::V6(v6), port),
        Host::Domain(name) => {
            let name = name.to_string();
            let candidates: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), port))
                .await
                .map_err(|e| {
                    DialFailure::Unreachable(format!(
                        "WebSocket to {} could not be opened: the host {name} could not be \
                         resolved: {e}",
                        loggable(url)
                    ))
                })?
                .collect();

            // The lookup above is a second answer to the same question, and a
            // second answer can differ from the first -- that is DNS
            // rebinding, and it is the standard way past a check like the one
            // that just passed. So the address this is about to connect to is
            // put to the policy in its own right, as a literal, which involves
            // no resolver and cannot go stale between the verdict and the
            // connect.
            let mut chosen = None;
            let mut refusal = None;
            for candidate in candidates {
                match policy.check(&literal_url(secure, candidate)).await {
                    Ok(()) => {
                        chosen = Some(candidate);
                        break;
                    }
                    // The first refusal is the one reported: it is the answer
                    // for the address the connector would have picked.
                    Err(why) => {
                        refusal.get_or_insert(why.to_string());
                    }
                }
            }

            match chosen {
                Some(addr) => addr,
                None => {
                    let why = refusal
                        .unwrap_or_else(|| "the name resolved to no addresses at all".to_string());
                    tracing::warn!(
                        target: "app",
                        app_id = %app_id,
                        destination = %loggable(url),
                        host = %name,
                        reason = %why,
                        "Refused an outbound WebSocket from {} to {}: {} passed the outbound \
                         policy and then resolved to an address the policy refuses -- {}. That \
                         is what a DNS rebind looks like from here; no socket was opened",
                        app_id,
                        loggable(url),
                        name,
                        why
                    );
                    return Err(DialFailure::Refused(format!(
                        "WebSocket to {} was refused: the host {name} resolved to an address \
                         this platform refuses between the policy check and the connection -- \
                         {why}",
                        loggable(url)
                    )));
                }
            }
        }
    };

    // From here on the name is only used for the TLS server name and the
    // `Host:` header the handshake carries. The socket goes to `addr`, which
    // is the address the policy approved a moment ago.
    let tcp = TcpStream::connect(addr).await.map_err(|e| {
        DialFailure::Unreachable(format!(
            "WebSocket to {} could not be opened: {e}",
            loggable(url)
        ))
    })?;
    // Handshake frames are small and answered one at a time; Nagle would hold
    // each one back waiting for a second that is not coming.
    let _ = tcp.set_nodelay(true);

    let stream: Box<dyn Duplex> = if secure {
        let config = tls_config().map_err(|e| {
            DialFailure::Unreachable(format!(
                "WebSocket to {} could not be opened: this node has no usable TLS \
                 configuration: {e}",
                loggable(url)
            ))
        })?;
        let server_name = ServerName::try_from(host.to_string()).map_err(|e| {
            DialFailure::Unreachable(format!(
                "WebSocket to {} could not be opened: {host} is not a valid TLS server name: {e}",
                loggable(url)
            ))
        })?;
        let session = ClientConnection::new(config, server_name).map_err(|e| {
            DialFailure::Unreachable(format!(
                "WebSocket to {} could not be opened: the TLS session could not be started: {e}",
                loggable(url)
            ))
        })?;
        Box::new(TlsStream::new_client_side(tcp, session, None))
    } else {
        Box::new(tcp)
    };

    let mut request = parsed.as_str().into_client_request().map_err(|e| {
        DialFailure::Refused(format!(
            "WebSocket to {} was refused: it is not a URL a WebSocket can be opened to: {e}",
            loggable(url)
        ))
    })?;
    if !protocols.is_empty() {
        let value = HeaderValue::from_str(&protocols.join(", ")).map_err(|_| {
            DialFailure::Refused(
                "the requested WebSocket subprotocols are not a valid header value".to_string(),
            )
        })?;
        request
            .headers_mut()
            .insert("sec-websocket-protocol", value);
    }

    let config = WebSocketConfig {
        max_message_size: Some(MAX_MESSAGE_BYTES),
        max_frame_size: Some(MAX_FRAME_BYTES),
        ..Default::default()
    };

    let (socket, response) =
        tokio_tungstenite::client_async_with_config(request, stream, Some(config))
            .await
            .map_err(|e| {
                DialFailure::Unreachable(format!(
                    "WebSocket to {} could not be opened: {e}",
                    loggable(url)
                ))
            })?;

    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };

    Ok((
        socket,
        header("sec-websocket-protocol"),
        header("sec-websocket-extensions"),
    ))
}

/// Owns one connection for as long as it lives.
///
/// Everything that can end the socket ends here, in one place: the peer, the
/// application, the invocation's execution deadline, and the registry entry
/// going away. Whichever it is, the task returns and the connection is dropped
/// with it.
async fn pump(
    mut socket: ClientSocket,
    mut outgoing: mpsc::UnboundedReceiver<Outgoing>,
    incoming: mpsc::UnboundedSender<Incoming>,
    buffered: Arc<AtomicUsize>,
    deadline: Instant,
    app_id: String,
    destination: String,
) {
    let cutoff = tokio::time::sleep_until(deadline.into());
    tokio::pin!(cutoff);

    loop {
        tokio::select! {
            // The invocation's budget. Terminating V8 does nothing to a socket
            // this task holds, so without this branch a connection an
            // application opened and then stopped waiting for would outlive
            // the request by however long the peer felt like keeping it.
            _ = &mut cutoff => {
                tracing::warn!(
                    target: "app",
                    app_id = %app_id,
                    destination = %destination,
                    "Closed {}'s outbound WebSocket to {}: the invocation's execution budget ran \
                     out while the socket was still open. The application sees a close event with \
                     code 1001",
                    app_id,
                    destination
                );
                let _ = incoming.send(Incoming::Close {
                    code: 1001,
                    reason: "the invocation's execution budget ran out".to_string(),
                });
                break;
            }

            command = outgoing.recv() => {
                match command {
                    // The registry entry is gone: either the application
                    // closed the socket, or the invocation that opened it did
                    // not survive to use it. Say goodbye if there is still
                    // budget to say it in.
                    None => {
                        let _ = tokio::time::timeout_at(
                            deadline.into(),
                            socket.close(Some(CloseFrame {
                                code: CloseCode::Away,
                                reason: "the invocation that opened this socket has ended".into(),
                            })),
                        ).await;
                        break;
                    }
                    Some(Outgoing::Close { code, reason }) => {
                        let frame = CloseFrame { code: CloseCode::from(code), reason: reason.clone().into() };
                        let _ = tokio::time::timeout_at(deadline.into(), socket.close(Some(frame))).await;
                        let _ = incoming.send(Incoming::Close { code, reason });
                        break;
                    }
                    Some(message) => {
                        let queued = message.queued_bytes();
                        let frame = match message {
                            Outgoing::Text(text) => Message::Text(text),
                            Outgoing::Binary(bytes) => Message::Binary(bytes),
                            Outgoing::Close { .. } => unreachable!("handled above"),
                        };
                        // Bounded by the deadline like everything else: a peer
                        // that accepts a connection and then stops reading
                        // would otherwise hold this task -- and the socket --
                        // past the request it belongs to.
                        let written = tokio::time::timeout_at(deadline.into(), socket.send(frame)).await;
                        buffered.fetch_sub(queued, Ordering::Relaxed);
                        match written {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                let _ = incoming.send(Incoming::Error(format!(
                                    "the WebSocket to {destination} failed while sending: {e}"
                                )));
                                break;
                            }
                            Err(_) => {
                                tracing::warn!(
                                    target: "app",
                                    app_id = %app_id,
                                    destination = %destination,
                                    "Gave up writing to {}'s outbound WebSocket to {}: the \
                                     invocation's execution budget ran out with the frame still \
                                     unwritten. The socket is closed",
                                    app_id,
                                    destination
                                );
                                let _ = incoming.send(Incoming::Close {
                                    code: 1001,
                                    reason: "the invocation's execution budget ran out".to_string(),
                                });
                                break;
                            }
                        }
                    }
                }
            }

            frame = socket.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        let _ = incoming.send(Incoming::Text(text));
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        let _ = incoming.send(Incoming::Binary(bytes));
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let (code, reason) = match frame {
                            Some(frame) => (u16::from(frame.code), frame.reason.to_string()),
                            None => (1005, String::new()),
                        };
                        let _ = incoming.send(Incoming::Close { code, reason });
                        break;
                    }
                    // Ping, Pong and Frame: tungstenite answers a ping itself,
                    // and none of these are events the specification surfaces.
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        let _ = incoming.send(Incoming::Error(format!(
                            "the WebSocket to {destination} failed: {e}"
                        )));
                        break;
                    }
                    // The stream ended without a close frame.
                    None => {
                        let _ = incoming.send(Incoming::Close {
                            code: 1006,
                            reason: "the connection was closed without a close frame".to_string(),
                        });
                        break;
                    }
                }
            }
        }
    }

    // Dropping the socket is what actually closes the connection; everything
    // above only decided when.
    drop(socket);
    tracing::debug!(
        target: "app",
        app_id = %app_id,
        destination = %destination,
        "Outbound WebSocket from {} to {} is closed",
        app_id,
        destination
    );
}

/// Open one outbound WebSocket, or say why not.
///
/// `budget_ms` is what `op_deis_fetch_budget_ms` answered on the JavaScript
/// side -- the same number, from the same op, that bounds an outbound `fetch`,
/// with the same margin left for the handler to notice a failure and answer
/// it. It can only shorten what this op allows: the deadline in the isolate's
/// own state is read here as well and is the ceiling.
#[op2(async)]
#[serde]
async fn op_deis_ws_connect(
    state: Rc<RefCell<OpState>>,
    #[string] url: String,
    budget_ms: f64,
    #[serde] protocols: Vec<String>,
) -> Result<Opened, JsErrorBox> {
    let (policy, app_id, deadline, registry) = {
        let mut state = state.borrow_mut();
        let policy = state.try_borrow::<FetchPolicy>().cloned();
        let app_id = state
            .try_borrow::<AppId>()
            .map(|app| app.0.clone())
            .unwrap_or_default();
        let deadline = state
            .try_borrow::<ExecutionDeadline>()
            .and_then(|slot| slot.get());
        let registry = clients(&mut state);
        (policy, app_id, deadline, registry)
    };

    let destination = loggable(&url);

    // An isolate with no policy installed would be one built by a path that
    // forgets to install one. Refuse rather than fall open: an outbound socket
    // with nothing judging it is the hole this module exists to close.
    let Some(policy) = policy else {
        tracing::error!(
            target: "app",
            app_id = %app_id,
            destination = %destination,
            "Refused an outbound WebSocket from {} to {}: this isolate was built without an \
             outbound fetch policy, which is a bug in the runtime, not in the application",
            app_id,
            destination
        );
        return Err(JsErrorBox::type_error(
            "outbound WebSocket is not available in this isolate",
        ));
    };

    // No invocation in flight means nothing would ever end this socket: not
    // the execution budget, which is what closes it, and not the next request,
    // which would simply find it open. So there is no request to charge it to
    // and it is not opened.
    let Some(deadline) = deadline else {
        tracing::warn!(
            target: "app",
            app_id = %app_id,
            destination = %destination,
            "Refused an outbound WebSocket from {} to {}: no invocation is in flight, so nothing \
             would bound the socket's lifetime. Open it inside the request that uses it",
            app_id,
            destination
        );
        return Err(JsErrorBox::type_error(format!(
            "WebSocket to {destination} was refused: an outbound WebSocket can only be opened \
             while a request is being handled, because it is closed when that request ends"
        )));
    };

    {
        let mut registry = registry.borrow_mut();
        registry.forget_closed();
        let open = registry.open.len();
        if open >= MAX_OPEN_SOCKETS {
            tracing::warn!(
                target: "app",
                app_id = %app_id,
                destination = %destination,
                open,
                "Refused an outbound WebSocket from {} to {}: this isolate already holds {} open \
                 outbound sockets, which is the limit",
                app_id,
                destination,
                open
            );
            return Err(JsErrorBox::type_error(format!(
                "WebSocket to {destination} was refused: this isolate already has \
                 {MAX_OPEN_SOCKETS} outbound WebSockets open, which is the limit. Close one \
                 before opening another"
            )));
        }
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() || budget_ms == 0.0 {
        tracing::warn!(
            target: "app",
            app_id = %app_id,
            destination = %destination,
            "Refused an outbound WebSocket from {} to {}: the invocation's execution budget was \
             already spent. Nothing was dialled",
            app_id,
            destination
        );
        return Err(JsErrorBox::type_error(format!(
            "WebSocket to {destination} was refused: this invocation's execution budget ran out \
             before the connection was opened"
        )));
    }
    // A negative answer from the budget op means the isolate is running
    // without one, which cannot happen with a deadline in hand; the deadline
    // is the ceiling either way.
    let allowance = if budget_ms < 0.0 {
        remaining
    } else {
        Duration::from_millis(budget_ms as u64).min(remaining)
    };

    let opened = match tokio::time::timeout(allowance, dial(&policy, &app_id, &url, &protocols))
        .await
    {
        Ok(Ok(opened)) => opened,
        Ok(Err(failure)) => {
            match &failure {
                DialFailure::Refused(reason) => tracing::warn!(
                    target: "app",
                    app_id = %app_id,
                    destination = %destination,
                    reason = %reason,
                    "Refused an outbound WebSocket from {} to {}: {}. The application sees it as \
                     an error event on the socket; nothing was dialled",
                    app_id,
                    destination,
                    reason
                ),
                DialFailure::Unreachable(reason) => tracing::debug!(
                    target: "app",
                    app_id = %app_id,
                    destination = %destination,
                    reason = %reason,
                    "An outbound WebSocket from {} to {} did not come up: {}",
                    app_id,
                    destination,
                    reason
                ),
            }
            return Err(JsErrorBox::type_error(failure.message().to_string()));
        }
        Err(_) => {
            tracing::warn!(
                target: "app",
                app_id = %app_id,
                destination = %destination,
                budget_ms = allowance.as_millis() as u64,
                "Gave up opening an outbound WebSocket from {} to {}: the invocation's execution \
                 budget ran out with {}ms allowed for the handshake",
                app_id,
                destination,
                allowance.as_millis()
            );
            return Err(JsErrorBox::type_error(format!(
                "WebSocket to {destination} was abandoned: this invocation's execution budget ran \
                 out while the connection was being opened"
            )));
        }
    };

    let (socket, protocol, extensions) = opened;
    let (to_socket, from_js) = mpsc::unbounded_channel();
    let (to_js, from_socket) = mpsc::unbounded_channel();
    let buffered = Arc::new(AtomicUsize::new(0));

    tokio::spawn(pump(
        socket,
        from_js,
        to_js,
        buffered.clone(),
        deadline,
        app_id.clone(),
        destination.clone(),
    ));

    let rid = registry.borrow_mut().insert(OpenSocket {
        outgoing: to_socket,
        incoming: Rc::new(Mutex::new(from_socket)),
        buffered,
        opened_under: deadline,
        destination: destination.clone(),
    });

    tracing::debug!(
        target: "app",
        app_id = %app_id,
        destination = %destination,
        rid,
        "Opened an outbound WebSocket from {} to {}; it is closed when this invocation ends",
        app_id,
        destination
    );

    Ok(Opened {
        rid,
        protocol,
        extensions,
    })
}

/// The error an application gets for a socket that is no longer its own.
///
/// Closing it here rather than merely refusing the call is the point: a
/// handler that kept a socket across requests must not be able to keep it
/// *open* across them either.
fn not_this_invocation(
    registry: &Rc<RefCell<WsClients>>,
    app_id: &str,
    rid: u32,
    destination: &str,
) -> JsErrorBox {
    registry.borrow_mut().open.remove(&rid);
    tracing::warn!(
        target: "app",
        app_id = %app_id,
        destination = %destination,
        "Closed {}'s outbound WebSocket to {}: it was opened by an earlier invocation and used \
         from a later one. An isolate is reused across requests, so a socket that outlived the \
         request that opened it would be shared with somebody else's traffic",
        app_id,
        destination
    );
    JsErrorBox::type_error(format!(
        "the WebSocket to {destination} was opened by an earlier invocation and has been closed: \
         a socket does not outlive the request that opened it. Open one inside the request that \
         uses it"
    ))
}

/// Look one socket up and confirm it still belongs to the invocation running.
///
/// Answers what the caller needs off the entry rather than the entry itself,
/// because the registry is borrowed for the lookup and an op that awaits
/// cannot hold that borrow across the await.
fn borrow_socket<T>(
    state: &mut OpState,
    rid: u32,
    take: impl FnOnce(&OpenSocket) -> T,
) -> Result<T, JsErrorBox> {
    let app_id = state
        .try_borrow::<AppId>()
        .map(|app| app.0.clone())
        .unwrap_or_default();
    let registry = clients(state);

    let (taken, belongs, destination) = {
        let registry_ref = registry.borrow();
        let Some(socket) = registry_ref.open.get(&rid) else {
            return Err(JsErrorBox::type_error(
                "this WebSocket is closed: there is no connection behind it any more",
            ));
        };
        (
            take(socket),
            still_the_same_invocation(state, socket.opened_under),
            socket.destination.clone(),
        )
    };

    if !belongs {
        return Err(not_this_invocation(&registry, &app_id, rid, &destination));
    }
    Ok(taken)
}

/// Queue a text frame.
///
/// Synchronous, because `send()` is synchronous in the specification: it hands
/// the frame over and returns, and `bufferedAmount` is how an application sees
/// what has not been written yet.
#[op2(fast)]
fn op_deis_ws_send_text(
    state: &mut OpState,
    rid: u32,
    #[string] text: String,
) -> Result<(), JsErrorBox> {
    let queued = text.len();
    let (outgoing, buffered) = borrow_socket(state, rid, |socket| {
        (socket.outgoing.clone(), socket.buffered.clone())
    })?;
    queue(&outgoing, &buffered, Outgoing::Text(text), queued)
}

/// Queue a binary frame.
///
/// The bytes arrive as a buffer rather than as a string: a binary frame is
/// whatever the application put in it, and only the application knows whether
/// that is text.
#[op2]
fn op_deis_ws_send_binary(
    state: &mut OpState,
    rid: u32,
    #[buffer] data: JsBuffer,
) -> Result<(), JsErrorBox> {
    let bytes = data.to_vec();
    let queued = bytes.len();
    let (outgoing, buffered) = borrow_socket(state, rid, |socket| {
        (socket.outgoing.clone(), socket.buffered.clone())
    })?;
    queue(&outgoing, &buffered, Outgoing::Binary(bytes), queued)
}

fn queue(
    outgoing: &mpsc::UnboundedSender<Outgoing>,
    buffered: &Arc<AtomicUsize>,
    message: Outgoing,
    queued: usize,
) -> Result<(), JsErrorBox> {
    let waiting = buffered.load(Ordering::Relaxed);
    if waiting.saturating_add(queued) > MAX_BUFFERED_BYTES {
        return Err(JsErrorBox::type_error(format!(
            "the WebSocket send buffer is full: {waiting} bytes are already waiting to be \
             written and the limit is {MAX_BUFFERED_BYTES}. Wait for bufferedAmount to fall \
             before sending more"
        )));
    }
    buffered.fetch_add(queued, Ordering::Relaxed);
    outgoing.send(message).map_err(|_| {
        buffered.fetch_sub(queued, Ordering::Relaxed);
        JsErrorBox::type_error("this WebSocket is closed: the frame was not sent")
    })
}

/// How many queued bytes are still unwritten, for `bufferedAmount`.
///
/// A socket that has gone away answers zero rather than failing: reading a
/// property is not a place to throw, and nothing is waiting on a closed
/// socket.
#[op2(fast)]
fn op_deis_ws_buffered_amount(state: &mut OpState, rid: u32) -> f64 {
    borrow_socket(state, rid, |socket| {
        socket.buffered.load(Ordering::Relaxed) as f64
    })
    .unwrap_or(0.0)
}

/// Ask for the closing handshake.
///
/// The entry stays in the registry until the pump task answers with the close
/// event, so the application still gets its `close` event with the code the
/// peer agreed to.
#[op2(fast)]
fn op_deis_ws_close(
    state: &mut OpState,
    rid: u32,
    code: u32,
    #[string] reason: String,
) -> Result<(), JsErrorBox> {
    let outgoing = borrow_socket(state, rid, |socket| socket.outgoing.clone())?;
    let _ = outgoing.send(Outgoing::Close {
        code: code as u16,
        reason,
    });
    Ok(())
}

/// Wait for whatever this socket says next.
///
/// The read loop in `js/ws_client.js` awaits this and turns each answer into a
/// DOM event. A terminal event takes the socket out of the registry, which
/// drops the last handle on the pump task's command channel and ends it.
#[op2(async)]
#[serde]
async fn op_deis_ws_next_event(
    state: Rc<RefCell<OpState>>,
    rid: u32,
) -> Result<IncomingJs, JsErrorBox> {
    let incoming = {
        let mut state = state.borrow_mut();
        borrow_socket(&mut state, rid, |socket| socket.incoming.clone())?
    };

    let event = {
        let mut incoming = incoming.lock().await;
        incoming.recv().await
    };

    let event = event.unwrap_or(Incoming::Close {
        code: 1006,
        reason: "the connection ended".to_string(),
    });

    if event.is_terminal() {
        let mut state = state.borrow_mut();
        clients(&mut state).borrow_mut().open.remove(&rid);
    }

    Ok(event.into())
}

extension!(
    deis_runtime_ws_client_ops,
    ops = [
        op_deis_ws_connect,
        op_deis_ws_send_text,
        op_deis_ws_send_binary,
        op_deis_ws_buffered_amount,
        op_deis_ws_close,
        op_deis_ws_next_event,
    ],
    esm = ["src/runtime/js/ws_client.js"],
);

pub fn init_ws_client_extension() -> deno_core::Extension {
    deis_runtime_ws_client_ops::init()
}

#[cfg(test)]
#[path = "ws_client_ops_test.rs"]
mod ws_client_ops_test;
