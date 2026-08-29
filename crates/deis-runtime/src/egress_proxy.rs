//! Whether an HTTP proxy named in this node's environment gets to see an
//! application's outbound traffic.
//!
//! # What the environment decided, unasked
//!
//! `deno_fetch` builds its HTTP client with `proxy::from_env()`, and it does so
//! unconditionally: there is no option to suppress it. So `HTTP_PROXY`,
//! `HTTPS_PROXY` and `ALL_PROXY` in the *node process's* environment -- a
//! systemd drop-in, a container's env file, a shell somebody happened to start
//! the binary from -- decided where every application's `fetch()` actually
//! connected. Nothing in the platform's configuration said so and nothing
//! logged it.
//!
//! That is worse than a surprise. When a proxy intercepts, the connector dials
//! the *proxy* and the destination travels as a `CONNECT` target or a request
//! line. [`crate::runtime::fetch_policy::PolicyResolver`] -- the layer that
//! makes a name resolve only to addresses the policy accepts, and the reason a
//! name cannot be rebound between the check and the connect -- then resolves
//! the proxy's host and never sees the destination's at all. The pre-flight
//! check still judges the URL, so a destination an application names is refused
//! exactly as before; what is lost is the guarantee that the socket goes where
//! the check looked.
//!
//! # What this does instead
//!
//! The variables are read once, on the main thread, before the tokio runtime
//! exists, and taken out of the process environment. Every client built after
//! that -- and `deno_fetch`'s is built lazily, inside the first `op_fetch` of
//! an isolate, so it is always after -- sees an environment with no proxy in
//! it. What was removed is logged at warn, naming each variable, because a node
//! that was reaching the internet through a proxy yesterday needs to be told
//! why it is not today.
//!
//! # The node's own traffic still goes through it
//!
//! Something *does* need those variables: this node's own `reqwest` clients --
//! the assignment poll, the registration call and the bundle download -- read
//! the same names, and a node deployed behind a corporate proxy reaches its
//! control plane through one. Removing the variables and stopping there would
//! have taken that node off the air.
//!
//! So the values are kept and handed back to those three clients explicitly,
//! through [`for_node_clients`]. The node keeps its proxy; applications do not
//! inherit it. That split is the whole point: the proxy is part of how the
//! operator connects *their node*, not a routing decision applications were
//! ever told about.
//!
//! # If applications really are meant to egress through it
//!
//! `[fetch] proxy_from_env = true` leaves the variables in place, which puts
//! `deno_fetch` back the way it was. It is an explicit choice, it is logged at
//! warn every time the node starts, and the log line says what it costs: the
//! resolver guarantee is gone for application traffic, so a name is resolved
//! once by the policy and once more by the proxy, and nothing in this process
//! constrains the second answer.

use std::sync::OnceLock;

/// The variables `deno_fetch`'s `proxy::from_env()` reads, in the order it
/// reads them. Both cases of each: it tries the upper-case spelling and falls
/// back to the lower-case one, so removing only one of the pair removes
/// nothing.
///
/// `NO_PROXY` is deliberately absent. It can only ever *narrow* what a proxy
/// intercepts, so leaving it in an environment that no longer names a proxy
/// costs nothing, and taking it out of one that does would widen the proxy's
/// reach.
const PROXY_VARS: &[&str] = &[
    "ALL_PROXY",
    "all_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
];

/// What was found in the environment and what became of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyDecision {
    /// Variables that were set, in `PROXY_VARS` order, with their values.
    found: Vec<(String, String)>,
    /// Which of them were taken out of the process environment. Empty when the
    /// operator opted applications back in.
    removed: Vec<String>,
    /// `NO_PROXY` (or `no_proxy`), left where it was and passed on to this
    /// node's own clients so they read it the same way `reqwest` would have.
    no_proxy: Option<String>,
    /// Whether applications were left able to reach the proxy.
    applications_may_use_it: bool,
}

impl ProxyDecision {
    /// Read the proxy variables and, unless the operator asked otherwise, take
    /// them out of the process environment.
    ///
    /// **Call this while the process is still single-threaded.** Removing an
    /// environment variable mutates a table the whole process shares, and a
    /// thread reading it at the same moment is a data race. `main` does it
    /// before the tokio runtime is built, which is also before any HTTP client
    /// could have been.
    pub fn settle(applications_may_use_it: bool) -> Self {
        Self::decide(
            applications_may_use_it,
            |name| std::env::var(name).ok(),
            |name| std::env::remove_var(name),
        )
    }

    /// The decision itself, with reading and removing handed in so it can be
    /// tested without touching the environment of the test process -- which
    /// every other test in this binary is running in at the same time.
    fn decide(
        applications_may_use_it: bool,
        read: impl Fn(&str) -> Option<String>,
        mut remove: impl FnMut(&str),
    ) -> Self {
        let mut found = Vec::new();
        let mut removed = Vec::new();

        for name in PROXY_VARS {
            let Some(value) = read(name) else {
                continue;
            };
            found.push(((*name).to_string(), value));
            if !applications_may_use_it {
                remove(name);
                removed.push((*name).to_string());
            }
        }

        let no_proxy = read("NO_PROXY").or_else(|| read("no_proxy"));

        Self {
            found,
            removed,
            no_proxy,
            applications_may_use_it,
        }
    }

