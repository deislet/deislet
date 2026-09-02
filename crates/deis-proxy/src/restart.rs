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
//! # What has to be true before the readiness byte
//!
//! `SIGQUIT` is a one-way door: pingora's main loop commits to shutting down
//! the moment it handles the signal, whether or not the socket transfer that
//! follows succeeds. So everything the replacement can fail at has to have
//! already succeeded when it writes its readiness byte, or the running
//! process is killed for nothing and the machine is left with no listener at
//! all.
//!
//! Nothing after the byte can be checked from here. `bootstrap()` in upgrade
//! mode *blocks* waiting for a transfer that only starts once the running
//! process has been signalled, and the running process is only signalled
//! because of the byte, so a check moved after `bootstrap()` would deadlock
//! rather than protect anything. Everything below therefore runs before it.
//!
//! Three things can fail after the byte and every one of them is an outage:
//!
//! - **The upgrade socket.** Pingora binds it inside `Server::bootstrap()`
//!   with `UnixAddr::new(path).unwrap()` and `bind(...).unwrap()`. A path over
//!   [`MAX_UPGRADE_SOCK_PATH_LEN`] bytes is `ENAMETOOLONG`, a path whose
//!   parent directory does not exist is `ENOENT`, and both are a panic.
//!   [`prove_upgrade_socket_bindable`] binds it first, for real.
//! - **The listening sockets.** Pingora binds these later still, in
//!   `start_service`, and only the ones it did *not* inherit: an address the
//!   running process holds arrives as a descriptor over the upgrade socket,
//!   an address added to the configuration since then has to be bound. A
//!   privileged port, an address that is not on this host, a port another
//!   process already holds - each is a `BindError` thirty seconds of retries
//!   later, and by then the running process has gone. That was a permanent
//!   outage produced by editing `listen_ports`.
//!   [`prove_listeners_bindable`] binds those addresses first, for real,
//!   which is why the running process tells the replacement which addresses
//!   it is going to hand over ([`RelaunchArgs::listeners`]).
//! - **The pid file**, in daemon mode only. `daemonize()` runs inside
//!   pingora's `run()`, which is later again than `start_service`, and
//!   creates the pid file with `daemonize.start().unwrap()`. A directory that
//!   does not exist is `OpenPidfile(2)` and a panic, with the listening
//!   sockets already taken over. [`prove_pid_file_writable`] writes a file
//!   beside it first, and `main` calls that with the rest of the daemon
//!   checks, above the byte and above `bootstrap()`.
//!
//! [`check_upgrade_socket_path`] is the same rule as the first of those,
//! minus the bind. It runs at every startup so an operator who mistyped the
//! path hears about it on the first run instead of on the day somebody
//! upgrades - as a warning there, because an unusable `upgrade_sock` costs
//! this instance nothing except the ability to be replaced, and a refusal to
//! start would cost it everything.
//!
//! # Which binary the replacement runs
//!
//! The whole point of a zero-downtime upgrade is to start a *new* build, and
//! on Linux there is no way to put one in place that leaves the running
//! file's path pointing at it. `cp` onto a running executable is refused with
//! `ETXTBSY`; `install`, `mv`, `dpkg` and `rpm` all rename a new file over
//! the old one, which unlinks the old inode and leaves `/proc/self/exe`
//! reading `<path> (deleted)`. So `std::env::current_exe()` asked at upgrade
//! time names a file that no longer exists, and spawning it fails.
//!
//! The path is therefore captured once, at startup ([`executable_path`]),
//! when it still names a file, and carried in [`RelaunchArgs::executable`].
//! Spawning *that* runs whatever binary is at that path now, which is the new
//! one.

