// HTTP request handlers

use super::AppState;
use crate::app::AppKey;
use crate::runtime::DispatchError;
use crate::runtime::header_list;
use crate::{
    APP_ID_HEADER, DEPLOYMENT_GENERATION_HEADER, ENVIRONMENT_HEADER, PLATFORM_HEADER_PREFIX,
    is_platform_header,
};
use axum::{
    body::{Body, Bytes},
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use deno_core::url::Url;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Whether this overload refusal gets a warn line of its own.
///
/// An overload burst is exactly when a warn per refusal stops being useful:
/// the node is doing the right thing, and a stream of requests piling up
/// behind a wedged thread would write one line per request for as long as
/// the burst lasts. Every one of the first few refusals of the process is
/// logged, because the first is the moment an operator learns something new;
/// after that one in a hundred gets the line, and the rest are in the
/// metric `deis_runtime_overloaded_total`.
static OVERLOAD_REFUSALS_SEEN: AtomicU64 = AtomicU64::new(0);

fn overload_refusal_is_logged() -> bool {
    let seen = OVERLOAD_REFUSALS_SEEN.fetch_add(1, Ordering::Relaxed) + 1;
    seen <= 5 || seen.is_multiple_of(100)
}

/// How the kind of an inbound WebSocket frame reaches the isolate.
///
/// `crate::runtime::AppRuntime::dispatch_ws_message` carries one string per
/// frame, and the isolate has to know whether the client sent TEXT or BINARY.
/// The bridge used to say so *inside* the payload -- a binary frame went over
/// as `{"type":"binary","data":"<base64>"}` while a text frame went bare --
/// and `WebSocketServer._handleMessage` believed any text frame shaped like
/// that object. The payload is the client's, so the client chose which kind
/// the application saw, and an application protocol that happened to use those
/// two field names had its own messages silently turned into bytes.
///
/// So the kind is one character in front of the payload, in a position the
/// client's own bytes can never occupy, and everything after it is passed
/// through untouched. `js/runtime.js` spells the same two characters.
pub(crate) mod ws_frame {
    /// A TEXT frame; the client's text follows verbatim.
    pub(crate) const TEXT: char = '\u{1}';

    /// A BINARY frame; the client's bytes follow as base64, because what
    /// crosses into the isolate is a string and arbitrary bytes are not text.
    pub(crate) const BINARY: char = '\u{2}';

    fn stamped(kind: char, payload: &str) -> String {
        let mut framed = String::with_capacity(payload.len() + kind.len_utf8());
        framed.push(kind);
        framed.push_str(payload);
        framed
    }

    /// One TEXT frame, as the isolate receives it.
    pub(crate) fn text(text: &str) -> String {
        stamped(TEXT, text)
    }

    /// One BINARY frame, as the isolate receives it.
    pub(crate) fn binary(data: &[u8]) -> String {
        stamped(
            BINARY,
            &base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data),
        )
    }
}

/// The methods this node refuses at the door, whatever application would have
/// served them.
///
/// The Fetch standard's forbidden-method list lives in `deis_wire::conventions`
/// (`FORBIDDEN_METHODS`, tested via `method_is_forbidden`) so the inbound HTTP
/// path here and the service-call path in [`crate::runtime`] refuse the same
/// set before either reaches the throw a handler's `Request` would produce.
/// See the refusal in [`handle_request`] for why it is applied there and why
/// nothing else is added to it.
pub(crate) use deis_wire::conventions::method_is_forbidden;

/// Why a request body was not read.
#[derive(Debug)]
pub(crate) enum BodyRefusal {
    /// Over the node's `max_request_body_bytes`. Answered 413.
    TooLarge,
    /// The client stopped sending, or the connection failed part way through.
    /// Answered 400: nothing about the request was wrong, but there is no
    /// complete request to run.
    Unreadable(String),
}

/// Read a request body, refusing one that is over the ceiling.
///
/// Two checks, not one. `Content-Length` is a claim and is checked first, so
/// that a body announcing itself as too large is refused before a byte of it
/// is read. The bytes are then counted as they arrive as well, because a
/// chunked body announces nothing and a header that under-declares is the
/// obvious way past the first check.
///
/// Collected chunk by chunk rather than in one call so the read stops at the
/// chunk that crosses the ceiling. Collecting first and measuring afterwards
/// would mean the node had already taken the memory it is refusing.
pub(crate) async fn read_body_within_limit(
    body: Body,
    declared: Option<usize>,
    limit: usize,
) -> Result<Bytes, BodyRefusal> {
    if let Some(declared) = declared {
        if declared > limit {
            return Err(BodyRefusal::TooLarge);
        }
    }

    let mut stream = body.into_data_stream();
    // Sized from what the client declared, so an honest upload is one
    // allocation; capped at the limit so a dishonest one cannot ask this node
    // to reserve more than it will ever accept.
    let mut collected = bytes::BytesMut::with_capacity(declared.unwrap_or(0).min(limit));

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(e) => return Err(BodyRefusal::Unreadable(e.to_string())),
        };
        if collected.len() + chunk.len() > limit {
            return Err(BodyRefusal::TooLarge);
        }
        collected.extend_from_slice(&chunk);
    }

    Ok(collected.freeze())
}

/// What the client says it is about to send, when it says anything.
pub(crate) fn declared_body_length(headers: &HeaderMap) -> Option<usize> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<usize>().ok())
}

/// The scheme this node's listener speaks.
///
/// Not read off the request, and that is the point. A client can put anything
/// in an absolute request target (`GET https://anywhere/ HTTP/1.1`) or in an
/// HTTP/2 `:scheme`, and this node accepts requests straight off the network:
/// every other node on the installation can reach it and the demo talks to it
/// directly, which is the same reason the `x-deis-` sweep below has to be made
/// here as well as at the edge. So the scheme is what this node *knows* --
/// what it is serving on -- rather than what the request claimed.
///
/// The cost is on an installation whose edge terminates TLS: an application
/// there is answering an `https://` request and is told `http://`. That is the
/// honest answer this node can give today. Closing it needs the edge to say
/// so, on a channel a client cannot write: deis-proxy knows (`RequestCtx::tls`)
/// and stamps only the two routing headers, and a header a client could also
/// send would not be evidence of anything. An absolute redirect built from
/// this reaches the right site either way -- the edge answers a plain request
/// for an HTTPS application with a 301 to itself.
const NODE_SCHEME: &str = "http";

