//! Where an application's `fetch()` may go, and where it may not.
//!
//! # Why an outbound call needs a policy at all
//!
//! Until `fetch` existed in an isolate, deis-store's only protection was that
//! nothing but this process could reach it: it trusts the `x-deis-tenant`
//! header it is given and listens on loopback. The moment an application can
//! open an outbound HTTP connection, that stops being protection. A handler
//! could `POST http://127.0.0.1:8090` with any tenant header it liked and read
//! or delete another application's keys, rows and blobs. The same reasoning
//! covers deis-queue, deis-control (which holds every node's credentials and
//! every application's secrets) and deis-telemetry.
//!
//! So an outbound destination is **denied by default and allowed by
//! exception**, which is also the model an edge platform is expected to have:
//! the public internet is reachable, the machine and the network the node
//! happens to sit on are not.
//!
//! # What is refused
//!
//! Every address that is not a public one: loopback, the private ranges, the
//! link-local range (which is where a cloud instance's metadata service lives,
//! `169.254.169.254`, and reading that is how a node's cloud credentials
//! leak), unique-local and link-local IPv6, multicast, broadcast, the
//! unspecified address, and the ranges reserved for documentation,
//! benchmarking, carrier NAT and future use. An IPv4 address written as IPv6
//! (`::ffff:127.0.0.1`) is classified as the IPv4 address it is, because it
//! reaches the same machine, and so are the transition forms that carry an
//! IPv4 address: NAT64 (`64:ff9b::/96`, `64:ff9b:1::/48`) and 6to4
//! (`2002::/16`). Teredo (`2001::/32`) hides its inner address behind an
//! obfuscation and is refused outright, as is the discard-only `100::/64`.
//!
//! # The address, not the name
//!
//! The check is on the **resolved address**. A name under an attacker's
//! control can be pointed at `127.0.0.1` as easily as it can be pointed
//! anywhere else, so a policy that only reads the hostname string is
//! decoration.
//!
//! ## The gap between resolving and connecting
//!
//! Resolving a name in order to judge it, and then handing the *name* to the
//! HTTP client so that it resolves it again, leaves a window: the second
//! answer can differ from the first. That is DNS rebinding, and it is the
//! standard way past a check like this one. It is closed here in two layers,
//! which between them cover both kinds of destination:
//!
//! * A URL whose host is an **IP literal** involves no DNS at all. The
//!   connector parses it directly and never calls a resolver, so
//!   [`FetchPolicy::check`] is both the first and the last word, and there is
//!   no window to exploit.
//! * A URL whose host is a **name** is resolved twice: once by
//!   [`FetchPolicy::check`], which produces the refusal message and the log
//!   line, and once by the connector -- through [`PolicyResolver`], which is
//!   installed as `deno_fetch`'s resolver and returns *only* addresses this
//!   policy accepts. The socket can therefore only be opened to an address
//!   the policy approved, whatever the name resolved to a moment earlier. A
//!   rebind between the two lookups changes which of the two layers refuses
//!   the call, and the wording of the error; it cannot produce a connection.
//!
//! ## Redirects
//!
//! A redirect is a second destination the application never named, and `fetch`
//! follows one by default. `js/bootstrap.js` therefore issues every request
//! with `redirect: "manual"` and follows the chain itself, calling
//! [`FetchPolicy::check`] again -- through `op_deis_fetch_guard` -- for each
//! hop, with the URL the connector will actually be given. So a hop is judged
//! by exactly the rules the first destination was judged by: the same
//! schemes, the same ranges, the same ports, the same allowlist, the same
//! refusal of this platform's own endpoints.
//!
//! It used to be otherwise, and the two holes that left are the reason the
//! loop exists. deno re-enters `op_fetch` for each hop, so the only thing
//! looking at a redirect was deno's own net permission -- the deny list
//! [`FetchPolicy::denied_net_ranges`] hands it. A deny list of address
//! descriptors cannot say what this policy says:
//!
//! * **IPv6 ranges.** deno's net descriptor has no spelling for an IPv6
//!   *subnet* -- it parses `fc00::/7` as a host and a port and gives up -- so
//!   the list can carry the IPv4 ranges and the two exact addresses `::1` and
//!   `::`, and not `fc00::/7` or `fe80::/10`. A `Location:` naming an IPv6
//!   unique-local or link-local literal was refused by nothing; deno logged it
//!   as "Granted net access".
//! * **Ports on an address the operator opened.** A deny list has no per-port
//!   exception, so an allowlist entry that is a literal address is punched out
//!   of the range that covers it -- one address wide, every port. Every other
//!   port on that address became a legal redirect hop, and on a node that is
//!   where deis-control, deis-store, deis-queue and deis-telemetry live. The
//!   telemetry metrics port was reachable this way in testing.
//!
//! Both are gone because nothing decides a redirect from an address
//! descriptor any more. The deny list is still installed, and is still worth
//! having: it is now a backstop behind the loop rather than the mechanism, and
//! it is what would still refuse a hop if the wrapper were ever bypassed.
//!
//! ## An HTTP proxy in the node's environment would move the socket
//!
//! `deno_fetch` builds its client with `proxy::from_env()`, unconditionally
//! and with no option to suppress it, so `HTTP_PROXY`, `HTTPS_PROXY` and
//! `ALL_PROXY` in the *node process's* environment would decide where an
//! application's `fetch` actually connects. When a proxy intercepts, the
//! connector dials the proxy and the destination travels as a `CONNECT`
//! target, which means [`PolicyResolver`] resolves the proxy's host and never
//! sees the destination's -- the rebinding guarantee is gone.
//!
//! So those variables are taken out of the process environment before any
//! client is built, and the node's own control-plane clients are handed the
//! values explicitly instead. An operator who genuinely needs applications to
//! egress through a proxy asks for it in the configuration, and is told what
//! it costs. See [`crate::egress_proxy`].
//!
//! # The platform's own services are not merely denied by default
//!
//! An operator may widen this policy for their own internal APIs
//! (`[fetch] allow` in the node's configuration). They must not be able to
//! widen it into deis-store by accident, so the platform's own endpoints are
//! refused *after* the allowlist has been consulted rather than before, and no
//! allowlist entry can reach them. That holds for every hop of a redirect
//! chain as well, because every hop goes through [`FetchPolicy::check`]; the
//! explicit entries [`FetchPolicy::denied_net_ranges`] adds say the same thing
//! again one layer down.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use deno_core::url::{Host, Url};
use hyper_util::client::legacy::connect::dns::Name;

