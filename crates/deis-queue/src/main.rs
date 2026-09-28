//! Deislet message queue.
//!
//! This service is deliberately "dumb": one tenant dimension, named by the
//! capability a call carries, and no idea what a version, an environment, a
//! deployment or a node is. Those concepts live in deis-control and
//! deis-runtime - here the tenant is one opaque key. Consumers come to it and lease work; it never pushes to
//! anyone.
//!
//! Who a caller is allowed to be, it does check: a call carries a capability
//! token for the tenant it names, an Ed25519-signed JCS-JWT verified against
//! the control plane's published keys (its JWKS). Every call must present a valid token;
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
            let (identity, control) = server::machine_identity(&config)?;
            let listener = server::bind(&config).await?;
            let keys = capability_keys(control.clone());
            server::serve(
                config,
                listener,
                server::shutdown_signal(),
                keys,
                control,
                identity,
            )
            .await
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

/// The verifier's key ring, kept current from the control plane's JWKS.
///
/// Starts empty and never blocks startup: the control plane may come up
/// after this process, and until the first fetch lands every call is refused
/// as signed by an unknown key - which is true - and the refresher retries.
fn capability_keys(control: deis_capability::ControlClient) -> deis_capability::KeyRing {
    let keys = deis_capability::KeyRing::new();
    tracing::info!(
        control = ?control,
        "Fetching the control plane's capability verifying keys; calls are refused until they arrive"
    );
    deis_capability::fetch::spawn_refresher(
        keys.clone(),
        control,
        deis_capability::fetch::RefreshPolicy::default(),
    );
    keys
}
