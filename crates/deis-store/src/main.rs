//! Deislet storage services: KV, database and blob.
//!
//! This service is deliberately "dumb": one tenant dimension, carried in the
//! `x-deis-tenant` header, and no idea what a version, an environment or a
//! deployment is. Those concepts live in deis-control and deis-runtime, which
//! is also where the tenant id is built - here it is one opaque key.
//!
//! It also does not authenticate its callers: the tenant header is taken at
//! face value, which is why it listens on loopback by default. See the crate
//! docs for what would have to exist before it can be bound anywhere else.

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
    /// Path to the TOML configuration file. Every key is optional and
    /// `deis-store.example.toml` lists them all with their defaults; a
    /// missing file starts the service on those defaults.
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
