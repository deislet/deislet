//! A real WebSocket handshake, first against the node and then through
//! deis-proxy, which is the hop nothing had ever exercised.
//!
//! `websocket_bridging_test` proves the dispatcher can bridge a client socket
//! to an application's `WebSocketPair`, but it talks to the dispatcher
//! directly. In production every client reaches the node through deis-proxy,
//! and an upgrade is the one exchange a reverse proxy can quietly ruin: it can
//! answer the 101 itself, rewrite `Sec-WebSocket-Accept`, buffer the frames,
//! or hold the connection only as long as it holds an ordinary response. None
//! of that had ever been checked.
//!
//! So the handshake here is written out by hand rather than left to
//! tokio-tungstenite's client. A generated `Sec-WebSocket-Key` is different on
//! every run and the library verifies the answer privately, which is exactly
//! the evidence this file exists to produce; with the fixed key from RFC 6455
//! section 1.3 the correct answer is a constant published in the RFC, and the
//! test can assert on the bytes that actually came back.
//!
//! Every test runs the node in-process and, for the proxy leg, starts the real
//! `deis-proxy` binary from `target/debug`. That binary is built by
//! `cargo build --workspace` and by `cargo test --workspace`; running only
//! `cargo test -p deis-runtime` in a tree where it was never built makes the
//! proxy tests fail with a message saying so, which is the honest outcome -
//! a skip here would mean the hop is unverified while the suite is green.

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::routing::any;
use axum::Router;
use deis_runtime::app::{AppKey, ApplicationManager};
use deis_runtime::dispatcher::{handlers, AppState};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::{Message, Role};
use tokio_tungstenite::WebSocketStream;

/// The application id every test in this file addresses.
const APP: &str = "ws_proxy_app";

/// The nonce and the answer to it, both copied from RFC 6455 section 1.3.
/// Fixed on purpose: it makes `Sec-WebSocket-Accept` a value this test can
/// compare against rather than one it has to trust the client library about.
const HANDSHAKE_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const HANDSHAKE_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

/// An edge-canon WebSocket echo server.
///
/// Text comes back with a prefix so a reply cannot be mistaken for the frame
/// that caused it. A binary frame comes back as its length and its bytes in
/// hex: the platform hands a binary frame to the isolate as an `ArrayBuffer`
/// but has no way to send one back (`op_ws_server_send` takes a string and
/// only ever emits `WsServerMessage::Text`), so describing the bytes in text
/// is the only way an application can prove which bytes it received.
const ECHO_APP: &str = r#"
    export default async function handler(context) {
        if (context.request.headers.get("upgrade") === "websocket") {
            const pair = new WebSocketPair();
            const client = pair[0];
            const server = pair[1];
            server.accept();
            server.addEventListener("message", (event) => {
                const data = event.data;
                if (typeof data === "string") {
                    server.send("text:" + data);
                    return;
                }
                const bytes = new Uint8Array(data);
                let hex = "";
                for (let i = 0; i < bytes.length; i++) {
                    hex += bytes[i].toString(16).padStart(2, "0");
                }
                server.send("binary:" + bytes.length + ":" + hex);
            });
            return new Response(null, { status: 101, webSocket: client });
        }
        return new Response("WebSocket echo app");
    }
"#;

/// The node, serving the dispatcher on a port the kernel picked.
struct Node {
    addr: SocketAddr,
    server: tokio::task::JoinHandle<()>,
}

impl Node {
    async fn start() -> Node {
        let app_manager = Arc::new(ApplicationManager::new());
        let state = AppState::new(
            app_manager.clone(),
            deis_runtime::config::default_max_request_body_bytes(),
        );
        app_manager
            .load_app(
                AppKey::production(APP),
                ECHO_APP.to_string(),
                Default::default(),
                None,
            )
            .await
            .expect("the echo app loads");

        let router = Router::new()
            .route("/*path", any(handlers::handle_request))
            .route("/", any(handlers::handle_request))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port to serve on");
        let addr = listener.local_addr().expect("the address it bound");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Node { addr, server }
    }

    fn stop(self) {
        self.server.abort();
    }
}

/// The handshake response as it came off the wire, plus the socket it was
/// read from, positioned exactly at the first WebSocket frame.
struct Handshake {
    status_line: String,
    headers: Vec<(String, String)>,
    stream: TcpStream,
}

impl Handshake {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Take over the socket as a WebSocket client. `Role::Client` is what
    /// makes tungstenite mask the frames it sends, which a server is entitled
    /// to close the connection over if it is missing.
    async fn into_socket(self) -> WebSocketStream<TcpStream> {
        WebSocketStream::from_raw_socket(self.stream, Role::Client, None).await
    }
}