    /// Whether this node's environment named a proxy at all.
    pub fn is_empty(&self) -> bool {
        self.found.is_empty()
    }

    /// The value of one variable, upper-case spelling first, as
    /// `proxy::from_env` and `reqwest` both read them.
    fn value_of(&self, upper: &str, lower: &str) -> Option<&str> {
        self.found
            .iter()
            .find(|(name, _)| name == upper || name == lower)
            .map(|(_, value)| value.as_str())
    }

    /// Say on the node's log what happened. Called once, after logging is set
    /// up: this runs before that, so it cannot log for itself.
    ///
    /// Warn, not info. A node whose outbound path changed at startup is
    /// something an operator has to be able to find afterwards, and the line
    /// that explains why an application's `fetch` stopped reaching the internet
    /// is worth more than a tidy log.
    pub fn log(&self) {
        if self.found.is_empty() {
            tracing::debug!(
                "No HTTP proxy in this node's environment; applications reach their \
                 destinations directly, which is what the outbound fetch policy assumes"
            );
            return;
        }

        let names: Vec<&str> = self.found.iter().map(|(name, _)| name.as_str()).collect();

        if self.applications_may_use_it {
            tracing::warn!(
                variables = ?names,
                "fetch.proxy_from_env is on, so {} stay(s) in this node's environment and \
                 applications' outbound calls go through that proxy. This gives up the \
                 rebinding guarantee for application traffic: the connector dials the proxy \
                 and the destination travels as a CONNECT target, so the policy's own DNS \
                 resolver never sees the destination's addresses and cannot hold the socket \
                 to the addresses the pre-flight check approved. The check on the URL itself, \
                 and the refusal of this platform's own service endpoints, are unaffected",
                names.join(", ")
            );
            return;
        }

        tracing::warn!(
            removed = ?self.removed,
            "Removed {} from this node's environment before any HTTP client was built. \
             deno_fetch reads those variables unconditionally when it builds an \
             application's client, so they decided where every application's fetch() \
             connected -- and a proxied connection dials the proxy, which leaves the \
             outbound policy's DNS resolver looking at the proxy's addresses instead of the \
             destination's. This node's own control-plane traffic still uses the proxy; only \
             applications no longer do. Set fetch.proxy_from_env = true to put it back",
            self.removed.join(", ")
        );
    }

    /// Give one of this node's own `reqwest` clients the proxy that was taken
    /// out of the environment for it.
    ///
    /// A no-op when nothing was found, and when the operator left the variables
    /// in place: `reqwest` reads them itself in that case, and configuring the
    /// same proxy twice would only risk the two readings disagreeing.
    pub fn apply(&self, builder: reqwest::ClientBuilder) -> anyhow::Result<reqwest::ClientBuilder> {
        if self.applications_may_use_it || self.found.is_empty() {
            return Ok(builder);
        }

        let no_proxy = self
            .no_proxy
            .as_deref()
            .and_then(reqwest::NoProxy::from_string);

        // The fallback to `ALL_PROXY` is how both `reqwest` and `deno_fetch`
        // read these: a scheme-specific variable wins, `ALL_PROXY` covers what
        // is left.
        let all = self.value_of("ALL_PROXY", "all_proxy");
        let http = self.value_of("HTTP_PROXY", "http_proxy").or(all);
        let https = self.value_of("HTTPS_PROXY", "https_proxy").or(all);

        // `no_proxy()` first: it clears the environment lookup `reqwest` would
        // otherwise do. The environment no longer has anything to find, but
        // saying so keeps this client's proxying entirely in the values below.
        let mut builder = builder.no_proxy();
        if let Some(url) = http {
            builder = builder.proxy(
                reqwest::Proxy::http(url)
                    .map_err(|e| anyhow::anyhow!("HTTP_PROXY is not a proxy URL: {e}"))?
                    .no_proxy(no_proxy.clone()),
            );
        }
        if let Some(url) = https {
            builder = builder.proxy(
                reqwest::Proxy::https(url)
                    .map_err(|e| anyhow::anyhow!("HTTPS_PROXY is not a proxy URL: {e}"))?
                    .no_proxy(no_proxy),
            );
        }
        Ok(builder)
    }
}

/// The decision this process made, for the clients that are built long after
/// `main` made it.
static DECISION: OnceLock<ProxyDecision> = OnceLock::new();

/// Publish the decision. Called once, from `main`; a second call is ignored.
pub fn install(decision: ProxyDecision) {
    let _ = DECISION.set(decision);
}

/// Configure one of this node's own HTTP clients with whatever proxy was taken
/// out of the environment on its behalf.
///
/// Unchanged when `main` never made a decision -- a test binary, or an embedder
/// that uses this crate as a library. The environment was never touched on
/// those paths either, so `reqwest`'s own reading of it is still correct.
pub fn for_node_clients(builder: reqwest::ClientBuilder) -> anyhow::Result<reqwest::ClientBuilder> {
    match DECISION.get() {
        Some(decision) => decision.apply(builder),
        None => Ok(builder),
    }
}

#[cfg(test)]
#[path = "egress_proxy_test.rs"]
mod egress_proxy_test;