/// The authority a request that named none is answered under.
///
/// HTTP/1.0 has no `Host`, and a request built inside this node -- a test, a
/// probe -- may carry none either. Something has to stand in, and the name of
/// this machine's own loopback is the one answer that cannot be mistaken for a
/// real site.
const UNNAMED_AUTHORITY: &str = "localhost";

/// What the request was for, as the handler reads it off `context.request.url`.
///
/// This used to be `format!("http://localhost{uri}")`. The path and the query
/// were right and everything to the left of them was invented, so an
/// application that derived a canonical URL, an absolute redirect, a signed
/// callback or a link in an email put `localhost` in front of it -- correct on
/// a developer's machine and wrong everywhere the platform actually runs.
///
/// The authority is the one the client addressed: HTTP/2's `:authority` (which
/// `hyper` puts in the URI) when there is one, and the `Host` header
/// otherwise. That is what "the host the client used" means, and it is what
/// the edge routed on when it routed by host at all.
///
/// # What a client may and may not do with it
///
/// A client chooses its own `Host`, on this platform as on every other, so an
/// application that signs or emails a URL built from this must treat the
/// authority as input -- the same care every framework's documentation asks
/// for. What it may *not* do is change the shape of the URL, and that is what
/// [`usable_authority`] is for: a value carrying `/`, `?`, `#`, `@`, a space
/// or a control character would move the path, add a query, turn the host into
/// userinfo, or split the string outright. Those are refused here rather than
/// pasted into a URL, because a handler cannot defend against a `request.url`
/// whose path is not the path it was asked for.
pub(crate) fn request_url(uri: &Uri, headers: &HeaderMap, key: &AppKey) -> String {
    let claimed = uri
        .authority()
        .map(|authority| authority.as_str())
        .or_else(|| {
            headers
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
        });

    let authority = match claimed {
        None => UNNAMED_AUTHORITY,
        Some(claimed) if usable_authority(claimed) => claimed,
        Some(refused) => {
            // Warn, not error: the request is still answered, and answered
            // correctly -- the path and the query are the client's own. What
            // is lost is the application's ability to build an absolute URL
            // for this request, which is worth an operator's attention because
            // nothing else will report it. The value is escaped and the tenant
            // named, so a client sending these in a loop is findable rather
            // than merely noisy.
            tracing::warn!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                authority = %refused.escape_debug(),
                path = %uri.path(),
                "A request for {} named an authority that is not a host, so the URL its handler \
                 is given says {} instead. Anything but a host and a port there would move the \
                 path, add a query or turn the host into userinfo inside a URL the application \
                 reads as this request's own",
                key,
                UNNAMED_AUTHORITY
            );
            UNNAMED_AUTHORITY
        }
    };

    let target = uri
        .path_and_query()
        .map(|target| target.as_str())
        .unwrap_or("/");
    format!("{NODE_SCHEME}://{authority}{target}")
}

/// Whether a claimed authority is a host, and only a host.
///
/// # One parser decides, and it is the one the isolate uses
///
/// This used to answer the question itself: a whitelist of the characters a
/// registered name, an IPv4 literal, a bracketed IPv6 literal and a port are
/// spelled with, plus a pass that split those four forms apart. Every version
/// of that check has had the same shape of hole, because it describes what a
/// name may *contain* while the URL parser decides what a name *is*. The last
/// one accepted anything made of letters, digits, `-`, `.` and `_`, which lets
/// `1.2.3.4.5` through: a URL parser reads a trailing all-digit label as an
/// IPv4 literal, that is not one, so `new Request(url)` threw inside the
/// isolate and the client got 500 with this platform's own error text in the
/// body -- from one header, on a request the proxy had routed correctly.
/// `999.1.1.1`, `1.2.3.999` and `12345678901` are the same shape.
///
/// So the question is asked of the parser instead. The authority is put where
/// it will actually be used -- in front of a path, behind this node's scheme --
/// and parsed with the `url` crate, which is the crate `deno_url` is built on
/// and therefore the very parser `new URL()` and `new Request()` run inside
/// the isolate. What it accepts is accepted here by construction, and there is
/// no second opinion left to drift.
///
/// # What is asked of the result
///
/// Parsing is not enough on its own, because a URL parser is glad to read a
/// string as something other than a host: `shop.example.com@evil.invalid` is
/// userinfo and an origin of `evil.invalid`, `shop.example.com/../admin` moves
/// the path, `a?b` adds a query. Each of those parses. So the parse has to
/// come back as a *bare* authority: a host, an optional port, no credentials,
/// and a URL whose path is still the `/` this function supplied and whose
/// query and fragment are still absent.
///
/// # And the characters a parser deletes rather than refuses
///
/// Tab, carriage return and newline are removed from the input by the WHATWG
/// parser before it looks at anything, so `Host: exa<tab>mple.com` parses
/// happily as `example.com` -- and this node pastes the *original* string into
/// `request.url`, tab and all. For those the parser did not judge the string
/// that will be used, which is the one thing that would let the two disagree
/// again, so the graphic-ASCII test below rules them out first. It rules out
/// space and the C0 controls with them, none of which belongs in a host.
fn usable_authority(authority: &str) -> bool {
    // A `Host` longer than this is not a name anybody serves; the ceiling is
    // here so the string cannot become a way to make every log line and every
    // URL in an isolate arbitrarily long.
    const LONGEST: usize = 255;

    if authority.is_empty() || authority.len() > LONGEST {
        return false;
    }

    // See the header: only characters the parser will judge, never ones it
    // silently drops on its way to a verdict.
    if !authority.bytes().all(|byte| byte.is_ascii_graphic()) {
        return false;
    }

    let Ok(parsed) = Url::parse(&format!("{NODE_SCHEME}://{authority}/")) else {
        return false;
    };

    parsed.host().is_some()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.path() == "/"
        && parsed.query().is_none()
        && parsed.fragment().is_none()
}

