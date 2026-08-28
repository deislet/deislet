//! Process control: zero-downtime upgrade and graceful shutdown.
//!
//! Both actions are carried out by pingora's server through Unix signals:
//!
//! - `SIGQUIT` makes the running server hand its listening sockets to a
//!   freshly started `deis-proxy --upgrade` process over the upgrade socket and
//!   exit once in-flight requests complete. The replacement must already be
//!   waiting on that socket, so it is spawned first, and the signal is only
//!   sent once it has reported that its startup succeeded (see
//!   [`graceful_upgrade`]).
//! - `SIGTERM` stops accepting connections and exits after the grace period.
//!
//! Requests arrive from `SIGUSR1` (`deis-proxy -s reload`) and from the
//! control client.

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::process::{Child, Command};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;
use tracing::{error, info, warn};

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
    /// Absolute path of the configuration file.
    pub config_file: PathBuf,
    pub daemon: bool,
    /// Directory this process was started from, captured before pingora's
    /// daemon mode changes it to `/`. The replacement runs from the same
    /// directory so every relative path (database, logs, certificate files
    /// named by the control) resolves exactly as it did for this process,
    /// and each replacement captures its own launch directory in turn.
    pub working_dir: PathBuf,
}

/// Byte the replacement writes once its startup has succeeded.
const READY: u8 = b'1';

/// Report readiness to the process that spawned this one; `fd` is the write
/// end of the pipe it passed with `--ready-fd`. The descriptor is closed
/// afterwards, so an exit before this point is seen by the parent as EOF.
pub fn announce_ready(fd: RawFd) -> Result<()> {
    // SAFETY: the descriptor was inherited from the parent for exactly this
    // purpose and nothing else in this process refers to it.
    let mut pipe = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    pipe.write_all(&[READY])
        .with_context(|| format!("writing to ready fd {fd}"))
}

/// Spawn `deis-proxy --upgrade` with the same configuration, wait for it to
/// report that its startup succeeded, then ask pingora (via `SIGQUIT`) to
/// transfer the listening sockets to it. If the replacement exits first the
/// current process keeps serving.
///
/// The handshake only shrinks the window. Pingora's main loop commits to
/// shutting down as soon as it handles `SIGQUIT`, whether or not the socket
/// transfer succeeds, so a replacement that dies after it has bound the
/// upgrade socket still takes the service down; deis-proxy only decides when the
/// signal is sent. The replacement announces readiness right before it binds
/// that socket, and pingora's transfer retries for about five seconds, which
/// covers the gap.
pub async fn graceful_upgrade(args: &RelaunchArgs) -> Result<()> {
    let (mut child, ready) = spawn_replacement(args)?;
    let pid = child.id();
    match await_ready(ready).await {
        Ok(true) => {
            info!(
                pid,
                "replacement process is ready; handing over listening sockets"
            );
            // In daemon mode the child forks and exits; it stays a zombie
            // until this process exits at the end of the grace period.
            kill(Pid::this(), Signal::SIGQUIT).context("requesting socket handover")
        }
        Ok(false) => {
            let status = tokio::task::spawn_blocking(move || child.wait())
                .await
                .context("joining the wait for the replacement process")?
                .with_context(|| format!("waiting for replacement process {pid}"))?;
            bail!(
                "replacement process {pid} exited with {status} before it was ready; keeping the current process"
            );
        }
        Err(e) => {
            warn!(pid, error = %e, "readiness of the replacement process is unknown; keeping the current process");
            Err(e)
        }
    }
}

fn spawn_replacement(args: &RelaunchArgs) -> Result<(Child, pipe::Receiver)> {
    let exe = std::env::current_exe().context("locating the running executable")?;
    let (reader, writer) = std::io::pipe().context("creating the readiness pipe")?;
    // Both ends are close-on-exec; the child must inherit the write end.
    fcntl(&writer, FcntlArg::F_SETFD(FdFlag::empty()))
        .context("making the readiness pipe inheritable")?;

    let mut command = Command::new(&exe);
    command
        .arg("-c")
        .arg(&args.config_file)
        .arg("--upgrade")
        .arg("--ready-fd")
        .arg(writer.as_raw_fd().to_string())
        .current_dir(&args.working_dir);
    if args.daemon {
        command.arg("-d");
    }
    let child = command
        .spawn()
        .with_context(|| format!("spawning {}", exe.display()))?;
    // Only the child holds a write end now, so EOF means it exited.
    drop(writer);
    info!(
        pid = child.id(),
        working_dir = %args.working_dir.display(),
        "replacement process started; waiting for it to become ready"
    );

    let ready = pipe::Receiver::from_owned_fd(OwnedFd::from(reader))
        .context("registering the readiness pipe with the runtime")?;
    Ok((child, ready))
}

/// `Ok(true)` once the replacement wrote its readiness byte, `Ok(false)` when
/// the pipe hit EOF first, meaning the replacement exited.
async fn await_ready(mut ready: pipe::Receiver) -> Result<bool> {
    let mut byte = [0u8; 1];
    let read = ready
        .read(&mut byte)
        .await
        .context("reading the readiness pipe")?;
    Ok(read == 1)
}

/// Ask pingora to stop accepting connections and exit after the grace period.
pub fn graceful_shutdown() -> Result<()> {
    info!("graceful shutdown requested");
    kill(Pid::this(), Signal::SIGTERM).context("requesting graceful shutdown")
}

/// Background service that turns `SIGUSR1` and control commands into
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
                ProcessCommand::Upgrade => tokio::select! {
                    result = graceful_upgrade(&self.args) => result,
                    _ = shutdown.changed() => break,
                },
                ProcessCommand::Shutdown => graceful_shutdown(),
            };
            if let Err(e) = result {
                error!(?command, error = %e, "process command failed");
            }
        }
    }
}
