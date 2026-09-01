//! A backend and a real `deis-proxy` process, shared by the integration tests
//! in this directory.
//!
//! The proxy is the binary from `target/debug`, started on a real port with a
//! real configuration file, so what the tests exercise is the process an
//! operator runs rather than a `ProxyHttp` built in-process. The backend
//! speaks HTTP/1.1 by hand for the same reason: the bytes on both sides of
//! the proxy are bytes these tests wrote, so a proxy that answered a request
//! itself instead of relaying it is visible.

// Each integration test binary compiles this module whole and uses part of
// it; the unused part is not dead code, it is another file's.
#![allow(dead_code)]

use std::io::ErrorKind;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The nonce and the answer to it, both copied from RFC 6455 section 1.3.
///
/// Fixed on purpose: a generated key makes `Sec-WebSocket-Accept` a value a
/// test has to trust a client library about, while for this one the RFC
/// publishes the answer and the test can compare the bytes that came back.
pub const HANDSHAKE_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
pub const HANDSHAKE_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

/// The proxy's `max_request_body_bytes` in these tests. Small enough that a
/// body over it fits in one write, large enough that no handshake comes near
/// it.
pub const BODY_CEILING: usize = 1024;

/// What the backend answers on a path it has no special handling for.
pub const BACKEND_BODY: &str = "backend";

// ----------------------------------------------------------------------
// A backend that speaks HTTP/1.1 by hand.
// ----------------------------------------------------------------------

/// The node stand-in, on a port the kernel picked.
///
/// Three routes, one per shape these tests need:
///
/// * `/ws` answers `101` and then echoes every byte back, which is what an
///   upgraded connection looks like from the outside;
/// * `/upload` reads a chunked body to its terminating chunk and answers
///   `200 uploaded` only then, so a proxy that carried a whole upload is
///   visible as a success and one that refused it is visible as a refusal;
/// * anything else answers `200 backend`, which is the readiness probe.
pub struct Backend {
    pub addr: SocketAddr,
    server: tokio::task::JoinHandle<()>,
    completed_uploads: Arc<AtomicUsize>,
    /// Every request head that reached this side of the hop, in order.
    ///
    /// What a node was actually handed, which is the only place a test can
    /// see the headers deis-proxy decided on rather than the ones the client
    /// sent. A request the proxy refused leaves nothing here at all, which is
    /// how "it was not forwarded" is asserted.
    heads: Arc<Mutex<Vec<String>>>,
}

impl Backend {
    pub async fn start() -> Backend {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a port for the backend");
        let addr = listener.local_addr().expect("the backend address");
        let completed_uploads = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&completed_uploads);
        let heads = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&heads);
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve_one(stream, Arc::clone(&counter), Arc::clone(&seen)));
            }
        });
        Backend {
            addr,
            server,
            completed_uploads,
            heads,
        }
    }

    /// The request heads this backend has been handed that came from a
    /// client, newest last.
    ///
    /// The edge's own health probes are left out. It asks every upstream
    /// `GET /health` with `x-deis-app-id: _system` before it will route
    /// anything to it (see `deis_proxy::config::system::health`), so a
    /// backend sees requests no test made; every assertion here is about what
    /// a client's request turned into after crossing the hop, and a probe is
    /// not one. [`Backend::probe_heads`] has them.
    pub fn heads(&self) -> Vec<String> {
        self.all_heads()
            .into_iter()
            .filter(|head| !is_health_probe(head))
            .collect()
    }

    /// The edge's health probes, for a test that wants to prove it was asked.
    pub fn probe_heads(&self) -> Vec<String> {
        self.all_heads()
            .into_iter()
            .filter(|head| is_health_probe(head))
            .collect()
    }

    /// Everything this backend was handed, probes included.
    pub fn all_heads(&self) -> Vec<String> {
        self.heads
            .lock()
            .expect("the head log is not poisoned")
            .clone()
    }

    /// One header value out of the last request head, lower-cased name, or
    /// `None` when the head does not carry it.
    pub fn last_header(&self, name: &str) -> Option<String> {
        let heads = self.heads();
        let head = heads.last()?;
        head.lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.trim().eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim().to_string())
    }

    /// How many uploads arrived here complete, terminating chunk and all.
    ///
    /// The witness a caller can trust unconditionally. What the client reads
    /// back from the proxy is subject to the reset described on
    /// [`read_until_closed`], but whether the node was made to swallow an
    /// upload is decided here, on this side of the hop, and no race on the
    /// client's socket can change the answer.
    pub fn completed_uploads(&self) -> usize {
        self.completed_uploads.load(Ordering::SeqCst)
    }

    pub fn stop(self) {
        self.server.abort();
    }
}

