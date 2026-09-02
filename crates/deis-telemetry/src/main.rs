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
        // This service's own log, not the tenant logs it aggregates: those go
        // to storage, not through here. It still handles tenant text (app ids,
        // node ids, rejected payloads in error messages), so it gets the same
        // floor - every control character escaped, ANSI off because the
        // wrapper escapes ESC too. See `deis_wire::logging`.
        .with_ansi(false)
        .with_writer(deis_wire::LineSafe::new(std::io::stdout))
        .init();

    let cli = Cli::parse();
    let config = config::Config::load(&cli.config)?;
    let running = server::start(&config).await?;
    running.wait_for_signal().await
}
