//! The headers a handler answered with, on the way out of the isolate.
//!
//! A response's header list is not a map. One name may legitimately appear
//! more than once, and for exactly one name it *has* to: `Set-Cookie`. The
//! Fetch specification says so explicitly, and `deno_fetch`'s `Headers`
//! implements it - iterating a `Headers` combines every repeated name into one
//! comma-joined value, and skips that combining for `Set-Cookie` alone, which
//! comes back as one entry per cookie.
//!
//! This node used to hand that iteration to a JSON object and then to a
//! `HashMap<String, String>`. Both keep one value per name, so an application
//! answering with a session cookie and a CSRF cookie sent exactly one of them
//! (the last one) and nothing anywhere said a cookie had been dropped. It is
//! the kind of defect that is invisible until somebody's login stops working
//! in production.
//!
//! The list therefore travels as pairs from `js/runtime.js` to here, and
//! [`fold_duplicate_names`] is where the old answer is reproduced for the
//! applications that predate the fix. Which of the two an application gets is
//! not decided here; it is decided once, when its version is installed, and
//! asked as `compatibility.is_on(Flag::MultipleSetCookieHeaders)` at the one
//! call site in `super::isolate`.

/// Status-line headers as the isolate hands them over: in the order the
/// handler set them, repeats included.
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

/// The first value under a name, compared without regard to case.
///
/// A list has no `get`, and the two places that need one are looking for
/// headers this node itself wrote, so the first match is the right one.
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
