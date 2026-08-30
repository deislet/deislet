//! Data plane: the pingora `ProxyHttp` implementation.
//!
//! Request flow:
//!
//! 1. Resolve the application: a non-empty `x-deis-app-id` header names the
//!    app and must match a configured id (an unknown id is a 404, never a
//!    fallback to the `Host`); otherwise the `Host` name is matched against
//!    the route table. Unknown hosts get a 404 once any application is
//!    configured; with none configured every request is forwarded unchanged
//!    so a bare instance still works as a plain reverse proxy.
//! 2. Disabled applications get a 403. Plain HTTP requests for an application
//!    whose certificate is actually loaded are redirected to HTTPS when the
//!    policy says so.
//! 3. A request body larger than `max_request_body_bytes` is answered 413,
//!    either from what the client declared in `Content-Length` (before an
//!    upstream is picked at all) or from the bytes as they stream past. The
//!    body is not buffered here, so without this the whole over-large upload
//!    would be carried across the edge-to-node hop before the node refused it.
//!    An upgraded connection is exempt: its frames are a conversation with no
//!    total size, not a body a node will buffer.
//! 4. A backend is picked from the shared upstream pool and the request is
//!    forwarded. `x-deis-app-id` is set to the id deis-proxy decided on and
//!    `x-deis-environment` to that app's configured environment (defaulting
//!    to production when the route names none), replacing whatever the
//!    client sent for either header, so deis-runtime only ever sees values
//!    that passed the checks above.
//! 5. A request the upstream upgrades (a WebSocket handshake) becomes a
//!    tunnel: everything after the 101 is relayed in both directions until
//!    one end closes. The upstream read timeout is a bound on how long a
//!    reply may take and is not applied to that tunnel, so a connection is
//!    not closed for being quiet.

// Re-exported so every component in the platform spells these the same
// way; see deis_wire::conventions for why they live in one place.
pub use deis_wire::conventions::{APP_ID_HEADER, ENVIRONMENT_HEADER};

pub mod access_log;
pub mod balancer;
pub mod service;

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::modules::http::HttpModules;
use pingora::modules::http::compression::{ResponseCompression, ResponseCompressionBuilder};
use pingora::protocols::http::compression::Algorithm;
use pingora::proxy::{ProxyHttp, Session};
use pingora::upstreams::peer::HttpPeer;
use pingora::{Error, ErrorType};
use tracing::{debug, warn};

use crate::config::routes::{RouteTable, normalize_host};
use crate::config::system::{CompressionAlgo, DEFAULT_MAX_REQUEST_BODY_BYTES};
use crate::config::{AppConfig, ConfigManager};

pub use access_log::AccessLog;
pub use balancer::Balancer;
pub use service::ExitOnPanic;

pub struct DeisProxy {
    config: Arc<ConfigManager>,
    balancer: Balancer,
    access_log: Option<AccessLog>,
}

/// Per-request state shared between proxy phases.
pub struct RequestCtx {
    started: Option<Instant>,
    /// Normalized request host (no port, lowercase).
    pub host: String,
    pub app_id: Option<String>,
    /// Environment of the resolved app (`production` or a preview name).
    /// Set together with `app_id`; `None` only in plain reverse proxy mode
    /// where no application was resolved at all.
    pub environment: Option<String>,
    /// Backend address the request was sent to, once selected.
    pub upstream: Option<String>,
    /// Whether the client connection is TLS.
    pub tls: bool,
    /// Ceiling on this request's body, taken from the configuration once when
    /// the request starts so a hot reload cannot change the answer half way
    /// through a stream.
    pub max_request_body_bytes: usize,
    /// Request body bytes carried so far, counted as they stream past.
    /// `Content-Length` is a claim; this is what actually arrived.
    body_bytes: usize,
}

/// Written out rather than derived so the ceiling is the documented default in
/// a context built any other way than [`DeisProxy::new_ctx`]. A derived
/// `Default` would make it 0, which reads as "refuse every body".
impl Default for RequestCtx {
    fn default() -> Self {
        Self {
            started: None,
            host: String::new(),
            app_id: None,
            environment: None,
            upstream: None,
            tls: false,
            max_request_body_bytes: DEFAULT_MAX_REQUEST_BODY_BYTES,
            body_bytes: 0,
        }
    }
}