/// Unified request handler - routes to applications via the tenant headers.
///
/// Two headers, because a running application is an application *in an
/// environment*: [`APP_ID_HEADER`] names the application and
/// [`ENVIRONMENT_HEADER`] names which of its environments to serve. An absent
/// environment means [`crate::DEFAULT_ENVIRONMENT`], so a caller that predates
/// previews still reaches production rather than being refused.
///
/// The header names come from `deis_wire::conventions` rather than literals
/// here: deis-proxy stamps them and this reads them, and a node that disagreed
/// with the proxy about the spelling would reject every request with 400 while
/// both sides looked correct in isolation.
pub async fn handle_request(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ws: Option<WebSocketUpgrade>,
    // The body is taken unread, and read below against the node's
    // `max_request_body_bytes`.
    //
    // Bytes, not `String`: an upload is whatever the client sent. Extracting
    // it as text made axum reject every request whose body is not valid UTF-8
    // -- a file upload, a protobuf, an image -- before the application saw it.
    // Taking `Bytes` here instead would read it in full with only axum's own
    // 2 MiB default standing between this node and an upload of any size, and
    // would refuse an over-large one with a bare rejection that names no
    // limit and logs nothing.
    body: Body,
) -> impl IntoResponse {
    // Get app_id from header
    let app_id = match headers.get(APP_ID_HEADER).and_then(|v| v.to_str().ok()) {
        Some(id) => id.to_string(),
        None => {
            tracing::warn!(
                "Request missing {} header, so there is no application to route it to: {} {}",
                APP_ID_HEADER,
                method,
                uri
            );
            return (
                StatusCode::BAD_REQUEST,
                format!("Missing {APP_ID_HEADER} header"),
            )
                .into_response();
        }
    };

    let environment = requested_environment(&headers).to_string();
    let routing_generation = match headers
        .get(DEPLOYMENT_GENERATION_HEADER)
        .map(|value| value.to_str())
    {
        None => None,
        Some(Ok(value)) if value.parse::<deis_wire::CanonicalId>().is_ok() => Some(value),
        Some(_) => {
            tracing::warn!(
                app_id = %app_id,
                "Refusing a request whose trusted routing generation is malformed"
            );
            return (
                StatusCode::BAD_REQUEST,
                format!("Invalid {DEPLOYMENT_GENERATION_HEADER} header"),
            )
                .into_response();
        }
    };

    // The inspector channel is not here. It moved to the management
    // listener: the token it answers to is minted by `StartInspect` on the
    // machine channel, and the WebSocket that token opens gives a debugger
    // full access to a running isolate - a door like that belongs behind the
    // same handshake the machine channel is behind, not on a port that used
    // to admit anyone who could open a socket. See
    // `handle_inspector_upgrade`, served by the management listener's router.
    // The key that used to route it is refused here like any other name this
    // port does not serve.
    let key = AppKey::new(app_id, environment);
    if let Err(e) = key.validate() {
        tracing::warn!(
            app_id = %key.app_id(),
            environment = %key.environment(),
            "Refusing a request whose routing headers are not usable: {}",
            e
        );
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }

    // The legacy/direct path has no immutable generation to distinguish a
    // not-yet-prepared deployment from a name this node has never seen. Keep
    // the established API contract for that path: an unknown application is
    // a 404. Exact-generation requests are checked below and deliberately
    // remain 503 when this particular node has not prepared the selected
    // generation, because that is a retryable placement/convergence fault.
    if routing_generation.is_none() && !state.app_manager.is_loaded(&key) {
        tracing::debug!(
            app_id = %key.app_id(),
            environment = %key.environment(),
            method = %method,
            path = uri.path(),
            "No application {} is running on this node, so a request naming it is answered 404",
            key
        );
        return (StatusCode::NOT_FOUND, "No such application on this node.").into_response();
    }

    if !state
        .app_manager
        .serves_generation(&key, routing_generation)
        .await
    {
        tracing::warn!(
            app_id = %key.app_id(),
            environment = %key.environment(),
            routing_generation = routing_generation.unwrap_or("<legacy>"),
            "This node does not have the exact deployment generation selected by the edge; refusing instead of substituting another version"
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "The selected deployment generation is not prepared on this node.",
        )
            .into_response();
    }
    let generation_traffic = match routing_generation {
        Some(generation) => match state.app_manager.begin_generation_traffic(&key, generation) {
            Ok(lease) => Some(lease),
            Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The selected deployment generation is draining.",
                )
                    .into_response();
            }
        },
        None => None,
    };

    // A method no application on this platform can be written to answer.
    //
    // Refused here, before an isolate is entered, because the refusal already
    // existed one layer too deep: the runtime builds a standard `Request` for
    // the handler, deno_fetch's constructor throws `TypeError: Method is
    // forbidden` on these three, and the throw came back out of the invoke
    // wrapper as a 500 carrying that constructor's stack. So `curl -X TRACE`
    // at any application answered 500 with the platform's internals in the
    // body, and every one of them spent a runtime thread on a request that
    // could never be served.
    //
    // The list is the Fetch standard's forbidden methods and nothing else.
    // TRACE and TRACK are also the cross-site tracing pair -- a server that
    // echoes a request echoes its cookies -- and CONNECT asks this node to be
    // a tunnel, which it is not. Compared case-insensitively because that is
    // how the standard compares them, and because a client sending `trace`
    // means the same thing as one sending `TRACE`.
    //
    // Everything else this node does not recognise is passed through
    // untouched: PROPFIND, REPORT, and whatever method an application invents
    // are all served today, and a handler is free to answer any of them.
    //
    // No `Allow` header. RFC 9110 asks a 405 to carry one, but this node
    // cannot state a truthful list: the set it accepts is every method except
    // these three, which is not a list, and the set *this application* accepts
    // is inside the handler and unknown here. A guessed `Allow` would be a
    // wrong answer given confidently, which is worse than an absent header.
    if method_is_forbidden(method.as_str()) {
        tracing::warn!(
            app_id = %key.app_id(),
            environment = %key.environment(),
            method = %method,
            path = uri.path(),
            "Refusing {} for {}: the Fetch standard forbids this method, so no handler can be \
             built to answer it. The application was not called",
            method,
            key
        );
        return (StatusCode::METHOD_NOT_ALLOWED, "This method is not served.").into_response();
    }

    // Asset-first, the platform's first design principle: a request whose path
    // names a file the bundle carries is answered from that file and never
    // reaches an isolate. The index and the site's rules were built when the
    // version loaded, so this is a hash probe and a scan of a compiled table,
    // not a read of the zip.
    //
    // `AssetBundle::serve` rather than a plain index lookup, because a static
    // site answers with more than the files it carries: a `_redirects` rule,
    // the headers a `_headers` block sets, and the SPA shell or the custom 404
    // page a path that names no file falls back to. Those were parsed at build
    // time and compiled at load time, and this is the only place a visitor's
    // request can reach them.
    //
    // Only GET and HEAD are answered this way. A POST to a path that happens
    // to collide with a static file is far more likely to be a route the app
    // implements than a request for the file, so those fall through.
    if matches!(method, Method::GET | Method::HEAD) {
        if let Some(assets) = state
            .app_manager
            .assets_for_generation(&key, routing_generation)
        {
            // Path *and* query. A `_redirects` rule matches on the path
            // alone, but the query is carried through to the target it sends
            // the visitor to, and there is nothing to carry if it was dropped
            // here: `/old?ref=x` would land on `/new` with the campaign the
            // author was tracking gone.
            let request_target = uri
                .path_and_query()
                .map(|target| target.as_str())
                .unwrap_or_else(|| uri.path());
            if let Some(response) = assets.serve(request_target, &headers, method == Method::HEAD) {
                // The bundle logs which file answered; it knows the
                // application but not which of its environments this node is
                // serving, and a node runs several. Without the environment
                // here the line cannot answer "which one", so it is named
                // where it is known.
                tracing::debug!(
                    app_id = %key.app_id(),
                    environment = %key.environment(),
                    method = %method,
                    path = %uri.path(),
                    status = response.status().as_u16(),
                    "Answered a request from this version's static site without entering an \
                     isolate"
                );
                return hold_generation_traffic(response, generation_traffic.clone());
            }
        }
    }

    // The URL of the request being answered, as the handler will read it off
    // `context.request.url`.
    let url = request_url(&uri, &headers, &key);

    tracing::debug!(
        app_id = %key.app_id(),
        environment = %key.environment(),
        "Routing request: app={}, method={}, url={}",
        key,
        method,
        url
    );

    // Convert headers, less the platform's own namespace.
    //
    // Everything under `x-deis-` that reaches a handler has to be something
    // this node wrote, because that is what the platform tells applications
    // it is: `x-deis-caller` is documented as the node's record of which
    // application made a service call and as evidence a callee may branch on
    // rather than a claim it has to check. Every header here arrived from
    // outside, and the loop below used to copy all of them into the isolate
    // one for one -- so `curl -H 'x-deis-caller: admin-app'` handed a callee
    // exactly the value a service call would have stamped, and a callee that
    // followed the documented pattern granted admin to anyone who could reach
    // this node. `x-deis-tenant`, `x-deis-storage-token` and `x-deis-binding`
    // are the same shape one hop further in.
    //
    // deis-proxy sweeps the same namespace at the edge. This is not that fix
    // repeated for tidiness: a node is reachable without the proxy -- every
    // other node on the network can reach it, and the demo talks to it
    // directly -- so the guarantee has to be made here to be a guarantee at
    // all.
    //
    // A prefix and not a list of names, for the reason a list is always one
    // release behind: the next header some component invents would arrive
    // forgeable and nothing would fail until something trusted it. The cost is
    // that an application cannot receive a header of its own under `x-deis-`,
    // which is the same bargain deis-bundle already strikes on the response
    // side by refusing a bundle that sets one.
    //
    // A list and not a map, which is the other half of what a handler is
    // given: see `crate::runtime::header_list`. A map kept one value per name,
    // so a request with two `Cookie` lines, two `Accept` lines or an
    // `X-Forwarded-For` chain reached the handler with one of them and nothing
    // said the rest had been dropped.
    let (mut request_headers, swept) =
        header_list::from_header_map(&headers, |name| !is_platform_header(name));
    // The two routing headers are ordinary, documented traffic on the way in;
    // they are put back below with the values this node resolved. Anything
    // else under the prefix has no honest sender.
    let forged: Vec<&str> = swept
        .iter()
        .map(String::as_str)
        .filter(|name| *name != APP_ID_HEADER && *name != ENVIRONMENT_HEADER)
        .collect();
    if !forged.is_empty() {
        tracing::warn!(
            app_id = %key.app_id(),
            environment = %key.environment(),
            removed = forged.len(),
            headers = %forged.join(", "),
            "A request for {} carried {} header(s) in the platform's own {} namespace; all {} \
             were dropped before the isolate saw them. An application reads that namespace as \
             this node's own account of the request, so a value from outside would be read as \
             evidence the platform had stamped",
            key,
            forged.len(),
            PLATFORM_HEADER_PREFIX,
            forged.len()
        );
    }

    // Put back what this node decided, so an application reading either of
    // them reads the request it is actually serving. Not the client's copy:
    // routing takes the *first* value of a repeated header, and a client
    // sending the environment twice would otherwise have a handler running in
    // one environment read the name of the other.
    //
    // `set_unique` and not a push, because these two are this node's answer
    // rather than a contribution to a list: appending a second value would be
    // read by a handler as `"production, preview"`.
    header_list::set_unique(
        &mut request_headers,
        APP_ID_HEADER,
        key.app_id().to_string(),
    );
    header_list::set_unique(
        &mut request_headers,
        ENVIRONMENT_HEADER,
        key.environment().to_string(),
    );

    // This node's own readiness, for the one application that answers the
    // edge's health probe.
    //
    // deis-proxy asks `GET /health` with `x-deis-app-id: _system`, evicts an
    // upstream that does not answer 200, and reads the readiness the answer
    // states to judge a node that is up but not running what it was told to.
    // The system application used to answer 200 from a literal that consulted
    // nothing, so a node that could not reach its control plane and had
    // nothing loaded passed the probe for ever while answering 500 to every
    // real request.
    //
    // It cannot read `crate::health` itself - `js/runtime.js` deletes
    // `globalThis.Deno` before any application code runs - and the sweep above
    // is what makes these two headers trustworthy: the client's copy of
    // anything under `x-deis-` has already been dropped, so what the isolate
    // reads here is this node's word and nobody else's. See `crate::health`
    // for which answer each state gets and why the count travels with it.
    //
    // Stamped for `_system` alone. Every other application gets a smaller
    // request, and none of them has any business branching on the health of
    // the node it happens to be running on.
    if key.app_id() == crate::health::SYSTEM_APP_ID {
        if let Some(readiness) = crate::health::serving_readiness() {
            header_list::set_unique(
                &mut request_headers,
                crate::health::READINESS_HEADER,
                readiness.state_name().to_string(),
            );
            header_list::set_unique(
                &mut request_headers,
                crate::health::SERVING_APPS_HEADER,
                readiness.serving_apps().to_string(),
            );
            // Why, for the one state that has a why. Stamped only when the
            // node recorded one, so the system application can tell "this
            // node did not say" from "this node said the following" instead
            // of guessing a cause, which is what it used to do: every
            // degraded node was reported as one that cannot reach its
            // control plane, including the ones whose control plane had
            // answered and whose real trouble was a bundle that would not
            // load.
            //
            // Put through `header_safe_reason` because a reason is built
            // from somebody else's error message and a header value cannot
            // hold a newline: a carriage return in one is how a header
            // becomes two.
            if let Some(reason) = readiness.reason() {
                header_list::set_unique(
                    &mut request_headers,
                    crate::health::REASON_HEADER,
                    crate::health::header_safe_reason(reason),
                );
            }
        }
    }

    // Add WebSocket upgrade flag.
    //
    // Lower-cased deliberately, because the loop above spells every name the
    // way `HeaderName` prints it, which is lower case. Spelled `Upgrade` this
    // was a *second* entry beside the client's own `upgrade`, and once the
    // isolate started building a standard `Headers` out of this map -- which
    // appends rather than overwrites -- `request.headers.get("upgrade")`
    // answered "websocket, websocket". Every handler that compares it to
    // "websocket" then took its non-upgrade branch and the handshake never
    // happened.
    if ws.is_some() {
        header_list::set_unique(&mut request_headers, "upgrade", "websocket".to_string());
    }

    let limit = state.max_request_body_bytes;
    let declared = declared_body_length(&headers);
    let body = match read_body_within_limit(body, declared, limit).await {
        Ok(body) => body,
        Err(BodyRefusal::TooLarge) => {
            tracing::warn!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                limit,
                declared = declared.unwrap_or_default(),
                method = %method,
                path = uri.path(),
                "Refusing a request body larger than this node will hold in memory; the \
                 application never saw the request. Raise max_request_body_bytes in this \
                 node's configuration if uploads of this size are expected, or have the \
                 application take them through blob storage instead"
            );
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "Request body is larger than this node accepts: the limit is {limit} bytes"
                ),
            )
                .into_response();
        }
        Err(BodyRefusal::Unreadable(e)) => {
            tracing::warn!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                method = %method,
                path = uri.path(),
                "Could not read the request body, so the application was not called: {}",
                e
            );
            return (StatusCode::BAD_REQUEST, "Could not read the request body").into_response();
        }
    };

    // Empty stays absent: a request with no body and a request with a
    // zero-length one are the same thing to a handler, and `new Request(url,
    // { body })` refuses an empty body on a GET.
    let body_opt = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };

    // Whether the client offered an upgrade at all.
    //
    // Read before the extractor is consumed by the upgrade below, because the
    // refusal further down has to be able to say which half of an upgrade was
    // missing: a handler that attached no socket, or a client that never asked
    // for one.
    let upgrade_offered = ws.is_some();

    // Execute application
    match state
        .app_manager
        .execute_app_generation(
            &key,
            routing_generation,
            url,
            method.to_string(),
            request_headers,
            body_opt,
        )
        .await
    {
        Ok((status, resp_headers, resp_body)) => {
            // Check if it's a WebSocket upgrade response
            if status == 101 {
                if let Some(ws_upgrade) = ws {
                    // Extract WebSocket ID
                    if let Some(ws_id) = crate::runtime::header_list::first_value(
                        &resp_headers,
                        "X-Runtime-WebSocket-ID",
                    ) {
                        let ws_id = ws_id.to_string();
                        let key_clone = key.clone();
                        let app_manager = state.app_manager.clone();
                        let generation = routing_generation.map(str::to_string);

                        tracing::info!(
                            app_id = %key.app_id(),
                            environment = %key.environment(),
                            "WebSocket upgrade for app {} with ws_id: {}",
                            key,
                            ws_id
                        );

                        // Execute WebSocket upgrade and start message bridging
                        return ws_upgrade
                            .on_upgrade(move |socket| {
                                handle_websocket_bridged(
                                    socket,
                                    key_clone,
                                    generation,
                                    ws_id,
                                    app_manager,
                                    generation_traffic,
                                )
                            })
                            .into_response();
                    }
                }
            }

            // A status this node can actually write on this connection.
            //
            // Reaching here with 101 means the upgrade did not happen: either
            // the client asked for none, or the handler attached no socket. A
            // 101 written anyway is a promise nothing keeps -- hyper writes
            // `101 Switching Protocols` and then goes on speaking HTTP, so the
            // client waits for a protocol that is never coming and the request
            // hangs until something times it out. One line in a handler,
            // `new Response(null, { status: 101 })`, did that to every client
            // that reached it.
            //
            // The rest of the range is the same defect one step out. `status`
            // is read off the object the handler returned, and an own property
            // shadows `Response.prototype.status`, so the number arriving here
            // is the handler's to choose: 1000 reached `Response::builder`,
            // which refuses anything outside 100-999, and the result was
            // unwrapped -- a panic on this node's thread rather than an answer.
            // 200-599 is what a response may carry, which is also the range the
            // `Response` constructor enforces on every handler that does not go
            // out of its way.
            if !(200..=599).contains(&status) {
                let reason = if status == 101 && upgrade_offered {
                    "the handler answered 101 without attaching a WebSocket, so there is no \
                     socket for this connection to become"
                } else if status == 101 {
                    "the handler answered 101 but this request asked for no upgrade, so \
                     there is nothing for this connection to switch to"
                } else {
                    "a response may only carry a status of 200 through 599"
                };
                tracing::error!(
                    app_id = %key.app_id(),
                    environment = %key.environment(),
                    status,
                    method = %method,
                    path = uri.path(),
                    "App {} answered with status {}, which this node will not write on a \
                     client's connection: {}. The request is answered 500 instead; writing \
                     {} would leave the client waiting for an answer that never completes",
                    key,
                    status,
                    reason,
                    status
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "The application answered with status {status}, which is not an \
                         answer this node can send: {reason}"
                    ),
                )
                    .into_response();
            }

            // Normal HTTP response
            let mut response = Response::builder().status(status);

            for (key, value) in resp_headers {
                response = response.header(key, value);
            }

            // Built, not unwrapped. The builder holds the first thing it could
            // not accept -- a status, a header name, a header value -- and
            // hands it over here; unwrapping made an unusable answer a panic on
            // the runtime thread, which the client sees as a dropped connection
            // and the operator sees as a stack trace with no request in it.
            match response.body(http_body(resp_body)) {
                Ok(response) => hold_generation_traffic(response, generation_traffic),
                Err(e) => {
                    tracing::error!(
                        app_id = %key.app_id(),
                        environment = %key.environment(),
                        status,
                        method = %method,
                        path = uri.path(),
                        "App {} answered with a status and headers this node cannot write: \
                         {}. The request is answered 500 rather than failing the connection",
                        key,
                        e
                    );
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "The application's answer could not be written as an HTTP response"
                            .to_string(),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            // The node's own refusal travels down this channel as itself.
            //
            // This is the one place the pool's two kinds of failure meet the
            // client. An application error and a node error are different
            // things for the caller: the first is answered with an incident
            // so the operator can find it, the second is a scheduling answer
            // the caller may act on -- retry after a moment, or not at all.
            // The pool is the only sender of a [`DispatchError`], so a
            // downcast is the whole of the check.
            if let Some(refusal) = e.downcast_ref::<DispatchError>() {
                return match refusal {
                    DispatchError::Overloaded { retry_after } => {
                        // Whole seconds, floored at one: a fractional or zero
                        // Retry-After is not a hint a client can schedule on,
                        // and the budget this refusal came from is already
                        // back in the neighbourhood of zero by the time the
                        // retry lands.
                        let retry_after = retry_after.as_secs().max(1);
                        crate::runtime::metrics::recorder().record_overloaded(
                            &key.tenant_id(),
                            crate::runtime::metrics::OVERLOAD_CLASS_ADMISSION,
                        );
                        if overload_refusal_is_logged() {
                            tracing::warn!(
                                app_id = %key.app_id(),
                                environment = %key.environment(),
                                method = %method,
                                path = uri.path(),
                                "Refusing a request as overloaded: the runtime thread for {} was at \
                                 capacity, so the application never saw it. The caller is told to \
                                 retry in {retry_after}s",
                                key
                            );
                        } else {
                            tracing::debug!(
                                app_id = %key.app_id(),
                                environment = %key.environment(),
                                "Refusing another request as overloaded; the warn line for these \
                                 is sampled, and the count is in the overloaded metric"
                            );
                        }
                        // Built rather than unwrapped: every value here is a
                        // fixed literal or a positive integer, so the builder
                        // cannot fail -- and the refusal the caller is owed
                        // must not itself be able to become a 500.
                        let response = Response::builder()
                            .status(StatusCode::SERVICE_UNAVAILABLE)
                            .header("retry-after", retry_after.to_string())
                            .header("x-deis-outcome", "overloaded")
                            .header("x-deis-outcome-detail", "admission")
                            .body(Body::from("Runtime overloaded, retry shortly"));
                        match response {
                            Ok(response) => response.into_response(),
                            Err(_) => (
                                StatusCode::SERVICE_UNAVAILABLE,
                                "Runtime overloaded, retry shortly",
                            )
                                .into_response(),
                        }
                    }
                    DispatchError::Stopped | DispatchError::GenerationNotPrepared => {
                        // A placement answer, not an overload: the application
                        // (or the exact generation the edge selected for it)
                        // is not on this node at this moment. The same shape
                        // as the other 503s this file gives out -- a fixed
                        // sentence and no incident, because nothing failed
                        // here; reinstalling or converging the placement is
                        // the control plane's job, not this request's.
                        tracing::warn!(
                            app_id = %key.app_id(),
                            environment = %key.environment(),
                            method = %method,
                            path = uri.path(),
                            "Refusing a request for {}: the node was told to run it but its \
                             placement is not here ({refusal})",
                            key
                        );
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            "The application is not currently available on this node.",
                        )
                            .into_response()
                    }
                    DispatchError::NotFound => {
                        // The check at the top of this handler said the
                        // application was placed here; the post says it is not.
                        // It was unloaded or moved in the between. The
                        // established answer for a placement that is not here
                        // yet is the 503 with no incident: nothing failed,
                        // and the caller may retry while the placement
                        // converges.
                        tracing::warn!(
                            app_id = %key.app_id(),
                            environment = %key.environment(),
                            method = %method,
                            path = uri.path(),
                            "Refusing a request for {}: it was checked as placed a moment ago \
                             and is not on this node any more",
                            key
                        );
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            "The application is not currently available on this node.",
                        )
                            .into_response()
                    }
                };
            }

            // The other half of the same disclosure the invoke wrapper had.
            //
            // This arm is not only "the runtime thread went away". A bundle
            // whose module fails to evaluate is reported here too -- see
            // `AppRuntime::execute_fetch`, which sends the module-init error
            // down this channel -- and a deno module error carries the
            // application's own stack, so the tenant's bundle layout used to
            // leave through this body as well. The cut-off errors that also
            // land here name the application and the budget it was held to,
            // which is a configuration fact and equally nobody else's.
            //
            // The client gets a fixed sentence and an incident id; the error
            // itself stays in the line below, which carries `app_id`,
            // `environment` and the same id.
            //
            // That line is the node's own log and not the application's log
            // stream: `crate::logging::RuntimeLayer` ships only `target:
            // "app"` events, so this is found with `grep incident=<id>` in the
            // node's log file rather than with `deis logs -a <app>`. Moving it
            // into the application's stream is the right next step and is not
            // done here, because `e` can be an application's own error text --
            // a module that would not evaluate -- and a line shipped to
            // telemetry has to go through `runtime::ops::escape_control`
            // first, which is private to `runtime`. A newline in a message
            // forges a whole log line, stamped with the real ids.
            //
            // The id is minted here because this failure never reached an
            // isolate and so has no trace id of its own to quote. The isolate's
            // own 500s carry an id in the same shape, from `js/runtime.js`, and
            // those lines do reach `deis logs -a <app>`.
            let incident = uuid::Uuid::new_v4().simple().to_string();
            tracing::error!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                incident = %incident,
                "Application {} execution failed: {}",
                key,
                e
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Internal Server Error (incident {incident})"),
            )
                .into_response()
        }
    }
}

