//! Deislet message queue.
//!
//! This service is deliberately "dumb": one tenant dimension, carried in the
//! `x-deis-tenant` header, and no idea what a version, an environment, a
//! deployment or a node is. Those concepts live in deis-control and
//! deis-runtime, which is also where the tenant id is built - here it is one
//! opaque key. Consumers come to it and lease work; it never pushes to
//! anyone.
//!
//! Who a caller is allowed to be, it does check: a call carries a capability
//! token for the tenant it names, an Ed25519-signed JCS-JWT verified against
//! the control plane's public key. Every call must present a valid token;
//! there is no anonymous or loopback bypass. See [`deis_queue::auth`].

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use deis_queue::{config::Config, server, snapshot};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "deis-queue", about = "Deislet message queue")]
struct Cli {
    /// Path to the TOML configuration file. Every key is optional and
    /// `deis-queue.example.toml` lists them all with their defaults; a
    /// missing file starts the service on those defaults.
    #[arg(short, long, default_value = "deis-queue.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the queue. This is also the default with no subcommand.
    Serve,
    /// Write a consistent, verifiable queue snapshot into an empty directory.
    Snapshot {
        #[arg(long)]
        out: PathBuf,
    },
    /// Restore a queue snapshot into an absent or empty data directory.
    Restore {
        #[arg(long)]
        from: PathBuf,
        /// Defaults to `data_dir` from the queue configuration.
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
        // Every control character escaped on the way out, so a queue name a
        // tenant chose cannot end this line and start a forged one. ANSI off
        // because the wrapper escapes ESC too; see `deis_wire::logging`.
        .with_ansi(false)
        .with_writer(deis_wire::LineSafe::new(std::io::stdout))
        .init();

    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    match cli.command {
        None | Some(Command::Serve) => {
            let listener = server::bind(&config).await?;
            let verifying_key = load_verifying_key(&config)?;
            server::serve(config, listener, server::shutdown_signal(), verifying_key).await
        }
        Some(Command::Snapshot { out }) => {
            let manifest = snapshot::take(&config.data_dir, &out).with_context(|| {
                format!(
                    "cannot snapshot queue {} into {}",
                    config.data_dir.display(),
                    out.display()
                )
            })?;
            println!(
                "queue snapshot written to {}\n  backend:      {}\n  messages:     {}\n  dead letters: {}\n  restore:      {}",
                out.display(),
                manifest.source_backend_incarnation_id,
                manifest
                    .database
                    .as_ref()
                    .map_or(0, |database| database.messages),
                manifest
                    .database
                    .as_ref()
                    .map_or(0, |database| database.dead_letters),
                manifest.restore_with
            );
            Ok(())
        }
        Some(Command::Restore { from, into }) => {
            let target = into.unwrap_or_else(|| config.data_dir.clone());
            let report = snapshot::restore(&from, &target).with_context(|| {
                format!(
                    "cannot restore queue {} into {}",
                    from.display(),
                    target.display()
                )
            })?;
            println!(
                "queue restored from {} into {}\n  source backend:   {}\n  restored backend: {}\n  messages:          {}\n  dead letters:      {}",
                from.display(),
                target.display(),
                report.source_backend_incarnation_id,
                report.restored_backend_incarnation_id,
                report
                    .database
                    .as_ref()
                    .map_or(0, |database| database.messages),
                report
                    .database
                    .as_ref()
                    .map_or(0, |database| database.dead_letters)
            );
            Ok(())
        }
    }
}

fn load_verifying_key(config: &Config) -> Result<ed25519_dalek::VerifyingKey> {
    use ed25519_dalek::VerifyingKey;

    // Try the config file first
    if let Some(ref hex_key) = config.capability_public_key {
        let bytes =
            hex::decode(hex_key).context("capability_public_key in config is not valid hex")?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
            anyhow::anyhow!("capability_public_key must be 32 bytes (64 hex chars)")
        })?;
        return Ok(VerifyingKey::from_bytes(&bytes)
            .map_err(|e| anyhow::anyhow!("invalid Ed25519 public key: {e}"))?);
    }

    // Try the environment variable
    if let Ok(hex_key) = std::env::var("DEIS_CAPABILITY_PUBLIC_KEY") {
        let bytes = hex::decode(&hex_key).context("DEIS_CAPABILITY_PUBLIC_KEY is not valid hex")?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
            anyhow::anyhow!("DEIS_CAPABILITY_PUBLIC_KEY must be 32 bytes (64 hex chars)")
        })?;
        return Ok(VerifyingKey::from_bytes(&bytes)
            .map_err(|e| anyhow::anyhow!("invalid Ed25519 public key: {e}"))?);
    }

    anyhow::bail!(
        "No capability verifying key configured. Set capability_public_key in the config \
         file, or set the DEIS_CAPABILITY_PUBLIC_KEY environment variable."
    )
}
