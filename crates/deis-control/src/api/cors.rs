//! Which browser origins may read this control plane's answers.
//!
//! # Why this is not `CorsLayer::permissive()` any more
//!
//! It was, and that was the third leg of a takeover. `CorsLayer::permissive()`
//! answered every preflight with `access-control-allow-origin: *`, so any
//! page the operator visited in a browser could `fetch()` the control plane -
//! and *read every answer*, because `*` is exactly the permission to read the
//! response. The gate now demands a credential from every caller, but the
//! permissive layer let the page read refusals too, and an origin list is
//! what stops that.
//!
//! The console this binary serves needs none of that. It is delivered from
//! this same port, so its calls are same-origin and a browser sends them
//! without consulting CORS at all; `npm run dev` proxies `/api` through the
//! vite server, so the development console is same-origin too. The
//! permissive layer was buying nothing and paying for it with the whole
//! human API.
//!
//! So the default is now no CORS headers at all: a cross-origin page can
//! still cause a browser to *send* a simple request, but it can never read
//! the answer, and anything with a JSON body or a method past POST does not
//! leave the browser because the preflight goes unanswered.
//!
//! # When an operator does host a console elsewhere
//!
//! `api_cors_allowed_origins` names the origins, in full, and each is echoed
//! back only to itself. `*` is refused rather than honoured - that spelling
//! is the hole this replaced, and an origin that may read the audit log has
//! to be written down. Cookies are never in play (the credential is a header
//! on every route), so `Access-Control-Allow-Credentials` stays off.
//!
//! Being listed here grants no authority. A page on another origin presents a
//! token like any other remote caller; the origin list is what lets the
//! browser read the answers at all. See [`crate::api::authz::authorize`].

use std::time::Duration;

use axum::http::{HeaderName, HeaderValue, Method, header};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::{error, info, warn};

/// How long a browser may cache a preflight answer. Ten minutes: long enough
/// that a console does not preflight every call, short enough that removing
/// an origin from the file takes effect within one coffee break rather than
/// within the browser's own maximum.
const PREFLIGHT_MAX_AGE: Duration = Duration::from_secs(600);

/// The cross-origin policy this control plane will serve, and the list an
/// operator can be shown at startup.
///
/// `layer` is `None` for the default - no origin named, so no CORS header is
/// ever written and no preflight is answered.
pub struct CorsPolicy {
    pub layer: Option<CorsLayer>,
    /// The origins that survived validation, in the order they were written.
    /// Kept as text purely so [`crate::api::log_api_ready`] can print them.
    pub origins: Vec<String>,
}

impl CorsPolicy {
    /// Read `api_cors_allowed_origins` and say out loud what it will mean.
    ///
    /// Every entry that cannot be honoured is dropped with an `error!` naming
    /// it, because the failure it would otherwise cause is silent: the
    /// console at that origin gets a browser error with no server-side trace,
    /// and an operator has nothing to look at.
    pub fn from_origins(configured: &[String]) -> Self {
        let mut origins = Vec::with_capacity(configured.len());
        let mut values = Vec::with_capacity(configured.len());

        for entry in configured {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            match validate_origin(entry) {
                Ok(()) => match HeaderValue::from_str(entry) {
                    Ok(value) => {
                        // A duplicate is harmless but says the file was edited
                        // twice; keeping one copy keeps the startup line honest.
                        if origins.iter().any(|kept: &String| kept == entry) {
                            continue;
                        }
                        origins.push(entry.to_string());
                        values.push(value);
                    }
                    Err(e) => error!(
                        origin = %entry,
                        error = %e,
                        "Ignoring an api_cors_allowed_origins entry that cannot be sent as a header value; a browser on that origin will be refused by this control plane with no further explanation"
                    ),
                },
                Err(reason) => error!(
                    origin = %entry,
                    reason,
                    "Ignoring an api_cors_allowed_origins entry; a browser on that origin will be refused by this control plane with no further explanation, so fix the spelling rather than expecting it to work"
                ),
            }
        }

        if values.is_empty() {
            if !configured.is_empty() {
                warn!(
                    configured = configured.len(),
                    "api_cors_allowed_origins named origins and none of them could be honoured, so this control plane serves no cross-origin browser call at all; a console hosted anywhere but this port will not load any data"
                );
            }
            return Self {
                layer: None,
                origins,
            };
        }

        info!(
            origins = ?origins,
            "Cross-origin browser access is allowed from these origins and no others; a page on any other origin cannot read this control plane's answers, and being listed here is not authority - such a page still presents a token, because the unauthenticated admission is for direct callers on this machine only"
        );

        let layer = CorsLayer::new()
            // `AllowOrigin::list` echoes the request's origin only when it is
            // one of these, and adds `Vary: origin` so a cache cannot serve
            // one origin's answer to another.
            .allow_origin(AllowOrigin::list(values))
            .allow_methods([
                Method::GET,
                Method::HEAD,
                Method::POST,
                Method::PUT,
                Method::PATCH,
                Method::DELETE,
            ])
            // The two spellings of the credential plus the content type. Not
            // `Any`: a header list that says "anything" is how a header the
            // API never meant to read becomes reachable from a page.
            .allow_headers([
                header::AUTHORIZATION,
                header::CONTENT_TYPE,
                HeaderName::from_static("x-api-key"),
            ])
            .max_age(PREFLIGHT_MAX_AGE);

        Self {
            layer: Some(layer),
            origins,
        }
    }
}