/// The axum body for what the isolate answered with.
///
/// The first two arms are the road every response but a handful takes: a body
/// of known length, framed by hyper with a `Content-Length`, exactly as before
/// streaming existed.
///
/// The third is a body the application is still producing. It is handed over as
/// a stream, so hyper frames it chunked and writes each chunk as it arrives
/// rather than waiting for a length it cannot know. Two things follow from
/// that and both are the point: the head reaches the client before the first
/// chunk exists, and dropping this body -- which is what hyper does when the
/// client hangs up -- closes the queue, which is how the isolate learns to stop
/// producing.
fn http_body(body: crate::runtime::ResponseBody) -> Body {
    use crate::runtime::ResponseBody;
    match body {
        ResponseBody::None => Body::empty(),
        ResponseBody::Bytes(bytes) => Body::from(bytes),
        ResponseBody::Stream(mut reader) => {
            Body::from_stream(futures_util::stream::poll_fn(move |cx| {
                let chunk = std::task::ready!(reader.poll_chunk(cx));
                std::task::Poll::Ready(match chunk {
                    Some(chunk) => Some(Ok(chunk)),
                    // The producer is done. If it stopped badly the reason is
                    // waiting beside the queue, and the body ends with an error
                    // rather than with its terminating chunk: the client was
                    // answered 200 long ago, so a failed transfer is the only
                    // way left to tell it the answer is incomplete. A clean end
                    // here would leave it believing a truncated body was the
                    // whole of one.
                    None => reader.failure().map(|reason| {
                        Err(std::io::Error::other(format!(
                            "the response body was not finished: {reason}"
                        )))
                    }),
                })
            }))
        }
    }
}

