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
//!
//! # Why the upgrade socket is checked twice
//!
//! `SIGQUIT` is a one-way door: pingora's main loop commits to shutting down
//! the moment it handles the signal, whether or not the socket transfer that
//! follows succeeds. So everything the replacement can fail at has to have
//! already succeeded when it writes its readiness byte, or the running
//! process is killed for nothing and the machine is left with no listener at
//! all.
//!
//! The last thing the replacement does, and the likeliest to fail, is bind
//! the upgrade socket - and it is pingora that binds it, inside
//! `Server::bootstrap()`, with `UnixAddr::new(path).unwrap()` and
//! `bind(...).unwrap()`. A path over [`MAX_UPGRADE_SOCK_PATH_LEN`] bytes is
//! `ENAMETOOLONG`, a path whose parent directory does not exist is `ENOENT`,
//! and both are a panic in the replacement. Neither can be caught after the
//! fact, and `bootstrap()` in upgrade mode blocks waiting for the transfer
//! that only starts once the running process has been signalled, so the
//! readiness byte cannot simply be moved after it.
//!
//! Hence two checks, in two places, for two audiences:
//!
//! - [`check_upgrade_socket_path`] runs at every startup, upgrade or not, so
//!   an operator who mistyped the path is told on the first run rather than
//!   on the day somebody upgrades.
//! - [`prove_upgrade_socket_bindable`] runs in the replacement, immediately
//!   before the readiness byte, and actually binds the path. It is the only
//!   evidence that the bind pingora is about to attempt will work.

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::sys::signal::{Signal, kill};
use nix::unistd::{AccessFlags, Pid, access};
use pingora::server::ShutdownWatch;
use pingora::services::background::BackgroundService;
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;
use tracing::{error, info, warn};

use crate::config::system::MAX_UPGRADE_SOCK_PATH_LEN;

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

/// Report readiness only once the upgrade socket has been proved bindable.
///
/// This is the whole of the ordering guarantee described in the module
/// documentation, and it is a real bind rather than a guess: the path is
/// unlinked (pingora's `get_fds_from` does the same before it binds), a
/// `SOCK_STREAM` Unix listener is bound on it, and both the listener and the
/// file are then removed again. What that rules out is every reason
/// `bootstrap()` panics a moment later - `ENAMETOOLONG`, `ENOENT` for a
/// missing parent directory, `ENOTDIR`, `EACCES`, `EADDRINUSE` behind a file
/// that cannot be unlinked - while this process is still just a process and
/// an error is still only an exit.
///
/// Removing the socket again leaves a gap between here and pingora's own
/// bind. It is not a race: the running process is only signalled after the
/// readiness byte this function precedes, and its `send_fds_to` retries
/// `ENOENT` five times a second apart, which is far longer than the rest of
/// this process's startup.
///
/// Only ever call this in a process that has not yet announced readiness. In
/// a process that has - or in one started with `--upgrade` by hand, where the
/// operator may have sent `SIGQUIT` already - the unlink would take away the
/// socket the running process is mid-handover on.
pub fn prove_upgrade_socket_bindable(path: &str) -> Result<()> {
    check_upgrade_socket_path(path)?;
    // Mirror pingora: it unlinks whatever is at the path before binding, so
    // a leftover file here is not evidence of anything and removing it
    // changes nothing about what pingora will do.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).with_context(|| {
        format!(
            "binding the upgrade socket {path}; the replacement cannot take over the listening \
             sockets, so the running process is being kept"
        )
    })?;
    drop(listener);
    // Best effort: pingora unlinks the path itself before binding, so a file
    // left here is harmless. Failing the upgrade over it would be worse than
    // the leftover.
    if let Err(e) = std::fs::remove_file(path) {
        warn!(
            path,
            error = %e,
            "could not remove the upgrade socket this process just bound to prove it could; \
             pingora unlinks the path itself before binding, so the upgrade still works and what \
             is left behind is a stale socket file"
        );
    }
    Ok(())
}

/// Everything about an `upgrade_sock` path that can be judged without
/// binding it, checked at every startup so an operator hears about it on the
/// first run instead of on the day somebody upgrades.
///
/// The length is refused by [`SystemConfig`]'s own validation too, so a
/// configuration naming one never gets this far; it is repeated here because
/// this function is what [`prove_upgrade_socket_bindable`] leans on and the
/// two must not be able to disagree about the limit.
///
/// The directory checks answer for the *effective* identity, which is the
/// right one twice over: pingora binds the upgrade socket inside
/// `bootstrap()`, before `daemonize()` has switched to `user`/`group`, and a
/// replacement is spawned by a process that has already switched, so it runs
/// this check as the identity that will do its own binding.
///
/// [`SystemConfig`]: crate::config::SystemConfig
pub fn check_upgrade_socket_path(path: &str) -> Result<()> {
    if path.len() > MAX_UPGRADE_SOCK_PATH_LEN {
        bail!(
            "upgrade_sock is {} bytes ({path}) but a Unix socket address holds at most \
             {MAX_UPGRADE_SOCK_PATH_LEN}",
            path.len()
        );
    }
    let Some(parent) = Path::new(path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    else {
        // A bare file name resolves against the working directory, which
        // always exists; `resolve_paths` normally makes this unreachable.
        return Ok(());
    };
    if !parent.is_dir() {
        bail!(
            "the directory holding upgrade_sock, {}, does not exist or is not a directory; \
             the socket cannot be bound there",
            parent.display()
        );
    }
    // Binding creates an entry in that directory and pingora unlinks the old
    // one first, so both bits are needed. `access` answers for the effective
    // identity, which is the one that will do the binding.
    access(parent, AccessFlags::W_OK | AccessFlags::X_OK).with_context(|| {
        format!(
            "the directory holding upgrade_sock, {}, cannot be written to by this process; \
             the socket cannot be bound there",
            parent.display()
        )
    })?;
    Ok(())
}

/// Spawn `deis-proxy --upgrade` with the same configuration, wait for it to
/// report that its startup succeeded, then ask pingora (via `SIGQUIT`) to
/// transfer the listening sockets to it. If the replacement exits first, or
/// never answers, the current process keeps serving and the two branches
/// below say so.
///
/// Those branches are the point of the handshake, and they are only worth
/// anything because of what the replacement does before it answers: it has
/// loaded its configuration, opened its database and its logs, prepared
/// automatic certificates, and bound the upgrade socket once to prove it can
/// be bound (see [`prove_upgrade_socket_bindable`]). Everything that
/// ordinarily fails has already failed by then, so an upgrade that cannot
/// work is a failed upgrade rather than an outage.
///
/// What is left is genuinely pingora's: its main loop commits to shutting
/// down as soon as it handles `SIGQUIT`, whether or not the transfer that
/// follows succeeds. A replacement killed in the window between its readiness
/// byte and pingora's own bind still takes the service down. That window is
/// microseconds of this process's own startup, and pingora's `send_fds_to`
/// retries a refused or missing socket five times a second apart, which
/// covers the handover itself.
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