/// Whether a configured entry is a serialised origin a browser could ever
/// send: `scheme://host[:port]`, nothing before it and nothing after it.
///
/// The two mistakes this catches are the ones that otherwise fail silently.
/// `*` is the policy this file exists to remove, so it is refused by name
/// rather than quietly turned back on. A trailing slash - `https://host/` -
/// is what an operator writes by copying the address bar, and it can never
/// match: a browser's `Origin` header has no path component, so that entry
/// would sit in the file looking correct while every call from it was
/// refused.
fn validate_origin(entry: &str) -> Result<(), &'static str> {
    if entry == "*" {
        return Err(
            "'*' would let any page on the internet read this control plane's answers, which is \
             the exact hole this setting replaced; name the origins instead",
        );
    }
    let Some((scheme, rest)) = entry.split_once("://") else {
        return Err("an origin is 'scheme://host[:port]' and this has no '://'");
    };
    if !matches!(scheme, "http" | "https") {
        return Err("only http:// and https:// origins are browser origins");
    }
    if rest.is_empty() {
        return Err("an origin needs a host after the scheme");
    }
    if rest.contains('/') {
        return Err(
            "an origin has no path, so a trailing slash or any path makes it something a browser \
             never sends",
        );
    }
    if entry.chars().any(|c| c.is_ascii_uppercase()) {
        return Err(
            "a browser sends the scheme and host lowercased, so an entry with capitals can never \
             match",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default, and the whole point of it: nothing configured means no
    /// CORS layer, so no answer this control plane gives is readable by a
    /// page on another origin.
    #[test]
    fn no_configured_origin_means_no_cors_at_all() {
        let policy = CorsPolicy::from_origins(&[]);
        assert!(policy.layer.is_none());
        assert!(policy.origins.is_empty());
    }

    /// `*` is not a shortcut an operator may take here. It is the setting
    /// that let a page on any origin read the audit log and the token it had
    /// just minted, and writing it in the file must not bring it back.
    #[test]
    fn a_wildcard_origin_is_refused_rather_than_honoured() {
        let policy = CorsPolicy::from_origins(&["*".to_string()]);
        assert!(
            policy.layer.is_none(),
            "'*' in the file put the permissive policy back"
        );
        assert!(policy.origins.is_empty());
    }

    /// Everything a browser will never send is dropped, and said out loud,
    /// rather than sitting in the file looking like it works.
    #[test]
    fn an_entry_a_browser_could_never_send_is_dropped() {
        for bad in [
            "https://console.example.com/",
            "https://console.example.com/app",
            "console.example.com",
            "ftp://console.example.com",
            "https://",
            "https://Console.Example.com",
        ] {
            let policy = CorsPolicy::from_origins(&[bad.to_string()]);
            assert!(
                policy.layer.is_none() && policy.origins.is_empty(),
                "{bad} was accepted, and a browser would never match it"
            );
        }
    }

    /// A good entry survives, and a bad one next to it does not take the
    /// good one down with it.
    #[test]
    fn a_named_origin_is_kept_and_a_bad_neighbour_does_not_take_it_down() {
        let policy = CorsPolicy::from_origins(&[
            "https://console.example.com".to_string(),
            "*".to_string(),
            "  http://localhost:5173  ".to_string(),
            "https://console.example.com".to_string(),
            String::new(),
        ]);
        assert!(policy.layer.is_some());
        assert_eq!(
            policy.origins,
            vec![
                "https://console.example.com".to_string(),
                "http://localhost:5173".to_string()
            ],
            "the surviving list is what the startup banner prints, so it has to be exact"
        );
    }
}