use std::io::Write;
use std::net::{SocketAddr, ToSocketAddrs};
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
use tokio::net::TcpSocket;
use tokio::net::unix::pipe;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

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
    /// File to execute for the replacement, captured at startup by
    /// [`executable_path`] rather than looked up when the upgrade runs.
    ///
    /// Asking `/proc/self/exe` at upgrade time is what made a zero-downtime
    /// upgrade unable to ever run a new build: by then the file has been
    /// renamed out from under this process and the answer is `<path>
    /// (deleted)`. See the module documentation.
    pub executable: PathBuf,
    /// Addresses this process is listening on, spelled the way pingora keys
    /// them ([`SystemConfig::listen_addresses`]).
    ///
    /// Handed to the replacement so it can tell the two kinds of address in
    /// its own configuration apart: one of these arrives as an inherited
    /// descriptor and must not be bound, anything else has to be bound and is
    /// therefore worth proving bindable before this process commits to
    /// leaving. Without the list a port already held by a stranger and a port
    /// about to be handed over look identical - both answer `EADDRINUSE`.
    ///
    /// [`SystemConfig::listen_addresses`]: crate::config::SystemConfig::listen_addresses
    pub listeners: Vec<String>,
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
/// binding it.
///
/// The only rule about this path, in one place. Two callers read it and they
/// want different things done with the answer, which is the whole reason it
/// returns one instead of acting:
///
/// - startup, on every run, upgrade or not, so an operator who mistyped the
///   path hears about it on the first run instead of on the day somebody
///   upgrades. A **warning** there: an unusable `upgrade_sock` costs this
///   instance nothing but the ability to be replaced without an outage, and
///   `prove_upgrade_socket_bindable` has taken even that away. Refusing to
///   start would turn the length of a deployment directory into a proxy that
///   serves nothing, which is strictly worse than the thing being prevented.
/// - [`prove_upgrade_socket_bindable`], in the replacement, before the
///   readiness byte. A **refusal** there, because that is the process whose
///   exit costs nothing.
///
/// The directory checks answer for the *effective* identity, which is the
/// right one twice over: pingora binds the upgrade socket inside
/// `bootstrap()`, before `daemonize()` has switched to `user`/`group`, and a
/// replacement is spawned by a process that has already switched, so it runs
/// this check as the identity that will do its own binding.
pub fn check_upgrade_socket_path(path: &str) -> Result<()> {
    if path.len() > MAX_UPGRADE_SOCK_PATH_LEN {
        bail!(
            "upgrade_sock is {len} bytes ({path}) but a Unix socket address holds at most \
             {MAX_UPGRADE_SOCK_PATH_LEN}, so it is {over} too long. A relative path is resolved \
             against the working directory first, so the length that counts is the resolved one: \
             point upgrade_sock at a short absolute path such as /run/deis-proxy/upgrade.sock or \
             /tmp/deis_proxy_upgrade.sock. It does not have to sit beside the database and the \
             pid file",
            len = path.len(),
            over = path.len() - MAX_UPGRADE_SOCK_PATH_LEN,
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

/// Prove that pingora will be able to write the pid file.
///
/// Daemon mode only: that is the only mode in which pingora writes one at
/// all, and `daemonize()` creates it with `daemonize.start().unwrap()`. Two
/// things make that unwrap worth getting ahead of.
///
/// It runs inside `run()`, which is *after* `bootstrap()` - the same side of
/// the door as the listening sockets, and reached the same way. An operator
/// who edits `pid_file` and reloads gets a replacement that reports itself
/// ready, takes the listening sockets over, logs `Bootstrap done`, and only
/// then panics with `OpenPidfile(2)`. The process that used to serve has
/// already gone, so nothing is listening and nothing is coming back.
///
/// And on an ordinary `deis-proxy -d` the failure is silent. The daemonize
/// crate forks before it creates the file, so the panic happens in the child
/// while the parent exits `0`: the command reports success, the error log
/// stops at `Daemonizing the server`, and no port is ever bound.
///
/// A file is created and removed rather than asking `access` alone, because
/// `W_OK` on the directory is not the question - a read-only remount or a
/// full filesystem answers it yes and refuses the create anyway. The probe
/// deliberately is not the pid file itself: the running process's pid file
/// is at that path, pingora renames it aside when it writes its own, and
/// writing there now would take a live process's pid file away from it.
pub fn prove_pid_file_writable(path: &str) -> Result<()> {
    let path = Path::new(path);
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        bail!(
            "the directory holding pid_file, {}, does not exist or is not a directory. In daemon              mode pingora writes the pid file after it has taken the listening sockets over and              panics if it cannot, so this would be a daemon that serves nothing: create the              directory (a service manager usually does, e.g. systemd RuntimeDirectory=) or point              pid_file somewhere that exists",
            parent.display()
        );
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "deis-proxy.pid".to_string());
    let probe = parent.join(format!(".{name}.probe.{}", std::process::id()));
    std::fs::write(&probe, b"").with_context(|| {
        format!(
            "creating a file beside pid_file in {} to prove the pid file can be written. In              daemon mode pingora writes the pid file after it has taken the listening sockets              over and panics if it cannot, so this would be a daemon that serves nothing",
            parent.display()
        )
    })?;
    if let Err(e) = std::fs::remove_file(&probe) {
        // Not a refusal: the pid file itself is what had to be provable and
        // it is. Said out loud because a file nobody meant to create is now
        // sitting in an operator's runtime directory.
        warn!(
            path = %probe.display(),
            error = %e,
            "the file created to prove pid_file writable could not be removed again; it is empty              and can be deleted by hand"
        );
    }
    debug!(
        pid_file = %path.display(),
        "the pid file can be written, so daemonizing after the handover will not panic"
    );
    Ok(())
}

/// Suffix the kernel appends to `/proc/<pid>/exe` once the file it names has
/// been unlinked. Not a path component: it is the marker `d_path()` adds, and
/// `std::env::current_exe()` hands it back verbatim.
const DELETED_MARKER: &str = " (deleted)";

/// The file to execute for a replacement, resolved once at startup.
///
/// Must be called before anything can have replaced the binary - in practice
/// on the first lines of `main` - because the answer stops being usable the
/// moment it does. See the module documentation for why every way of
/// installing a new build unlinks the running one.
///
/// A process started from an already-unlinked file is handled rather than
/// refused: that happens when a build is replaced in the window between one
/// process spawning its replacement and the replacement reaching this line,
/// and the file at the stripped path is then exactly the binary the *next*
/// upgrade should run. The marker is only stripped when the raw path does not
/// exist and the stripped one does, so a file genuinely named `... (deleted)`
/// is left alone.
pub fn executable_path() -> Result<PathBuf> {
    let raw = std::env::current_exe().context(
        "locating the running executable; without it no replacement can be started for a \
         zero-downtime upgrade",
    )?;
    if raw.exists() {
        return Ok(raw);
    }
    let text = raw.to_string_lossy().into_owned();
    if let Some(stripped) = text.strip_suffix(DELETED_MARKER) {
        let stripped = PathBuf::from(stripped);
        if stripped.exists() {
            info!(
                path = %stripped.display(),
                "the file this process was started from has already been replaced; a \
                 zero-downtime upgrade will run whatever build is at that path now"
            );
            return Ok(stripped);
        }
    }
    // Not fatal: this process serves perfectly well. It is the next upgrade
    // that has nothing to start, and an operator can put the binary back
    // before then.
    warn!(
        path = %text,
        "the file this process was started from is gone and no file stands in its place, so a \
         zero-downtime upgrade has no binary to start and `-s reload` will fail while this \
         process keeps serving. Reinstall the binary at that path"
    );
    Ok(raw)
}

/// Prove that every address in `addresses` can be bound, before the caller
/// does anything it cannot take back.
///
/// The listening sockets are the second half of the same one-way door as the
/// upgrade socket, and the more dangerous half, because pingora binds them
/// last of all - inside `start_service`, after `bootstrap()` has already
/// blocked for the handover. A `BindError` there arrives thirty seconds
/// later (pingora retries `EADDRINUSE` thirty times a second apart) and by
/// then the process that used to hold the listeners has exited. Nothing is
/// listening, and `ExitOnPanic` then takes the replacement down too.
///
/// So each address is bound here for real and let go again, with
/// `SO_REUSEADDR` set exactly as pingora sets it - without it a port whose
/// previous listener still has connections in `TIME_WAIT` would be refused
/// here and accepted by pingora, and a busy edge would fail upgrades for no
/// reason.
///
/// The caller must have removed the addresses the running process is handing
/// over ([`RelaunchArgs::listeners`]). Those answer `EADDRINUSE` because the
/// process about to leave still holds them, which is not a failure at all,
/// and there is no way to tell that apart from a stranger holding the port by
/// looking at the error.
///
/// The same gap as [`prove_upgrade_socket_bindable`] applies: the socket is
/// closed again microseconds before pingora binds it, and pingora sets
/// `SO_REUSEADDR` too, so nothing else is invited in by the gap that was not
/// already free to take the port a moment earlier.
pub fn prove_listeners_bindable(addresses: &[String]) -> Result<()> {
    for address in addresses {
        let resolved = first_socket_addr(address)?;
        let socket = match resolved {
            SocketAddr::V4(_) => TcpSocket::new_v4(),
            SocketAddr::V6(_) => TcpSocket::new_v6(),
        }
        .with_context(|| format!("creating a socket to test {address} with"))?;
        // Pingora's `bind_tcp` sets this before every bind; the probe has to
        // ask the same question pingora will.
        socket
            .set_reuseaddr(true)
            .with_context(|| format!("setting SO_REUSEADDR while testing {address}"))?;
        socket.bind(resolved).with_context(|| {
            format!(
                "binding {address}, which this process would have to bind for itself because the \
                 process it is replacing does not hold it. The listening sockets are bound after \
                 the handover has already happened, so failing there would leave this machine \
                 with nothing listening; the running process is being kept instead"
            )
        })?;
        drop(socket);
        debug!(
            address,
            "listening address proved bindable before the handover was requested"
        );
    }
    Ok(())
}

/// The address pingora would bind for `address`, which is the first one it
/// resolves to - `bind_tcp` takes `to_socket_addrs().next()` and does not try
/// the rest.
fn first_socket_addr(address: &str) -> Result<SocketAddr> {
    address
        .to_socket_addrs()
        .with_context(|| format!("resolving the listening address {address}"))?
        .next()
        .ok_or_else(|| {
            anyhow::anyhow!("the listening address {address} resolves to no address at all")
        })
}

/// Spawn `deis-proxy --upgrade` with the same configuration, wait for it to
/// report that its startup succeeded, then ask pingora (via `SIGQUIT`) to
/// transfer the listening sockets to it. If the replacement exits first, or
/// never answers, the current process keeps serving and the two branches
/// below say so.
///
/// Those branches are the point of the handshake, and they are only worth
/// anything because of what the replacement does before it answers: it has
/// found and executed the binary that is on disk *now* (see
/// [`RelaunchArgs::executable`]), loaded its configuration, opened its
/// database and its logs, prepared automatic certificates, bound the upgrade
/// socket once to prove it can be bound
/// ([`prove_upgrade_socket_bindable`]), and bound every listening address
/// this process is not handing over ([`prove_listeners_bindable`]).
/// Everything that ordinarily fails has already failed by then, so an upgrade
/// that cannot work is a failed upgrade rather than an outage.
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
            // Give the control plane this instance's node id back first, so
            // the replacement's registration is accepted instead of refused
            // for an id whose holder the control plane still hears from. The
            // sockets are the hand-over that matters, so a channel that will
            // not close in time is noted and not waited for: the control
            // plane gives it up after its heartbeat timeout either way.
            let patience = std::time::Duration::from_secs(5);
            if crate::control::client::release_control_channel_for_handover(patience).await {
                info!(pid, "control channel released for the replacement process");
            } else {
                warn!(
                    pid,
                    "the control channel was not released within {}s; the replacement will be \
                     refused registration until the control plane gives this channel up",
                    patience.as_secs()
                );
            }
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
            warn!(
                pid,
                // `{:#}` rather than `{}`: anyhow's plain Display prints only
                // the outermost context, which is how a failed upgrade used
                // to reach the log as "spawning /path/to/deis-proxy" with the
                // reason - `No such file or directory` - dropped.
                error = format!("{e:#}"),
                "readiness of the replacement process is unknown; keeping the current process"
            );
            Err(e)
        }
    }
}

