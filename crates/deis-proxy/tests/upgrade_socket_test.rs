//! The one-way door in a zero-downtime upgrade.
//!
//! `SIGQUIT` makes pingora commit to shutting down whether or not the socket
//! transfer that follows works, and the running process only sends it once the
//! replacement has said it is ready. So everything the replacement can fail at
//! has to have already succeeded by the time it says so - the upgrade socket,
//! which pingora binds inside `bootstrap()` with an `unwrap()`, and the
//! listening sockets, which it binds later still and which are what an edited
//! `listen_ports` goes through.
//!
//! These tests drive the real binary through all of it: an upgrade that runs
//! the build that is on disk now rather than the inode this process was
//! started from, a path that stops being bindable while the process runs, a
//! port that cannot be bound, and a path no Unix address can hold - which
//! costs an instance its upgrades and, deliberately, not its ability to serve.
//!
//! The pid file is here for the same reason, one step further out: pingora
//! writes it inside `daemonize()`, which is later again than the listening
//! sockets, so in daemon mode it is the last thing that can turn a handover
//! that has already happened into a machine with nothing listening.

mod harness;

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use harness::{Backend, Proxy, ProxyOptions};

/// How long a test waits for one of the two outcomes it is watching for.
/// Generous: pingora's own transfer retries run to five seconds and a debug
/// build starting a second process is not fast.
const OUTCOME_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a proxy gets to come up before a test gives up on it.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

// ----------------------------------------------------------------------
// A proxy this file can rewrite the configuration of and replace the binary
// of, which is what an upgrade actually happens to.
// ----------------------------------------------------------------------