impl RequestCtx {
    pub fn elapsed(&self) -> Duration {
        self.started.map(|t| t.elapsed()).unwrap_or_default()
    }

    /// Account for one streamed chunk of request body.
    ///
    /// `false` means this request has now gone past the ceiling and must not
    /// be carried any further. The count is of bytes that actually arrived, so
    /// it catches a chunked body (which declares no length at all) and one
    /// whose `Content-Length` under-declared.
    fn carry_body(&mut self, chunk_len: usize) -> bool {
        self.body_bytes = self.body_bytes.saturating_add(chunk_len);
        self.body_bytes <= self.max_request_body_bytes
    }
}

impl DeisProxy {
    pub fn new(
        config: Arc<ConfigManager>,
        balancer: Balancer,
        access_log: Option<AccessLog>,
    ) -> Self {
        Self {
            config,
            balancer,
            access_log,
        }
    }
}

/// What the client says it is about to send, when it says anything usable.
///
/// A `Content-Length` that is missing, not a number, or negative is not read as
/// zero: such a request still has its bytes counted as they arrive.
fn declared_body_length(session: &Session) -> Option<usize> {
    session
        .get_header("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<usize>().ok())
}

/// The refusal a client gets, naming the ceiling so the number is actionable
/// rather than a bare 413.
fn over_the_body_ceiling(limit: usize) -> Bytes {
    Bytes::from(format!(
        "request body is larger than this proxy carries: the limit is {limit} bytes\n"
    ))
}

/// Host the client asked for: the URI authority (HTTP/2) or the `Host`
/// header (HTTP/1.1), normalized. Empty when the request carries neither;
/// pingora does not enforce the HTTP/1.1 `Host` requirement, so this is
/// reachable for any protocol version.
fn request_host(session: &Session) -> String {
    let req = session.req_header();
    let raw = req
        .uri
        .host()
        .map(str::to_string)
        .or_else(|| {
            session
                .get_header("host")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        })
        .unwrap_or_default();
    normalize_host(&raw)
}

/// Authority for an HTTPS redirect: a normalized host (IPv6 literals have
/// lost their brackets) plus the TLS port, omitted when it is the default.
fn https_authority(host: &str, port: u16) -> String {
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    if port == 443 {
        host
    } else {
        format!("{host}:{port}")
    }
}

/// Outcome of matching a request against the route table.
#[derive(Debug)]
enum Resolution {
    /// No application is configured at all: plain reverse proxy mode.
    Passthrough,
    /// Applications exist but none matches: 404.
    NotFound,
    App(Arc<AppConfig>),
}

/// The routing rule. A non-empty `x-deis-app-id` is authoritative: it must
/// name a configured app, and an unknown value is rejected rather than
/// silently replaced by whatever the `Host` maps to, because the id is what
/// deis-runtime routes on and the two must not disagree. An empty header counts as
/// absent.
fn resolve_app(routes: &RouteTable, requested_id: Option<&str>, host: &str) -> Resolution {
    if routes.is_empty() {
        return Resolution::Passthrough;
    }
    let app = match requested_id.filter(|id| !id.is_empty()) {
        Some(id) => routes.app_by_id(id),
        None => routes.app_by_host(host),
    };
    match app {
        Some(app) => Resolution::App(Arc::clone(app)),
        None => Resolution::NotFound,
    }
}

/// Set `x-deis-app-id` and `x-deis-environment` on the upstream request from
/// what deis-proxy resolved, replacing whatever the client sent under either
/// name (`insert_header` replaces rather than appends, and normalizes header
/// case). With no application resolved (plain reverse proxy mode) both
/// fields are `None` and both headers are left exactly as the client sent
/// them, since pingora already copied the downstream headers over.
fn stamp_routing_headers(
    upstream_request: &mut RequestHeader,
    ctx: &RequestCtx,
) -> pingora::Result<()> {
    if let Some(app_id) = &ctx.app_id {
        upstream_request.insert_header(APP_ID_HEADER, app_id.as_str())?;
    }
    // An app that names no environment still carries one here: see
    // AppConfig::environment's default, which guarantees this is never
    // empty once an app has been resolved, so the header on the wire is
    // never ambiguous.
    if let Some(environment) = &ctx.environment {
        upstream_request.insert_header(ENVIRONMENT_HEADER, environment.as_str())?;
    }
    Ok(())
}

#[async_trait]
impl ProxyHttp for DeisProxy {
    type CTX = RequestCtx;

    fn new_ctx(&self) -> RequestCtx {
        RequestCtx {
            started: Some(Instant::now()),
            max_request_body_bytes: self.config.get_system_config().max_request_body_bytes(),
            ..Default::default()
        }
    }

    fn init_downstream_modules(&self, modules: &mut HttpModules) {
        // Registered disabled; the level is set per request from the current
        // configuration so compression settings hot-reload.
        modules.add_module(ResponseCompressionBuilder::enable(0));
    }

    async fn early_request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut RequestCtx,
    ) -> pingora::Result<()> {
        let system = self.config.get_system_config();
        if let Some(compression) = &system.compression
            && compression.enabled
            && let Some(module) = session
                .downstream_modules_ctx
                .get_mut::<ResponseCompression>()
        {
            let algorithm = match compression.algo {
                CompressionAlgo::Gzip => Algorithm::Gzip,
                CompressionAlgo::Brotli => Algorithm::Brotli,
                CompressionAlgo::Zstd => Algorithm::Zstd,
            };
            module.adjust_algorithm_level(algorithm, compression.level);
        }
        Ok(())
    }

    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut RequestCtx,
    ) -> pingora::Result<bool> {
        ctx.tls = session
            .digest()
            .and_then(|digest| digest.ssl_digest.as_ref())
            .is_some();
        ctx.host = request_host(session);

        // Checked before the route table, because refusing here costs nothing:
        // no upstream has been picked and not a byte of the body has been
        // read. A client that declares an over-large upload is told so on the
        // connection it already has.
        if let Some(declared) = declared_body_length(session)
            && declared > ctx.max_request_body_bytes
        {
            warn!(
                host = %ctx.host,
                declared,
                limit = ctx.max_request_body_bytes,
                "refusing a request that declares a body larger than this proxy carries; it was \
                 not forwarded to any node"
            );
            session
                .respond_error_with_body(413, over_the_body_ceiling(ctx.max_request_body_bytes))
                .await?;
            return Ok(true);
        }

        let routes = self.config.routes();
        let requested_id = session
            .get_header(APP_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        let app = match resolve_app(&routes, requested_id.as_deref(), &ctx.host) {
            Resolution::Passthrough => {
                debug!(host = %ctx.host, "no applications configured; forwarding unchanged");
                return Ok(false);
            }
            Resolution::NotFound => {
                debug!(host = %ctx.host, app_id = ?requested_id, "no application for request");
                session
                    .respond_error_with_body(
                        404,
                        Bytes::from_static(b"no application for this host\n"),
                    )
                    .await?;
                return Ok(true);
            }
            Resolution::App(app) => app,
        };

        if !app.enabled {
            debug!(host = %ctx.host, app_id = %app.id, environment = %app.environment, "application disabled");
            session
                .respond_error_with_body(403, Bytes::from_static(b"application disabled\n"))
                .await?;
            return Ok(true);
        }
        ctx.app_id = Some(app.id.clone());
        ctx.environment = Some(app.environment.clone());

        let system = self.config.get_system_config();
        let tls_ports = system.tls_ports();
        if !ctx.tls
            && system.https().auto_redirect_to_https
            && routes.https_available(&app)
            && let Some(port) = tls_ports.first()
        {
            if ctx.host.is_empty() {
                // Only reachable through the explicit app id header; the
                // redirect is a convenience, so forward rather than build a
                // Location without a host.
                debug!(app_id = %app.id, "request carries no host; cannot build an HTTPS redirect, forwarding");
                return Ok(false);
            }
            let path = session
                .req_header()
                .uri
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or("/");
            let authority = https_authority(&ctx.host, *port);
            let location = format!("https://{authority}{path}");
            debug!(host = %ctx.host, app_id = %app.id, %location, "redirecting to HTTPS");

            let mut resp = ResponseHeader::build(301, Some(2))?;
            resp.insert_header("Location", location.as_str())?;
            resp.insert_header("Content-Length", "0")?;
            session.write_response_header(Box::new(resp), true).await?;
            return Ok(true);
        }

        Ok(false)
    }

    /// The ceiling on a body that declared nothing, or that declared less than
    /// it sends.
    ///
    /// `Content-Length` is refused in `request_filter`, before an upstream
    /// exists. This is the other half: a chunked body announces no length at
    /// all, and a header that under-declares is the obvious way past a check
    /// that only reads the header. Nothing is buffered here -- the bytes are
    /// counted as they stream through -- so the cost of the refusal is the
    /// chunk that crossed the line, not the upload.
    async fn request_body_filter(
        &self,
        session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut RequestCtx,
    ) -> pingora::Result<()> {
        // Pingora runs the client-to-upstream half of an upgraded connection
        // through this same filter, so the frames of a WebSocket arrive here
        // as "request body". They are not one: a request body is a bounded
        // thing a node buffers whole, while a socket is a conversation with no
        // total size, and counting it would close every chat that ever carried
        // more than the ceiling -- mid-sentence, for no fault. The same
        // reasoning takes the upstream read timeout off a tunnel in
        // `upstream_peer`.
        if session.is_upgrade_req() {
            return Ok(());
        }

        let Some(chunk) = body.as_ref() else {
            return Ok(());
        };
        if !ctx.carry_body(chunk.len()) {
            warn!(
                host = %ctx.host,
                app_id = ?ctx.app_id,
                carried = ctx.body_bytes,
                limit = ctx.max_request_body_bytes,
                "a request body went past the ceiling while streaming; the rest of the upload is \
                 not carried and the request is refused"
            );
            return Err(Error::explain(
                ErrorType::HTTPStatus(413),
                "request body is larger than this proxy carries",
            ));
        }
        Ok(())
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut RequestCtx,
    ) -> pingora::Result<Box<HttpPeer>> {
        let key = self.balancer.key(session);
        let Some(backend) = self.balancer.select(&key) else {
            warn!(host = %ctx.host, app_id = ?ctx.app_id, "no healthy upstream available");
            return Err(Error::explain(
                ErrorType::HTTPStatus(503),
                "no healthy upstream",
            ));
        };
        let Some(addr) = backend.addr.as_inet().copied() else {
            return Err(Error::explain(
                ErrorType::InternalError,
                "backend is not an inet address",
            ));
        };
        ctx.upstream = Some(addr.to_string());

        let system = self.config.get_system_config();
        // Offloading means the upstream leg is plain even when the client
        // leg is TLS; otherwise TLS is carried through to the backend.
        let tls_upstream = ctx.tls && !system.https().auto_offload_https;
        // SNI only matters on a TLS leg; leaving it empty on plain legs keeps
        // one connection pool per backend instead of one per client Host.
        let sni = if tls_upstream {
            ctx.host.clone()
        } else {
            String::new()
        };
        let mut peer = HttpPeer::new(addr, tls_upstream, sni);
        peer.options.connection_timeout = Some(system.upstream_connect_timeout());
        // Bounds an in-flight write to the upstream, which only exists while
        // there are bytes to push, so it means the same thing on a tunnel as
        // on a request and stays set either way.
        peer.options.write_timeout = Some(system.upstream_read_timeout());
        // `upstream_read_timeout` answers "how long may an upstream take to
        // reply". Once a request has been upgraded there is no reply still
        // owed: the 101 has arrived and what follows is a tunnel that both
        // ends drive whenever they have something to say. Pingora sets the
        // peer's read timeout on the upstream connection once and applies it
        // to every read, the duplexed ones included, so leaving it set turns
        // it into "how long a WebSocket may stay quiet" - and a chat, a
        // notification stream or anything else waiting on a person is quiet
        // for far longer than that. It used to close every WebSocket after
        // `upstream_read_timeout` (30s by default) with nothing wrong,
        // mid-conversation and with no close frame.
        //
        // It is cleared rather than raised because any finite value is the
        // same bug at a different number. The cost is that an upstream which
        // accepts an upgrade and then never answers holds that connection
        // until the client goes away; the connection still ends the moment
        // either end closes, and unreachable backends are taken out of
        // rotation by the health check rather than by this timeout.
        if session.is_upgrade_req() {
            debug!(
                host = %ctx.host,
                app_id = ?ctx.app_id,
                upstream = %addr,
                "upgrade request: the upstream read timeout does not apply to the tunnel"
            );
        } else {
            peer.options.read_timeout = Some(system.upstream_read_timeout());
        }
        Ok(Box::new(peer))
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut RequestCtx,
    ) -> pingora::Result<()> {
        stamp_routing_headers(upstream_request, ctx)
    }

    async fn logging(&self, session: &mut Session, error: Option<&Error>, ctx: &mut RequestCtx) {
        if let Some(log) = &self.access_log {
            log.record(session, ctx);
        }
        let status = session
            .response_written()
            .map(|resp| resp.status.as_u16())
            .unwrap_or(0);
        match error {
            Some(e) => warn!(
                host = %ctx.host,
                app_id = ?ctx.app_id,
                environment = ?ctx.environment,
                upstream = ?ctx.upstream,
                status,
                elapsed_ms = ctx.elapsed().as_millis() as u64,
                error = %e,
                "request failed"
            ),
            None => debug!(
                host = %ctx.host,
                app_id = ?ctx.app_id,
                environment = ?ctx.environment,
                upstream = ?ctx.upstream,
                status,
                elapsed_ms = ctx.elapsed().as_millis() as u64,
                "request completed"
            ),
        }
    }
}