fn spawn_replacement(args: &RelaunchArgs) -> Result<(Child, pipe::Receiver)> {
    let exe = &args.executable;
    // Asked before the pipe is built so the message is about the binary and
    // not about a failure three steps further on. `Command::spawn` would say
    // "No such file or directory" and leave an operator guessing which file.
    access(exe, AccessFlags::X_OK).with_context(|| {
        format!(
            "the binary this process was started from is not executable at {}. Nothing has been \
             replaced and this process keeps serving; put the new build at that path and try the \
             upgrade again",
            exe.display()
        )
    })?;
    let (reader, writer) = std::io::pipe().context("creating the readiness pipe")?;
    // Both ends are close-on-exec; the child must inherit the write end.
    fcntl(&writer, FcntlArg::F_SETFD(FdFlag::empty()))
        .context("making the readiness pipe inheritable")?;

    let mut command = Command::new(exe);
    command
        .arg("-c")
        .arg(&args.config_file)
        .arg("--upgrade")
        .arg("--ready-fd")
        .arg(writer.as_raw_fd().to_string())
        .current_dir(&args.working_dir);
    // What this process is going to hand over. Everything else in the
    // replacement's configuration is an address it has to bind for itself,
    // and it proves it can before it reports ready; see
    // `prove_listeners_bindable`.
    for address in &args.listeners {
        command.arg("--inherited-listener").arg(address);
    }
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
        executable = %exe.display(),
        working_dir = %args.working_dir.display(),
        inherited_listeners = ?args.listeners,
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
                error!(
                    ?command,
                    error = format!("{e:#}"),
                    "process command failed; this process goes on serving, so the fleet is \
                     unchanged and whatever asked for this command has not had it done"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn an_over_long_socket_path_is_named_with_the_limit_and_the_excess() {
        let path = format!("/tmp/{}.sock", "x".repeat(MAX_UPGRADE_SOCK_PATH_LEN));
        let said = check_upgrade_socket_path(&path)
            .expect_err("a path past the limit is not usable")
            .to_string();
        assert!(said.contains("upgrade_sock"), "{said}");
        assert!(
            said.contains(&MAX_UPGRADE_SOCK_PATH_LEN.to_string()),
            "the message has to name the limit so an operator knows how much to cut: {said}"
        );
        assert!(
            said.contains(&(path.len() - MAX_UPGRADE_SOCK_PATH_LEN).to_string()),
            "and how much is over: {said}"
        );
    }

    #[test]
    fn a_path_that_fits_in_an_existing_directory_is_accepted() {
        let dir = tempfile::tempdir().expect("a directory");
        let path = dir.path().join("upgrade.sock");
        check_upgrade_socket_path(&path.display().to_string())
            .expect("a short path in a writable directory is usable");
    }

    #[test]
    fn a_listening_address_nothing_holds_is_proved_and_given_back() {
        // The probe binds for real, so it has to let go for real too: if it
        // kept the port, pingora's own bind a moment later would find it
        // taken and every upgrade would fail instead of every broken one.
        let scout = TcpListener::bind("127.0.0.1:0").expect("a port to name");
        let address = scout.local_addr().expect("its address").to_string();
        drop(scout);

        prove_listeners_bindable(std::slice::from_ref(&address))
            .expect("a free address is bindable");
        TcpListener::bind(&address).unwrap_or_else(|e| {
            panic!("{address} is still held after the probe let it go: {e}");
        });
    }

    #[test]
    fn an_address_another_process_holds_is_refused_and_named() {
        // The whole point. This is the answer that used to arrive thirty
        // seconds after the running process had already gone.
        let held = TcpListener::bind("127.0.0.1:0").expect("a port to hold");
        let address = held.local_addr().expect("its address").to_string();

        let said = prove_listeners_bindable(std::slice::from_ref(&address))
            .expect_err("an address something else is listening on is not bindable")
            .to_string();
        assert!(
            said.contains(&address),
            "the refusal has to name the address that could not be bound: {said}"
        );
        drop(held);
    }

    #[test]
    fn an_empty_list_is_the_ordinary_upgrade_and_proves_nothing() {
        // An upgrade that changes no ports hands every address over, so there
        // is nothing left for the replacement to bind. That has to be a pass
        // rather than a special case, or the common upgrade would be the one
        // this guard broke.
        prove_listeners_bindable(&[]).expect("nothing to bind is not a failure");
    }

    #[test]
    fn a_listening_address_that_resolves_to_nothing_is_refused() {
        // `listen_addr = ["::"]` spells `:::80`, which is not an address
        // anything can parse - pingora included, which would fail the same
        // way in `start_service` with the running process already gone.
        let said = prove_listeners_bindable(&[":::80".to_string()])
            .expect_err("an unparseable address is not bindable")
            .to_string();
        assert!(said.contains(":::80"), "{said}");
    }

    #[test]
    fn a_pid_file_whose_directory_is_missing_is_refused_and_the_directory_named() {
        // The failure this exists to move earlier: pingora reaches the same
        // path from inside `daemonize()`, which is after `bootstrap()` has
        // taken the listening sockets over, and answers it with
        // `OpenPidfile(2)` through an `unwrap`.
        let dir = tempfile::tempdir().expect("a directory");
        let missing = dir.path().join("no-such-dir");
        let path = missing.join("deis-proxy.pid");
        let said = prove_pid_file_writable(&path.display().to_string())
            .expect_err("a pid file in a directory that does not exist cannot be written")
            .to_string();
        assert!(
            said.contains(&missing.display().to_string()),
            "the refusal has to name the directory an operator must create: {said}"
        );
    }

    #[test]
    fn proving_the_pid_file_leaves_the_directory_as_it_found_it() {
        // Two things at once, and both matter. The probe must not be the pid
        // file: a running process's pid file lives at that path and pingora
        // renames it aside rather than reading it, so writing there would
        // take a live process's pid file away from it. And the probe must
        // clean up after itself, or every start would drop another file in
        // an operator's runtime directory.
        let dir = tempfile::tempdir().expect("a directory");
        let path = dir.path().join("deis-proxy.pid");
        std::fs::write(&path, "12345").expect("a pid file the running process owns");

        prove_pid_file_writable(&path.display().to_string())
            .expect("a writable directory can hold a pid file");

        assert_eq!(
            std::fs::read_to_string(&path).expect("the pid file is still there"),
            "12345",
            "the probe overwrote the pid file of the process that is still running"
        );
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .expect("the directory is readable")
            .map(|entry| entry.expect("an entry").file_name())
            .collect();
        assert_eq!(
            left.len(),
            1,
            "the probe left something behind in the runtime directory: {left:?}"
        );
    }

    #[test]
    fn a_pid_file_in_a_directory_nothing_may_write_is_refused() {
        // `access(W_OK)` would answer this one too; the create is what also
        // catches a directory that is writable and a filesystem that is not.
        let dir = tempfile::tempdir().expect("a directory");
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("a directory to take the permissions off");
        let mut permissions = std::fs::metadata(&locked)
            .expect("its permissions")
            .permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&locked, permissions).expect("the directory is made read-only");

        let refused = prove_pid_file_writable(&locked.join("deis-proxy.pid").display().to_string());

        // Restored before the assertion so a failure here does not leave a
        // directory the temporary-directory cleanup cannot remove.
        let mut permissions = std::fs::metadata(&locked)
            .expect("its permissions")
            .permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o700);
        }
        let _ = std::fs::set_permissions(&locked, permissions);

        let said = refused
            .expect_err("a pid file in a directory nothing may write cannot be written")
            .to_string();
        assert!(
            said.contains(&locked.display().to_string()),
            "the refusal has to name the directory: {said}"
        );
    }

    #[test]
    fn the_executable_path_is_the_file_this_process_is_running() {
        // The guarantee `RelaunchArgs::executable` rests on: at startup the
        // answer names a file that exists. It stops being true the moment
        // somebody installs over it, which is why it is asked exactly once.
        let exe = executable_path().expect("this process knows its own binary");
        assert!(
            exe.exists(),
            "{} does not exist, so a replacement spawned from it would fail",
            exe.display()
        );
        assert!(
            !exe.to_string_lossy().ends_with(DELETED_MARKER),
            "the deleted marker reached the path a replacement would be spawned from: {}",
            exe.display()
        );
    }
}