/// Copies of the binary and spawns of it never overlap across this binary's
/// test threads.
///
/// Without this the suite failed now and then with `ExecutableFileBusy` on
/// `spawn`, only when run beside the rest of the workspace. The mechanism is
/// fork's: while one thread is inside `fs::copy` with a destination open for
/// writing, another thread forks to exec *its own* copy, and the child holds
/// every open descriptor of the process - the first thread's write handle
/// included - until its exec closes them. A third thread whose exec lands in
/// that window finds its file "open for writing" by a process that is about
/// to exec something else, and the kernel answers ETXTBSY. Serialising copy
/// and spawn removes the overlap; nothing else in the test changes.
static COPY_OR_SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A running `deis-proxy` started from a *copy* of the binary.
///
/// The copy is the point of the whole struct: an upgrade is only an upgrade
/// if it can start a build that was not there when this process started, and
/// the only way to put one in place on Linux unlinks the running file. A test
/// that ran the shared `target/debug/deis-proxy` could not replace it without
/// breaking every other test in the suite.
struct Instance {
    dir: tempfile::TempDir,
    binary: PathBuf,
    config: PathBuf,
    log: PathBuf,
    child: Child,
    addr: SocketAddr,
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Instance {
    /// Start a proxy listening on `ports` in front of `upstream`.
    async fn start(upstream: SocketAddr, ports: &[u16]) -> Instance {
        let dir = tempfile::tempdir().expect("a directory for the proxy");
        let binary = dir.path().join("deis-proxy");
        let config = dir.path().join("deis-proxy.toml");
        let log = dir.path().join("deis-proxy-error.log");
        // Copying and spawning under one lock: see `COPY_OR_SPAWN`.
        let serial = COPY_OR_SPAWN
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::fs::copy(Proxy::binary_path(), &binary).expect("a private copy of the proxy binary");
        let mut instance = Instance {
            addr: SocketAddr::from(([127, 0, 0, 1], ports[0])),
            binary,
            config,
            log,
            child: Command::new("true").spawn().expect("a placeholder child"),
            dir,
        };
        instance.write_config(upstream, ports);
        instance.child = instance.spawn();
        drop(serial);
        instance.wait_until_answering().await;
        instance
    }

    fn write_config(&self, upstream: SocketAddr, ports: &[u16]) {
        let ports = ports
            .iter()
            .map(|port| port.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            &self.config,
            format!(
                "listen_addr = [\"127.0.0.1\"]\n\
                 listen_ports = [{ports}]\n\
                 database = \"{db}\"\n\
                 pid_file = \"{pid}\"\n\
                 upgrade_sock = \"{sock}\"\n\
                 \n\
                 [[upstreams]]\n\
                 host = \"{host}\"\n\
                 port = {upstream_port}\n\
                 \n\
                 [log.error_log]\n\
                 level = \"info\"\n\
                 path = \"{log}\"\n",
                db = self.dir.path().join("deis-proxy.db").display(),
                pid = self.dir.path().join("deis-proxy.pid").display(),
                sock = self.dir.path().join("upgrade.sock").display(),
                host = upstream.ip(),
                upstream_port = upstream.port(),
                log = self.log.display(),
            ),
        )
        .expect("the proxy configuration is written");
    }

    fn spawn(&self) -> Child {
        Command::new(&self.binary)
            .arg("-c")
            .arg(&self.config)
            .current_dir(self.dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the proxy binary runs")
    }

    /// Replace the binary the way `install`, `mv`, `dpkg` and `rpm` all do
    /// it: a rename over the running file, which unlinks the inode the
    /// process is executing and leaves `/proc/<pid>/exe` reading `(deleted)`.
    ///
    /// `cp` is not an alternative - Linux answers `ETXTBSY` for a write to a
    /// file that is being executed - so this is not one way of several, it is
    /// the only shape a real upgrade ever has.
    fn install_new_build(&self) -> u64 {
        let _serial = COPY_OR_SPAWN
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let staged = self.dir.path().join("deis-proxy.new");
        std::fs::copy(Proxy::binary_path(), &staged).expect("a new build is staged");
        std::fs::rename(&staged, &self.binary).expect("the new build is installed");
        inode(&self.binary)
    }

    fn reload(&self) {
        Command::new("kill")
            .arg("-USR1")
            .arg(self.child.id().to_string())
            .status()
            .expect("SIGUSR1 is delivered");
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Wait until the log says one of `needles`, and return what it said.
    async fn wait_for_log(&self, needles: &[&str]) -> String {
        let deadline = Instant::now() + OUTCOME_TIMEOUT;
        while Instant::now() < deadline {
            let log = self.log_text();
            if needles.iter().any(|needle| log.contains(needle)) {
                return log;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "none of {needles:?} appeared within {OUTCOME_TIMEOUT:?}. The log said:\n{}",
            self.log_text()
        );
    }

    async fn wait_until_answering(&mut self) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "deis-proxy exited during startup with {status}; its log said:\n{}",
                    self.log_text()
                );
            }
            if answers(self.addr).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "deis-proxy never answered on {} within {STARTUP_TIMEOUT:?}; its log said:\n{}",
            self.addr,
            self.log_text()
        );
    }
}

/// Whether a complete HTTP response comes back from `addr`.
///
/// Asynchronous, and it has to be: the backend these tests proxy to runs as a
/// task on this same current-thread runtime, so a blocking read here would
/// stop it answering. The proxy's own health probe would then time out, evict
/// the backend, and the test would watch a `503` it had caused itself.
async fn answers(addr: SocketAddr) -> bool {
    harness::request_to(addr, "/", &addr.to_string())
        .await
        .starts_with("HTTP/1.1 ")
}

fn inode(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
        .ino()
}

/// The inode of the file a process is executing, which is what says *which
/// build* it is running rather than which path it was started from.
fn running_inode(pid: u32) -> u64 {
    let exe = PathBuf::from(format!("/proc/{pid}/exe"));
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(&exe)
        .unwrap_or_else(|e| panic!("reading {}: {e}", exe.display()))
        .ino()
}

/// The replacement's pid, out of the line the running process logged.
fn replacement_pid(log: &str) -> u32 {
    log.split("replacement process started")
        .nth(1)
        .and_then(|rest| rest.split("pid=").nth(1))
        .and_then(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|digits| digits.parse::<u32>().ok())
        })
        .unwrap_or_else(|| panic!("the log does not name the replacement's pid:\n{log}"))
}