/// One `host` or `host:port` an operator wrote down, either as an allowlist
/// entry or as one of the platform's own service endpoints.
///
/// An entry that names no port matches every port on that host. That is the
/// wider reading on purpose: an operator writing `internal-api.svc` means the
/// service, and a policy that silently only covered port 80 would send them
/// hunting for why their allowlist "did not work".
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostPort {
    /// Lower-cased, without the brackets an IPv6 URL host carries.
    host: String,
    /// The address `host` is, when it is a literal rather than a name. Kept
    /// alongside the string so a resolved address can be compared with a
    /// platform endpoint without resolving anything.
    addr: Option<IpAddr>,
    port: Option<u16>,
}

impl HostPort {
    /// Parse `host`, `host:port`, `[v6]`, `[v6]:port`, or a whole URL
    /// (`http://host:port`), which is the shape the platform's own endpoints
    /// are configured in.
    fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("an empty entry names no destination".to_string());
        }

        // A configured service endpoint is a URL; an allowlist entry is
        // usually bare. Try the URL reading first, and only when it actually
        // carries a scheme, so that `example.com:8080` is not read as the
        // scheme `example.com`.
        if raw.contains("://") {
            let url = Url::parse(raw).map_err(|e| format!("{raw:?} is not a URL: {e}"))?;
            let host = url.host().ok_or_else(|| format!("{raw:?} names no host"))?;
            return Ok(Self::from_host(host, url.port_or_known_default()));
        }

        let (host, port) = split_host_port(raw)?;
        // `url`'s host parser wants an IPv6 address in brackets, and the
        // brackets are exactly what the split above took off -- so an address
        // is recognised here rather than handed back as a malformed name.
        if let Ok(v6) = host.parse::<Ipv6Addr>() {
            return Ok(Self::from_host(Host::<&str>::Ipv6(v6), port));
        }
        let host = Host::parse(&host).map_err(|e| format!("{raw:?} is not a host: {e}"))?;
        Ok(Self::from_host(host, port))
    }

    fn from_host<S: AsRef<str>>(host: Host<S>, port: Option<u16>) -> Self {
        let (host, addr) = match host {
            Host::Domain(name) => (name.as_ref().to_ascii_lowercase(), None),
            Host::Ipv4(v4) => (v4.to_string(), Some(IpAddr::V4(v4))),
            Host::Ipv6(v6) => (v6.to_string(), Some(IpAddr::V6(v6))),
        };
        Self { host, addr, port }
    }

    /// Does this entry cover the destination `(host, port)`?
    ///
    /// `port` is `None` when the caller does not know it -- the DNS layer is
    /// asked for a name with no port attached. An entry that names a port then
    /// still matches, because refusing to match would make the backstop
    /// *narrower* than the gate in front of it, which is the wrong direction
    /// for a backstop.
    fn covers(&self, host: &str, port: Option<u16>) -> bool {
        if !self.host.eq_ignore_ascii_case(host) {
            return false;
        }
        match (self.port, port) {
            (Some(mine), Some(theirs)) => mine == theirs,
            _ => true,
        }
    }

    /// Does this entry name exactly this address, on this port?
    ///
    /// Used for the platform's endpoints: an allowlist that opened up a whole
    /// internal subnet must still not reach deis-store, and the application
    /// may well have spelled it under a different name than the one the
    /// deployment configured.
    fn covers_addr(&self, addr: IpAddr, port: Option<u16>) -> bool {
        let Some(mine) = self.addr else {
            return false;
        };
        if !reaches_the_same_socket(mine, addr) {
            return false;
        }
        match (self.port, port) {
            (Some(mine), Some(theirs)) => mine == theirs,
            _ => true,
        }
    }

    /// How deno's net descriptor spells this entry, for the deny list.
    ///
    /// An IPv6 address has to be bracketed, and an entry that names no port
    /// is written bare, which deno reads as every port on that host.
    ///
    /// An IPv4 address gets a second descriptor, its IPv4-mapped IPv6 form.
    /// deno matches a deny entry against the host *as the URL spells it*, and
    /// `http://[::ffff:127.0.0.1]:8085/` reaches the same socket as
    /// `http://127.0.0.1:8085/` while matching neither the address nor any
    /// IPv4 range. Measured, not assumed: without the second spelling deno
    /// logs "Granted net access to `[::ffff:127.0.0.1]:8085`" for a redirect
    /// to the control plane.
    fn to_deno_descriptors(&self) -> Vec<String> {
        let with_port = |host: String| match self.port {
            Some(port) => format!("{host}:{port}"),
            None => host,
        };
        match self.addr {
            Some(IpAddr::V6(v6)) => vec![with_port(format!("[{v6}]"))],
            Some(IpAddr::V4(v4)) => vec![
                with_port(v4.to_string()),
                with_port(format!("[{}]", v4.to_ipv6_mapped())),
            ],
            None => vec![with_port(self.host.clone())],
        }
    }
}

fn split_host_port(raw: &str) -> Result<(String, Option<u16>), String> {
    if let Some(rest) = raw.strip_prefix('[') {
        // `[::1]` or `[::1]:8090`
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| format!("{raw:?} opens a bracket it never closes"))?;
        let port = match tail {
            "" => None,
            _ => Some(parse_port(tail.trim_start_matches(':'), raw)?),
        };
        return Ok((host.to_string(), port));
    }

    match raw.rsplit_once(':') {
        // More than one colon and no brackets: a bare IPv6 address.
        Some(_) if raw.matches(':').count() > 1 => Ok((raw.to_string(), None)),
        Some((host, port)) => Ok((host.to_string(), Some(parse_port(port, raw)?))),
        None => Ok((raw.to_string(), None)),
    }
}

fn parse_port(raw: &str, whole: &str) -> Result<u16, String> {
    raw.parse::<u16>()
        .map_err(|_| format!("{whole:?} does not end in a port number"))
}

