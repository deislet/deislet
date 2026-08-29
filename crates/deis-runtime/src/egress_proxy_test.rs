#[cfg(test)]
mod tests {
    use crate::egress_proxy::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// A stand-in environment. The real one cannot be used: every other test
    /// in this binary shares it, and mutating it under them is a data race.
    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn decide(
        vars: HashMap<String, String>,
        applications_may_use_it: bool,
    ) -> (ProxyDecision, Vec<String>) {
        let removed = Arc::new(Mutex::new(Vec::new()));
        let seen = removed.clone();
        let decision = ProxyDecision::decide(
            applications_may_use_it,
            move |name| vars.get(name).cloned(),
            move |name| seen.lock().unwrap().push(name.to_string()),
        );
        let removed = removed.lock().unwrap().clone();
        (decision, removed)
    }

    /// The point of the whole module: nothing an operator put in the node's
    /// environment gets to decide where an application's fetch connects.
    #[test]
    fn every_proxy_variable_is_taken_out_of_the_environment() {
        let (_, removed) = decide(
            env(&[
                ("HTTP_PROXY", "http://corp:3128"),
                ("http_proxy", "http://corp:3128"),
                ("HTTPS_PROXY", "http://corp:3128"),
                ("https_proxy", "http://corp:3128"),
                ("ALL_PROXY", "socks5://corp:1080"),
                ("all_proxy", "socks5://corp:1080"),
            ]),
            false,
        );

        // Both spellings of each. deno_fetch tries the upper-case name and
        // falls back to the lower-case one, so removing only one of a pair
        // removes nothing at all.
        assert_eq!(
            removed,
            [
                "ALL_PROXY",
                "all_proxy",
                "HTTPS_PROXY",
                "https_proxy",
                "HTTP_PROXY",
                "http_proxy"
            ]
        );
    }

    /// `NO_PROXY` can only narrow what a proxy intercepts. Taking it out of an
    /// environment that still names a proxy would widen that proxy's reach,
    /// which is the opposite of what this module is for.
    #[test]
    fn no_proxy_is_left_where_it_was() {
        let (_, removed) = decide(
            env(&[
                ("HTTP_PROXY", "http://corp:3128"),
                ("NO_PROXY", "127.0.0.1,.internal"),
            ]),
            false,
        );

        assert_eq!(removed, ["HTTP_PROXY"]);
    }

    /// A node with nothing in its environment must not be told anything was
    /// done to it.
    #[test]
    fn an_environment_with_no_proxy_in_it_is_left_alone() {
        let (decision, removed) = decide(env(&[("PATH", "/usr/bin")]), false);

        assert!(removed.is_empty());
        assert!(
            decision.is_empty(),
            "nothing was found, so nothing should be reported"
        );
    }

    /// The escape hatch. An operator who genuinely egresses through a proxy
    /// keeps it, and the warning that says what it costs is the price.
    #[test]
    fn the_operator_can_opt_applications_back_in() {
        let (decision, removed) = decide(env(&[("HTTPS_PROXY", "http://corp:3128")]), true);

        assert!(
            removed.is_empty(),
            "fetch.proxy_from_env = true must leave the environment as the operator wrote it"
        );
        assert!(
            !decision.is_empty(),
            "the proxy is still there and the startup line has to say so"
        );
    }

    /// The half that keeps the node itself working. The variables are gone
    /// from the environment `reqwest` would have read, so the values have to
    /// reach this node's own clients some other way, or a node behind a
    /// corporate proxy loses its control plane.
    #[test]
    fn the_nodes_own_clients_still_get_the_proxy_that_was_removed() {
        let (decision, _) = decide(
            env(&[("ALL_PROXY", "http://corp:3128"), ("NO_PROXY", "127.0.0.1")]),
            false,
        );

        decision
            .apply(reqwest::Client::builder())
            .expect("the captured proxy is a URL reqwest accepts")
            .build()
            .expect("a client configured with it builds");
    }

    /// A proxy URL that is not one has to fail loudly at startup rather than
    /// silently leaving the node without a proxy it was told to use.
    #[test]
    fn a_proxy_url_that_is_not_a_url_fails_rather_than_being_dropped() {
        let (decision, _) = decide(env(&[("HTTP_PROXY", "not a url")]), false);

        let error = decision
            .apply(reqwest::Client::builder())
            .expect_err("a malformed proxy URL must not be swallowed");
        assert!(error.to_string().contains("HTTP_PROXY"), "{error}");
    }

    /// With the variables left in place, `reqwest` reads them itself. Setting
    /// the same proxy a second time from a snapshot taken at startup could
    /// only make the two readings disagree.
    #[test]
    fn nothing_is_configured_twice_when_the_environment_was_left_alone() {
        let (decision, _) = decide(env(&[("HTTP_PROXY", "http://corp:3128")]), true);

        decision
            .apply(reqwest::Client::builder())
            .expect("opting in leaves the builder untouched")
            .build()
            .expect("a client builds");
    }
}
