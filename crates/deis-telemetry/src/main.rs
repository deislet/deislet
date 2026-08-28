//! Deislet log aggregation and metrics.
//!
//! See the crate documentation in `lib.rs` for what this service does and,
//! just as importantly, what it deliberately does not do.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use deis_telemetry::{config, server};

#[derive(Parser)]
#[command(name = "deis-telemetry", about = "Deislet log aggregation and metrics")]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long, default_value = "deis-telemetry.toml")]
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
    let running = server::start(&config).await?;
    running.wait_for_signal().await
}