/// Why one outbound call was not made.
///
/// Each variant carries what the application needs in order to fix its own
/// code, and nothing about the node it happens to be running on: an
/// application learns that a destination is out of bounds, not which internal
/// addresses exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Not a URL, or a URL with no host to judge.
    Unaddressable(String),
    /// A scheme this platform does not let an application open. `file:` is the
    /// one that matters: it is the node's own disk.
    Scheme(String),
    /// A platform service endpoint. Refused whatever the allowlist says.
    PlatformService,
    /// The operator named one or more allowed ports for this hostname, and
    /// the request chose another. Restricting the host as well as opening the
    /// named port is what lets the portless connector resolver remain safe.
    PortNotAllowed { host: String, port: u16 },
    /// The destination resolves to an address applications may not reach.
    BlockedAddress { addr: IpAddr, kind: &'static str },
    /// The destination is an IPv6 address that carries an IPv4 address inside
    /// it -- IPv4-mapped, NAT64, 6to4 -- and the inner address is what the
    /// policy judged. The refusal names both spellings, because a message
    /// that only said "64:ff9b::7f00:1 is a loopback address" would read as
    /// a mistake rather than as the sentence it is.
    BlockedTransitionAddress {
        addr: IpAddr,
        embedded: Ipv4Addr,
        kind: &'static str,
    },
    /// The name does not resolve, so there is nothing to judge and nothing to
    /// connect to either.
    Unresolvable { host: String, error: String },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unaddressable(what) => {
                write!(f, "{what} is not an absolute http(s) URL")
            }
            Self::Scheme(scheme) => write!(
                f,
                "the {scheme}: scheme is not available to applications; fetch() may open http \
                 and https destinations only"
            ),
            Self::PlatformService => write!(
                f,
                "that address is one of the platform's own service endpoints, which no \
                 application may call directly; use ctx.services instead"
            ),
            Self::PortNotAllowed { host, port } => write!(
                f,
                "the outbound allowlist restricts {host} to explicitly named ports, and port \
                 {port} is not one of them"
            ),
            Self::BlockedAddress { addr, kind } => write!(
                f,
                "it resolves to {addr}, which is {kind}. Applications may only reach public \
                 internet destinations; ask the operator to add the host to [fetch] allow if \
                 this is an internal service the application is meant to reach"
            ),
            Self::BlockedTransitionAddress {
                addr,
                embedded,
                kind,
            } => write!(
                f,
                "it resolves to {addr}, which carries the IPv4 address {embedded}, and that \
                 is {kind}. Applications may only reach public internet destinations; ask the \
                 operator to add the host to [fetch] allow if this is an internal service the \
                 application is meant to reach"
            ),
            Self::Unresolvable { host, error } => {
                write!(f, "the host {host} could not be resolved: {error}")
            }
        }
    }
}

/// The rules, shared by every isolate that was built with the same node
/// configuration. Behind an `Arc` because a copy travels into every
/// `IsolateSpec` and into the DNS resolver of every isolate's HTTP client.
#[derive(Debug, Default)]
struct Rules {
    /// Destinations an operator has opened up even though they are not public.
    allow: Vec<HostPort>,
    /// The platform's own services. Refused regardless of `allow`.
    platform: Vec<HostPort>,
}

/// The outbound-fetch policy of one isolate.
#[derive(Debug, Clone, Default)]
pub struct FetchPolicy {
    rules: Arc<Rules>,
}

