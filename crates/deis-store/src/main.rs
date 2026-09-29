//! Deislet storage services: KV, database and blob.
//!
//! This service is deliberately "dumb": one tenant dimension, named by the
//! capability a call carries, and no idea what a version, an environment or a
//! deployment is. Those concepts live in deis-control and deis-runtime - here
//! the tenant is one opaque key.
//!
//! Who a caller is, it does check: a call carries a capability token naming
//! its tenant, an Ed25519-signed JCS-JWT verified against
//! the control plane's published keys, read from its JWKS over the machine
//! channel under mutual TLS. Every call must present a valid token; there is
//! no anonymous or loopback bypass. See [`deis_store::auth`]. The listener
//! itself speaks TLS only, with this store's own machine identity.
//!
//! Beside serving, the binary is also how a data directory is backed up and
//! put back: `snapshot` and `restore`. Those are subcommands rather than RPCs
//! because they write local files an operator names, and because they have to
//! work when the service is not running - see [`deis_store::snapshot`]. The
//! running service takes the same snapshots on a schedule once `snapshot_dir`
//! is configured; see [`deis_store::backup`].

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use deis_store::{config::Config, server, snapshot};
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
    ///
    /// The subcommands read it too, so a snapshot or a restore acts on the
    /// same data directory the service serves.
    #[arg(short, long, default_value = "deis-store.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve KvStore, Database and BlobStore. This is what running the
    /// binary with no subcommand does.
    Serve,
    /// Write a consistent, restorable copy of the data directory.
    ///
    /// Safe to run while the service is serving: every database is copied
    /// under one read transaction and the blob metadata is copied before the
    /// content it names, so the copy never refers to bytes it does not hold.
    /// Copying the data directory with `cp -r` instead is *not* safe - SQLite
    /// runs in WAL mode here, so a plain copy can restore as a database
    /// missing its last commits, or as a corrupt one, with no warning.
    Snapshot {
        /// Directory to write the snapshot into. It must not already exist,
        /// or must be empty: a snapshot written over an older one is neither
        /// of them.
        #[arg(long)]
        out: PathBuf,
    },
    /// Put a snapshot back.
    ///
    /// Stop the service first and move the old data directory aside: the
    /// restore refuses to write into a directory that already holds
    /// anything, which is also what keeps it from being poured over live
    /// data. Every object is verified against the digest it is named by on
    /// the way in.
    Restore {
        /// The snapshot directory, as written by `snapshot`.
        #[arg(long)]
        from: PathBuf,
        /// Where to put it. Defaults to `data_dir` from the configuration
        /// file, so a restore lands where the service will look for it; name
        /// something else to stage a restore and inspect it first.
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
        // Every control character escaped on the way out, so a bucket or key
        // name a tenant chose cannot end this line and start a forged one.
        // ANSI off because the wrapper escapes ESC too; see
        // `deis_wire::logging`.
        .with_ansi(false)
        .with_writer(deis_wire::LineSafe::new(std::io::stdout))
        .init();

    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;

    match cli.command {
        None | Some(Command::Serve) => serve(config).await,
        // Blocking work on the runtime's own thread. These are one-shot
        // operator commands with nothing else running beside them, so there
        // is no worker to starve.
        Some(Command::Snapshot { out }) => take_snapshot(&config, &out),
        Some(Command::Restore { from, into }) => restore_snapshot(&config, &from, into),
    }
}

async fn serve(config: Config) -> Result<()> {
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

fn take_snapshot(config: &Config, out: &std::path::Path) -> Result<()> {
    let manifest = snapshot::take(&config.data_dir, out).with_context(|| {
        format!(
            "cannot snapshot {} into {}",
            config.data_dir.display(),
            out.display()
        )
    })?;
    // Printed as well as logged: this is a command a person ran and is
    // watching, and the restore line is the thing they will need later.
    println!(
        "snapshot written to {}\n  backend:   {}\n  databases: {}\n  objects:   {} ({} bytes)\n  restore:   {}",
        out.display(),
        manifest.source_backend_incarnation_id,
        manifest.databases.len(),
        manifest.objects,
        manifest.object_bytes,
        manifest.restore_with
    );
    if manifest.dropped_objects > 0 {
        println!(
            "  note:      {} object(s) were deleted from the live store while this ran and are \
             not in the snapshot",
            manifest.dropped_objects
        );
    }
    if !manifest.damaged_objects.is_empty() {
        println!(
            "  warning:   {} content file(s) in the live store do not match their digest; see \
             {} for the list",
            manifest.damaged_objects.len(),
            snapshot::MANIFEST_NAME
        );
    }
    Ok(())
}

fn restore_snapshot(config: &Config, from: &std::path::Path, into: Option<PathBuf>) -> Result<()> {
    let target = into.unwrap_or_else(|| config.data_dir.clone());
    let report = snapshot::restore(from, &target).with_context(|| {
        format!(
            "cannot restore {} into {}",
            from.display(),
            target.display()
        )
    })?;
    println!(
        "restored {} into {}\n  source backend:   {}\n  restored backend: {}\n  databases: {}\n  objects:   {} ({} bytes)",
        from.display(),
        target.display(),
        report.source_backend_incarnation_id,
        report.restored_backend_incarnation_id,
        report.databases,
        report.objects,
        report.object_bytes
    );
    if report.damaged_objects > 0 {
        println!(
            "  warning:   {} object(s) were already damaged when the snapshot was taken and are \
             restored as they were",
            report.damaged_objects
        );
    }
    println!("Start deis-store against {} now.", target.display());
    Ok(())
}
