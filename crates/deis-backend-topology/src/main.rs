use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use deis_backend_topology::{
    TopologyConfig, capture_restore_set, execute_restore, inspect_restore,
};
use deis_wire::{BackendRestoreSet, CanonicalId};

#[derive(Parser)]
#[command(name = "deis-backend-topology")]
struct Cli {
    #[arg(long)]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Capture a store/queue/telemetry recovery point under one outage.
    SnapshotSet {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        organization_id: CanonicalId,
        #[arg(long)]
        installation_id: CanonicalId,
    },
    /// Execute or resume one coordinated embedded-backend restore.
    Restore {
        #[arg(long)]
        operation_id: CanonicalId,
        /// JSON restore-set path, or `-` to read it from standard input.
        #[arg(long, default_value = "-")]
        restore_set: String,
    },
    /// Return terminal evidence from a durable restore journal.
    Status {
        #[arg(long)]
        operation_id: CanonicalId,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(false)
        .with_writer(deis_wire::LineSafe::new(std::io::stderr))
        .init();
    let cli = Cli::parse();
    let config = TopologyConfig::load(&cli.config)?;
    match cli.command {
        Command::SnapshotSet {
            out,
            organization_id,
            installation_id,
        } => {
            let restore_set =
                capture_restore_set(&config, &out, organization_id, installation_id).await?;
            println!("{}", serde_json::to_string(&restore_set)?);
            Ok(())
        }
        Command::Restore {
            operation_id,
            restore_set,
        } => {
            let (bytes, source) = if restore_set == "-" {
                let mut bytes = Vec::new();
                std::io::stdin()
                    .take(1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .context("cannot read restore set from stdin")?;
                anyhow::ensure!(bytes.len() <= 1024 * 1024, "restore set exceeds 1 MiB");
                (bytes, "stdin".to_string())
            } else {
                let path = PathBuf::from(&restore_set);
                (
                    std::fs::read(&path)
                        .with_context(|| format!("cannot read restore set {}", path.display()))?,
                    path.display().to_string(),
                )
            };
            let restore_set: BackendRestoreSet = serde_json::from_slice(&bytes)
                .with_context(|| format!("cannot parse restore set from {source}"))?;
            let report = execute_restore(&config, &restore_set, operation_id).await?;
            println!("{}", serde_json::to_string(&report)?);
            Ok(())
        }
        Command::Status { operation_id } => {
            let report = inspect_restore(&config, operation_id)?
                .context("restore journal has not reached a verified terminal state")?;
            println!("{}", serde_json::to_string(&report)?);
            Ok(())
        }
    }
}
