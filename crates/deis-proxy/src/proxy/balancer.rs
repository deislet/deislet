//! Upstream pool: a pingora load balancer fed by the configuration.
//!
//! The selection algorithm is chosen once at startup. The upstream list is
//! re-read on pingora's update schedule, so edits to `upstreams` take effect
//! without a restart; health checks remove backends that say they are not
//! ready, and backends that say nothing at all, from rotation.
//!
//! # The window where a node is starting
//!
//! A node whose port is open but which is not yet answering is exactly the
//! case [`crate::config::system::health`] exists for. Three things decide
//! what it gets:
//!
//! 1. [`Balancer::warm_up`] runs one discovery pass *and* one health check
//!    pass before this process serves its first request, so such a node is
//!    already out of rotation by the first request rather than for the first
//!    [`health_check_interval`] of this process's life.
//! 2. Pingora internally marks a newly discovered backend healthy. The
//!    platform therefore keeps a separate admission set: a backend is not
//!    selectable until this process has observed one successful probe. This
//!    applies both at startup and when an address is added mid-life; after
//!    admission, pingora's configured success/failure hysteresis takes over.
//! 3. When nothing passes, [`Balancer::select`] returns `None` and the
//!    request is answered `503` at the edge, with [`Balancer::rotation`]
//!    naming how many upstreams there are and how many are in rotation. That
//!    is the honest answer: a `503` that says "nothing answered" beats
//!    forwarding into a node whose own error the client cannot act on.
//!
//! # Where the pool comes from, and where it does not
//!
//! [`ConfigDiscovery`] resolves `system.upstreams` and nothing else. That is
//! the whole of it, and it has a consequence worth stating plainly because
//! nothing about installing a second machine hints at it:
//!
//! **A node that registers itself with the control plane does not become an
//! upstream of any proxy.** Registration reaches deis-control, which records
//! the node so it can *manage* it - push deployments, ask it for status -
//! and the edge is not told. Adding a machine to a fleet therefore takes two
//! separate acts: start the node so the control plane can manage it, and add
//! an `[[upstreams]]` entry naming its data address to the configuration of
//! every proxy that should send it traffic, then reload each of them. Miss
//! the second and the new machine is managed, healthy, deployed to, and
//! serving nothing.
//!
//! This is documented rather than wired, and the reason is not effort. What
//! a node registers is its **management** gRPC address
//! (`deis_control::api::registration::RegisterRequest::address`, which the
//! node reports precisely because it is *not* the port traffic arrives on).
//! The control plane never learns the address the edge would have to proxy
//! to, so there is no fact in the control plane today that this discovery
//! could read. Wiring it means the node reporting a data address, the wire
//! crate carrying it, the control plane storing and serving it, and this
//! discovery merging it with the configured list - four crates, and a new
//! decision about what happens when the two sources disagree. Until that
//! exists, the honest answer is the sentence above, said here, in
//! `config.toml.example` beside `[[upstreams]]`, and in a line this process
//! logs at startup so an operator meets it without reading either.
//!
//! [`health_check_interval`]: crate::config::SystemConfig::health_check_interval

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use futures::future::join_all;
use pingora::http::RequestHeader;
use pingora::lb::discovery::ServiceDiscovery;
use pingora::lb::health_check::{HealthCheck, HealthObserve, HttpHealthCheck, TcpHealthCheck};
use pingora::lb::selection::{
    BackendIter, BackendSelection, Consistent, FNVHash, Random, RoundRobin,
};
use pingora::lb::{Backend, Backends, LoadBalancer};
use pingora::proxy::Session;
use pingora::services::ServiceWithDependents;
use pingora::services::background::background_service;
use tracing::{debug, error, info, warn};

use crate::config::system::{
    BalancingStrategy, HealthCheckMode, NODE_READINESS_HEADER, NODE_REASON_HEADER, ReadinessPolicy,
    ReadinessVerdict, judge_readiness,
};
use crate::config::{ConfigManager, SystemConfig};
use crate::proxy::APP_ID_HEADER;

/// Resolves the configured upstreams into backends.
pub struct ConfigDiscovery {
    config: Arc<ConfigManager>,
    admission: Arc<Admission>,
}

