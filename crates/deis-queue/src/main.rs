//! Deislet message queue.
//!
//! This service is deliberately "dumb": it knows only `app_id` as a tenant
//! dimension, and knows nothing about versions, environments or deployments.
//! Those concepts live in deis-control and deis-runtime.

// Re-exported so every component in the platform spells these the same
// way; see deis_wire::conventions for why they live in one place.
pub use deis_wire::conventions::MAX_GRPC_MESSAGE_BYTES;

mod config;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use tokio::signal::unix::{SignalKind, signal};
use tonic::transport::Server;
use tonic_health::pb::health_server::HealthServer;
use tracing::{error, info};


#[derive(Parser)]
#[command(name = "deis-queue", about = "Deislet message queue")]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long, default_value = "deis-queue.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
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
    info!(%addr, data_dir = %config.data_dir.display(), "deis-queue starting");

    // Health reporting is always on so the control plane has a uniform
    // liveness probe across every Deislet service.
    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter.set_serving::<HealthServer<()>>().await;

    // Domain services are registered alongside health once implemented.
    Server::builder()
        .add_service(
            health_service
                .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES),
        )
        .serve_with_shutdown(addr, shutdown_signal())
        .await
        .context("gRPC server terminated")?;

    info!("deis-queue stopped");
    Ok(())
}

/// Resolve when the process is asked to stop.
///
/// Both signals are honoured, because both are used: an operator pressing
/// Ctrl-C sends SIGINT, while systemd, Docker and Kubernetes stop a container
/// with SIGTERM. Without this the server would run until the process was
/// killed outright, so in-flight requests would be cut mid-response instead
/// of being allowed to finish.
async fn shutdown_signal() {
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(stream) => Some(stream),
        Err(e) => {
            error!(
                error = %e,
                "cannot listen for SIGTERM; a supervisor stopping this service will kill it \
                 outright and in-flight requests will be cut mid-response"
            );
            None
        }
    };

    match terminate.as_mut() {
        Some(terminate) => {
            tokio::select! {
                result = tokio::signal::ctrl_c() => report_interrupt(result),
                _ = terminate.recv() => info!("SIGTERM received, draining in-flight requests"),
            }
        }
        None => report_interrupt(tokio::signal::ctrl_c().await),
    }
}

fn report_interrupt(result: std::io::Result<()>) {
    match result {
        Ok(()) => info!("interrupt received, draining in-flight requests"),
        Err(e) => error!(
            error = %e,
            "cannot listen for the interrupt signal; the process will only stop when killed, \
             which cuts in-flight requests"
        ),
    }
}