/// Perform the opening handshake by hand against `addr`.
///
/// The response headers are read one byte at a time. A buffered read would
/// pull whatever follows them out of the socket and into a buffer this
/// function then throws away, which would silently eat the first frames of
/// any server that starts talking immediately.
///
/// `app_id_header` names the application in `x-deis-app-id`. A handshake
/// against the node itself has to carry it - the node trusts that header and
/// answers 400 without one. A handshake through the proxy must not rely on
/// it: the proxy deletes the whole `x-deis-` namespace before it reads
/// anything and routes by `Host`, so the direct handshake passes `Some` and
/// the proxied ones `None`, which is itself the assertion that the header is
/// not needed.
async fn handshake(addr: SocketAddr, path: &str, app_id_header: Option<&str>) -> Handshake {
    let mut stream = TcpStream::connect(addr)
        .await
        .unwrap_or_else(|e| panic!("connecting to {addr}: {e}"));

    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {HANDSHAKE_KEY}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         {}\r\n",
        app_id_header
            .map(|app| format!("x-deis-app-id: {app}\r\n"))
            .unwrap_or_default()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("the handshake request is sent");

    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte).await {
            Ok(0) => panic!(
                "the connection closed before the handshake response ended; got {:?}",
                String::from_utf8_lossy(&raw)
            ),
            Ok(_) => raw.push(byte[0]),
            Err(e) => panic!("reading the handshake response: {e}"),
        }
        assert!(
            raw.len() < 8192,
            "handshake response never terminated: {:?}",
            String::from_utf8_lossy(&raw)
        );
    }

    let text = String::from_utf8(raw).expect("the handshake response is text");
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default().to_string();
    let headers = lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            line.split_once(':')
                .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        })
        .collect();

    Handshake {
        status_line,
        headers,
        stream,
    }
}

/// Assert the four things that make a 101 a WebSocket, and return the socket.
fn assert_switched_protocols(handshake: &Handshake) {
    assert_eq!(
        handshake.status_line, "HTTP/1.1 101 Switching Protocols",
        "the upgrade did not switch protocols"
    );
    assert_eq!(
        handshake
            .header("upgrade")
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("websocket"),
        "the 101 does not name the websocket protocol"
    );
    assert!(
        handshake
            .header("connection")
            .map(|value| value.eq_ignore_ascii_case("upgrade"))
            .unwrap_or(false),
        "the 101 does not carry Connection: Upgrade, got {:?}",
        handshake.header("connection")
    );
    assert_eq!(
        handshake.header("sec-websocket-accept"),
        Some(HANDSHAKE_ACCEPT),
        "Sec-WebSocket-Accept is not the answer to the key that was sent; \
         something on the path answered the handshake instead of relaying it"
    );
}

/// Read frames until one is text, so a ping or a pong on the way does not
/// decide the test.
async fn next_text(socket: &mut WebSocketStream<TcpStream>) -> String {
    loop {
        let message = socket
            .next()
            .await
            .expect("the connection stays open")
            .expect("a readable frame");
        match message {
            Message::Text(text) => return text,
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("expected a text frame, got {other:?}"),
        }
    }
}

/// Send a text frame and a binary frame and check both arrived intact.
async fn echo_both_frame_kinds(socket: &mut WebSocketStream<TcpStream>) {
    socket
        .send(Message::Text("hello over the wire".to_string()))
        .await
        .expect("the client can send text");
    assert_eq!(
        next_text(socket).await,
        "text:hello over the wire",
        "the text frame did not survive the round trip"
    );

    // Bytes chosen so a path that mistook the frame for UTF-8 text, or
    // trimmed it, cannot produce this answer: 0x00 ends a C string, 0xff and
    // 0xfe never occur in UTF-8 at all, and 0x80 is a continuation byte with
    // no lead byte in front of it.
    let payload = vec![0x00u8, 0xff, 0xfe, 0x80, 0x7f, 0x41];
    socket
        .send(Message::Binary(payload.clone()))
        .await
        .expect("the client can send binary");
    let expected = format!(
        "binary:{}:{}",
        payload.len(),
        payload
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    assert_eq!(
        next_text(socket).await,
        expected,
        "the binary frame did not reach the isolate byte for byte"
    );
}

#[tokio::test]
async fn a_handshake_against_the_node_switches_protocols_and_echoes_both_frame_kinds() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let node = Node::start().await;
    let handshake = handshake(node.addr, "/ws", Some(APP)).await;
    assert_switched_protocols(&handshake);

    let mut socket = handshake.into_socket().await;
    echo_both_frame_kinds(&mut socket).await;

    socket
        .send(Message::Close(None))
        .await
        .expect("the client can close");
    node.stop();
}