#[cfg(test)]
mod tests {

    use std::collections::HashMap;

    use pingora::http::Method;
    use serde_json::json;

    use super::*;

    fn table(apps: &[(&str, &str)]) -> RouteTable {
        let apps: HashMap<String, serde_json::Value> = apps
            .iter()
            .map(|(id, domain)| {
                (
                    id.to_string(),
                    json!({
                        "id": id,
                        "description": "",
                        "domains": [domain],
                        "certificates": [],
                        "enabled": true,
                        "cache": [],
                    }),
                )
            })
            .collect();
        RouteTable::build(&apps, None)
    }

    fn resolved_id(resolution: Resolution) -> Option<String> {
        match resolution {
            Resolution::App(app) => Some(app.id.clone()),
            _ => None,
        }
    }

    fn resolved_environment(resolution: Resolution) -> Option<String> {
        match resolution {
            Resolution::App(app) => Some(app.environment.clone()),
            _ => None,
        }
    }

    #[test]
    fn header_wins_when_it_names_a_known_app() {
        let routes = table(&[("shop", "shop.example.com"), ("blog", "blog.example.com")]);
        let resolution = resolve_app(&routes, Some("blog"), "shop.example.com");
        assert_eq!(resolved_id(resolution).as_deref(), Some("blog"));
    }