impl FetchPolicy {
    /// Build the node's policy.
    ///
    /// `allow` is what the operator wrote under `[fetch] allow`; `platform` is
    /// every address of this platform's own services that is known at this
    /// point. Both are `host`, `host:port` or a whole URL. A malformed entry
    /// is an error rather than a skipped line: an allowlist entry that is
    /// silently ignored looks exactly like one that worked, and a platform
    /// entry that is silently ignored is a hole.
    pub fn new<A, P>(allow: A, platform: P) -> Result<Self, String>
    where
        A: IntoIterator,
        A::Item: AsRef<str>,
        P: IntoIterator,
        P::Item: AsRef<str>,
    {
        let allow = allow
            .into_iter()
            .map(|e| HostPort::parse(e.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        let platform = platform
            .into_iter()
            .map(|e| HostPort::parse(e.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            rules: Arc::new(Rules { allow, platform }),
        })
    }

    /// The same policy, plus service endpoints this application in particular
    /// was wired to.
    ///
    /// The node's own configuration knows where deis-control and
    /// deis-telemetry are; where deis-store and deis-queue are arrives per
    /// deployment, in the application's wiring, so it can only be added when
    /// the isolate is built. A malformed address here is reported and dropped
    /// rather than failing the load: the same string is validated where the
    /// clients are built, and refusing to start an application because a
    /// *deny* rule could not be parsed would take the application down over a
    /// value that was never going to be reachable anyway.
    pub fn with_platform_endpoints<I>(&self, endpoints: I, app_id: &str) -> Self
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let mut platform = self.rules.platform.clone();
        for entry in endpoints {
            match HostPort::parse(entry.as_ref()) {
                Ok(parsed) => {
                    if !platform.contains(&parsed) {
                        platform.push(parsed);
                    }
                }
                Err(e) => tracing::warn!(
                    target: "app",
                    app_id = %app_id,
                    "Could not read a platform service address into the outbound fetch policy \
                     of {}: {}. That address is still refused by the deny-by-default rule \
                     unless an allowlist entry covers it",
                    app_id,
                    e
                ),
            }
        }
        Self {
            rules: Arc::new(Rules {
                allow: self.rules.allow.clone(),
                platform,
            }),
        }
    }

    /// Judge one destination, resolving its host if it is a name.
    ///
    /// The order is deliberate: scheme, then the platform's own services, then
    /// the allowlist, then the address ranges. The platform check comes before
    /// the allowlist so that no allowlist entry can reach deis-store, and the
    /// allowlist comes before the ranges so that an operator's internal
    /// service is reachable.
    pub async fn check(&self, url: &str) -> Result<(), Refusal> {
        let parsed = Url::parse(url).map_err(|_| Refusal::Unaddressable(format!("{url:?}")))?;

        match parsed.scheme() {
            "http" | "https" => {}
            // `data:` and `blob:` never leave the isolate, so they are not the
            // business of a network policy.
            "data" | "blob" => return Ok(()),
            other => return Err(Refusal::Scheme(other.to_string())),
        }

        let port = parsed.port_or_known_default();
        let host = parsed
            .host()
            .ok_or_else(|| Refusal::Unaddressable(format!("{url:?}")))?;
        let host_text = match host {
            Host::Domain(name) => name.to_ascii_lowercase(),
            Host::Ipv4(v4) => v4.to_string(),
            Host::Ipv6(v6) => v6.to_string(),
        };

        if self.names_a_platform_service(&host_text, port) {
            return Err(Refusal::PlatformService);
        }

        // The connector's DNS resolver receives only the hostname; hyper
        // attaches the request port afterwards. It must therefore treat an
        // allowlisted name as opened at connect time without being able to
        // repeat this port comparison. Refuse every other port here so a
        // `name:8443` exception cannot be inherited by a request to
        // `name:9000` after a public-to-private DNS rebind.
        if let Some(port) = port {
            if self.is_restricted_to_named_ports(&host_text)
                && !self.is_allowlisted(&host_text, Some(port))
            {
                return Err(Refusal::PortNotAllowed {
                    host: host_text,
                    port,
                });
            }
        }

        // A literal address needs no lookup, and the connector will not do one
        // either -- see the module comment on the resolve/connect gap.
        let addresses: Vec<IpAddr> = match host {
            Host::Ipv4(v4) => vec![IpAddr::V4(v4)],
            Host::Ipv6(v6) => vec![IpAddr::V6(v6)],
            Host::Domain(name) => {
                resolve(name, port.unwrap_or(0))
                    .await
                    .map_err(|e| Refusal::Unresolvable {
                        host: name.to_string(),
                        error: e.to_string(),
                    })?
            }
        };

        // Checked on the resolved addresses too: an allowlist that opened a
        // whole internal subnet by name must still not reach deis-store, and
        // an application can spell the same machine under a name of its own.
        for addr in &addresses {
            if self.is_a_platform_service_address(*addr, port) {
                return Err(Refusal::PlatformService);
            }
        }

        if self.is_allowlisted(&host_text, port) {
            return Ok(());
        }

        // Every address, not just the first: a name that resolves to one
        // public address and one loopback address is exactly the shape an
        // attack takes, and the client is free to try any of them.
        for addr in addresses {
            if let Some(refusal) = blocked_reason(addr) {
                return Err(refusal);
            }
        }

        Ok(())
    }

    fn names_a_platform_service(&self, host: &str, port: Option<u16>) -> bool {
        self.rules.platform.iter().any(|e| e.covers(host, port))
    }

    fn is_a_platform_service_address(&self, addr: IpAddr, port: Option<u16>) -> bool {
        self.rules
            .platform
            .iter()
            .any(|e| e.covers_addr(addr, port))
    }

    fn is_allowlisted(&self, host: &str, port: Option<u16>) -> bool {
        self.rules.allow.iter().any(|e| e.covers(host, port))
    }

    /// Whether hostname entries restrict this name to explicitly listed
    /// ports. A second unqualified entry deliberately opens every port and
    /// removes the restriction.
    fn is_restricted_to_named_ports(&self, host: &str) -> bool {
        let matching_names = self
            .rules
            .allow
            .iter()
            .filter(|entry| entry.addr.is_none() && entry.host.eq_ignore_ascii_case(host));
        let mut saw_port = false;
        for entry in matching_names {
            if entry.port.is_none() {
                return false;
            }
            saw_port = true;
        }
        saw_port
    }

    /// Judge an address obtained immediately before opening a socket while
    /// retaining the original hostname and port.
    ///
    /// This is used by the WebSocket connector, whose DNS hook does know the
    /// destination port. It closes the resolve/connect gap without turning a
    /// hostname allowlist entry into an unrelated IP-literal policy.
    pub(crate) fn check_resolved_address(
        &self,
        host: &str,
        port: u16,
        addr: IpAddr,
    ) -> Result<(), Refusal> {
        if self.names_a_platform_service(host, Some(port))
            || self.is_a_platform_service_address(addr, Some(port))
        {
            return Err(Refusal::PlatformService);
        }
        if self.is_allowlisted(host, Some(port)) {
            return Ok(());
        }
        if let Some(refusal) = blocked_reason(addr) {
            return Err(refusal);
        }
        Ok(())
    }

    /// The blocked ranges as deno's net permission spells them, for
    /// `PermissionsOptions::deny_net`.
    ///
    /// **A backstop, not the mechanism.** Redirects are followed in
    /// `js/bootstrap.js` now, and every hop passes [`Self::check`] with the
    /// URL the connector will be given, so this list no longer decides
    /// anything a redirect does. It is kept because deno applies it inside
    /// `op_fetch`, underneath everything above: if the wrapper were ever
    /// bypassed -- a path that reached `op_fetch` without going through
    /// `globalThis.fetch`, a future change that forgot the guard -- a call
    /// into a private or loopback address would still be refused here. What
    /// this list cannot express is exactly why it stopped being the mechanism:
    /// it has no spelling for an IPv6 prefix and no per-port exception. See
    /// the redirect section of the module comment.
    ///
    /// An address the operator's allowlist opens is *punched out* of the range
    /// that covers it, rather than the range being dropped. deno's deny list
    /// wins over its allow list with no way to spell an exception, so keeping
    /// the range whole would refuse a destination the operator deliberately
    /// opened; dropping the range would leave the rest of it -- the whole of
    /// `127.0.0.0/8` because one node allows `127.0.0.1` -- unchecked at
    /// redirect time. Splitting it keeps both promises, at the cost of a
    /// handful more entries in a list deno holds in a set.
    ///
    /// The hole is one address wide and covers every port on it, because a
    /// deny list cannot carry a per-port exception. That no longer opens a
    /// redirect hop, because [`Self::check`] runs on every hop and is
    /// port-exact; it only means this backstop is coarser than the policy in
    /// front of it, which is what a backstop is allowed to be.
    ///
    /// An allowlist entry that is a *name* cannot be punched out, and the
    /// ranges are dropped whole when one is present.
    ///
    /// deno used to compare only the URL's host against this list, and a
    /// hostname never matches an address range, so a named entry needed no
    /// hole. It now re-checks the address the name *resolved to* as well
    /// (`check_net_resolved`, the connector's own rebinding guard). That check
    /// knows nothing about the allowlist, so on a node whose operator opened
    /// `internal-api.svc` -- which is on a private range, that being the point
    /// of opening it -- the backstop would veto the destination the operator
    /// deliberately allowed, and the application would see a network error
    /// naming a `--allow-net` flag this platform does not have.
    ///
    /// A named entry can resolve to any address, so there is no subset of the
    /// ranges that both keeps the backstop and honours the allowlist. The
    /// ranges therefore go, and what is left is the platform's own endpoints,
    /// which no allowlist may reach at all. Nothing is lost that is not
    /// checked elsewhere: [`PolicyResolver`] is inside the HTTP client and
    /// applies this policy -- allowlist included -- to every address a name
    /// resolves to, and `op_deis_fetch_guard` applies it to every URL and
    /// every redirect hop before that. A node with no allowlist, which is the
    /// default, keeps the ranges in full.
    pub fn denied_net_ranges(&self, app_id: &str) -> Vec<String> {
        let opened: Vec<IpAddr> = self
            .rules
            .allow
            .iter()
            .filter_map(|entry| entry.addr)
            .collect();
        let named = self.rules.allow.iter().any(|entry| entry.addr.is_none());

        let mut denied = Vec::new();
        if named {
            tracing::info!(
                target: "app",
                app_id = %app_id,
                "The outbound allowlist names a host rather than an address, so the address-range \
                 backstop is not installed for {}: deno re-checks the address a name resolved to \
                 against this list and would refuse the destination the operator opened. The \
                 policy itself still decides every call and every redirect hop, and the \
                 platform's own endpoints stay denied here",
                app_id
            );
        } else {
            for range in ranges_deno_can_deny() {
                for piece in range.without(&opened) {
                    denied.push(piece.to_deno_descriptor());
                }
            }
        }

        // The platform's own endpoints, named exactly, on top of the ranges.
        //
        // Without these the hole punched just above would swallow them. An
        // operator who opens one address on the node -- `127.0.0.1:8095`, the
        // shape the demo ships -- takes the whole of that address out of the
        // deny list, because deno's deny list has no per-port exception. Every
        // other port on it would then be unremarked here, and on a node that
        // is where deis-control, deis-store, deis-queue and deis-telemetry all
        // live.
        //
        // `check` refuses them on every hop regardless, so this is no longer
        // the only thing standing in the way; naming them keeps the backstop
        // from being *silent* about the one class of destination that must
        // never be reachable. Entries with no port cover every port on that
        // host, which is the same reading `covers` takes.
        let platform: Vec<String> = self
            .rules
            .platform
            .iter()
            .flat_map(|entry| entry.to_deno_descriptors())
            .collect();
        let platform_entries = platform.len();
        denied.extend(platform);

        tracing::debug!(
            target: "app",
            app_id = %app_id,
            entries = denied.len(),
            opened = opened.len(),
            platform_entries,
            "Outbound fetch redirect backstop for {}: {} denied entries, of which {} name the \
             platform's own services, with {} address(es) punched out for the operator's \
             allowlist",
            app_id,
            denied.len(),
            platform_entries,
            opened.len()
        );
        denied
    }

    /// How many destinations an operator has opened up. For the startup line;
    /// the entries themselves are not logged, because an allowlist is a map of
    /// the internal network.
    pub fn allowlist_len(&self) -> usize {
        self.rules.allow.len()
    }
}

/// One range of addresses applications may not reach, in the form deno's net
/// permission accepts.
///
/// This table and [`blocked_reason`] are two spellings of one decision, and
/// `the_two_spellings_of_the_blocked_set_agree` holds them together: the
/// classifier is what an application is told and why, this is what deno
/// enforces on a redirect hop nothing else sees.
#[derive(Debug, Clone, Copy)]
struct BlockedRange {
    net: IpAddr,
    prefix: u8,
}

impl BlockedRange {
    fn contains(&self, addr: IpAddr) -> bool {
        match (self.net, addr) {
            (IpAddr::V4(net), IpAddr::V4(addr)) => {
                let shift = 32 - u32::from(self.prefix);
                let mask = if shift == 32 { 0 } else { u32::MAX << shift };
                u32::from(net) & mask == u32::from(addr) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(addr)) => {
                let shift = 128 - u32::from(self.prefix);
                let mask = if shift == 128 { 0 } else { u128::MAX << shift };
                u128::from(net) & mask == u128::from(addr) & mask
            }
            // An IPv4 range never covers an IPv6 address and the other way
            // round; the IPv4-in-IPv6 spellings are separate entries below.
            _ => false,
        }
    }