#[async_trait]
impl ServiceDiscovery for ConfigDiscovery {
    async fn discover(&self) -> pingora::Result<(BTreeSet<Backend>, HashMap<u64, bool>)> {
        let system = self.config.get_system_config();
        let mut backends = BTreeSet::new();

        // Resolve every upstream at once so a slow resolver costs one lookup
        // latency rather than one per upstream.
        let lookups = system
            .upstreams
            .iter()
            .flatten()
            .map(|upstream| async move {
                let address = upstream.address();
                let result = tokio::net::lookup_host(address.as_str())
                    .await
                    .map(|addrs| addrs.collect::<Vec<_>>());
                (address, upstream.weight, result)
            });
        for (address, weight, result) in join_all(lookups).await {
            match result {
                Ok(addrs) => {
                    let mut resolved = 0;
                    for addr in addrs {
                        match Backend::new_with_weight(&addr.to_string(), weight) {
                            Ok(backend) => {
                                backends.insert(backend);
                                resolved += 1;
                            }
                            Err(e) => {
                                warn!(upstream = %address, %addr, error = %e, "backend rejected")
                            }
                        }
                    }
                    if resolved == 0 {
                        warn!(upstream = %address, "upstream resolved to no addresses");
                    }
                }
                Err(e) => {
                    warn!(upstream = %address, error = %e, "upstream did not resolve; skipped")
                }
            }
        }

        if backends.is_empty() {
            warn!("no upstreams available; requests will be answered with 503");
        }
        self.admission.discovered(&backends);
        debug!(count = backends.len(), "upstreams discovered");
        Ok((backends, HashMap::new()))
    }
}

/// Backends for which this process has observed at least one successful
/// probe since their most recent discovery.
///
/// Pingora's health state deliberately starts at healthy. That is useful for
/// passive pools but unsafe for a readiness-gated runtime: a newly added node
/// could receive a request before the first probe says it is ready. This set
/// is the missing "unknown" state. Once a backend has passed, ordinary
/// `healthy_after`/`unhealthy_after` hysteresis remains authoritative.
struct Admission {
    discovered: Mutex<BTreeSet<Backend>>,
    passed: ArcSwap<BTreeSet<Backend>>,
}

impl Default for Admission {
    fn default() -> Self {
        Self {
            discovered: Mutex::new(BTreeSet::new()),
            passed: ArcSwap::from_pointee(BTreeSet::new()),
        }
    }
}

impl Admission {
    fn discovered(&self, backends: &BTreeSet<Backend>) {
        // The mutex serialises discovery with a probe completing, while
        // request selection reads the published set without taking a lock.
        let mut discovered = self.discovered.lock().expect("upstream admission state");
        let mut passed = (**self.passed.load()).clone();
        passed.retain(|backend| backends.contains(backend));
        self.passed.store(Arc::new(passed));
        *discovered = backends.clone();
    }

    fn passed(&self, backend: &Backend) {
        let discovered = self.discovered.lock().expect("upstream admission state");
        // A probe can finish while discovery is removing its backend. Do not
        // let that stale success pre-admit the same address if it is added
        // again later.
        if discovered.contains(backend) {
            let mut passed = (**self.passed.load()).clone();
            passed.insert(backend.clone());
            self.passed.store(Arc::new(passed));
        }
    }

    fn permits(&self, backend: &Backend) -> bool {
        self.passed.load().contains(backend)
    }
}

/// Records a successful first probe without changing the health check's
/// configured hysteresis.
struct AdmittingHealthCheck {
    inner: Box<dyn HealthCheck + Send + Sync + 'static>,
    admission: Arc<Admission>,
}

#[async_trait]
impl HealthCheck for AdmittingHealthCheck {
    async fn check(&self, target: &Backend) -> pingora::Result<()> {
        let result = self.inner.check(target).await;
        if result.is_ok() {
            self.admission.passed(target);
        }
        result
    }

    async fn health_status_change(&self, target: &Backend, healthy: bool) {
        self.inner.health_status_change(target, healthy).await;
    }

    fn backend_summary(&self, target: &Backend) -> String {
        self.inner.backend_summary(target)
    }

    fn health_threshold(&self, success: bool) -> usize {
        self.inner.health_threshold(success)
    }
}

/// The active selection algorithm wrapped around pingora balancers.
///
/// `Clone` is a handle clone and nothing more: the selected implementation
/// holds one `Arc` to the single `LoadBalancer` the background service updates, so two
/// `Balancer` values are two views of the same pool and the same health
/// state. `main` keeps one to warm the pool up with after `bootstrap()` and
/// gives the other to [`crate::proxy::DeisProxy`], which lets every fallible
/// step of startup finish before the replacement reports itself ready.
#[derive(Clone)]
pub struct Balancer {
    inner: BalancerKind,
    admission: Arc<Admission>,
}

