//! Data plane: the pingora `ProxyHttp` implementation.
//!
//! Request flow:
//!
//! 1. Resolve the application. A non-empty `x-deis-app-id` header names the
//!    app and must match one this instance knows (an unknown id is a 404,
//!    never a fallback to the `Host`). Otherwise `Host` plus the request
//!    path are matched against the control plane's catalog
//!    ([`crate::catalog`]) first and the locally configured application
//!    list second - the catalog is the table an operator can change without
//!    touching this machine, and it is the only one that knows path
//!    prefixes and per-environment addresses. Unknown hosts get a 404 once
//!    either table has anything in it; with both empty every request is
//!    forwarded unchanged so a bare instance still works as a plain reverse
//!    proxy.
//! 2. Disabled applications get a 403. Plain HTTP requests for an application
//!    whose certificate is actually loaded are redirected to HTTPS when the
//!    policy says so.
//! 3. When automatic certificates are on, a request under
//!    `/.well-known/acme-challenge/` is answered here from the published
//!    answers and never routed. It is answered before the application is
//!    resolved, because the whole point of the challenge is that it arrives
//!    for a name that may have no certificate, no route and no application
//!    yet - and after the declared-body check, so the path cannot be used to
//!    walk past the body ceiling.
//! 4. A request body larger than `max_request_body_bytes` is answered 413,
//!    either from what the client declared in `Content-Length` (before an
//!    upstream is picked at all) or from the bytes as they stream past. The
//!    body is not buffered here, so without this the whole over-large upload
//!    would be carried across the edge-to-node hop before the node refused it.
//!    A connection that has *completed* an upgrade is exempt: its frames are a
//!    conversation with no total size, not a body a node will buffer. Asking
//!    for an upgrade is not enough; `request_body_filter` says why not.
//! 5. A backend is picked from the shared upstream pool and the request is
//!    forwarded. `x-deis-app-id` is set to the id deis-proxy decided on and
//!    `x-deis-environment` to that app's configured environment (defaulting
//!    to production when the route names none), replacing whatever the
//!    client sent for either header, so deis-runtime only ever sees values
//!    that passed the checks above.
//! 6. A request the upstream upgrades (a WebSocket handshake) becomes a
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

use crate::acme::{CHALLENGE_PREFIX, ChallengeResponder};
use crate::catalog::table::{CatalogRoutes, MatchSource};
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
    /// The published `http-01` answers, present only when `[tls.acme]` is
    /// switched on. `None` means the challenge path is routed like any other
    /// path, which is what an instance with its own certificate files wants.
    acme: Option<Arc<ChallengeResponder>>,
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
        acme: Option<Arc<ChallengeResponder>>,
    ) -> Self {
        Self {
            config,
            balancer,
            access_log,
            acme,
        }
    }

    /// Answer the authority's `http-01` fetch, or refuse it.
    ///
    /// Always answers - a challenge request is never forwarded to a node.
    /// The 404 matters as much as the 200: an authority that got a node's
    /// page back instead of a refusal would have to guess at what went
    /// wrong, and a token this instance never published must not be given
    /// anything that could be mistaken for an answer.
    async fn answer_acme_challenge(
        &self,
        session: &mut Session,
        responder: &ChallengeResponder,
        host: &str,
    ) -> pingora::Result<bool> {
        let path = session.req_header().uri.path().to_string();
        match responder.answer_for_path(&path) {
            Some(answer) => {
                // Every fetch of a published answer is one step of a
                // validation that is about to succeed or fail, and there are
                // only a handful of them per certificate, so each is worth a
                // line: it is the evidence that the name really does reach
                // this process.
                tracing::info!(
                    host,
                    "an http-01 challenge answer was served; the certificate authority reached \
                     this instance for this name"
                );
                let mut response = ResponseHeader::build(200, Some(2))?;
                // RFC 8555 section 8.3 asks for this content type, and an
                // authority is entitled to be strict about it.
                response.insert_header("Content-Type", "application/octet-stream")?;
                response.insert_header("Content-Length", answer.len().to_string())?;
                session
                    .write_response_header(Box::new(response), false)
                    .await?;
                session
                    .write_response_body(Some(Bytes::from(answer)), true)
                    .await?;
                Ok(true)
            }
            None => {
                debug!(
                    host,
                    path = %path,
                    "an ACME challenge was requested for a token this instance never published"
                );
                session
                    .respond_error_with_body(404, Bytes::from_static(b"no such acme challenge\n"))
                    .await?;
                Ok(true)
            }
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

/// Which table answered "which application is this".
///
/// Carried into the log lines because it is the first thing an operator
/// wants when a request went somewhere unexpected: a catalog route can be
/// changed through the API, a local one cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedFrom {
    /// The `x-deis-app-id` header named it outright.
    Header,
    /// A route in the control plane's catalog.
    Catalog,
    /// The automatic address of an environment.
    CatalogPreview,
    /// The application list configured on this machine.
    Local,
}

impl ResolvedFrom {
    fn as_str(self) -> &'static str {
        match self {
            ResolvedFrom::Header => "header",
            ResolvedFrom::Catalog => "catalog-route",
            ResolvedFrom::CatalogPreview => "catalog-preview",
            ResolvedFrom::Local => "local-config",
        }
    }
}

