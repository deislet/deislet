//! The one shape a header list has on this platform, wherever it crosses a
//! boundary.
//!
//! A header list is not a map. One name may legitimately appear more than
//! once - `Set-Cookie` on the way out, `Cookie`, `Accept`, `Accept-Language`
//! and `X-Forwarded-For` on the way in - and for `Set-Cookie` it *has* to: the
//! Fetch specification says so explicitly, and `deno_fetch`'s `Headers`
//! implements it. Iterating a `Headers` combines every repeated name into one
//! comma-joined value and skips that combining for `Set-Cookie` alone, which
//! comes back as one entry per cookie.
//!
//! This node used to hand each of those crossings to a JSON object and then to
//! a `HashMap<String, String>`. Both keep one value per name, so:
//!
//! * an application answering with a session cookie and a CSRF cookie sent
//!   exactly one of them (the last one) and nothing anywhere said a cookie had
//!   been dropped;
//! * a request carrying two `Cookie` lines, two `Accept` lines or an
//!   `X-Forwarded-For` chain reached the handler with one value, and which one
//!   survived was whichever the map happened to see last - so a handler
//!   reading the client's address read the last proxy's, and one reading a
//!   session cookie could not find it because a second cookie header had
//!   overwritten it.
//!
//! # The one name the comma rule is wrong for
//!
//! Keeping the repeats is not the whole answer for `Cookie`. A client may
//! split its cookies across several `Cookie` fields -- RFC 9113 section 8.2.3
//! invites it, so that HTTP/2's header compressor can index each cookie
//! separately, and browsers take the invitation -- and a server reconstructing
//! the request must join them with `"; "`, not with the `", "` a `Headers`
//! joins everything else with. [`combine_cookie_fields`] does that join, on
//! the way into an isolate and nowhere else.
//!
//! [`HeaderPairs`] is that shape, and every crossing uses it: the request
//! entering an isolate (`dispatcher/handlers.rs`), the request of an
//! application-to-application call (`runtime/service_ops.rs`), the answer
//! leaving an isolate (`runtime/isolate.rs`), and the entry `Cache.put` writes
//! (`js/runtime.js`). `js/runtime.js` builds and reads the JavaScript half of
//! all four through one function, `headerPairs`.
//!
//! # Where the old answer still lives
//!
//! [`fold_duplicate_names`] reproduces the map for the applications that
//! predate the fix. Which of the two an application gets is not decided here;
//! it is decided once, when its version is installed, and asked as
//! `compatibility.is_on(Flag::MultipleSetCookieHeaders)` at the one call site
//! in `super::isolate`.
//!
//! That gate covers the *response* only, and deliberately: the flag is named
//! for what a client sees, and the day it shipped is the day repeated
//! `Set-Cookie` headers started reaching clients. The request side has no gate
//! because there is no old answer worth reproducing - a `HashMap` has no
//! order, so which of two `Cookie` lines a handler used to receive was not
//! defined by anything an application could have been written against. Giving
//! it both is strictly more information, in the order the client sent them.

/// A header list as it crosses a boundary: in the order it was written,
/// repeats included.
pub type HeaderPairs = Vec<(String, String)>;

/// Reproduce the answer this platform gave before duplicate header names
/// survived: one value per name, the last one written winning, in the
/// position the name first appeared.
///
/// Written for duplicate names in general rather than for `Set-Cookie`
/// specifically, even though `Set-Cookie` is the only name that can reach here
/// twice today. That is not a guess about the future: the rule about which
/// headers may not be combined belongs to the Fetch specification, and the day
/// it exempts a second name, a fold that special-cased `Set-Cookie` would
/// start dropping that one in silence - the exact failure this whole module
/// exists to have caught once.
///
/// The position rule matters as much as the value rule: it is what a JSON
/// object did, and an application that predates the fix has to get back the
/// bytes it was getting, not something close to them.
pub fn fold_duplicate_names(pairs: HeaderPairs) -> HeaderPairs {
    let mut folded: HeaderPairs = Vec::with_capacity(pairs.len());
    for (name, value) in pairs {
        match folded
            .iter_mut()
            .find(|(seen, _)| seen.eq_ignore_ascii_case(&name))
        {
            Some(existing) => existing.1 = value,
            None => folded.push((name, value)),
        }
    }
    folded
}