#[derive(Clone)]
enum BalancerKind {
    RoundRobin(Arc<LoadBalancer<RoundRobin>>),
    Random(Arc<LoadBalancer<Random>>),
    HeaderHash {
        header: String,
        lb: Arc<LoadBalancer<FNVHash>>,
    },
    IpHash(Arc<LoadBalancer<Consistent>>),
}

impl Balancer {
    /// Build the balancer for the configured strategy. The returned service
    /// runs discovery and health checks and must be registered with the
    /// server.
    pub fn build(config: Arc<ConfigManager>) -> (Balancer, Box<dyn ServiceWithDependents>) {
        let system = config.get_system_config();
        // Said once, at startup, because there is no other moment an operator
        // finds it out. See "Where the pool comes from, and where it does
        // not" in the module documentation.
        let configured = system.upstreams.as_ref().map_or(0, |list| list.len());
        info!(
            upstreams = configured,
            addresses = ?system
                .upstreams
                .iter()
                .flatten()
                .map(|upstream| upstream.address())
                .collect::<Vec<_>>(),
            "these are the only upstreams this proxy will ever have. A deis-runtime node that \
             registers itself with the control plane does NOT appear here: the control plane \
             learns a node's management address and never its data address, so adding a machine \
             to the fleet needs an [[upstreams]] entry in this proxy's configuration and a \
             reload, at this edge and at every other one. Without that, the new machine is \
             managed and deployed to and takes no traffic"
        );
        let admission = Arc::new(Admission::default());
        let discovery = Box::new(ConfigDiscovery {
            config: Arc::clone(&config),
            admission: Arc::clone(&admission),
        });
        match system.balancing_strategy() {
            BalancingStrategy::RoundRobin => {
                let (lb, service) = make::<RoundRobin>(discovery, &system, &admission);
                (
                    Balancer {
                        inner: BalancerKind::RoundRobin(lb),
                        admission,
                    },
                    service,
                )
            }
            BalancingStrategy::Random => {
                let (lb, service) = make::<Random>(discovery, &system, &admission);
                (
                    Balancer {
                        inner: BalancerKind::Random(lb),
                        admission,
                    },
                    service,
                )
            }
            BalancingStrategy::HeaderHash(header) => {
                let (lb, service) = make::<FNVHash>(discovery, &system, &admission);
                (
                    Balancer {
                        inner: BalancerKind::HeaderHash { header, lb },
                        admission,
                    },
                    service,
                )
            }
            BalancingStrategy::IpHash => {
                let (lb, service) = make::<Consistent>(discovery, &system, &admission);
                (
                    Balancer {
                        inner: BalancerKind::IpHash(lb),
                        admission,
                    },
                    service,
                )
            }
        }
    }

    /// Run discovery and one health check pass so the first request finds
    /// backends that have already been asked whether they are ready.
    ///
    /// The health check pass is the half that matters. Without it the first
    /// requests this process serves are spread over every upstream that
    /// resolves, because pingora marks a newly discovered backend healthy
    /// until something says otherwise - so a node whose port is open but
    /// which is not answering would take its full share of the traffic for
    /// the first interval and fail all of it. Running the pass here moves
    /// that decision to before the first request rather than up to one
    /// interval after it.
    pub async fn warm_up(&self) -> pingora::Result<()> {
        match &self.inner {
            BalancerKind::RoundRobin(lb) => warm_up_one(lb, &self.admission).await,
            BalancerKind::Random(lb) => warm_up_one(lb, &self.admission).await,
            BalancerKind::HeaderHash { lb, .. } => warm_up_one(lb, &self.admission).await,
            BalancerKind::IpHash(lb) => warm_up_one(lb, &self.admission).await,
        }
    }

    /// How many backends there are, and how many of them are in rotation.
    ///
    /// Only read on the path where nothing could be selected, to turn "no
    /// healthy upstream" into a sentence that separates the three ways to get
    /// there: nothing configured or nothing resolving (`0` of `0`), every node
    /// refusing the health check (`0` of `n`), and a selection that ran out of
    /// iterations with healthy nodes present (`m` of `n`, `m > 0`).
    pub fn rotation(&self) -> (usize, usize) {
        match &self.inner {
            BalancerKind::RoundRobin(lb) => rotation_of(lb, &self.admission),
            BalancerKind::Random(lb) => rotation_of(lb, &self.admission),
            BalancerKind::HeaderHash { lb, .. } => rotation_of(lb, &self.admission),
            BalancerKind::IpHash(lb) => rotation_of(lb, &self.admission),
        }
    }

