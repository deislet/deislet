//! Deislet storage services: KV, database and blob.
//!
//! This service is deliberately "dumb": it knows only `app_id` as a tenant
//! dimension, and knows nothing about versions, environments or deployments.
//! Those concepts live in deis-control and deis-runtime.

use anyhow::Result;
use clap::Parser;
use deis_store::{config::Config, server};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "deis-store",
    about = "Deislet storage services: KV, database and blob"
)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long, default_value = "deis-store.toml")]
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