/// The one header whose repeats are joined with something other than a comma.
const COOKIE: &str = "cookie";

/// What separates two cookies inside one `Cookie` field.
///
/// RFC 9113 section 8.2.3 names these two octets for this one header: an
/// HTTP/2 client may split its cookies across several `Cookie` fields, and a
/// server reconstructing the request "MUST" join them with `0x3B 0x20`. The
/// `Headers` rule -- join everything with `", "` -- is right for every other
/// name and wrong for this one.
const COOKIE_SEPARATOR: &str = "; ";

/// Join the request's `Cookie` fields into the single field HTTP semantics
/// say they are.
///
/// A `Cookie` field is a list of `name=value` pairs separated by `; `, and a
/// client is allowed to split that list across several fields -- HTTP/2 makes
/// it likely, because RFC 9113 section 8.2.3 lets a client split them so the
/// header compressor can index each cookie separately, and browsers do.
/// Everything downstream reads them the way the web reads them: the router
/// deis-build generates does `request.headers.get("cookie").split(";")`, and
/// `Headers.get` joins repeats with `", "`.
///
/// Left alone, a request that arrived as `Cookie: session=abc` and
/// `Cookie: csrf=xyz` therefore reached a handler as the single value
/// `"session=abc, csrf=xyz"` -- measured through the node, in
/// `dispatcher::handlers_test` -- and the router's split cuts that into one
/// entry: the cookie `session`, whose value is `abc, csrf=xyz`. Everything
/// after the first cookie is gone. It is a real login failing, not a
/// formatting nuisance: a session cookie sent second cannot be read at all.
///
/// So the fields are joined here, once, on the way into the isolate, with the
/// separator that header actually uses. Every repeat collapses into the
/// position of the first, which is the order the client sent them in and the
/// order a `Cookie` field is meant to preserve. Nothing else is touched: this
/// is the only header the Fetch specification's comma rule is wrong for, and
/// a general "join by something else" would need the specification to say so
/// for a second name before it could be written honestly.
pub fn combine_cookie_fields(pairs: &mut HeaderPairs) {
    if count_of(pairs, COOKIE) < 2 {
        return;
    }
    let combined = pairs
        .iter()
        .filter(|(name, value)| name.eq_ignore_ascii_case(COOKIE) && !value.is_empty())
        .map(|(_, value)| value.as_str())
        .collect::<Vec<&str>>()
        .join(COOKIE_SEPARATOR);

    let mut placed = false;
    pairs.retain_mut(|(name, value)| {
        if !name.eq_ignore_ascii_case(COOKIE) {
            return true;
        }
        if placed {
            return false;
        }
        placed = true;
        // The first field keeps its place and its spelling; only its value
        // grows. A handler iterating the list sees the header where the client
        // put it.
        value.clear();
        value.push_str(&combined);
        true
    });
}

/// The first value under a name, compared without regard to case.
///
/// A list has no `get`, and the places that need one are looking for headers
/// this node itself wrote, so the first match is the right one.
pub fn first_value<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(seen, _)| seen.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// How many values a name carries. Used only to say something true in a log
/// line about what was dropped.
pub fn count_of(pairs: &[(String, String)], name: &str) -> usize {
    pairs
        .iter()
        .filter(|(seen, _)| seen.eq_ignore_ascii_case(name))
        .count()
}