    /// Selection key for a request under the active strategy.
    pub fn key(&self, session: &Session) -> Vec<u8> {
        match &self.inner {
            BalancerKind::RoundRobin(_) | BalancerKind::Random(_) => Vec::new(),
            BalancerKind::HeaderHash { header, .. } => session
                .get_header(header.as_str())
                .map(|value| value.as_bytes().to_vec())
                .unwrap_or_default(),
            BalancerKind::IpHash(_) => session
                .client_addr()
                .and_then(|addr| addr.as_inet())
                .map(|addr| addr.ip().to_string().into_bytes())
                .unwrap_or_default(),
        }
    }

    /// Pick a healthy backend this request has not already failed to reach,
    /// trying up to 256 candidates.
    ///
    /// `avoid` is what makes a retry worth anything. Pingora calls
    /// [`ProxyHttp::upstream_peer`] again for every retry, and under a hash
    /// strategy the same key picks the same backend every time, so without
    /// the exclusion a refused connection would be retried against the node
    /// that just refused it until the retry budget ran out and the client got
    /// its `502` anyway. Under round robin it would have worked by accident,
    /// which is not the same as working.
    ///
    /// The exclusion is per request and never touches the health state: which
    /// nodes are in rotation is the health check's answer, and one connection
    /// failure is not enough evidence to take a node away from every other
    /// request in flight.
    ///
    /// [`ProxyHttp::upstream_peer`]: pingora::proxy::ProxyHttp::upstream_peer
    pub fn select(&self, key: &[u8], avoid: &[String]) -> Option<Backend> {
        const MAX_ITERATIONS: usize = 256;
        let accept = |backend: &Backend, healthy: bool, admission: &Admission| {
            if !healthy || !admission.permits(backend) {
                return false;
            }
            // The overwhelming majority of requests never fail to connect and
            // arrive here with nothing to avoid, so that case does exactly
            // what `select` did before: no formatting, no allocation.
            if avoid.is_empty() {
                return true;
            }
            let addr = backend.addr.to_string();
            !avoid.contains(&addr)
        };
        match &self.inner {
            BalancerKind::RoundRobin(lb) => {
                lb.select_with(key, MAX_ITERATIONS, |backend, healthy| {
                    accept(backend, healthy, &self.admission)
                })
            }
            BalancerKind::Random(lb) => lb.select_with(key, MAX_ITERATIONS, |backend, healthy| {
                accept(backend, healthy, &self.admission)
            }),
            BalancerKind::HeaderHash { lb, .. } => {
                lb.select_with(key, MAX_ITERATIONS, |backend, healthy| {
                    accept(backend, healthy, &self.admission)
                })
            }
            BalancerKind::IpHash(lb) => lb.select_with(key, MAX_ITERATIONS, |backend, healthy| {
                accept(backend, healthy, &self.admission)
            }),
        }
    }

    pub fn strategy(&self) -> BalancingStrategy {
        match &self.inner {
            BalancerKind::RoundRobin(_) => BalancingStrategy::RoundRobin,
            BalancerKind::Random(_) => BalancingStrategy::Random,
            BalancerKind::HeaderHash { header, .. } => {
                BalancingStrategy::HeaderHash(header.clone())
            }
            BalancerKind::IpHash(_) => BalancingStrategy::IpHash,
        }
    }
}

async fn warm_up_one<S>(lb: &Arc<LoadBalancer<S>>, admission: &Admission) -> pingora::Result<()>
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    lb.update().await?;
    // Parallel: one unreachable node must not add its whole probe timeout to
    // the wait every other node's answer is already behind.
    lb.backends().run_health_check(true).await;
    let (ready, total) = rotation_of(lb, admission);
    if total == 0 {
        // Not fatal: `upstreams` may be empty on purpose while a control
        // plane fills it in, and discovery runs again on every interval.
        warn!("no upstream resolved; every request is answered 503 until one does");
    } else if ready == 0 {
        error!(
            upstreams = total,
            "no upstream answered the health check, so every request is answered 503 at this \
             edge until one does. An upstream whose port is open but which is not serving counts \
             as one of these on purpose: forwarding to it would turn this into an error the \
             client cannot tell apart from the application's own"
        );
    } else {
        info!(
            ready,
            upstreams = total,
            "upstreams checked before serving; only the ones that answered are given traffic"
        );
    }
    Ok(())
}