async fn serve_one(
    mut stream: TcpStream,
    completed_uploads: Arc<AtomicUsize>,
    heads: Arc<Mutex<Vec<String>>>,
) {
    let Some(head) = read_head(&mut stream).await else {
        return;
    };
    heads
        .lock()
        .expect("the head log is not poisoned")
        .push(head.clone());
    let path = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();

    if path == "/ws" {
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {HANDSHAKE_ACCEPT}\r\n\
             \r\n"
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
        // The tunnel: whatever arrives goes straight back, so a test can
        // assert on bytes it chose rather than on a protocol implementation.
        let mut buf = vec![0u8; 8192];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if stream.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            }
        }
    }

    if path == "/upload" {
        // Answer only once the whole chunked body has arrived. A proxy that
        // cut the upload short never gets here, and its client sees the
        // refusal instead of this.
        let mut body = Vec::new();
        let mut buf = vec![0u8; 8192];
        while !body.ends_with(b"0\r\n\r\n") {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => body.extend_from_slice(&buf[..n]),
            }
        }
        completed_uploads.fetch_add(1, Ordering::SeqCst);
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nuploaded")
            .await;
        return;
    }

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BACKEND_BODY}",
        BACKEND_BODY.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

/// Write applications into the proxy's database, in the shape the control
/// plane would have pushed them.
///
/// Through the crate's own `ConfigDatabase` rather than by writing SQL here,
/// so a test cannot end up seeding a schema the proxy does not read.
async fn seed_apps(database: &Path, apps: &[serde_json::Value]) {
    use std::collections::HashMap;
    let mut db = deis_proxy::config::ConfigDatabase::init(database)
        .await
        .expect("the proxy's database opens");
    let configs: HashMap<String, serde_json::Value> = apps
        .iter()
        .map(|app| {
            (
                app["id"].as_str().expect("every app has an id").to_string(),
                app.clone(),
            )
        })
        .collect();
    db.save_all_app_configs(&configs, chrono::Utc::now())
        .await
        .expect("the applications are written");
}

/// Whether a head is the edge's own health probe rather than a client's
/// request.
///
/// Both halves are needed and neither alone would do: `GET /health` is a path
/// a client may ask any application for, and `x-deis-app-id: _system` is a
/// header a client may send - the edge refuses that pair from a client, which
/// is exactly why the pair only ever arrives here from the edge itself.
fn is_health_probe(head: &str) -> bool {
    let Some(request_line) = head.lines().next() else {
        return false;
    };
    let target = request_line.split_whitespace().nth(1).unwrap_or_default();
    target == "/health"
        && head.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.trim().eq_ignore_ascii_case("x-deis-app-id") && value.trim() == "_system"
            })
        })
}

/// One plain HTTP request to an address, head and body together.
///
/// Not a method on [`Proxy`] because a listener can outlive the process that
/// first bound it: after a zero-downtime upgrade the port is served by the
/// replacement, and a test that keeps asking has to ask the address rather
/// than the process it started.
pub async fn request_to(addr: SocketAddr, path: &str, host: &str) -> String {
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        return String::from("the connection was refused");
    };
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return String::from("the request could not be written");
    }
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer).await;
    answer
}

/// Read a message head and stop on the blank line that ends it.
///
/// One byte at a time on purpose: a buffered read would pull whatever follows
/// the head into a buffer the caller never sees, which on an upgraded
/// connection is the peer's first frames.
pub async fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte).await {
            Ok(0) | Err(_) => return None,
            Ok(_) => raw.push(byte[0]),
        }
        if raw.len() > 8192 {
            return None;
        }
    }
    String::from_utf8(raw).ok()
}

/// Send a request head and read the response head back, leaving the socket
/// positioned at the first byte after it.
pub async fn exchange_head(stream: &mut TcpStream, request: &str) -> String {
    stream
        .write_all(request.as_bytes())
        .await
        .expect("the request head is sent");
    read_head(stream)
        .await
        .expect("a response head comes back before the connection closes")
}