    /// This range with `excluded` taken out of it, as the smallest set of
    /// ranges that covers the rest.
    ///
    /// Splitting in halves and recursing is the standard way to subtract an
    /// address from a network: a half that contains none of the excluded
    /// addresses is kept whole, and only the halves that do are split further.
    /// One address out of a `/8` becomes 24 ranges, which is why this is not
    /// done for the ranges nobody opened.
    fn without(self, excluded: &[IpAddr]) -> Vec<BlockedRange> {
        if !excluded.iter().any(|addr| self.contains(*addr)) {
            return vec![self];
        }

        let full_width = match self.net {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        if self.prefix >= full_width {
            // The excluded address *is* this range.
            return Vec::new();
        }

        let Some((lower, upper)) = self.halves() else {
            return vec![self];
        };
        let mut out = lower.without(excluded);
        out.extend(upper.without(excluded));
        out
    }

    /// The two halves of this range, or `None` for a single address.
    fn halves(self) -> Option<(BlockedRange, BlockedRange)> {
        let prefix = self.prefix + 1;
        match self.net {
            IpAddr::V4(net) => {
                if prefix > 32 {
                    return None;
                }
                let bit = 1u32 << (32 - u32::from(prefix));
                Some((
                    BlockedRange {
                        net: IpAddr::V4(net),
                        prefix,
                    },
                    BlockedRange {
                        net: IpAddr::V4(Ipv4Addr::from(u32::from(net) | bit)),
                        prefix,
                    },
                ))
            }
            IpAddr::V6(net) => {
                if prefix > 128 {
                    return None;
                }
                let bit = 1u128 << (128 - u32::from(prefix));
                Some((
                    BlockedRange {
                        net: IpAddr::V6(net),
                        prefix,
                    },
                    BlockedRange {
                        net: IpAddr::V6(Ipv6Addr::from(u128::from(net) | bit)),
                        prefix,
                    },
                ))
            }
        }
    }

    /// How deno's net descriptor spells this range. An IPv6 address has to be
    /// bracketed, and a single address is written without a prefix at all --
    /// `[::1]/128` is not something that parser accepts.
    fn to_deno_descriptor(self) -> String {
        match self.net {
            IpAddr::V4(v4) if self.prefix == 32 => v4.to_string(),
            IpAddr::V4(v4) => format!("{v4}/{}", self.prefix),
            IpAddr::V6(v6) => format!("[{v6}]"),
        }
    }
}

/// The IPv4 ranges, as (network, prefix length). Exactly the set
/// [`blocked_v4`] refuses.
const BLOCKED_V4: &[([u8; 4], u8)] = &[
    ([0, 0, 0, 0], 8),       // "this network"
    ([10, 0, 0, 0], 8),      // private
    ([100, 64, 0, 0], 10),   // carrier-grade NAT
    ([127, 0, 0, 0], 8),     // loopback
    ([169, 254, 0, 0], 16),  // link-local, incl. the cloud metadata service
    ([172, 16, 0, 0], 12),   // private
    ([192, 0, 0, 0], 24),    // IETF protocol assignments
    ([192, 0, 2, 0], 24),    // documentation
    ([192, 168, 0, 0], 16),  // private
    ([198, 18, 0, 0], 15),   // benchmarking
    ([198, 51, 100, 0], 24), // documentation
    ([203, 0, 113, 0], 24),  // documentation
    ([224, 0, 0, 0], 4),     // multicast
    ([240, 0, 0, 0], 4),     // reserved, incl. 255.255.255.255
];

/// The IPv6 ranges, as (network, prefix length). Together with
/// [`embedded_ipv4`], exactly the set [`blocked_reason`] refuses. The NAT64
/// and 6to4 networks are listed whole, but their addresses are judged by the
/// IPv4 address each carries, so one carrying a public address is allowed;
/// the classifier is finer-grained than the table there, never coarser.
const BLOCKED_V6: &[(u128, u8)] = &[
    (0, 128),                     // ::
    (1, 128),                     // ::1
    (0x0064_ff9b << 96, 96),      // NAT64, well-known prefix
    (0x0064_ff9b_0001 << 80, 48), // NAT64, local-use (RFC 8215)
    (0x2002 << 112, 16),          // 6to4
    (0x2001 << 112, 32),          // Teredo, refused outright
    (0x0100 << 112, 64),          // discard-only
    (0xfc00 << 112, 7),           // unique local
    (0xfe80 << 112, 10),          // link-local
    (0xff00 << 112, 8),           // multicast
    (0x2001_0db8 << 96, 32),      // documentation
];

/// Every blocked range, as this module classifies addresses.
fn blocked_ranges() -> Vec<BlockedRange> {
    let mut ranges = Vec::with_capacity(BLOCKED_V4.len() + BLOCKED_V6.len());
    for (octets, prefix) in BLOCKED_V4 {
        ranges.push(BlockedRange {
            net: IpAddr::V4(Ipv4Addr::from(*octets)),
            prefix: *prefix,
        });
    }
    for (net, prefix) in BLOCKED_V6 {
        ranges.push(BlockedRange {
            net: IpAddr::V6(Ipv6Addr::from(*net)),
            prefix: *prefix,
        });
    }
    ranges
}

/// The subset of [`blocked_ranges`] that deno's net descriptor can express.
///
/// It reads `10.0.0.0/8` as a subnet, and a bracketed `[::1]` as one address,
/// but it has no spelling for an IPv6 subnet at all -- `fc00::/7` is read as a
/// host and a port and refused. So the IPv6 ranges survive only where the
/// range happens to be a single address. See the module comment on redirects
/// for what that leaves uncovered.
fn ranges_deno_can_deny() -> Vec<BlockedRange> {
    blocked_ranges()
        .into_iter()
        .filter(|range| match range.net {
            IpAddr::V4(_) => true,
            IpAddr::V6(_) => range.prefix == 128,
        })
        .collect()
}

/// The destination as it is safe to write into a log line.
///
/// A URL may carry credentials -- `https://user:token@api.example.com/` is a
/// shape applications do use -- and a refusal is logged on the node and
/// shipped to deis-telemetry. Logging the URL as given would put that password
/// somewhere the application never agreed to put it. The userinfo is the only
/// part removed: the host and path are what make the line useful to whoever is
/// working out why a call was refused.
pub fn loggable(url: &str) -> String {
    let Ok(mut parsed) = Url::parse(url) else {
        return url.to_string();
    };
    if parsed.username().is_empty() && parsed.password().is_none() {
        return url.to_string();
    }
    // Both setters fail only on a URL that cannot have a host, which this one
    // has already been shown to have.
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.to_string()
}

async fn resolve(host: &str, port: u16) -> std::io::Result<Vec<IpAddr>> {
    let addresses: Vec<IpAddr> = tokio::net::lookup_host((host, port))
        .await?
        .map(|addr: SocketAddr| addr.ip())
        .collect();
    if addresses.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "the name resolved to no addresses",
        ));
    }
    Ok(addresses)
}

