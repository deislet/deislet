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
//! 2. A backend pingora has just discovered starts out marked healthy - that
//!    is pingora's default and it cannot be set from here, so a node added
//!    mid-life is believed until it is asked. The exposure is one probe, not
//!    one interval: the background loop runs discovery and the health check
//!    back to back in the same pass, and both are on the same schedule.
//! 3. When nothing passes, [`Balancer::select`] returns `None` and the
//!    request is answered `503` at the edge, with [`Balancer::rotation`]
//!    naming how many upstreams there are and how many are in rotation. That
//!    is the honest answer: a `503` that says "nothing answered" beats
//!    forwarding into a node whose own error the client cannot act on.
//!
//! [`health_check_interval`]: crate::config::SystemConfig::health_check_interval

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

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

use crate::config::system::{BalancingStrategy, HealthCheckMode};
use crate::config::{ConfigManager, SystemConfig};
use crate::proxy::APP_ID_HEADER;

/// Resolves the configured upstreams into backends.
pub struct ConfigDiscovery {
    config: Arc<ConfigManager>,
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
        debug!(count = backends.len(), "upstreams discovered");
        Ok((backends, HashMap::new()))
    }
}

/// The active selection algorithm wrapped around pingora balancers.
pub enum Balancer {
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
        let discovery = Box::new(ConfigDiscovery {
            config: Arc::clone(&config),
        });
        match system.balancing_strategy() {
            BalancingStrategy::RoundRobin => {
                let (lb, service) = make::<RoundRobin>(discovery, &system);
                (Balancer::RoundRobin(lb), service)
            }
            BalancingStrategy::Random => {
                let (lb, service) = make::<Random>(discovery, &system);
                (Balancer::Random(lb), service)
            }
            BalancingStrategy::HeaderHash(header) => {
                let (lb, service) = make::<FNVHash>(discovery, &system);
                (Balancer::HeaderHash { header, lb }, service)
            }
            BalancingStrategy::IpHash => {
                let (lb, service) = make::<Consistent>(discovery, &system);
                (Balancer::IpHash(lb), service)
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
        match self {
            Balancer::RoundRobin(lb) => warm_up_one(lb).await,
            Balancer::Random(lb) => warm_up_one(lb).await,
            Balancer::HeaderHash { lb, .. } => warm_up_one(lb).await,
            Balancer::IpHash(lb) => warm_up_one(lb).await,
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
        match self {
            Balancer::RoundRobin(lb) => rotation_of(lb),
            Balancer::Random(lb) => rotation_of(lb),
            Balancer::HeaderHash { lb, .. } => rotation_of(lb),
            Balancer::IpHash(lb) => rotation_of(lb),
        }
    }

    /// Selection key for a request under the active strategy.
    pub fn key(&self, session: &Session) -> Vec<u8> {
        match self {
            Balancer::RoundRobin(_) | Balancer::Random(_) => Vec::new(),
            Balancer::HeaderHash { header, .. } => session
                .get_header(header.as_str())
                .map(|value| value.as_bytes().to_vec())
                .unwrap_or_default(),
            Balancer::IpHash(_) => session
                .client_addr()
                .and_then(|addr| addr.as_inet())
                .map(|addr| addr.ip().to_string().into_bytes())
                .unwrap_or_default(),
        }
    }

    /// Pick a healthy backend, trying up to 256 candidates.
    pub fn select(&self, key: &[u8]) -> Option<Backend> {
        const MAX_ITERATIONS: usize = 256;
        match self {
            Balancer::RoundRobin(lb) => lb.select(key, MAX_ITERATIONS),
            Balancer::Random(lb) => lb.select(key, MAX_ITERATIONS),
            Balancer::HeaderHash { lb, .. } => lb.select(key, MAX_ITERATIONS),
            Balancer::IpHash(lb) => lb.select(key, MAX_ITERATIONS),
        }
    }

    pub fn strategy(&self) -> BalancingStrategy {
        match self {
            Balancer::RoundRobin(_) => BalancingStrategy::RoundRobin,
            Balancer::Random(_) => BalancingStrategy::Random,
            Balancer::HeaderHash { header, .. } => BalancingStrategy::HeaderHash(header.clone()),
            Balancer::IpHash(_) => BalancingStrategy::IpHash,
        }
    }
}

async fn warm_up_one<S>(lb: &Arc<LoadBalancer<S>>) -> pingora::Result<()>
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    lb.update().await?;
    // Parallel: one unreachable node must not add its whole probe timeout to
    // the wait every other node's answer is already behind.
    lb.backends().run_health_check(true).await;
    let (ready, total) = rotation_of(lb);
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

fn rotation_of<S>(lb: &Arc<LoadBalancer<S>>) -> (usize, usize)
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    let backends = lb.backends();
    let all = backends.get_backend();
    let ready = all.iter().filter(|b| backends.ready(b)).count();
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

/// Build the check that decides whether a node may be given traffic.
///
/// See [`crate::config::system::health`] for why the default asks the node
/// rather than its kernel.
fn build_health_check(system: &SystemConfig) -> Box<dyn HealthCheck + Send + Sync + 'static> {
    let settings = system.health_check();
    let timeout = settings.timeout();
    match settings.mode() {
        HealthCheckMode::Tcp => {
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
                "an upstream is given traffic only while it answers 200 to this, so an upstream \
                 whose port is open but which is not serving takes none of it"
            );
            Box::new(check)
        }
    }
}

fn make<S>(
    discovery: Box<dyn ServiceDiscovery + Send + Sync + 'static>,
    system: &SystemConfig,
) -> (Arc<LoadBalancer<S>>, Box<dyn ServiceWithDependents>)
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    let mut backends = Backends::new(discovery);
    backends.set_health_check(build_health_check(system));
    let mut lb = LoadBalancer::<S>::from_backends(backends);
    let interval = system.health_check_interval();
    lb.health_check_frequency = Some(interval);
    lb.update_frequency = Some(interval);
    lb.parallel_health_check = true;

    let service = background_service("upstream discovery and health checks", lb);
    let lb = service.task();
    (lb, Box::new(service))
}
