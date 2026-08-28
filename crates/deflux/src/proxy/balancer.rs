//! Upstream pool: a pingora load balancer fed by the configuration.
//!
//! The selection algorithm is chosen once at startup. The upstream list is
//! re-read on pingora's update schedule, so edits to `upstreams` take effect
//! without a restart; health checks remove unreachable backends from rotation.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::join_all;
use pingora::lb::discovery::ServiceDiscovery;
use pingora::lb::health_check::TcpHealthCheck;
use pingora::lb::selection::{
    BackendIter, BackendSelection, Consistent, FNVHash, Random, RoundRobin,
};
use pingora::lb::{Backend, Backends, LoadBalancer};
use pingora::proxy::Session;
use pingora::services::Service;
use pingora::services::background::background_service;
use tracing::{debug, warn};

use crate::config::system::BalancingStrategy;
use crate::config::{ConfigManager, SystemConfig};

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
    pub fn build(config: Arc<ConfigManager>) -> (Balancer, Box<dyn Service>) {
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

    /// Run discovery once so the first requests find backends.
    pub async fn warm_up(&self) -> pingora::Result<()> {
        match self {
            Balancer::RoundRobin(lb) => lb.update().await,
            Balancer::Random(lb) => lb.update().await,
            Balancer::HeaderHash { lb, .. } => lb.update().await,
            Balancer::IpHash(lb) => lb.update().await,
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

fn make<S>(
    discovery: Box<dyn ServiceDiscovery + Send + Sync + 'static>,
    system: &SystemConfig,
) -> (Arc<LoadBalancer<S>>, Box<dyn Service>)
where
    S: BackendSelection + Send + Sync + 'static,
    S::Iter: BackendIter,
{
    let mut backends = Backends::new(discovery);
    backends.set_health_check(TcpHealthCheck::new());
    let mut lb = LoadBalancer::<S>::from_backends(backends);
    let interval = system.health_check_interval();
    lb.health_check_frequency = Some(interval);
    lb.update_frequency = Some(interval);
    lb.parallel_health_check = true;

    let service = background_service("upstream discovery and health checks", lb);
    let lb = service.task();
    (lb, Box::new(service))
}