/// `deno_fetch`'s DNS resolver for one isolate: the same policy, applied where
/// the socket is actually opened.
///
/// This is the half of the policy an application cannot get in front of. It
/// sees only the name -- hyper asks for the name and attaches the port itself
/// -- so it enforces the part of the policy that does not depend on a port:
/// the address ranges, and the platform's own service hosts. The port-aware
/// part (an allowlist entry that names a port, a platform endpoint on one
/// port of a shared host) is enforced by [`FetchPolicy::check`] before the
/// request is ever handed to the client.
#[derive(Debug)]
pub struct PolicyResolver {
    policy: FetchPolicy,
    app_id: String,
}

impl PolicyResolver {
    pub fn new(policy: FetchPolicy, app_id: impl Into<String>) -> Self {
        Self {
            policy,
            app_id: app_id.into(),
        }
    }

    /// Wrap this in the shape `deno_fetch::Options` wants.
    pub fn into_resolver(self) -> deno_runtime::deno_fetch::dns::Resolver {
        deno_runtime::deno_fetch::dns::Resolver::custom(Arc::new(self))
    }
}

impl deno_runtime::deno_fetch::dns::Resolve for PolicyResolver {
    fn resolve(&self, name: Name) -> deno_runtime::deno_fetch::dns::Resolving {
        let policy = self.policy.clone();
        let app_id = self.app_id.clone();
        let host = name.as_str().to_ascii_lowercase();

        Box::pin(async move {
            if policy.names_a_platform_service(&host, None) {
                tracing::warn!(
                    target: "app",
                    app_id = %app_id,
                    host = %host,
                    "Refused to resolve {} for {}: it names one of the platform's own service \
                     endpoints. The outbound call fails; no connection was opened",
                    host,
                    app_id
                );
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("{host} is a platform service endpoint"),
                ));
            }

            let addresses = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .collect::<Vec<SocketAddr>>();

            if policy.is_allowlisted(&host, None) {
                // The allowlist opens destinations that are not public -- an
                // operator's internal service on a private range is the whole
                // point -- so the blocked-range filter below is deliberately
                // not applied here. The platform's own endpoints are the one
                // exception the allowlist may never cover, and that exception
                // has to hold at connect time as well: an allowlisted *name*
                // that resolved to a public address when `check` ran and to a
                // platform endpoint's address now is a rebind, and letting it
                // through here would reach deis-store past every gate. So the
                // platform's addresses are dropped even for an allowlisted
                // name; everything else the operator opened is kept.
                let mut kept = Vec::with_capacity(addresses.len());
                for addr in addresses {
                    if policy.is_a_platform_service_address(addr.ip(), None) {
                        tracing::warn!(
                            target: "app",
                            app_id = %app_id,
                            host = %host,
                            address = %addr.ip(),
                            "Dropped {} from the addresses of {} for {}: it is one of the \
                             platform's own service endpoints, which no allowlist entry may \
                             reach. The allowlisted name resolved to it at connect time, which \
                             is a rebind; the connection is not opened",
                            addr.ip(),
                            host,
                            app_id
                        );
                    } else {
                        kept.push(addr);
                    }
                }
                if kept.is_empty() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!(
                            "{host} is allowlisted but resolves only to the platform's own \
                             service endpoints"
                        ),
                    ));
                }
                return Ok(kept.into_iter());
            }

            let mut kept = Vec::with_capacity(addresses.len());
            for addr in addresses {
                match blocked_reason(addr.ip()) {
                    None => kept.push(addr),
                    Some(refusal) => {
                        // Warn rather than debug: reaching this line means the
                        // name resolved differently than it did a moment ago
                        // in `FetchPolicy::check`, which is what a rebinding
                        // attack looks like from here.
                        tracing::warn!(
                            target: "app",
                            app_id = %app_id,
                            host = %host,
                            address = %addr.ip(),
                            "Dropped {} from the addresses of {} for {}: {}. The name passed \
                             the outbound fetch policy and then resolved to a blocked address, \
                             so the connection is not opened",
                            addr.ip(),
                            host,
                            app_id,
                            refusal
                        );
                    }
                }
            }

            if kept.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("{host} resolves only to addresses this platform refuses"),
                ));
            }
            Ok(kept.into_iter())
        })
    }
}

