//! Deflux: managed multi-site reverse proxy and load balancer.
//!
//! The data plane is Pingora: HTTP/1.1 and HTTP/2 termination, TLS with
//! per-application certificates selected by SNI, upstream load balancing with
//! health checks, and zero-downtime binary upgrades. Deflux adds domain based
//! application routing, configuration layering (defaults + local file +
//! supervisor), and the supervisor client that registers the instance with
//! denira.

pub mod cli;
pub mod config;
pub mod proxy;
pub mod restart;
pub mod supervisor;
pub mod tls;

pub use config::{AppConfig, ConfigManager, SystemConfig};
pub use supervisor::DefluxSupervisorClient;