/// One resolved request: which application and environment it belongs to,
/// and the local policy for that application when this machine has one.
#[derive(Debug)]
struct Resolved {
    app_id: String,
    environment: String,
    /// Certificates, cache policy and the enabled flag. Absent for an
    /// application only the catalog knows, which is the ordinary case on an
    /// edge that is driven entirely from the control plane.
    config: Option<Arc<AppConfig>>,
    from: ResolvedFrom,
}

/// Outcome of matching a request.
#[derive(Debug)]
enum Resolution {
    /// Nothing is configured and nothing has been fetched: plain reverse
    /// proxy mode.
    Passthrough,
    /// Something is configured but none of it matches: 404.
    NotFound,
    Found(Resolved),
}

/// The routing rule.
///
/// A non-empty `x-deis-app-id` is authoritative: it must name an app this
/// instance knows, and an unknown value is rejected rather than silently
/// replaced by whatever the `Host` maps to, because the id is what
/// deis-runtime routes on and the two must not disagree. An empty header
/// counts as absent. The header names an *application* and not an
/// environment, so an app that only the catalog knows resolves to its
/// default environment.
///
/// Without the header, the catalog is consulted before the local list. The
/// catalog is what an operator edits through the API or the dashboard, and
/// it is strictly more specific: it matches on host *and* path prefix where
/// the local list matches on host alone. An application the catalog resolves
/// still picks up its local configuration when this machine has one, so a
/// certificate or a cache policy written here keeps applying.
fn resolve_app(
    routes: &RouteTable,
    catalog: &CatalogRoutes,
    requested_id: Option<&str>,
    host: &str,
    path: &str,
) -> Resolution {
    // Nothing here claims any host: forward the request untouched. Checked
    // last of all, after every lookup has failed, so an app the catalog
    // knows by id is still reachable by header on an instance whose routing
    // table happens to hold no rule.
    let nothing_claims_a_host = || {
        if routes.is_empty() && catalog.is_empty() {
            Resolution::Passthrough
        } else {
            Resolution::NotFound
        }
    };

    if let Some(id) = requested_id.filter(|id| !id.is_empty()) {
        if let Some(app) = routes.app_by_id(id) {
            return Resolution::Found(Resolved {
                app_id: app.id.clone(),
                environment: app.environment.clone(),
                config: Some(Arc::clone(app)),
                from: ResolvedFrom::Header,
            });
        }
        return match catalog.target_by_app_id(id) {
            Some(target) => Resolution::Found(Resolved {
                app_id: target.app_id,
                environment: target.environment,
                config: None,
                from: ResolvedFrom::Header,
            }),
            None => nothing_claims_a_host(),
        };
    }

    if let Some(found) = catalog.match_request(host, path) {
        // The same app id may also be configured locally; that entry is
        // this machine's policy for it, not a second opinion on where the
        // request should go.
        let config = routes.app_by_id(&found.target.app_id).map(Arc::clone);
        return Resolution::Found(Resolved {
            app_id: found.target.app_id,
            environment: found.target.environment,
            config,
            from: match found.source {
                MatchSource::Route => ResolvedFrom::Catalog,
                MatchSource::Preview => ResolvedFrom::CatalogPreview,
            },
        });
    }

    match routes.app_by_host(host) {
        Some(app) => Resolution::Found(Resolved {
            app_id: app.id.clone(),
            environment: app.environment.clone(),
            config: Some(Arc::clone(app)),
            from: ResolvedFrom::Local,
        }),
        None => nothing_claims_a_host(),
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

        // Before any routing decision. The authority fetches this path for a
        // name that, by construction, may have no certificate, no route and
        // no application here yet - so resolving an application first would
        // answer the first certificate this instance ever ordered with a 404.
        if let Some(responder) = &self.acme
            && session
                .req_header()
                .uri
                .path()
                .starts_with(CHALLENGE_PREFIX)
        {
            let responder = Arc::clone(responder);
            let host = ctx.host.clone();
            return self.answer_acme_challenge(session, &responder, &host).await;
        }

        let routes = self.config.routes();
        let catalog = self.config.catalog_routes();
        let requested_id = session
            .get_header(APP_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        // Resolved in its own statement so the borrow of the request header
        // ends here: the arms below need the session mutably, and the path is
        // read again rather than copied, which keeps an allocation off every
        // request.
        let resolution = resolve_app(
            &routes,
            &catalog,
            requested_id.as_deref(),
            &ctx.host,
            session.req_header().uri.path(),
        );

        let resolved = match resolution {
            Resolution::Passthrough => {
                debug!(
                    host = %ctx.host,
                    "no applications configured and no routing table fetched; forwarding unchanged"
                );
                return Ok(false);
            }
            Resolution::NotFound => {
                debug!(
                    host = %ctx.host,
                    path = %session.req_header().uri.path(),
                    app_id = ?requested_id,
                    catalog_version = %catalog.version(),
                    "no application for request"
                );
                session
                    .respond_error_with_body(
                        404,
                        Bytes::from_static(b"no application for this host\n"),
                    )
                    .await?;
                return Ok(true);
            }
            Resolution::Found(resolved) => resolved,
        };

        // A local entry that is switched off is this machine's kill switch
        // for that application; it applies however the request was routed.
        if let Some(config) = &resolved.config
            && !config.enabled
        {
            debug!(
                host = %ctx.host,
                app_id = %resolved.app_id,
                environment = %resolved.environment,
                "application disabled"
            );
            session
                .respond_error_with_body(403, Bytes::from_static(b"application disabled\n"))
                .await?;
            return Ok(true);
        }
        debug!(
            host = %ctx.host,
            path = %session.req_header().uri.path(),
            app_id = %resolved.app_id,
            environment = %resolved.environment,
            resolved_from = resolved.from.as_str(),
            "request routed"
        );
        ctx.app_id = Some(resolved.app_id.clone());
        ctx.environment = Some(resolved.environment.clone());

        let system = self.config.get_system_config();
        let tls_ports = system.tls_ports();
        // Whether a handshake for this request would actually succeed. An
        // application configured here answers on the certificates it names;
        // one only the catalog knows has none of its own, so the question is
        // whether any certificate would be served for this host - which is
        // how a wildcard certificate covers a whole preview zone.
        let https_ready = match &resolved.config {
            Some(config) => routes.https_available(config),
            None => routes.https_available_for_host(&ctx.host),
        };
        if !ctx.tls
            && system.https().auto_redirect_to_https
            && https_ready
            && let Some(port) = tls_ports.first()
        {
            if ctx.host.is_empty() {
                // Only reachable through the explicit app id header; the
                // redirect is a convenience, so forward rather than build a
                // Location without a host.
                debug!(app_id = %resolved.app_id, "request carries no host; cannot build an HTTPS redirect, forwarding");
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
            debug!(host = %ctx.host, app_id = %resolved.app_id, %location, "redirecting to HTTPS");

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
        //
        // The question is "has this connection been upgraded", not "did the
        // client ask for one". `is_upgrade_req` reads the request headers and
        // nothing else, so `Upgrade: websocket` plus `Transfer-Encoding:
        // chunked` used to exempt an ordinary upload from the ceiling for as
        // long as the node was willing to answer such a request normally -
        // the declared-length check in `request_filter` does not catch a
        // chunked body, because a chunked body declares no length. Pingora
        // sets `was_upgraded` only once the node has actually answered 101,
        // and resets the downstream body reader at that same moment, so every
        // byte that reaches this filter before the handshake completes is a
        // request body and is counted, and every byte after it is a frame and
        // is not.
        if session.was_upgraded() {
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

    /// Take compression off a response the node is still producing.
    ///
    /// A node answers with a `Content-Length` for everything it already holds
    /// -- a rendered page, a static file, a JSON document -- and without one
    /// only when the body is being written as it is made: a server-sent-events
    /// feed, a large object being passed through, any handler that returned a
    /// `ReadableStream`. That absence is the whole test here.
    ///
    /// Compression has to go for those, because the compressors are not flushed
    /// per chunk. `flate2` holds the bytes it was given until it has a block
    /// worth of them, so a 40-byte events frame handed to gzip comes back out
    /// as nothing at all, and the client sits waiting while the node believes
    /// it has already answered. What is meant to arrive frame by frame arrives
    /// in one lump minutes later, or not at all -- which is the same failure
    /// the whole streaming path exists to remove, moved from the isolate to the
    /// front door.
    ///
    /// The cost is real and small: a streamed body crosses the last hop
    /// uncompressed. Every response whose length the node knows -- which is
    /// nearly all of them, and all of the large static ones -- is compressed
    /// exactly as before.
    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut RequestCtx,
    ) -> pingora::Result<()>
    where
        Self::CTX: Send + Sync,
    {
        // 1xx is not the answer yet, and 101 is an upgrade the compression
        // module already leaves alone.
        if upstream_response.status.is_informational() {
            return Ok(());
        }
        if upstream_response.headers.contains_key("content-length") {
            return Ok(());
        }
        if let Some(module) = session
            .downstream_modules_ctx
            .get_mut::<ResponseCompression>()
        {
            // Level 0 is how this module is told to do nothing; it is the
            // state it is registered in, and `early_request_filter` is what
            // raised it for this request.
            module.adjust_level(0);
            debug!(
                host = %ctx.host,
                app_id = ?ctx.app_id,
                status = upstream_response.status.as_u16(),
                "the node is streaming this response, so it is relayed uncompressed: a \
                 compressor would hold each chunk until it had a block worth of them"
            );
        }
        Ok(())
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
        RouteTable::build(&apps, None, &[])
    }

    /// The catalog table the routing tests match against. `zone` turns the
    /// automatic per-environment addresses on.
    fn catalog(json: &str, zone: Option<&str>) -> CatalogRoutes {
        let document: crate::catalog::wire::RoutingTableDocument =
            serde_json::from_str(json).unwrap();
        document.validate().unwrap();
        let zone = zone.and_then(crate::catalog::PreviewZone::new);
        CatalogRoutes::build(&document, zone.as_ref())
    }

    fn no_catalog() -> CatalogRoutes {
        CatalogRoutes::default()
    }

    fn resolved(resolution: Resolution) -> Option<Resolved> {
        match resolution {
            Resolution::Found(found) => Some(found),
            _ => None,
        }
    }

    fn resolved_id(resolution: Resolution) -> Option<String> {
        resolved(resolution).map(|found| found.app_id)
    }

    fn resolved_environment(resolution: Resolution) -> Option<String> {
        resolved(resolution).map(|found| found.environment)
    }

    #[test]
    fn header_wins_when_it_names_a_known_app() {
        let routes = table(&[("shop", "shop.example.com"), ("blog", "blog.example.com")]);
        let resolution = resolve_app(
            &routes,
            &no_catalog(),
            Some("blog"),
            "shop.example.com",
            "/",
        );
        assert_eq!(resolved_id(resolution).as_deref(), Some("blog"));
    }

    #[test]
    fn unknown_header_is_rejected_even_for_a_known_host() {
        let routes = table(&[("shop", "shop.example.com")]);
        assert!(matches!(
            resolve_app(
                &routes,
                &no_catalog(),
                Some("Shop"),
                "shop.example.com",
                "/"
            ),
            Resolution::NotFound
        ));
        assert!(matches!(
            resolve_app(
                &routes,
                &no_catalog(),
                Some("other"),
                "shop.example.com",
                "/"
            ),
            Resolution::NotFound
        ));
    }

    #[test]
    fn empty_header_falls_back_to_the_host() {
        let routes = table(&[("shop", "shop.example.com")]);
        let resolution = resolve_app(&routes, &no_catalog(), Some(""), "shop.example.com", "/");
        assert_eq!(resolved_id(resolution).as_deref(), Some("shop"));
        let resolution = resolve_app(&routes, &no_catalog(), None, "shop.example.com", "/");
        assert_eq!(resolved_id(resolution).as_deref(), Some("shop"));
        assert!(matches!(
            resolve_app(&routes, &no_catalog(), None, "other.example.com", "/"),
            Resolution::NotFound
        ));
    }

    #[test]
    fn no_apps_and_no_routing_table_means_passthrough() {
        let routes = table(&[]);
        assert!(matches!(
            resolve_app(
                &routes,
                &no_catalog(),
                Some("anything"),
                "any.example.com",
                "/"
            ),
            Resolution::Passthrough
        ));
    }

    #[test]
    fn a_fetched_routing_table_alone_ends_passthrough() {
        // With no local application list, a catalog route is the only thing
        // routing this instance - and an unmatched host must then be a 404
        // rather than a blind forward to the upstream pool.
        let catalog = catalog(
            r#"{"version":"v1","routes":[{"app_id":"shop","environment":"production","host":"shop.example.com","path_prefix":"/"}],"environments":[]}"#,
            None,
        );
        let routes = table(&[]);
        assert_eq!(
            resolved_id(resolve_app(
                &routes,
                &catalog,
                None,
                "shop.example.com",
                "/"
            ))
            .as_deref(),
            Some("shop")
        );
        assert!(matches!(
            resolve_app(&routes, &catalog, None, "other.example.com", "/"),
            Resolution::NotFound
        ));
    }

    #[test]
    fn a_catalog_path_prefix_route_reaches_the_data_plane() {
        // The gap this closes: a route created through the control plane
        // used to be read by nobody at all.
        let catalog = catalog(
            r#"{"version":"v1","routes":[
                {"app_id":"api","environment":"pr-42","host":"example.com","path_prefix":"/api","priority":0},
                {"app_id":"site","environment":"production","host":"example.com","path_prefix":"/","priority":0}
            ],"environments":[]}"#,
            None,
        );
        let routes = table(&[]);
        let found = resolved(resolve_app(
            &routes,
            &catalog,
            None,
            "example.com",
            "/api/users",
        ))
        .unwrap();
        assert_eq!(found.app_id, "api");
        assert_eq!(found.environment, "pr-42");
        assert_eq!(found.from, ResolvedFrom::Catalog);
        assert_eq!(
            resolved_id(resolve_app(&routes, &catalog, None, "example.com", "/")).as_deref(),
            Some("site")
        );
    }

    #[test]
    fn a_catalog_route_beats_a_local_host_entry_and_keeps_its_local_policy() {
        // Both tables know shop.example.com. The catalog decides where the
        // request goes - it is the one an operator can change through the
        // API - and the local entry still supplies this machine's policy
        // for that application.
        let routes = table(&[("shop", "shop.example.com")]);
        let catalog = catalog(
            r#"{"version":"v1","routes":[{"app_id":"shop","environment":"pr-9","host":"shop.example.com","path_prefix":"/"}],"environments":[]}"#,
            None,
        );
        let found = resolved(resolve_app(
            &routes,
            &catalog,
            None,
            "shop.example.com",
            "/",
        ))
        .unwrap();
        assert_eq!(found.app_id, "shop");
        assert_eq!(found.environment, "pr-9");
        assert_eq!(found.from, ResolvedFrom::Catalog);
        assert!(
            found.config.is_some(),
            "the local entry is this machine's policy for the app, not a second opinion on routing"
        );
    }

    #[test]
    fn a_host_the_catalog_does_not_claim_still_falls_to_the_local_list() {
        let routes = table(&[("shop", "shop.example.com")]);
        let catalog = catalog(
            r#"{"version":"v1","routes":[{"app_id":"other","environment":"production","host":"other.example.com","path_prefix":"/"}],"environments":[]}"#,
            None,
        );
        let found = resolved(resolve_app(
            &routes,
            &catalog,
            None,
            "shop.example.com",
            "/",
        ))
        .unwrap();
        assert_eq!(found.app_id, "shop");
        assert_eq!(found.from, ResolvedFrom::Local);
    }

    #[test]
    fn a_preview_address_routes_to_its_environment() {
        let catalog = catalog(
            r#"{"version":"v1","routes":[],"environments":[{"app_id":"shop","environment":"pr-42"}]}"#,
            Some("preview.example.com"),
        );
        let routes = table(&[]);
        let found = resolved(resolve_app(
            &routes,
            &catalog,
            None,
            "pr-42--shop.preview.example.com",
            "/anything",
        ))
        .unwrap();
        assert_eq!(found.app_id, "shop");
        assert_eq!(found.environment, "pr-42");
        assert_eq!(found.from, ResolvedFrom::CatalogPreview);
    }

    #[test]
    fn the_header_can_name_an_app_only_the_catalog_knows() {
        let catalog = catalog(
            r#"{"version":"v1","routes":[{"app_id":"shop","environment":"production","host":"shop.example.com","path_prefix":"/"}],
                "environments":[{"app_id":"shop","environment":"production"},{"app_id":"shop","environment":"pr-42"}]}"#,
            None,
        );
        let routes = table(&[]);
        let found = resolved(resolve_app(
            &routes,
            &catalog,
            Some("shop"),
            "anything.test",
            "/",
        ))
        .unwrap();
        assert_eq!(found.app_id, "shop");
        assert_eq!(
            found.environment,
            deis_wire::conventions::DEFAULT_ENVIRONMENT,
            "the header names an application, so the only honest environment is the default one"
        );
        assert!(matches!(
            resolve_app(&routes, &catalog, Some("nothing"), "anything.test", "/"),
            Resolution::NotFound
        ));
    }

    #[test]
    fn a_catalog_with_apps_but_no_route_still_forwards_unmatched_traffic() {
        // The shape a platform has before anyone creates a route, and the
        // shape the bundled demo runs in: applications and environments
        // exist, nothing claims a hostname. A bare instance must still be a
        // plain reverse proxy, and an app named outright by header must
        // still resolve.
        let catalog = catalog(
            r#"{"version":"v1","routes":[],"environments":[{"app_id":"demo","environment":"production"}]}"#,
            None,
        );
        let routes = table(&[]);
        assert!(matches!(
            resolve_app(&routes, &catalog, None, "127.0.0.1", "/"),
            Resolution::Passthrough
        ));
        assert_eq!(
            resolved_id(resolve_app(
                &routes,
                &catalog,
                Some("demo"),
                "127.0.0.1",
                "/"
            ))
            .as_deref(),
            Some("demo")
        );
    }

    #[test]
    fn resolved_app_defaults_to_the_production_environment() {
        // The fixture apps above never mention "environment"; a route
        // pushed without one must still resolve to a concrete value rather
        // than leave it unset.
        let routes = table(&[("shop", "shop.example.com")]);
        let resolution = resolve_app(&routes, &no_catalog(), None, "shop.example.com", "/");
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