/// The replacement a successful upgrade leaves running, reaped when the test
/// that produced it goes out of scope.
///
/// A `kill` at the end of the test body is not enough, and this was found by
/// leaving three of them behind: an assertion that fails jumps straight past
/// it, and the orphan then outlives the whole `cargo test` run. It cannot be
/// cleaned up afterwards by the usual means either - a replacement that
/// reported ready and never received the sockets is blocked inside pingora's
/// `bootstrap()`, which is not its signal loop, so it catches `SIGTERM` and
/// never acts on it. `SIGKILL` is the only thing that reliably ends one,
/// which is why that is what this sends.
struct Replacement {
    pid: u32,
}

impl Replacement {
    /// The pid out of the line the running process logged when it spawned it.
    fn named_in(log: &str) -> Replacement {
        Replacement {
            pid: replacement_pid(log),
        }
    }
}

impl Drop for Replacement {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .arg("-9")
            .arg(self.pid.to_string())
            .status();
    }
}

// ----------------------------------------------------------------------
// Which binary an upgrade runs.
// ----------------------------------------------------------------------

#[tokio::test]
async fn an_upgrade_runs_the_build_that_is_on_disk_now() {
    // The one thing a zero-downtime upgrade exists to do. It could not do it:
    // the replacement was spawned from `std::env::current_exe()` asked at
    // upgrade time, which on Linux is the path of the *running inode* - and
    // installing a new build unlinks that inode, so the path came back as
    // `... (deleted)` and the spawn failed. Every upgrade re-executed the
    // build that was already running, or, once a build had been installed,
    // nothing at all.
    let backend = Backend::start().await;
    let mut proxy = Instance::start(backend.addr, &[Proxy::pick_port()]).await;
    let running = running_inode(proxy.child.id());

    let installed = proxy.install_new_build();
    assert_ne!(
        installed, running,
        "the fixture did not actually replace the binary, so this test proves nothing"
    );

    proxy.reload();
    let log = proxy
        .wait_for_log(&[
            "handing over listening sockets",
            "keeping the current process",
            "process command failed",
        ])
        .await;
    assert!(
        log.contains("handing over listening sockets"),
        "the upgrade never reached the handover after a new build was installed. Its log \
         said:\n{log}"
    );

    let replacement = Replacement::named_in(&log);
    assert_eq!(
        running_inode(replacement.pid),
        installed,
        "the replacement is running the same file as the process that started it, so an upgrade \
         can never pick up a new build. Its log said:\n{log}"
    );

    // And the service survived it, which is the other half of the promise.
    let deadline = Instant::now() + OUTCOME_TIMEOUT;
    while Instant::now() < deadline && proxy.is_running() {
        assert!(
            answers(proxy.addr).await,
            "a request was refused while the listeners were being handed over; the log \
             said:\n{}",
            proxy.log_text()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        answers(proxy.addr).await,
        "the replacement is not serving on the listener it was handed; the old process's log \
         said:\n{}",
        proxy.log_text()
    );

    drop(replacement);
    drop(proxy);
    backend.stop();
}

// ----------------------------------------------------------------------
// The listening sockets, which are bound after the door has already shut.
// ----------------------------------------------------------------------

#[tokio::test]
async fn an_upgrade_that_cannot_bind_an_added_port_leaves_the_running_proxy_serving() {
    // Adding a port to `listen_ports` and reloading used to be a permanent
    // outage. The replacement proved it could bind the *upgrade* socket,
    // reported ready, and the running process handed over and left - and only
    // then did pingora try to bind the new port, thirty seconds of retries
    // later, in a process that then exited. Nothing was listening and nothing
    // was coming back.
    let backend = Backend::start().await;
    let port = Proxy::pick_port();
    let mut proxy = Instance::start(backend.addr, &[port]).await;

    // Something else holds the port the operator is about to add. Held for
    // the whole test, so the replacement cannot have it at any point.
    let taken = StdTcpListener::bind("127.0.0.1:0").expect("a port to squat on");
    let taken_port = taken.local_addr().expect("its address").port();
    proxy.write_config(backend.addr, &[port, taken_port]);

    proxy.reload();
    let log = proxy
        .wait_for_log(&["keeping the current process", "SIGQUIT received"])
        .await;
    assert!(
        !log.contains("SIGQUIT received"),
        "the running process committed to shutting down for a replacement that cannot bind \
         {taken_port}. Every listener on this machine goes with it. Its log said:\n{log}"
    );
    assert!(
        log.contains(&taken_port.to_string()),
        "the failure does not name the port that could not be bound, so an operator cannot act \
         on it. Its log said:\n{log}"
    );
    assert!(
        proxy.is_running(),
        "the proxy process is gone after an upgrade that could not have worked. Its log \
         said:\n{}",
        proxy.log_text()
    );
    assert!(
        answers(proxy.addr).await,
        "the proxy stopped serving on the port it already had. Its log said:\n{}",
        proxy.log_text()
    );

    drop(taken);
    drop(proxy);
    backend.stop();
}

#[tokio::test]
async fn an_upgrade_that_adds_a_port_nothing_holds_still_hands_the_listeners_over() {
    // The other side of the same change. Proving the new addresses bindable
    // means binding them and letting them go again microseconds before
    // pingora binds them for real; if that probe held on to the port, or if
    // the *inherited* port were probed too, every upgrade would fail instead
    // of every broken one.
    let backend = Backend::start().await;
    let port = Proxy::pick_port();
    let proxy = Instance::start(backend.addr, &[port]).await;

    let added = Proxy::pick_port();
    proxy.write_config(backend.addr, &[port, added]);
    proxy.reload();

    let log = proxy
        .wait_for_log(&[
            "handing over listening sockets",
            "keeping the current process",
        ])
        .await;
    assert!(
        log.contains("handing over listening sockets"),
        "an upgrade that only adds a bindable port did not reach the handover. Its log \
         said:\n{log}"
    );
    let _replacement = Replacement::named_in(&log);

    // The added port is the replacement's own bind, so it only exists once
    // the replacement is serving.
    let added_addr = SocketAddr::from(([127, 0, 0, 1], added));
    let deadline = Instant::now() + OUTCOME_TIMEOUT;
    while Instant::now() < deadline && !answers(added_addr).await {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        answers(added_addr).await,
        "the replacement never bound the port that was added, so the upgrade silently did not do \
         what it was asked. Its log said:\n{}",
        proxy.log_text()
    );

    drop(proxy);
    backend.stop();
}

// ----------------------------------------------------------------------
// The upgrade socket itself.
// ----------------------------------------------------------------------

fn config_with_upgrade_sock(dir: &Path, sock: &str, port: u16, log: &Path) -> String {
    format!(
        "listen_addr = [\"127.0.0.1\"]\n\
         listen_ports = [{port}]\n\
         database = \"{db}\"\n\
         pid_file = \"{pid}\"\n\
         upgrade_sock = \"{sock}\"\n\
         \n\
         [log.error_log]\n\
         level = \"info\"\n\
         path = \"{log}\"\n",
        db = dir.join("deis-proxy.db").display(),
        pid = dir.join("deis-proxy.pid").display(),
        log = log.display(),
    )
}

/// Start the proxy on `config` and wait until its log says something.
///
/// Returns the process, so the caller can assert on whether it is still
/// running, and the log.
async fn start_and_watch(config: &str, dir: &Path, needles: &[&str]) -> (Child, String) {
    let config_file = dir.join("deis-proxy.toml");
    let log = dir.join("deis-proxy-error.log");
    std::fs::write(&config_file, config).expect("the configuration is written");
    let mut child = Command::new(Proxy::binary_path())
        .arg("-c")
        .arg(&config_file)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the proxy binary runs");

    let deadline = Instant::now() + OUTCOME_TIMEOUT;
    loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if needles.iter().any(|needle| text.contains(needle)) {
            return (child, text);
        }
        if let Ok(Some(_)) = child.try_wait() {
            return (child, text);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("none of {needles:?} appeared within {OUTCOME_TIMEOUT:?}. It said:\n{text}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn an_upgrade_socket_longer_than_an_address_does_not_stop_the_proxy_serving() {
    // A Unix address holds 107 bytes of path, and `resolve_paths` makes a
    // relative path absolute before anything measures it, so a deployment
    // directory deep enough is on its own enough to go over.
    //
    // That used to refuse the configuration, which made the length of a
    // directory name a reason for the edge not to exist - this repo's own
    // demo produced a proxy that exited. It is a warning now, because the
    // thing it was guarding against is gone: the replacement binds this path
    // before it reports ready, so an unusable one costs a failed upgrade and
    // never an outage.
    let dir = tempfile::tempdir().expect("a directory for the configuration");
    let log = dir.path().join("deis-proxy-error.log");
    let mut sock = dir.path().join("x".repeat(120));
    sock.set_extension("sock");
    let (mut child, said) = start_and_watch(
        &config_with_upgrade_sock(
            dir.path(),
            &sock.display().to_string(),
            Proxy::pick_port(),
            &log,
        ),
        dir.path(),
        &["cannot be replaced without an outage"],
    )
    .await;

    assert!(
        matches!(child.try_wait(), Ok(None)),
        "the proxy refused to serve because of how long its upgrade_sock path is. It said:\n{said}"
    );
    assert!(
        said.contains("upgrade_sock") && said.contains("107"),
        "the warning does not name the field and the limit, so an operator cannot act on it. It \
         said:\n{said}"
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test]
async fn an_upgrade_socket_with_no_directory_does_not_stop_the_proxy_serving() {
    // The second way the same bind fails, and it gets the same answer for the
    // same reason: a missing directory stops the next upgrade, not this
    // process's ability to serve the requests in front of it.
    let dir = tempfile::tempdir().expect("a directory for the configuration");
    let log = dir.path().join("deis-proxy-error.log");
    let sock = dir.path().join("no-such-directory").join("upgrade.sock");
    let (mut child, said) = start_and_watch(
        &config_with_upgrade_sock(
            dir.path(),
            &sock.display().to_string(),
            Proxy::pick_port(),
            &log,
        ),
        dir.path(),
        &["cannot be replaced without an outage"],
    )
    .await;

    assert!(
        matches!(child.try_wait(), Ok(None)),
        "the proxy refused to serve because its upgrade_sock directory is missing. It \
         said:\n{said}"
    );
    assert!(
        said.contains("upgrade_sock"),
        "the warning does not name the field, so an operator cannot act on it. It said:\n{said}"
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test]
async fn an_upgrade_that_cannot_bind_its_socket_leaves_the_running_proxy_serving() {
    let backend = Backend::start().await;
    // The upgrade socket lives in a directory of its own, so the test can
    // take it away without disturbing the database or the log.
    let dir = std::sync::Arc::new(tempfile::tempdir().expect("a directory for the proxy"));
    let sock_dir = dir.path().join("run");
    std::fs::create_dir(&sock_dir).expect("the upgrade socket's directory");
    let mut proxy = Proxy::launch(ProxyOptions {
        upstream: backend.addr,
        dir: Some(std::sync::Arc::clone(&dir)),
        upgrade_sock: Some(sock_dir.join("upgrade.sock")),
        ..Default::default()
    })
    .await;

    // The directory goes away while the proxy runs. Nothing the proxy did: a
    // tmpfiles.d rule, a container remount, an operator tidying up.
    std::fs::remove_dir_all(&sock_dir).expect("the upgrade socket's directory is removed");

    let pid = proxy.pid();
    Command::new("kill")
        .arg("-USR1")
        .arg(pid.to_string())
        .status()
        .expect("SIGUSR1 is delivered");

    // Two outcomes are possible and they are opposites, so wait for whichever
    // arrives rather than for a duration: either this process decided to keep
    // serving, or it handed itself SIGQUIT for a replacement that was never
    // going to work.
    let kept = "keeping the current process";
    let committed = "SIGQUIT received";
    let deadline = Instant::now() + OUTCOME_TIMEOUT;
    let mut log = String::new();
    while Instant::now() < deadline {
        log = proxy.log_text();
        if log.contains(kept) || log.contains(committed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(
        !log.contains(committed),
        "the running process committed to shutting down for a replacement that cannot bind the \
         upgrade socket. Every listener on this machine goes with it and no replacement takes \
         them over. Its log said:\n{}",
        proxy.log_tail()
    );
    assert!(
        log.contains(kept),
        "the upgrade neither failed nor succeeded within {OUTCOME_TIMEOUT:?}. Its log said:\n{}",
        proxy.log_tail()
    );
    assert!(
        proxy.is_running(),
        "the proxy process is gone after a failed upgrade. Its log said:\n{}",
        proxy.log_tail()
    );

    let answer = proxy.request("/still-here", "example.test").await;
    assert!(
        answer.starts_with("HTTP/1.1 200 OK"),
        "the proxy stopped serving after an upgrade that could not have worked; it answered:\n\
         {answer}\nits log said:\n{}",
        proxy.log_tail()
    );

    drop(proxy);
    backend.stop();
}

// ----------------------------------------------------------------------
// The pid file, which pingora writes later still and only as a daemon.
// ----------------------------------------------------------------------

#[tokio::test]
async fn a_daemon_that_cannot_write_its_pid_file_refuses_to_start_and_names_the_directory() {
    // `daemonize()` runs inside pingora's `run()`, after `bootstrap()` has
    // taken the listening sockets over, and creates the pid file with
    // `daemonize.start().unwrap()`. A directory that does not exist is
    // `OpenPidfile(2)`.
    //
    // On an upgrade that is the same permanent outage an unbindable port used
    // to be, reached one step later. On an ordinary start it is worse than an
    // outage, because it is silent: the daemonize crate forks before it
    // creates the file, so the panic happens in the child while the parent
    // exits 0. `deis-proxy -d` reported success, the error log stopped at
    // "Daemonizing the server", and no port was ever bound.
    //
    // Refusing in the foreground is what makes both cases safe: a replacement
    // runs this check long before its readiness byte, so the running process
    // is never asked to leave for it.
    let dir = tempfile::tempdir().expect("a directory for the configuration");
    let missing = dir.path().join("no-such-runtime-dir");
    let config_file = dir.path().join("deis-proxy.toml");
    let port = Proxy::pick_port();
    std::fs::write(
        &config_file,
        format!(
            "listen_addr = [\"127.0.0.1\"]\n\
             listen_ports = [{port}]\n\
             daemon = true\n\
             database = \"{db}\"\n\
             pid_file = \"{pid}\"\n\
             upgrade_sock = \"{sock}\"\n\
             \n\
             [log.error_log]\n\
             level = \"info\"\n\
             path = \"{log}\"\n",
            db = dir.path().join("deis-proxy.db").display(),
            pid = missing.join("deis-proxy.pid").display(),
            sock = dir.path().join("upgrade.sock").display(),
            log = dir.path().join("deis-proxy-error.log").display(),
        ),
    )
    .expect("the configuration is written");

    let finished = Command::new(Proxy::binary_path())
        .arg("-c")
        .arg(&config_file)
        .output()
        .expect("the proxy binary runs");
    let said = String::from_utf8_lossy(&finished.stderr).into_owned();

    assert!(
        !finished.status.success(),
        "`deis-proxy -d` reported success with a pid file it cannot write. Nothing ends up \
         listening on {port} and the exit code says otherwise, which is the one combination an \
         init system cannot recover from. It said:\n{said}"
    );
    assert!(
        said.contains(&missing.display().to_string()),
        "the refusal does not name the directory that has to be created, so an operator cannot \
         act on it. It said:\n{said}"
    );
}
