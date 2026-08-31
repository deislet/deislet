//! Proxy: managed multi-site reverse proxy and load balancer.
//!
//! The data plane is Pingora: HTTP/1.1 and HTTP/2 termination, TLS with
//! per-application certificates selected by SNI, upstream load balancing with
//! health checks, and zero-downtime binary upgrades. Proxy adds domain based
//! application routing, configuration layering (defaults + local file +
//! control), and the control client that registers the instance with
//! deis-control.
//!
//! Which application a request belongs to is answered by the control
//! plane's catalog, polled by [`catalog`]; the local application list adds
//! what to do with the request once the application is known - its
//! certificates, its cache policy, whether it is enabled at all.
//!
//! Those certificates can be obtained rather than placed: [`acme`] holds the
//! RFC 8555 client, the `http-01` responder the data plane answers from, and
//! the task that renews before expiry. It is off unless `[tls.acme]` turns
//! it on, and an instance with certificate files of its own never
//! constructs it.

pub mod acme;
pub mod catalog;
pub mod cli;
pub mod config;
pub mod control;
pub mod proxy;
pub mod restart;
pub mod tls;

pub use config::{AppConfig, ConfigManager, SystemConfig};
pub use control::ProxyControlClient;