    #[test]
    fn unknown_header_is_rejected_even_for_a_known_host() {
        let routes = table(&[("shop", "shop.example.com")]);
        assert!(matches!(
            resolve_app(&routes, Some("Shop"), "shop.example.com"),
            Resolution::NotFound
        ));
        assert!(matches!(
            resolve_app(&routes, Some("other"), "shop.example.com"),
            Resolution::NotFound
        ));
    }

    #[test]
    fn empty_header_falls_back_to_the_host() {
        let routes = table(&[("shop", "shop.example.com")]);
        let resolution = resolve_app(&routes, Some(""), "shop.example.com");
        assert_eq!(resolved_id(resolution).as_deref(), Some("shop"));
        let resolution = resolve_app(&routes, None, "shop.example.com");
        assert_eq!(resolved_id(resolution).as_deref(), Some("shop"));
        assert!(matches!(
            resolve_app(&routes, None, "other.example.com"),
            Resolution::NotFound
        ));
    }

    #[test]
    fn no_apps_means_passthrough() {
        let routes = table(&[]);
        assert!(matches!(
            resolve_app(&routes, Some("anything"), "any.example.com"),
            Resolution::Passthrough
        ));
    }

    #[test]
    fn resolved_app_defaults_to_the_production_environment() {
        // The fixture apps above never mention "environment"; a route
        // pushed without one must still resolve to a concrete value rather
        // than leave it unset.
        let routes = table(&[("shop", "shop.example.com")]);
        let resolution = resolve_app(&routes, None, "shop.example.com");
        assert_eq!(
            resolved_environment(resolution).as_deref(),
            Some(deis_wire::conventions::DEFAULT_ENVIRONMENT)
        );
    }