// ----------------------------------------------------------------------
// The hop under test: the same handshake, through deis-proxy.
// ----------------------------------------------------------------------

/// A running `deis-proxy`, killed when this value is dropped.
struct Proxy {
    child: Child,
    addr: SocketAddr,
    log: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Write one application into a fresh deis-proxy database, in the shape the
/// control plane pushes.
///
/// Straight SQL against the schema `deis_proxy::config::ConfigDatabase`
/// creates (`app_configs(app_id, config, version_timestamp)`), because
/// deis-runtime does not depend on deis-proxy and should not start to for a
/// single row. The proxy's own `CREATE TABLE IF NOT EXISTS` runs over this
/// happily. The proxy routes by `Host` alone - a client's `x-deis-*` headers
/// are deleted before it reads anything - and the handshake and the readiness
/// probe below are addressed to the address they dial, so the application's
/// domain is the bare loopback literal that `Host` carries (the port is
/// stripped before the match).
fn seed_app(database: &std::path::Path, app_id: &str) {
    let connection = rusqlite::Connection::open(database).expect("the proxy database opens");
    connection
        .execute(
            "CREATE TABLE IF NOT EXISTS app_configs (
                app_id TEXT PRIMARY KEY,
                config TEXT NOT NULL,
                version_timestamp TEXT NOT NULL
            )",
            [],
        )
        .expect("the app table exists");
    let config = format!(
        r#"{{"id":"{app_id}","description":"","domains":["127.0.0.1"],"certificates":[],"enabled":true,"cache":[]}}"#
    );
    connection
        .execute(
            "INSERT INTO app_configs (app_id, config, version_timestamp) VALUES (?1, ?2, ?3)",
            rusqlite::params![app_id, config, "1970-01-01T00:00:00+00:00"],
        )
        .expect("the application is written");
}

impl Proxy {
    /// Where `cargo build` leaves the proxy: this test binary lives in
    /// `target/<profile>/deps`, and the binary is its grandparent's child.
    fn binary() -> PathBuf {
        let mut dir = std::env::current_exe().expect("this test binary's own path");
        dir.pop();
        if dir.ends_with("deps") {
            dir.pop();
        }
        dir.join("deis-proxy")
    }

    /// A port nothing is listening on. The listener is closed before the
    /// address is handed out, so this is a hint rather than a reservation;
    /// a proxy that loses the race dies on bind and `start` says so.
    fn free_port() -> u16 {
        StdTcpListener::bind("127.0.0.1:0")
            .expect("a free port")
            .local_addr()
            .expect("its address")
            .port()
    }

    /// Start the proxy in front of `upstream` and wait until it forwards.
    ///
    /// `upstream_read_timeout_secs` is the proxy's own configuration name; it
    /// is a parameter here because it is what the idle test needs to move.
    async fn start(upstream: SocketAddr, upstream_read_timeout_secs: u64) -> Proxy {
        let binary = Proxy::binary();
        assert!(
            binary.exists(),
            "{} is not built, so the hop this file exists to test cannot be exercised. \
             Build it first: cargo build -p deis-proxy",
            binary.display()
        );

        let dir = tempfile::tempdir().expect("a directory for the proxy's files");
        let port = Proxy::free_port();
        let config = dir.path().join("deis-proxy.toml");
        let log = dir.path().join("deis-proxy-error.log");
        let database = dir.path().join("deis-proxy.db");
        // The application this test addresses, written into the proxy's own
        // database the way the control plane would have pushed it.
        //
        // It is not decoration. deis-proxy stamps `x-deis-app-id` from what
        // it resolved and strips it when it resolved nothing, so a proxy that
        // had never heard of this application would hand the node a request
        // with no application id at all - and the node answers 400 to that,
        // as it should. Seeding the row is what makes the hop under test the
        // real one: the header the node reads is the proxy's decision, not a
        // value this test smuggled past it.
        seed_app(&database, APP);
        // No [control] section: standalone, so nothing is polled and the
        // application list written above is the whole routing table.
        //
        // `[health_check]` is pointed at this test's own application because
        // deis-proxy asks every upstream whether it is ready before it will
        // route to it, and by default it asks the `_system` application every
        // real node loads at startup. The node above is a dispatcher stood up
        // by hand with one application in it, so `_system` is not there and
        // the default question has no answer. Asking for the application this
        // node does serve keeps the probe a real HTTP request rather than
        // turning the check off.
        std::fs::write(
            &config,
            format!(
                "listen_addr = [\"127.0.0.1\"]\n\
                 listen_ports = [{port}]\n\
                 database = \"{db}\"\n\
                 pid_file = \"{pid}\"\n\
                 upgrade_sock = \"{sock}\"\n\
                 upstream_read_timeout_secs = {upstream_read_timeout_secs}\n\
                 \n\
                 [[upstreams]]\n\
                 host = \"{host}\"\n\
                 port = {upstream_port}\n\
                 \n\
                 [health_check]\n\
                 path = \"/\"\n\
                 app_id = \"{APP}\"\n\
                 \n\
                 [log.error_log]\n\
                 level = \"debug\"\n\
                 path = \"{log}\"\n",
                db = database.display(),
                pid = dir.path().join("deis-proxy.pid").display(),
                sock = dir.path().join("upgrade.sock").display(),
                host = upstream.ip(),
                upstream_port = upstream.port(),
                log = log.display(),
            ),
        )
        .expect("the proxy configuration is written");

        let child = Command::new(&binary)
            .arg("-c")
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("starting {}: {e}", binary.display()));