/// Would a connection to `theirs` land on a socket bound at `mine`?
///
/// The platform's endpoints are configured by one spelling of an address,
/// and a machine has several spellings for itself. A service bound at
/// `127.0.0.1` is reached by `::ffff:127.0.0.1`; one bound at the
/// unspecified address is reached by every address the machine has, loopback
/// included; and a name that resolves to both loopback addresses - which is
/// what `localhost` does on any host with IPv6 - offers `::1` beside
/// `127.0.0.1`. Comparing addresses byte for byte let the second spelling
/// through: the first run of the CI workflow, on a dual-stack runner, saw
/// the resolver drop `127.0.0.1` from an allowlisted name and hand back
/// `[::1]`, the same machine by another name. So loopback is judged as a
/// class, the unspecified address as every address, and the IPv4-in-IPv6
/// forms as the IPv4 address they carry.
fn reaches_the_same_socket(mine: IpAddr, theirs: IpAddr) -> bool {
    let mine = as_ipv4_when_it_is_one(mine);
    let theirs = as_ipv4_when_it_is_one(theirs);
    if mine == theirs {
        return true;
    }
    if mine.is_unspecified() {
        // Bound on every interface: any address this machine answers at
        // reaches it. Public addresses of the machine itself are not known
        // here, so this is the loopback half of that promise; the blocked
        // ranges below refuse the private half for names the operator did
        // not open, and an operator who allowlists the node's own public
        // address has said what they meant.
        return theirs.is_loopback();
    }
    mine.is_loopback() && theirs.is_loopback()
}

/// An IPv6 address that carries an IPv4 address inside it as that IPv4
/// address, so a comparison between spellings compares machines rather than
/// spellings: `::ffff:a.b.c.d`, the deprecated `::a.b.c.d`, the NAT64 forms
/// `64:ff9b::a.b.c.d` and `64:ff9b:1::a.b.c.d`, and 6to4 `2002:a.b.c.d::` all
/// read as `a.b.c.d`. Teredo carries no address that can be read out of it
/// and is refused outright instead. `::1` and `::` stay themselves, because
/// reading an inner address out of them would produce `0.0.0.1` and
/// `0.0.0.0`.
fn as_ipv4_when_it_is_one(addr: IpAddr) -> IpAddr {
    match addr {
        IpAddr::V6(v6) if !v6.is_loopback() && !v6.is_unspecified() => {
            embedded_ipv4(v6).map(IpAddr::V4).unwrap_or(addr)
        }
        _ => addr,
    }
}

