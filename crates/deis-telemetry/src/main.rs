//! Deislet log aggregation and metrics.
//!
//! See the crate documentation in `lib.rs` for what this service does and,
//! just as importantly, what it deliberately does not do.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use deis_telemetry::{config, server, snapshot};

#[derive(Parser)]
#[command(name = "deis-telemetry", about = "Deislet log aggregation and metrics")]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long, default_value = "deis-telemetry.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve telemetry. This is also the default with no subcommand.
    Serve,
    /// Write a consistent, verifiable telemetry snapshot into an empty directory.
    Snapshot {
        #[arg(long)]
        out: PathBuf,
    },
    /// Restore a telemetry snapshot into an absent or empty data directory.
    Restore {
        #[arg(long)]
        from: PathBuf,
        /// Defaults to `data_dir` from the telemetry configuration.
        #[arg(long)]
        into: Option<PathBuf>,
    },
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
    match cli.command {
        None | Some(Command::Serve) => {
            let running = server::start(&config, capability_keys(&config)).await?;
            running.wait_for_signal().await
        }
        Some(Command::Snapshot { out }) => {
            let manifest = snapshot::take(&config.data_dir, &out).with_context(|| {
                format!(
                    "cannot snapshot telemetry {} into {}",
                    config.data_dir.display(),
                    out.display()
                )
            })?;
            println!(
                "telemetry snapshot written to {}\n  backend: {}\n  logs:    {}\n  metrics: {}\n  traces:  {}\n  restore: {}",
                out.display(),
                manifest.source_backend_incarnation_id,
                manifest
                    .database
                    .as_ref()
                    .map_or(0, |database| database.logs),
                manifest
                    .database
                    .as_ref()
                    .map_or(0, |database| database.metrics),
                manifest
                    .database
                    .as_ref()
                    .map_or(0, |database| database.traces),
                manifest.restore_with
            );
            Ok(())
        }
        Some(Command::Restore { from, into }) => {
            let target = into.unwrap_or_else(|| config.data_dir.clone());
            let report = snapshot::restore(&from, &target).with_context(|| {
                format!(
                    "cannot restore telemetry {} into {}",
                    from.display(),
                    target.display()
                )
            })?;
            println!(
                "telemetry restored from {} into {}\n  source backend:   {}\n  restored backend: {}\n  logs:              {}\n  metrics:           {}\n  traces:            {}",
                from.display(),
                target.display(),
                report.source_backend_incarnation_id,
                report.restored_backend_incarnation_id,
                report.database.as_ref().map_or(0, |database| database.logs),
                report
                    .database
                    .as_ref()
                    .map_or(0, |database| database.metrics),
                report
                    .database
                    .as_ref()
                    .map_or(0, |database| database.traces)
            );
            Ok(())
        }
    }
}

/// The verifier's key ring, kept current from the control plane's JWKS.
///
/// Starts empty and never blocks startup: the control plane may come up
/// after this process, and until the first fetch lands every call is refused
/// as signed by an unknown key - which is true - and the refresher retries.
fn capability_keys(config: &config::Config) -> deis_capability::KeyRing {
    let keys = deis_capability::KeyRing::new();
    tracing::info!(
        url = %config.capability_jwks_url,
        "Fetching the control plane's capability verifying keys; calls are refused until they arrive"
    );
    deis_capability::fetch::spawn_refresher(
        keys.clone(),
        config.capability_jwks_url.clone(),
        deis_capability::fetch::RefreshPolicy::default(),
    );
    keys
}