/// Read whatever the peer sends until it closes, and count a reset as a close.
///
/// A proxy that refuses a body part-way through has, by construction, not read
/// the rest of it: it writes `413` and closes with the remainder of the upload
/// still sitting in its receive queue. Closing a socket that still holds
/// unread bytes sends `RST` rather than `FIN`, and an `RST` makes the kernel on
/// this side discard everything it was holding for us -- the `413` included.
/// So on a refused upload a client sees, entirely according to how the two
/// kernels were scheduled, either the refusal or a bare
/// `ECONNRESET` with nothing before it.
///
/// Both are the same event, so this returns the bytes that did arrive instead
/// of failing on the reset, and the caller asserts on
/// [`Backend::completed_uploads`] and on the proxy's log for the part that must
/// hold every time.
pub async fn read_until_closed(stream: &mut TcpStream) -> String {
    let mut raw = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::ConnectionReset => break,
            Err(e) => panic!("reading the answer: {e}"),
        }
    }
    String::from_utf8_lossy(&raw).into_owned()
}

/// One chunk of a `Transfer-Encoding: chunked` body.
pub fn chunk(payload: &[u8]) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", payload.len()).into_bytes();
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\r\n");
    out
}

// ----------------------------------------------------------------------
// The proxy under test: the real binary.
// ----------------------------------------------------------------------

pub struct Proxy {
    child: Child,
    /// Plain HTTP listener.
    pub addr: SocketAddr,
    /// TLS listener, present only when a TLS port was asked for.
    pub tls_addr: Option<SocketAddr>,
    log: PathBuf,
    /// The directory holding the configuration, database and log. Kept
    /// alive for as long as the proxy, and shared between two proxies when a
    /// test restarts one.
    dir: Arc<tempfile::TempDir>,
}

/// Everything a test can vary about the proxy it starts.
///
/// A struct rather than more constructors because the ACME tests need to
/// choose the HTTP port before the proxy exists - the stub authority has to
/// be told where to fetch the challenge from - and to hand two successive
/// proxies the same directory, which is what "a restart finds what the last
/// process left" means.
pub struct ProxyOptions {
    pub upstream: SocketAddr,
    /// Plain HTTP port. `None` asks for one the kernel is not using.
    pub http_port: Option<u16>,
    /// TLS port, and the certificate to serve on it when one is named.
    pub tls: Option<TlsOptions>,
    /// Extra configuration appended verbatim, for tables this harness has no
    /// opinion about.
    pub extra_config: String,
    /// Where the upgrade socket goes. `None` puts it in the proxy's own
    /// directory; a test that wants to take the directory away while the
    /// proxy runs needs one of its own.
    pub upgrade_sock: Option<PathBuf>,
    /// Extra top-level keys, written before the first table.
    ///
    /// Separate from `extra_config` because TOML has no way back out of a
    /// table: a bare key appended after `[log.error_log]` becomes a key *of*
    /// that table, which the proxy refuses as an unknown field.
    pub top_level_config: String,
    /// Applications to write into the proxy's database before it starts, in
    /// the shape the control plane would have pushed them.
    pub apps: Vec<serde_json::Value>,
    /// Reuse a directory a previous proxy used, so its database, its
    /// certificate store and its account key are still there.
    pub dir: Option<Arc<tempfile::TempDir>>,
    /// Whether to wait for a request to come back from the backend before
    /// returning. With applications configured, an unknown host is a 404
    /// rather than a forwarded request, so that probe cannot be the readiness
    /// test; `false` waits for the port to answer anything at all.
    pub wait_for_forwarding: bool,
}

pub struct TlsOptions {
    pub port: Option<u16>,
    /// A certificate for `tls.default_certificate`, or nothing when the
    /// certificates are expected to arrive some other way.
    pub default_certificate: Option<(PathBuf, PathBuf)>,
}