        let mut proxy = Proxy {
            child,
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            log,
            _dir: dir,
        };
        proxy.wait_until_forwarding().await;
        proxy
    }

    /// Block until an ordinary request comes back through the proxy from the
    /// node behind it.
    ///
    /// Answering the port is not enough: the upstream pool is filled by a
    /// background service, and until it has a backend the proxy answers 503
    /// perfectly promptly. So the probe is a real request whose answer only
    /// the node can produce. Each round re-checks that the process is still
    /// alive, because a proxy that died on bind would otherwise be waited for
    /// until the deadline.
    async fn wait_until_forwarding(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut last = String::from("no attempt was made");
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "deis-proxy exited during startup with {status}; its log said:\n{}",
                    self.log_tail()
                );
            }
            match self.probe().await {
                Ok(body) if body == "WebSocket echo app" => return,
                Ok(body) => last = format!("the proxy answered {body:?}"),
                Err(e) => last = e,
            }
            // Paced retry against a real readiness probe, not a wait for a
            // guessed duration: the loop above decides when it is ready.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "deis-proxy never forwarded to the node within 30s: {last}; its log said:\n{}",
            self.log_tail()
        );
    }

    async fn probe(&self) -> Result<String, String> {
        let response = reqwest::Client::new()
            .get(format!("http://{}/", self.addr))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map_err(|e| format!("the proxy did not answer: {e}"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            Ok(body)
        } else {
            Err(format!("the proxy answered {status}: {body}"))
        }
    }

    fn log_tail(&self) -> String {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        text.lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[tokio::test]
async fn a_handshake_through_the_proxy_switches_protocols_and_echoes_both_frame_kinds() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let node = Node::start().await;
    let proxy = Proxy::start(node.addr, 30).await;

    let handshake = handshake(proxy.addr, "/ws", None).await;
    assert_switched_protocols(&handshake);

    let mut socket = handshake.into_socket().await;
    echo_both_frame_kinds(&mut socket).await;

    socket
        .send(Message::Close(None))
        .await
        .expect("the client can close");
    drop(proxy);
    node.stop();
}

#[tokio::test]
async fn a_websocket_through_the_proxy_survives_an_idle_period() {
    // A WebSocket is idle most of the time - that is what it is for - and the
    // proxy's `upstream_read_timeout` is a limit on how long an *ordinary*
    // response may take to arrive. Applied unchanged to an upgraded
    // connection it becomes a limit on how long a chat may stay quiet, and
    // the connection is torn down mid-conversation with nothing wrong.
    //
    // The timeout is configured down to two seconds so the test can prove it
    // in seconds rather than in the thirty the default would take. The
    // mechanism is identical; only the number moved.
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();

    let node = Node::start().await;
    let proxy = Proxy::start(node.addr, 2).await;

    let handshake = handshake(proxy.addr, "/ws", None).await;
    assert_switched_protocols(&handshake);
    let mut socket = handshake.into_socket().await;

    socket
        .send(Message::Text("before the pause".to_string()))
        .await
        .expect("the client can send");
    assert_eq!(next_text(&mut socket).await, "text:before the pause");

    // Nobody says anything for longer than the upstream read timeout. This
    // wait is the subject of the test, not a way of coordinating with it.
    tokio::time::sleep(Duration::from_secs(4)).await;

    socket
        .send(Message::Text("after the pause".to_string()))
        .await
        .expect("the client can still send after an idle period");
    assert_eq!(
        next_text(&mut socket).await,
        "text:after the pause",
        "the connection did not survive being idle for longer than the \
         upstream read timeout"
    );

    socket
        .send(Message::Close(None))
        .await
        .expect("the client can close");
    drop(proxy);
    node.stop();
}