fn rotation_of<S>(lb: &Arc<LoadBalancer<S>>, admission: &Admission) -> (usize, usize)
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    let backends = lb.backends();
    let all = backends.get_backend();
    let ready = all
        .iter()
        .filter(|b| backends.ready(b) && admission.permits(b))
        .count();
    (ready, all.len())
}

/// Logs a node entering or leaving rotation, at the level the event deserves.
///
/// Pingora logs these itself, but only as `Debug` of the backend and without
/// saying what the check asked. An operator reading this log wants the
/// address, the question and the consequence, because a node leaving rotation
/// is a capacity change they may need to act on.
struct RotationLog {
    /// What the probe asks, for the log line. `mode`, and for HTTP the
    /// request target and the application it names.
    asked: String,
    /// Consecutive failures behind an eviction, so the line says how long the
    /// node had been failing rather than just that it now is.
    unhealthy_after: usize,
    healthy_after: usize,
}

#[async_trait]
impl HealthObserve for RotationLog {
    async fn observe(&self, target: &Backend, healthy: bool) {
        if healthy {
            info!(
                upstream = %target.addr,
                asked = %self.asked,
                probes = self.healthy_after,
                "upstream is back in rotation"
            );
        } else {
            warn!(
                upstream = %target.addr,
                asked = %self.asked,
                probes = self.unhealthy_after,
                "upstream is out of rotation and will be given no requests until it answers \
                 again; the fleet is carrying its share"
            );
        }
    }
}

/// How often a fleet that is reporting `degraded` is worth saying so about.
///
/// The probe runs every `health_check_interval` (5 seconds by default) per
/// node, and a degraded node stays in rotation, so there is no transition for
/// [`RotationLog`] to report and nothing else would ever say it. Logging every
/// probe would be twelve lines a minute per node for a state that can last
/// hours; once a minute is enough to be noticed and little enough to live
/// with.
const DEGRADED_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Rate limit for the line above.
///
/// Pingora's validator is handed the response header and nothing else - not
/// the backend, not the check - so this cannot be per node and the message
/// does not pretend to name one. What it can say, and what an operator needs
/// first, is that some node in this pool is serving something other than what
/// it was assigned, together with whatever that node said about why. The
/// cause is not stated here: `degraded` has several, and this comment used to
/// name the one the edge happened to have guessed.
struct DegradedNotice {
    started: Instant,
    /// Seconds since `started` at the last warning, or `u64::MAX` for none.
    last: AtomicU64,
}

impl DegradedNotice {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            last: AtomicU64::new(u64::MAX),
        }
    }

    /// Whether this observation should be logged now.
    fn due(&self) -> bool {
        let now = self.started.elapsed().as_secs();
        let last = self.last.load(Ordering::Relaxed);
        if last != u64::MAX && now.saturating_sub(last) < DEGRADED_LOG_INTERVAL.as_secs() {
            return false;
        }
        self.last.store(now, Ordering::Relaxed);
        true
    }
}

