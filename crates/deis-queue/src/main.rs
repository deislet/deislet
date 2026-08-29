//! Deislet message queue.
//!
//! This service is deliberately "dumb": it knows only `app_id` as a tenant
//! dimension, and knows nothing about versions, environments, deployments or
//! nodes. Those concepts live in deis-control and deis-runtime. Consumers
//! come to it and lease work; it never pushes to anyone.
//!
//! It also does not authenticate its callers: the tenant header is taken at
//! face value, which is why it listens on loopback by default. See the crate
//! docs in `lib.rs` for the trust model and for what would have to exist
//! before it can be bound anywhere else.

use anyhow::Result;
use clap::Parser;
use deis_queue::{config::Config, server};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "deis-queue", about = "Deislet message queue")]
struct Cli {
    /// Path to the TOML configuration file. Every key is optional and
    /// `deis-queue.example.toml` lists them all with their defaults; a
    /// missing file starts the service on those defaults.
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
    let config = Config::load(&cli.config)?;
    let listener = server::bind(&config).await?;

    server::serve(config, listener, server::shutdown_signal()).await
}