/// Keep a generation's drain admission alive for the complete HTTP body,
/// including after Hyper has consumed and dropped the response head. A
/// response extension is insufficient here: extensions belong to the head
/// and may be dropped while a streaming body is still being sent.
pub(crate) fn hold_generation_traffic(
    response: Response,
    lease: Option<crate::app::GenerationTrafficLease>,
) -> Response {
    let Some(lease) = lease else {
        return response;
    };
    let (parts, body) = response.into_parts();
    let guarded = body.into_data_stream().map(move |chunk| {
        // Capturing the lease in the stream object, rather than merely
        // touching it at construction, gives it exactly the body's lifetime.
        let _keep_alive = &lease;
        chunk
    });
    Response::from_parts(parts, Body::from_stream(guarded))
}

/// Which environment of an application a request names.
///
/// An absent or unreadable header is an empty string, which
/// [`AppKey::new`] turns into [`crate::DEFAULT_ENVIRONMENT`]: a caller that
/// predates previews - an older proxy, a curl by hand - reaches production
/// rather than being refused, which is the rule the whole platform agrees on
/// in `deis_wire::conventions`.
pub(crate) fn requested_environment(headers: &HeaderMap) -> &str {
    headers
        .get(ENVIRONMENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

/// Handle WebSocket connection (bridge mode)
///
/// Implements full bidirectional message bridging between client WebSocket and JavaScript WebSocketServer
async fn handle_websocket_bridged(
    socket: WebSocket,
    key: AppKey,
    routing_generation: Option<String>,
    ws_id: String,
    app_manager: Arc<crate::app::ApplicationManager>,
    _generation_traffic: Option<crate::app::GenerationTrafficLease>,
) {
    tracing::info!(
        app_id = %key.app_id(),
        environment = %key.environment(),
        "WebSocket bridge established for app: {}, ws_id: {}",
        key,
        ws_id
    );

    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Create bidirectional channels
    // js_to_client: JavaScript message -> client (registered in WsServerManager)
    //
    // The counter travels with the channel. The isolate adds to it when it
    // queues a frame and this task subtracts once the frame has been handed to
    // the socket, which is what bounds how far ahead of a slow client an
    // application may run: without it the queue was Rust memory beside the
    // isolate with no ceiling in it at all, and a handler sending faster than
    // one client read grew the node's process without ever crossing its own
    // memory limit. See `crate::runtime::ws_server_ops::MAX_BUFFERED_BYTES`.
    let (js_to_client_tx, mut js_to_client_rx) = tokio::sync::mpsc::unbounded_channel();
    let buffered = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Register WebSocket connection in RuntimePool
    // Note: we only need to register js_to_client_tx, as client messages are sent directly via dispatch_ws_message
    if let Err(e) = app_manager
        .register_ws_connection_generation(
            &key,
            routing_generation.as_deref(),
            ws_id.clone(),
            crate::runtime::WsClientSink::new(js_to_client_tx, buffered.clone()),
        )
        .await
    {
        tracing::error!("Failed to register WebSocket connection {}: {}", ws_id, e);
        let _ = ws_sender
            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: 1011, // Internal Error
                reason: std::borrow::Cow::Borrowed("Failed to register connection"),
            })))
            .await;
        return;
    }

    tracing::debug!("WebSocket connection {} registered successfully", ws_id);

    // Task A: Receive message from client -> send to JavaScript
    let key_clone = key.clone();
    let ws_id_clone = ws_id.clone();
    let app_manager_clone = app_manager.clone();
    let generation_for_messages = routing_generation.clone();
    let client_to_js_task = tokio::spawn(async move {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    tracing::debug!("Client -> JS: {} (ws_id: {})", text, ws_id_clone);
                    // Stamped as TEXT whatever it says. A client used to be
                    // able to spell the binary envelope by hand and have the
                    // application receive an `ArrayBuffer`; the stamp is
                    // outside the payload, so what the payload says about
                    // itself no longer decides anything.
                    if let Err(e) = app_manager_clone
                        .send_ws_message_generation(
                            &key_clone,
                            generation_for_messages.as_deref(),
                            ws_id_clone.clone(),
                            ws_frame::text(&text),
                        )
                        .await
                    {
                        tracing::error!("Failed to send message to JavaScript: {}", e);
                        break;
                    }
                }
                Ok(Message::Binary(data)) => {
                    tracing::debug!(
                        "Client -> JS: {} bytes (ws_id: {})",
                        data.len(),
                        ws_id_clone
                    );
                    if let Err(e) = app_manager_clone
                        .send_ws_message_generation(
                            &key_clone,
                            generation_for_messages.as_deref(),
                            ws_id_clone.clone(),
                            ws_frame::binary(&data),
                        )
                        .await
                    {
                        tracing::error!("Failed to send binary message to JavaScript: {}", e);
                        break;
                    }
                }
                Ok(Message::Ping(data)) => {
                    tracing::trace!("Received ping (ws_id: {})", ws_id_clone);
                    // Axum handles Pong responses automatically
                    drop(data);
                }
                Ok(Message::Pong(_)) => {
                    tracing::trace!("Received pong (ws_id: {})", ws_id_clone);
                }
                Ok(Message::Close(close_frame)) => {
                    tracing::info!(
                        "Client closed connection (ws_id: {}): {:?}",
                        ws_id_clone,
                        close_frame.as_ref().map(|f| (f.code, f.reason.as_ref()))
                    );
                    break;
                }
                Err(e) => {
                    tracing::error!("WebSocket error (ws_id: {}): {}", ws_id_clone, e);
                    break;
                }
            }
        }

        tracing::debug!("Client -> JS task ended (ws_id: {})", ws_id_clone);
    });

    // Task B: Receive message from JavaScript -> send to client
    let ws_id_clone = ws_id.clone();
    let js_to_client_task = tokio::spawn(async move {
        use crate::runtime::WsServerMessage;

        while let Some(msg) = js_to_client_rx.recv().await {
            // Released before the write rather than after it: what the ceiling
            // is about is how much this node is holding, and from here the
            // frame is the socket's problem. Counting it as queued until the
            // write returned would charge the application for the client's own
            // slowness twice.
            buffered.fetch_sub(msg.queued_bytes(), std::sync::atomic::Ordering::Relaxed);
            match msg {
                WsServerMessage::Text(text) => {
                    tracing::debug!("JS -> Client: {} (ws_id: {})", text, ws_id_clone);
                    if let Err(e) = ws_sender.send(Message::Text(text)).await {
                        tracing::error!("Failed to send text to client: {}", e);
                        break;
                    }
                }
                WsServerMessage::Binary(data) => {
                    tracing::debug!(
                        "JS -> Client: {} bytes (ws_id: {})",
                        data.len(),
                        ws_id_clone
                    );
                    if let Err(e) = ws_sender.send(Message::Binary(data)).await {
                        tracing::error!("Failed to send binary to client: {}", e);
                        break;
                    }
                }
                WsServerMessage::Close { code, reason } => {
                    tracing::info!(
                        "JS requested close (ws_id: {}) code={}, reason={}",
                        ws_id_clone,
                        code,
                        reason
                    );
                    let _ = ws_sender
                        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                            code,
                            reason: std::borrow::Cow::Owned(reason),
                        })))
                        .await;
                    break;
                }
            }
        }

        tracing::debug!("JS -> Client task ended (ws_id: {})", ws_id_clone);
    });

    // Wait for either task to complete (indicates connection closed)
    tokio::select! {
        _ = client_to_js_task => {
            tracing::debug!("Client task completed first for ws_id: {}", ws_id);
        }
        _ = js_to_client_task => {
            tracing::debug!("JS task completed first for ws_id: {}", ws_id);
        }
    }

    // Cleanup: unregister WebSocket connection
    if let Err(e) = app_manager
        .unregister_ws_connection_generation(&key, routing_generation.as_deref(), ws_id.clone())
        .await
    {
        tracing::error!("Failed to unregister WebSocket {}: {}", ws_id, e);
    }

    tracing::info!(
        app_id = %key.app_id(),
        environment = %key.environment(),
        "WebSocket bridge closed for app: {}, ws_id: {}",
        key,
        ws_id
    );
}

