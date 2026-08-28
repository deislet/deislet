//! Process control: zero-downtime upgrade and graceful shutdown.
//!
//! Both actions are carried out by pingora's server through Unix signals:
//!
//! - `SIGQUIT` makes the running server hand its listening sockets to a
//!   freshly started `deflux --upgrade` process over the upgrade socket and
//!   exit once in-flight requests complete. The replacement must already be
//!   waiting on that socket, so it is spawned first.
//! - `SIGTERM` stops accepting connections and exits after the grace period.
//!
//! Requests arrive from `SIGUSR1` (`deflux -s reload`) and from the
//! supervisor client.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use async_trait::async_trait;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;
use tracing::{error, info};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessCommand {
    /// Start a replacement process, hand it the listening sockets, then exit
    /// once in-flight requests finish.
    Upgrade,
    /// Stop accepting connections and exit once in-flight requests finish.
    Shutdown,
}

/// How to relaunch this binary for an upgrade.
#[derive(Debug, Clone)]
pub struct RelaunchArgs {
    pub config_file: PathBuf,
    pub daemon: bool,
}

/// Spawn `deflux --upgrade` with the same configuration, then ask pingora
/// (via `SIGQUIT`) to transfer the listening sockets to it.
pub fn graceful_upgrade(args: &RelaunchArgs) -> Result<()> {
    let exe = std::env::current_exe().context("locating the running executable")?;
    let mut command = Command::new(&exe);
    command.arg("-c").arg(&args.config_file).arg("--upgrade");
    if args.daemon {
        command.arg("-d");
    }
    let child = command
        .spawn()
        .with_context(|| format!("spawning {}", exe.display()))?;
    info!(
        pid = child.id(),
        "replacement process started; handing over listening sockets"
    );
    kill(Pid::this(), Signal::SIGQUIT).context("requesting socket handover")?;
    Ok(())
}

/// Ask pingora to stop accepting connections and exit after the grace period.
pub fn graceful_shutdown() -> Result<()> {
    info!("graceful shutdown requested");
    kill(Pid::this(), Signal::SIGTERM).context("requesting graceful shutdown")
}

/// Background service that turns `SIGUSR1` and supervisor commands into
/// process actions.
pub struct ProcessControl {
    args: RelaunchArgs,
    commands: broadcast::Sender<ProcessCommand>,
}

impl ProcessControl {
    pub fn new(args: RelaunchArgs, commands: broadcast::Sender<ProcessCommand>) -> Self {
        Self { args, commands }
    }
}

#[async_trait]
impl BackgroundService for ProcessControl {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let mut sigusr1 = match signal(SignalKind::user_defined1()) {
            Ok(stream) => stream,
            Err(e) => {
                error!(error = %e, "cannot install SIGUSR1 handler; reload via signal unavailable");
                return;
            }
        };
        let mut commands = self.commands.subscribe();

        loop {
            let command = tokio::select! {
                _ = sigusr1.recv() => {
                    info!("SIGUSR1 received");
                    ProcessCommand::Upgrade
                }
                received = commands.recv() => match received {
                    Ok(command) => command,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = shutdown.changed() => break,
            };

            let result = match command {
                ProcessCommand::Upgrade => graceful_upgrade(&self.args),
                ProcessCommand::Shutdown => graceful_shutdown(),
            };
            if let Err(e) = result {
                error!(?command, error = %e, "process command failed");
            }
        }
    }
}