/// Every header of an inbound request, in the order it arrived, repeats kept,
/// minus the ones `keep` rejects.
///
/// The rejected names are answered beside the list rather than dropped in
/// silence, so the caller can say in a log line what it took away and why:
/// this is the sweep of the platform's own `x-deis-` namespace, and an
/// operator looking at a request that lost a header needs to be able to find
/// out that it did.
///
/// A value that is not text is skipped. `HeaderValue` may hold arbitrary
/// bytes, a JavaScript `Headers` may not, and a header nobody can spell is
/// better absent than mangled.
pub fn from_header_map(
    headers: &axum::http::HeaderMap,
    keep: impl Fn(&str) -> bool,
) -> (HeaderPairs, Vec<String>) {
    let mut kept = HeaderPairs::with_capacity(headers.len());
    let mut refused = Vec::new();
    for (name, value) in headers.iter() {
        if !keep(name.as_str()) {
            refused.push(name.as_str().to_string());
            continue;
        }
        if let Ok(value) = value.to_str() {
            kept.push((name.as_str().to_string(), value.to_string()));
        }
    }
    (kept, refused)
}

/// Give one name exactly one value, whatever the list held for it before.
///
/// What a map's `insert` did, kept as its own verb now that the list does not
/// do it by accident. The two callers are this node writing down what *it*
/// decided -- the application and environment a request resolved to, and
/// whether the client asked for a WebSocket upgrade -- and both of those are
/// answers, not contributions: a second value would be read by a handler as a
/// comma-joined pair of them.
pub fn set_unique(pairs: &mut HeaderPairs, name: &str, value: String) {
    pairs.retain(|(seen, _)| !seen.eq_ignore_ascii_case(name));
    pairs.push((name.to_string(), value));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(entries: &[(&str, &str)]) -> HeaderPairs {
        entries
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    /// The defect, stated as a test: two cookies in, one cookie out.
    #[test]
    fn folding_keeps_one_value_per_name() {
        let folded = fold_duplicate_names(pairs(&[
            ("content-type", "text/plain"),
            ("set-cookie", "session=abc; Path=/"),
            ("set-cookie", "csrf=xyz; Path=/"),
        ]));
        assert_eq!(
            folded,
            pairs(&[
                ("content-type", "text/plain"),
                ("set-cookie", "csrf=xyz; Path=/"),
            ]),
            "the last value wins, which is what a JSON object did"
        );
    }

    #[test]
    fn folding_keeps_the_position_the_name_first_appeared() {
        let folded = fold_duplicate_names(pairs(&[
            ("set-cookie", "a=1"),
            ("content-type", "text/plain"),
            ("set-cookie", "b=2"),
        ]));
        let names: Vec<&str> = folded.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec!["set-cookie", "content-type"]);
        assert_eq!(folded[0].1, "b=2");
    }

    #[test]
    fn folding_matches_names_without_regard_to_case() {
        let folded = fold_duplicate_names(pairs(&[("Set-Cookie", "a=1"), ("set-cookie", "b=2")]));
        assert_eq!(folded.len(), 1, "one header, spelled two ways");
        assert_eq!(folded[0].1, "b=2");
    }

    #[test]
    fn a_list_without_repeats_comes_through_unchanged() {
        let original = pairs(&[("content-type", "text/plain"), ("x-app", "demo")]);
        assert_eq!(fold_duplicate_names(original.clone()), original);
    }

    /// The defect, stated as a test: a login that arrives as two `Cookie`
    /// fields loses every cookie but the first.
    #[test]
    fn repeated_cookie_fields_become_the_one_field_they_are() {
        let mut list = pairs(&[
            ("accept", "text/html"),
            ("Cookie", "session=abc"),
            ("x-trace", "7"),
            ("cookie", "csrf=xyz"),
            ("cookie", "theme=dark"),
        ]);
        combine_cookie_fields(&mut list);
        assert_eq!(
            list,
            pairs(&[
                ("accept", "text/html"),
                ("Cookie", "session=abc; csrf=xyz; theme=dark"),
                ("x-trace", "7"),
            ]),
            "the cookies are one field, joined the way a Cookie field joins them, where the \
             first one arrived"
        );
    }

    /// `", "` is the rule for every other repeated name, and it stays that
    /// way: only this one header is joined differently, so nothing else may
    /// move.
    #[test]
    fn no_other_repeated_name_is_touched() {
        let original = pairs(&[
            ("accept", "text/html"),
            ("accept", "application/json"),
            ("x-forwarded-for", "203.0.113.7"),
            ("x-forwarded-for", "198.51.100.4"),
            ("cookie", "session=abc"),
        ]);
        let mut list = original.clone();
        combine_cookie_fields(&mut list);
        assert_eq!(list, original, "a single Cookie field has nothing to join");
    }

    #[test]
    fn an_empty_cookie_field_adds_no_separator() {
        // A client that sends `Cookie:` with nothing after it has sent no
        // cookie. Joining it in would produce a leading `; `, which the split
        // downstream reads as a cookie with an empty name.
        let mut list = pairs(&[("cookie", ""), ("cookie", "session=abc")]);
        combine_cookie_fields(&mut list);
        assert_eq!(list, pairs(&[("cookie", "session=abc")]));
    }

    /// The way in: every value of every name, in the order the client sent
    /// them, with the platform's own namespace answered separately so the
    /// caller can say what it took away.
    #[test]
    fn an_inbound_request_keeps_every_value_of_a_repeated_name() {
        use axum::http::{HeaderMap, HeaderName, HeaderValue};

        let mut inbound = HeaderMap::new();
        inbound.append(
            HeaderName::from_static("cookie"),
            HeaderValue::from_static("session=abc"),
        );
        inbound.append(
            HeaderName::from_static("cookie"),
            HeaderValue::from_static("csrf=xyz"),
        );
        inbound.append(
            HeaderName::from_static("x-forwarded-for"),
            HeaderValue::from_static("203.0.113.7"),
        );
        inbound.append(
            HeaderName::from_static("x-forwarded-for"),
            HeaderValue::from_static("198.51.100.4"),
        );
        inbound.append(
            HeaderName::from_static("x-deis-caller"),
            HeaderValue::from_static("admin-app.production"),
        );

        let (kept, refused) = from_header_map(&inbound, |name| !name.starts_with("x-deis-"));
        assert_eq!(count_of(&kept, "cookie"), 2, "a cookie was dropped");
        assert_eq!(first_value(&kept, "cookie"), Some("session=abc"));
        assert_eq!(
            count_of(&kept, "x-forwarded-for"),
            2,
            "the address chain arrived with one hop in it"
        );
        assert_eq!(refused, vec!["x-deis-caller".to_string()]);
        assert_eq!(
            count_of(&kept, "x-deis-caller"),
            0,
            "a header the platform stamps must not arrive from outside"
        );
    }

    /// What this node decided is one value, whatever the client sent under the
    /// same name: a second would reach a handler as a comma-joined pair.
    #[test]
    fn a_name_this_node_decides_gets_exactly_one_value() {
        let mut list = pairs(&[
            ("X-Deis-Environment", "preview"),
            ("accept", "text/html"),
            ("x-deis-environment", "staging"),
        ]);
        set_unique(&mut list, "x-deis-environment", "production".to_string());
        assert_eq!(count_of(&list, "x-deis-environment"), 1);
        assert_eq!(first_value(&list, "x-deis-environment"), Some("production"));
        assert_eq!(
            first_value(&list, "accept"),
            Some("text/html"),
            "everything else has to survive untouched"
        );
    }

    #[test]
    fn looking_a_header_up_ignores_case_and_takes_the_first() {
        let list = pairs(&[
            ("X-Runtime-WebSocket-ID", "ws-1"),
            ("x-runtime-websocket-id", "ws-2"),
        ]);
        assert_eq!(first_value(&list, "x-runtime-websocket-id"), Some("ws-1"));
        assert_eq!(first_value(&list, "x-nothing"), None);
        assert_eq!(count_of(&list, "X-RUNTIME-WEBSOCKET-ID"), 2);
    }
}