/// Open the inspector WebSocket, from the management listener.
///
/// The protocol is unchanged from when this rode on the data port: the caller
/// names its target as `x-deis-app-id: _inspect:<app>`, carries the token
/// `StartInspect` minted in `x-deis-inspect-token`, and upgrades. What changed
/// is the door: this handler is mounted on the listener only this fleet's
/// machines can reach, so a stolen token no longer works from a connection
/// that proves nothing, and the node's public port has nothing to say about
/// debuggers at all.
///
/// The caller was admitted by the TLS handshake - it proved a certificate of
/// this fleet. Nothing here re-checks *which* machine it was: minting a token
/// already required the control plane, and the token is the answer to "who may
/// attach", scoped to one application and one environment, with an expiry.
pub async fn handle_inspector_upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: Option<WebSocketUpgrade>,
) -> Response {
    let target = match headers
        .get(APP_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|id| id.strip_prefix("_inspect:"))
    {
        Some(target_app_id) => target_app_id,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Missing or malformed {APP_ID_HEADER} header: expected _inspect:<app>"),
            )
                .into_response();
        }
    };

    let token = headers
        .get("x-deis-inspect-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let environment = requested_environment(&headers).to_string();
    let target = AppKey::new(target, environment);
    if !state.app_manager.validate_inspect_token(&target, token) {
        return (StatusCode::FORBIDDEN, "Invalid or expired inspector token").into_response();
    }

    match ws {
        Some(ws_upgrade) => {
            let app_manager = state.app_manager.clone();
            ws_upgrade
                .on_upgrade(move |socket| handle_inspector_session(socket, target, app_manager))
                .into_response()
        }
        None => (
            StatusCode::BAD_REQUEST,
            "Inspector requires WebSocket connection",
        )
            .into_response(),
    }
}