/// The IPv4 address an IPv6 transition form carries, if it carries one.
///
/// Recognised: IPv4-mapped `::ffff:a.b.c.d`, the deprecated `::a.b.c.d`,
/// NAT64 in both the well-known (`64:ff9b::/96`) and the local-use
/// (`64:ff9b:1::/48`, RFC 8215) spelling, and 6to4 (`2002::/16`, inner
/// address in bits 16-48). Every one of them puts the packet on an IPv4
/// network, so the inner address is the one that has to be judged: without
/// this, every private-range rule is one URL rewrite away from being
/// bypassed through a NAT64 or 6to4 gateway.
///
/// Teredo (`2001::/32`) is deliberately absent. Its client and port halves
/// are XOR-obfuscated, so no reliable inner address can be read out of it,
/// and legitimate outbound traffic does not originate Teredo; the range is
/// refused outright in [`blocked_v6`] instead.
fn embedded_ipv4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = v6.segments();
    let inner =
        |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    // IPv4-mapped, and the deprecated plain spelling beside it.
    if let Some(v4) = v6.to_ipv4_mapped() {
        return Some(v4);
    }
    // NAT64. The two prefixes differ in how much zero padding sits above the
    // inner address; in both it is the last 32 bits. Not refused outright:
    // DNS64/NAT64 is how an IPv6-only node reaches the public IPv4 internet,
    // so the inner address is judged like any other address.
    if segments[0] == 0x0064
        && segments[1] == 0xff9b
        && (segments[2] == 0 || segments[2] == 1)
        && segments[3..6].iter().all(|&s| s == 0)
    {
        return Some(inner(segments[6], segments[7]));
    }
    // 6to4.
    if segments[0] == 0x2002 {
        return Some(inner(segments[1], segments[2]));
    }
    v6.to_ipv4()
}

/// The refusal for one address, or `None` when an application may reach it.
///
/// The wording is what the application is told, so each kind reads as the
/// tail of "it resolves to 10.0.0.1, which is ...". An IPv6 address carrying
/// an IPv4 address inside it ([`embedded_ipv4`]) is refused as the inner
/// address, and its refusal names both spellings.
fn blocked_reason(addr: IpAddr) -> Option<Refusal> {
    match addr {
        IpAddr::V4(v4) => blocked_v4(v4).map(|kind| Refusal::BlockedAddress { addr, kind }),
        IpAddr::V6(v6) => {
            // `::1` and `::` are IPv6 addresses in their own right, and
            // reading an inner IPv4 out of them would produce `0.0.0.1` and
            // `0.0.0.0`: still refused, but for a reason that would puzzle
            // whoever read the message. Judged as themselves first.
            if v6.is_loopback() || v6.is_unspecified() {
                return blocked_v6(v6).map(|kind| Refusal::BlockedAddress { addr, kind });
            }
            // An address that carries an IPv4 address inside it reaches the
            // machine that inner address names, so it is judged as that
            // address -- whatever the outer spelling suggests.
            if let Some(v4) = embedded_ipv4(v6) {
                return blocked_v4(v4).map(|kind| Refusal::BlockedTransitionAddress {
                    addr,
                    embedded: v4,
                    kind,
                });
            }
            blocked_v6(v6).map(|kind| Refusal::BlockedAddress { addr, kind })
        }
    }
}

fn blocked_v4(addr: Ipv4Addr) -> Option<&'static str> {
    let octets = addr.octets();
    if addr.is_unspecified() {
        Some("the unspecified address, which means \"this host\"")
    } else if addr.is_loopback() {
        Some("a loopback address, which is this node itself")
    } else if addr.is_private() {
        Some("in a private range, which is this node's own network")
    } else if addr.is_link_local() {
        // 169.254.169.254 lives here on every major cloud and answers with the
        // instance's credentials to anyone who asks.
        Some("link-local, which is where a cloud instance's metadata service lives")
    } else if addr.is_broadcast() {
        Some("the broadcast address")
    } else if addr.is_multicast() {
        Some("a multicast address")
    } else if addr.is_documentation() {
        Some("in a range reserved for documentation")
    } else if octets[0] == 0 {
        Some("in 0.0.0.0/8, which is not a destination")
    } else if octets[0] == 100 && (octets[1] & 0b1100_0000) == 64 {
        Some("in the carrier-grade NAT range")
    } else if octets[0] == 192 && octets[1] == 0 && octets[2] == 0 {
        Some("in a range reserved for IETF protocol assignments")
    } else if octets[0] == 198 && (octets[1] & 0xfe) == 18 {
        Some("in a range reserved for benchmarking")
    } else if octets[0] >= 240 {
        Some("in a range reserved for future use")
    } else {
        None
    }
}

fn blocked_v6(addr: Ipv6Addr) -> Option<&'static str> {
    let segments = addr.segments();
    if addr.is_unspecified() {
        Some("the unspecified address, which means \"this host\"")
    } else if addr.is_loopback() {
        Some("a loopback address, which is this node itself")
    } else if addr.is_multicast() {
        Some("a multicast address")
    } else if (segments[0] & 0xfe00) == 0xfc00 {
        Some("a unique-local address, which is this node's own network")
    } else if (segments[0] & 0xffc0) == 0xfe80 {
        Some("link-local")
    } else if segments[0] == 0x2001 && segments[1] == 0 {
        // Teredo. The client and port halves are XOR-obfuscated, so no inner
        // address can be judged; legitimate outbound traffic does not
        // originate Teredo, and the range is refused whole.
        Some(
            "in the Teredo range, which is refused outright: the address hides an IPv4 \
             address and a port behind an obfuscation, and no inner address can be judged",
        )
    } else if segments[0] == 0x0100 && segments[1..4].iter().all(|&s| s == 0) {
        // The discard-only range. Packets to it are dropped by definition,
        // which makes it a destination for nothing.
        Some("in the discard-only range, which is not a destination")
    } else if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        Some("in a range reserved for documentation")
    } else {
        None
    }
}

/// The network address of every blocked range, for the test that holds this
/// table and [`blocked_reason`] together.
#[cfg(test)]
pub fn blocked_range_networks_for_test() -> Vec<IpAddr> {
    blocked_ranges().into_iter().map(|r| r.net).collect()
}

/// Whether any blocked range covers `addr`, for the same test.
#[cfg(test)]
pub fn any_blocked_range_covers_for_test(addr: IpAddr) -> bool {
    blocked_ranges().iter().any(|range| range.contains(addr))
}

#[cfg(test)]
pub fn blocked_reason_for_test(addr: IpAddr) -> Option<Refusal> {
    blocked_reason(addr)
}

#[cfg(test)]
#[path = "fetch_policy_test.rs"]
mod fetch_policy_test;