    fn header_str(header: &RequestHeader, name: &str) -> Option<String> {
        header
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    fn empty_upstream_request() -> RequestHeader {
        RequestHeader::build(Method::GET, b"/", None).unwrap()
    }

    #[test]
    fn stamp_overwrites_a_client_supplied_app_id_and_environment() {
        let mut upstream_request = empty_upstream_request();
        upstream_request
            .insert_header(APP_ID_HEADER, "attacker-app")
            .unwrap();
        upstream_request
            .insert_header(ENVIRONMENT_HEADER, "attacker-env")
            .unwrap();

        let ctx = RequestCtx {
            app_id: Some("shop".to_string()),
            environment: Some("production".to_string()),
            ..Default::default()
        };
        stamp_routing_headers(&mut upstream_request, &ctx).unwrap();

        assert_eq!(
            header_str(&upstream_request, APP_ID_HEADER).as_deref(),
            Some("shop")
        );
        assert_eq!(
            header_str(&upstream_request, ENVIRONMENT_HEADER).as_deref(),
            Some("production")
        );
    }

    #[test]
    fn stamp_leaves_headers_untouched_in_passthrough_mode() {
        // No application was resolved (RequestCtx::default has both fields
        // as None), so whatever pingora already copied from the client must
        // survive unchanged: deis-proxy has no opinion to stamp.
        let mut upstream_request = empty_upstream_request();
        upstream_request
            .insert_header(APP_ID_HEADER, "client-value")
            .unwrap();
        upstream_request
            .insert_header(ENVIRONMENT_HEADER, "client-env")
            .unwrap();

        stamp_routing_headers(&mut upstream_request, &RequestCtx::default()).unwrap();

        assert_eq!(
            header_str(&upstream_request, APP_ID_HEADER).as_deref(),
            Some("client-value")
        );
        assert_eq!(
            header_str(&upstream_request, ENVIRONMENT_HEADER).as_deref(),
            Some("client-env")
        );
    }

    #[test]
    fn a_context_built_any_other_way_still_carries_a_ceiling() {
        // A derived `Default` would make this 0, which reads as "refuse every
        // body"; the hand-written one keeps the documented default.
        assert_eq!(
            RequestCtx::default().max_request_body_bytes,
            DEFAULT_MAX_REQUEST_BODY_BYTES
        );
    }

    #[test]
    fn a_body_inside_the_ceiling_streams_through_chunk_by_chunk() {
        let mut ctx = RequestCtx {
            max_request_body_bytes: 1024,
            ..Default::default()
        };
        for _ in 0..4 {
            assert!(
                ctx.carry_body(256),
                "1 KiB in 256-byte chunks is inside 1 KiB"
            );
        }
        assert_eq!(ctx.body_bytes, 1024);
    }

    #[test]
    fn a_body_is_refused_at_the_chunk_that_crosses_the_ceiling() {
        // The point of counting as the bytes stream: the refusal lands on the
        // chunk that crosses the line, so what the proxy carried is one chunk
        // over the limit rather than the whole upload.
        let mut ctx = RequestCtx {
            max_request_body_bytes: 1024,
            ..Default::default()
        };
        assert!(ctx.carry_body(1024));
        assert!(!ctx.carry_body(1), "one byte past the ceiling is past it");
    }

    #[test]
    fn an_undeclared_body_is_still_counted() {
        // A chunked upload declares no length, so `Content-Length` never sees
        // it; only the streaming count does.
        let mut ctx = RequestCtx {
            max_request_body_bytes: 8,
            ..Default::default()
        };
        assert!(ctx.carry_body(8));
        assert!(!ctx.carry_body(8));
    }

    #[test]
    fn the_refusal_names_the_ceiling_that_was_applied() {
        let body = over_the_body_ceiling(33_554_432);
        let text = String::from_utf8(body.to_vec()).expect("the refusal is text");
        assert!(
            text.contains("33554432"),
            "a refusal has to say what the limit is; it said {text:?}"
        );
    }

    #[test]
    fn https_authority_brackets_ipv6_and_omits_the_default_port() {
        assert_eq!(https_authority("::1", 19443), "[::1]:19443");
        assert_eq!(https_authority("::1", 443), "[::1]");
        assert_eq!(https_authority("shop.example.com", 443), "shop.example.com");
        assert_eq!(
            https_authority("shop.example.com", 8443),
            "shop.example.com:8443"
        );
    }
}