/// Build the check that decides whether a node may be given traffic.
///
/// See [`crate::config::system::health`] for why the default asks the node
/// rather than its kernel, and for what the node has to say for the answer to
/// mean anything.
fn build_health_check(system: &SystemConfig) -> Box<dyn HealthCheck + Send + Sync + 'static> {
    let settings = system.health_check();
    let timeout = settings.timeout();
    match settings.mode() {
        HealthCheckMode::Tcp => {
            if matches!(settings.readiness, Some(policy) if policy != ReadinessPolicy::Ignored) {
                warn!(
                    readiness = %settings.readiness(),
                    "health_check.readiness is set but health_check.mode is tcp, so nothing is \
                     ever asked and the setting does nothing. Leave the mode unset to have the \
                     node asked"
                );
            }
            let asked = "mode=tcp (connect only; a node that is listening and not ready passes)"
                .to_string();
            let mut check = TcpHealthCheck::new();
            check.consecutive_success = settings.healthy_after();
            check.consecutive_failure = settings.unhealthy_after();
            check.peer_template.options.connection_timeout = Some(timeout);
            check.health_changed_callback = Some(Box::new(RotationLog {
                asked,
                unhealthy_after: settings.unhealthy_after(),
                healthy_after: settings.healthy_after(),
            }));
            warn!(
                timeout_secs = settings.timeout_secs(),
                "health_check.mode is tcp: an upstream counts as healthy as soon as its port \
                 accepts a connection, and nothing here asks it anything. A deis-runtime node \
                 binds that port before it can serve and keeps it bound if the dispatcher \
                 behind it stops answering, so in either state it keeps its full share of the \
                 traffic and fails all of it. Only right for an upstream that is not a node; \
                 leave the mode unset to ask the node instead"
            );
            check
        }
        HealthCheckMode::Http => {
            // `HttpPeer`'s host is only the `Host` header here - the probe
            // goes to the backend's own address and the node routes on
            // `x-deis-app-id`, not on `Host`. `localhost` is what the node
            // itself falls back to for a request that names no authority.
            let mut check = HttpHealthCheck::new("localhost", false);
            let mut asked = format!("mode=http GET {}", settings.path());
            match RequestHeader::build("GET", settings.path().as_bytes(), None) {
                Ok(mut req) => {
                    if let Err(e) = req.append_header("Host", "localhost") {
                        error!(error = %e, "could not set the health probe's Host header");
                    }
                    if !settings.app_id().is_empty() {
                        match req.append_header(APP_ID_HEADER, settings.app_id()) {
                            Ok(_) => {
                                asked = format!("{asked} ({APP_ID_HEADER}: {})", settings.app_id())
                            }
                            // A node answers 400 to a request that names no
                            // application, so every probe would fail and the
                            // whole fleet would leave rotation. Saying so is
                            // the difference between a five-minute diagnosis
                            // and an hour of looking at the nodes.
                            Err(e) => error!(
                                error = %e,
                                app_id = settings.app_id(),
                                "health_check.app_id is not a legal header value, so the probe \
                                 names no application; a deis-runtime node answers 400 to that \
                                 and every upstream will read as unhealthy"
                            ),
                        }
                    }
                    check.req = req;
                }
                // The path was validated as a request target, so this is
                // unreachable through the configuration file; falling back to
                // pingora's `GET /` keeps the check running rather than
                // leaving the process with no health check at all.
                Err(e) => error!(
                    error = %e,
                    path = settings.path(),
                    "health_check.path is not a usable request target; probing / instead"
                ),
            }
            let policy = settings.readiness();
            asked = format!("{asked} readiness={policy}");
            // A 200 from `_system`'s `/health` is a constant: it proves the
            // dispatcher ran an isolate and says nothing about whether this
            // node is running what the control plane assigned it. The
            // readiness the node states on the answer is what turns this into
            // a question that can be answered no; `judge_readiness` holds the
            // rule and is tested on its own.
            let degraded = DegradedNotice::new();
            check.validator = Some(Box::new(move |resp| {
                let status = resp.status.as_u16();
                let reported = resp
                    .headers
                    .get(NODE_READINESS_HEADER)
                    .and_then(|value| value.to_str().ok());
                // Why, in the node's own words, when the node sent it.
                // Read off the head because that is all pingora hands a
                // validator - and today no deis-runtime node puts it there:
                // `dispatcher/system_app.js` stamps the readiness on the
                // response head and the reason only into the response body,
                // which a health check never reads. So this is `None` against
                // the current fleet and the verdict says the node did not say
                // why, which is true. What it no longer does is invent one.
                // The startup line below tells the operator the same thing.
                let reason = resp
                    .headers
                    .get(NODE_REASON_HEADER)
                    .and_then(|value| value.to_str().ok());
                match judge_readiness(status, reported, reason, policy) {
                    ReadinessVerdict::InRotation => Ok(()),
                    ReadinessVerdict::InRotationWithWarning(why) => {
                        if degraded.due() {
                            warn!(
                                status,
                                reported = ?reported,
                                reason = ?reason,
                                readiness = %policy,
                                interval_secs = DEGRADED_LOG_INTERVAL.as_secs(),
                                "{why}. Repeated at most once every {} seconds while it lasts",
                                DEGRADED_LOG_INTERVAL.as_secs()
                            );
                        }
                        Ok(())
                    }
                    ReadinessVerdict::OutOfRotation(why) => pingora::Error::e_explain(
                        pingora::ErrorType::CustomCode("upstream is not ready", status),
                        why,
                    ),
                }
            }));
            check.consecutive_success = settings.healthy_after();
            check.consecutive_failure = settings.unhealthy_after();
            check.peer_template.options.connection_timeout = Some(timeout);
            check.peer_template.options.read_timeout = Some(timeout);
            check.health_changed_callback = Some(Box::new(RotationLog {
                asked: asked.clone(),
                unhealthy_after: settings.unhealthy_after(),
                healthy_after: settings.healthy_after(),
            }));
            info!(
                asked = %asked,
                timeout_secs = settings.timeout_secs(),
                interval_secs = system.health_check_interval().as_secs(),
                healthy_after = settings.healthy_after(),
                unhealthy_after = settings.unhealthy_after(),
                readiness_header = NODE_READINESS_HEADER,
                "an upstream is given traffic only while it answers 200 to this, so an upstream \
                 whose port is open but which is not serving takes none of it"
            );
            match policy {
                ReadinessPolicy::Reported => info!(
                    readiness_header = NODE_READINESS_HEADER,
                    reason_header = NODE_REASON_HEADER,
                    "a node that states its own readiness is believed: `starting` takes it out \
                     of rotation, `degraded` keeps its share because every node goes degraded at \
                     once when the control plane does. A node that states nothing is judged on \
                     the 200 alone, which only proves it ran an isolate - so until deis-runtime \
                     sends this header, a node whose assignments failed to load still counts as \
                     healthy here. Why a node is degraded is repeated from the reason header \
                     when it sends one; a node that sends none is reported as one that did not \
                     say, and this edge never states a cause on the node's behalf"
                ),
                ReadinessPolicy::Required => warn!(
                    readiness_header = NODE_READINESS_HEADER,
                    "health_check.readiness is \"required\": an upstream that does not state \
                     `ready` in this header is given no traffic, a node that states nothing \
                     included. If any node in this pool is older than that contract the pool \
                     empties and every request is answered 503"
                ),
                ReadinessPolicy::Ignored => warn!(
                    "health_check.readiness is \"ignored\": the status code alone decides, so a \
                     deis-runtime node that reports `starting` keeps its full share of the \
                     traffic and fails it. Only right for an upstream that is not a node"
                ),
            }
            Box::new(check)
        }
    }
}