impl Default for ProxyOptions {
    fn default() -> Self {
        Self {
            upstream: SocketAddr::from(([127, 0, 0, 1], 0)),
            http_port: None,
            tls: None,
            extra_config: String::new(),
            upgrade_sock: None,
            top_level_config: String::new(),
            apps: Vec::new(),
            dir: None,
            wait_for_forwarding: true,
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Proxy {
    /// Where `cargo build` leaves the proxy: a test binary lives in
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
    /// address is handed out, so this is a hint rather than a reservation; a
    /// proxy that loses the race dies on bind and `start` says so.
    fn free_port() -> u16 {
        StdTcpListener::bind("127.0.0.1:0")
            .expect("a free port")
            .local_addr()
            .expect("its address")
            .port()
    }

    /// A free port, exposed so a test can decide one before the proxy exists.
    pub fn pick_port() -> u16 {
        Proxy::free_port()
    }

    /// The proxy binary, for a test that needs to run it without this
    /// harness waiting for it to become ready - a test of a startup that is
    /// supposed to fail, for instance.
    pub fn binary_path() -> PathBuf {
        Proxy::binary()
    }

    /// Plain HTTP in front of `upstream`.
    pub async fn start(upstream: SocketAddr) -> Proxy {
        Proxy::launch(ProxyOptions {
            upstream,
            ..Default::default()
        })
        .await
    }

    /// As [`Proxy::start`], plus a TLS listener serving `cert_file` for every
    /// SNI name, since a bare configuration file can only carry the default
    /// certificate.
    pub async fn start_with_tls(upstream: SocketAddr, cert_file: &Path, key_file: &Path) -> Proxy {
        Proxy::launch(ProxyOptions {
            upstream,
            tls: Some(TlsOptions {
                port: None,
                default_certificate: Some((cert_file.to_owned(), key_file.to_owned())),
            }),
            ..Default::default()
        })
        .await
    }

    /// The directory this proxy's files are in, for a test that wants to
    /// start a second one over the same state.
    pub fn dir(&self) -> Arc<tempfile::TempDir> {
        Arc::clone(&self.dir)
    }

    pub async fn launch(options: ProxyOptions) -> Proxy {
        let ProxyOptions {
            upstream,
            http_port,
            tls,
            extra_config,
            upgrade_sock,
            top_level_config,
            apps,
            dir,
            wait_for_forwarding,
        } = options;
        let binary = Proxy::binary();
        assert!(
            binary.exists(),
            "{} is not built, so the hop these tests exist to exercise cannot be reached. \
             Build it first: cargo build -p deis-proxy",
            binary.display()
        );

        let dir = dir.unwrap_or_else(|| {
            Arc::new(tempfile::tempdir().expect("a directory for the proxy's files"))
        });
        let port = http_port.unwrap_or_else(Proxy::free_port);
        let tls_port = tls
            .as_ref()
            .map(|tls| tls.port.unwrap_or_else(Proxy::free_port));
        let config = dir.path().join("deis-proxy.toml");
        let log = dir.path().join("deis-proxy-error.log");
        let database = dir.path().join("deis-proxy.db");

        if !apps.is_empty() {
            seed_apps(&database, &apps).await;
        }

        // No [control] section and no applications: standalone, so the proxy
        // is in plain reverse proxy mode and forwards every request to the
        // one upstream without a routing decision getting in the way.
        let mut text = format!(
            "listen_addr = [\"127.0.0.1\"]\n\
             listen_ports = [{port}]\n\
             database = \"{db}\"\n\
             pid_file = \"{pid}\"\n\
             upgrade_sock = \"{sock}\"\n\
             max_request_body_bytes = {BODY_CEILING}\n\
             {top_level_config}\
             \n\
             [[upstreams]]\n\
             host = \"{host}\"\n\
             port = {upstream_port}\n\
             \n\
             [log.error_log]\n\
             level = \"debug\"\n\
             path = \"{log}\"\n",
            db = database.display(),
            pid = dir.path().join("deis-proxy.pid").display(),
            sock = upgrade_sock
                .unwrap_or_else(|| dir.path().join("upgrade.sock"))
                .display(),
            host = upstream.ip(),
            upstream_port = upstream.port(),
            log = log.display(),
        );
        if let Some(tls_port) = tls_port {
            text.push_str(&format!("\n[tls]\nports = [{tls_port}]\n"));
            if let Some((cert_file, key_file)) = tls
                .as_ref()
                .and_then(|tls| tls.default_certificate.as_ref())
            {
                text.push_str(&format!(
                    "\n[tls.default_certificate]\n\
                     cert_file = \"{cert}\"\n\
                     key_file = \"{key}\"\n",
                    cert = cert_file.display(),
                    key = key_file.display(),
                ));
            }
        }
        text.push_str(&extra_config);
        std::fs::write(&config, text).expect("the proxy configuration is written");

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
            tls_addr: tls_port.map(|p| SocketAddr::from(([127, 0, 0, 1], p))),
            log,
            dir,
        };
        if wait_for_forwarding {
            proxy.wait_until_forwarding().await;
        } else {
            proxy.wait_until_answering().await;
        }
        proxy
    }

    /// Block until the proxy answers anything at all on its HTTP port.
    ///
    /// The readiness test for a proxy that has applications configured: such
    /// a proxy answers 404 to a host it does not know, so "the backend's body
    /// came back" is not a signal it can ever produce. What is being waited
    /// for is still a real condition - a complete HTTP response from the
    /// process under test - and the process is re-checked on every round so a
    /// proxy that died on bind is reported rather than waited out.
    async fn wait_until_answering(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "deis-proxy exited during startup with {status}; its log said:\n{}",
                    self.log_tail()
                );
            }
            if let Ok(mut stream) = TcpStream::connect(self.addr).await {
                let request = format!(
                    "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                    self.addr
                );
                if stream.write_all(request.as_bytes()).await.is_ok() {
                    let mut answer = String::new();
                    if stream.read_to_string(&mut answer).await.is_ok()
                        && answer.starts_with("HTTP/1.1 ")
                    {
                        return;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "deis-proxy never answered on {} within 30s; its log said:\n{}",
            self.addr,
            self.log_tail()
        );
    }

    /// One plain HTTP request through the proxy carrying extra headers, head
    /// and body together.
    ///
    /// `headers` are written verbatim as `name: value` lines, so a test can
    /// send exactly what a client would - `x-deis-app-id` included, which is
    /// the whole point: it is a header the edge is supposed to decide and a
    /// client is free to guess at.
    pub async fn request_with_headers(
        &self,
        path: &str,
        host: &str,
        headers: &[(&str, &str)],
    ) -> String {
        let mut stream = TcpStream::connect(self.addr)
            .await
            .expect("the proxy accepts a connection");
        let mut request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("Connection: close\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("the request is sent");
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer).await;
        answer
    }

    /// One plain HTTP request through the proxy, head and body together.
    pub async fn request(&self, path: &str, host: &str) -> String {
        let mut stream = TcpStream::connect(self.addr)
            .await
            .expect("the proxy accepts a connection");
        let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("the request is sent");
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer).await;
        answer
    }

    /// Stop the proxy and wait for it to be gone, leaving its directory
    /// behind for whatever starts next.
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Block until a request comes back through the proxy from the backend.
    ///
    /// Answering the port is not enough: the upstream pool is filled by a
    /// background service, and until it has a backend the proxy answers 503
    /// perfectly promptly. So the probe is a real request whose answer only
    /// the backend can produce. Each round re-checks that the process is
    /// still alive, because a proxy that died on bind would otherwise be
    /// waited for until the deadline.
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
                Ok(()) => return,
                Err(e) => last = e,
            }
            // Paced retry against a real readiness probe, not a wait for a
            // guessed duration: the loop above decides when it is ready.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "deis-proxy never forwarded to the backend within 30s: {last}; its log said:\n{}",
            self.log_tail()
        );
    }

    async fn probe(&self) -> Result<(), String> {
        let mut stream = TcpStream::connect(self.addr)
            .await
            .map_err(|e| format!("connecting to the proxy: {e}"))?;
        let request = format!(
            "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.addr
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| format!("sending the probe: {e}"))?;
        let mut answer = String::new();
        stream
            .read_to_string(&mut answer)
            .await
            .map_err(|e| format!("reading the probe answer: {e}"))?;
        if answer.ends_with(BACKEND_BODY) {
            Ok(())
        } else {
            Err(format!("the proxy answered {answer:?}"))
        }
    }

    /// Everything the proxy has written to its error log so far.
    pub fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// The process id of the running proxy, for a test that has to signal it.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether the proxy process is still running.
    ///
    /// A test that expects a process to survive something has to be able to
    /// say so about the process itself, not only about its port: a port that
    /// still answers may be a replacement that took the listener over.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn log_tail(&self) -> String {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        text.lines()
            .rev()
            .take(30)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    }
}