/// Handle debugger session
async fn handle_inspector_session(
    socket: WebSocket,
    key: AppKey,
    app_manager: Arc<crate::app::ApplicationManager>,
) {
    tracing::info!("Inspector session started for app: {}", key);
    let (mut ws_sender, mut ws_receiver) = socket.split();

    match app_manager.connect_inspector(&key).await {
        Ok((in_tx, mut out_rx)) => {
            // Task: WS -> V8
            let ws_to_v8 = tokio::spawn(async move {
                while let Some(msg) = ws_receiver.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            if in_tx.send(text).is_err() {
                                break;
                            }
                        }
                        Ok(Message::Close(_)) => break,
                        Err(_) => break,
                        _ => {}
                    }
                }
            });

            // Task: V8 -> WS
            let v8_to_ws = tokio::spawn(async move {
                while let Some(text) = out_rx.recv().await {
                    if ws_sender.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
            });

            let _ = tokio::join!(ws_to_v8, v8_to_ws);
        }
        Err(e) => {
            tracing::error!("Failed to connect inspector for app {}: {}", key, e);
            let _ = ws_sender
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: 1011,
                    reason: std::borrow::Cow::from(format!("Failed to connect: {}", e)),
                })))
                .await;
        }
    }
    tracing::info!("Inspector session ended for app: {}", key);
}
