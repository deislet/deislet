// HTTP request handlers

use super::AppState;
use crate::app::AppKey;
use crate::runtime::header_list;
use crate::{is_platform_header, APP_ID_HEADER, ENVIRONMENT_HEADER, PLATFORM_HEADER_PREFIX};
use axum::{
    body::{Body, Bytes},
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{header, HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;

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
/// Deliberately narrower than the grammar: no userinfo (`@`), because a URL
/// reader takes everything before it as a user name and the *host* becomes
/// whatever follows, so `Host: shop.example.com@evil.invalid` would answer
/// with a URL whose origin is `evil.invalid`. No percent-escapes either --
/// nothing legitimate needs one in a host, and allowing them would mean this
/// check and the URL parser inside the isolate had to agree about decoding.
///
/// What is left is what a real `Host` carries: a registered name, an IPv4
/// literal, a bracketed IPv6 literal, and an optional `:port`.
///
/// # Structure, not a set of characters
///
/// This used to be a single pass that accepted any string built out of the
/// characters those four forms are spelled with -- `:`, `[` and `]` among
/// them, anywhere and any number of times. That admits strings no URL parser
/// will take: `a:b` has a port that is not a number, `[::1` never closes its
/// bracket, `a:1:2` has two ports. Each of them was pasted into `request.url`
/// and `new Request(url)` threw inside the isolate, so the request was
/// answered 500 with this platform's own error text in the body -- and the
/// proxy strips the port before it matches a route, so such a request routes
/// to the right application and only then dies. `Host: a:b` was enough to
/// make every request to a node fail that way.
///
/// So each of the four forms is recognised as a whole: the host is split from
/// the port first, the port is a number a URL may carry, and the host is
/// either a bracketed address the standard library can parse or a name spelled
/// without any of the three delimiters. The property this check has to have is
/// that what it accepts, the isolate's URL parser also accepts, and both
/// halves of that are walked through a real isolate by
/// `dispatcher::handlers_test`.
fn usable_authority(authority: &str) -> bool {
    // A `Host` longer than this is not a name anybody serves; the ceiling is
    // here so the string cannot become a way to make every log line and every
    // URL in an isolate arbitrarily long.
    const LONGEST: usize = 255;

    if authority.is_empty() || authority.len() > LONGEST {
        return false;
    }

    // The brackets are read before anything else: an IPv6 literal is full of
    // colons itself, so the `:port` of `[::1]:8080` can only be looked for
    // after the closing bracket. Everything else has no bracket at all, and
    // its first colon starts its port.
    let (name, port) = match authority.strip_prefix('[') {
        Some(rest) => {
            let Some((inside, after)) = rest.split_once(']') else {
                // `[::1` -- an address that never closes. A URL parser refuses
                // it, so it cannot become the authority of `request.url`.
                return false;
            };
            if inside.parse::<std::net::Ipv6Addr>().is_err() {
                // Anything inside the brackets that is not an address,
                // including a zone id (`fe80::1%25eth0`) that this check
                // refuses along with every other percent-escape.
                return false;
            }
            match after {
                "" => (None, None),
                _ => match after.strip_prefix(':') {
                    Some(port) => (None, Some(port)),
                    // Something after the literal that is not a port, such as
                    // `[::1]x`.
                    None => return false,
                },
            }
        }
        None => match authority.split_once(':') {
            Some((name, port)) => (Some(name), Some(port)),
            None => (Some(authority), None),
        },
    };

    // A name, when there is one: `[::1]` has already been parsed as an address
    // and has nothing left to check. Empty is refused here, which is what
    // makes `:8080` -- a port with no host -- and `a:b:c`, whose "port" is
    // `b:c`, both fail.
    if matches!(name, Some(name) if !usable_host_name(name)) {
        return false;
    }

    // A port, when there is one. `u16` is the range a URL may carry: a WHATWG
    // URL parser refuses `http://a:70000/` as surely as it refuses
    // `http://a:b/`, and an empty port is refused here rather than accepted as
    // "the default", so what reaches an isolate is a port or no port at all.
    match port {
        Some(port) => port.parse::<u16>().is_ok(),
        None => true,
    }
}

/// A registered name or an IPv4 literal -- what a `Host` carries when it is
/// not a bracketed IPv6 address.
///
/// The three delimiters are absent by construction, so a name that gets here
/// cannot reopen the question the split above just answered. IPv4 is not
/// recognised separately because its spelling is already a subset of this one.
fn usable_host_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
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

    // Check if it's a debug request
    if let Some(target_app_id) = app_id.strip_prefix("_inspect:") {
        let token = headers
            .get("x-deis-inspect-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let target = AppKey::new(target_app_id, environment);
        if !state.app_manager.validate_inspect_token(&target, token) {
            return (StatusCode::FORBIDDEN, "Invalid or expired inspector token").into_response();
        }

        if let Some(ws_upgrade) = ws {
            let app_manager = state.app_manager.clone();
            return ws_upgrade
                .on_upgrade(move |socket| handle_inspector_session(socket, target, app_manager))
                .into_response();
        } else {
            return (
                StatusCode::BAD_REQUEST,
                "Inspector requires WebSocket connection",
            )
                .into_response();
        }
    }

    // The pair names a directory on this node and a storage tenant, so a
    // request that carries something else is refused here rather than being
    // allowed to reach code that would build a path out of it.
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
        if let Some(assets) = state.app_manager.assets(&key) {
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
                return response;
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

    // Execute application
    match state
        .app_manager
        .execute_app(&key, url, method.to_string(), request_headers, body_opt)
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
                                handle_websocket_bridged(socket, key_clone, ws_id, app_manager)
                            })
                            .into_response();
                    } else {
                        tracing::warn!(
                            app_id = %key.app_id(),
                            environment = %key.environment(),
                            "App {} returned status 101 but no X-Runtime-WebSocket-ID header",
                            key
                        );
                    }
                }
            }

            // Normal HTTP response
            let mut response = Response::builder().status(status);

            for (key, value) in resp_headers {
                response = response.header(key, value);
            }

            response.body(http_body(resp_body)).unwrap().into_response()
        }
        Err(e) => {
            tracing::error!(
                app_id = %key.app_id(),
                environment = %key.environment(),
                "Application {} execution failed: {}",
                key,
                e
            );
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
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
    ws_id: String,
    app_manager: Arc<crate::app::ApplicationManager>,
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
        .register_ws_connection(
            &key,
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
                        .send_ws_message(&key_clone, ws_id_clone.clone(), ws_frame::text(&text))
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
                        .send_ws_message(&key_clone, ws_id_clone.clone(), ws_frame::binary(&data))
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
        .unregister_ws_connection(&key, ws_id.clone())
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
