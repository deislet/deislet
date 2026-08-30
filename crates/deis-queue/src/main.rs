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
//! token for the tenant it names, derived from the `auth_secret` this service
//! shares with deis-store and deis-control. With no secret configured it
//! listens on loopback and believes the header, which is the default
//! single-box install; binding anywhere else in that state is refused at
//! startup. See the crate docs in `lib.rs` and [`deis_queue::auth`].

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