fn make<S>(
    discovery: Box<dyn ServiceDiscovery + Send + Sync + 'static>,
    system: &SystemConfig,
    admission: &Arc<Admission>,
) -> (Arc<LoadBalancer<S>>, Box<dyn ServiceWithDependents>)
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    let mut backends = Backends::new(discovery);
    backends.set_health_check(Box::new(AdmittingHealthCheck {
        inner: build_health_check(system),
        admission: Arc::clone(admission),
    }));
    let mut lb = LoadBalancer::<S>::from_backends(backends);
    let interval = system.health_check_interval();
    lb.health_check_frequency = Some(interval);
    lb.update_frequency = Some(interval);
    lb.parallel_health_check = true;

    let service = background_service("upstream discovery and health checks", lb);
    let lb = service.task();
    (lb, Box::new(service))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(port: u16) -> Backend {
        Backend::new(&format!("127.0.0.1:{port}")).expect("test backend")
    }

    #[test]
    fn a_discovered_backend_is_unknown_until_one_probe_passes() {
        let admission = Admission::default();
        let node = backend(8100);
        admission.discovered(&BTreeSet::from([node.clone()]));

        assert!(!admission.permits(&node));
        admission.passed(&node);
        assert!(admission.permits(&node));
    }

    #[test]
    fn removal_clears_admission_and_a_late_probe_cannot_restore_it() {
        let admission = Admission::default();
        let node = backend(8101);
        admission.discovered(&BTreeSet::from([node.clone()]));
        admission.passed(&node);
        assert!(admission.permits(&node));

        admission.discovered(&BTreeSet::new());
        admission.passed(&node);
        assert!(!admission.permits(&node));

        admission.discovered(&BTreeSet::from([node.clone()]));
        assert!(
            !admission.permits(&node),
            "re-adding an address requires a new successful probe"
        );
    }

    #[test]
    fn rediscovering_the_same_backend_keeps_its_proof() {
        let admission = Admission::default();
        let node = backend(8102);
        let pool = BTreeSet::from([node.clone()]);
        admission.discovered(&pool);
        admission.passed(&node);

        admission.discovered(&pool);
        assert!(admission.permits(&node));
    }
}
