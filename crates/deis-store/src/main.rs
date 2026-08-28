//! Deislet storage services: KV, database and blob.
//!
//! This service is deliberately "dumb": it knows only `app_id` as a tenant
//! dimension, and knows nothing about versions, environments or deployments.
//! Those concepts live in deis-control and deis-runtime.

mod config;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use tonic::transport::Server;
use tonic_health::pb::health_server::HealthServer;
use tracing::info;

/// gRPC payload ceiling. The 4 MB tonic default is too tight for batched
/// messages and manifests, so every Deislet service agrees on 16 MB.
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

#[derive(Parser)]
#[command(name = "deis-store", about = "Deislet storage services: KV, database and blob")]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long, default_value = "deis-store.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let config = config::Config::load(&cli.config)?;
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("cannot create data dir {}", config.data_dir.display()))?;

    let addr = config
        .listen
        .parse()
        .with_context(|| format!("invalid listen address {}", config.listen))?;
    info!(%addr, data_dir = %config.data_dir.display(), "deis-store starting");

    // Health reporting is always on so the control plane has a uniform
    // liveness probe across every Deislet service.
    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter.set_serving::<HealthServer<()>>().await;

    // Domain services are registered alongside health once implemented.
    Server::builder()
        .add_service(health_service)
        .serve(addr)
        .await
        .context("gRPC server terminated")?;

    Ok(())
}
