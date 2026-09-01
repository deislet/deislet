//! The one-way door in a zero-downtime upgrade.
//!
//! `SIGQUIT` makes pingora commit to shutting down whether or not the socket
//! transfer that follows works, and the running process only sends it once the
//! replacement has said it is ready. So everything the replacement can fail at
//! has to have already succeeded by the time it says so - and the last thing
//! it does, binding the upgrade socket, is both the likeliest to fail and a
//! `.unwrap()` inside pingora.
//!
//! These tests drive the real binary through both halves: a path that can
//! never be bound is refused before the process serves at all, and a path that
//! stops being bindable while the process runs makes the upgrade fail rather
//! than making the machine lose its listener.

mod harness;

use std::process::Command;
use std::time::{Duration, Instant};

use harness::{Backend, Proxy, ProxyOptions};

/// How long a test waits for one of the two outcomes it is watching for.
/// Generous: pingora's own transfer retries run to five seconds and a debug
/// build starting a second process is not fast.
const OUTCOME_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a startup that is supposed to be refused gets to refuse itself.
/// A process still running after this has accepted the configuration.
const STARTUP_VERDICT: Duration = Duration::from_secs(20);

/// Run the proxy binary on a configuration written for the occasion, and say
/// whether it came up and what it printed.
///
/// Bounded on purpose: a proxy that accepts the configuration serves for ever,
/// so waiting for it to exit is waiting for something that will not happen.
/// Staying up past [`STARTUP_VERDICT`] counts as "it started", which is the
/// answer these tests are asking for.
async fn start_and_collect(config: &str, dir: &std::path::Path) -> (bool, String) {
    let config_file = dir.join("deis-proxy.toml");
    std::fs::write(&config_file, config).expect("the configuration is written");
    let mut child = Command::new(Proxy::binary_path())
        .arg("-c")
        .arg(&config_file)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the proxy binary runs");

    let deadline = Instant::now() + STARTUP_VERDICT;
    let mut exit = None;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit = Some(status);
                break;
            }
            Ok(None) => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(e) => panic!("waiting for the proxy: {e}"),
        }
    }
    let started = match exit {
        Some(status) => status.success(),
        None => {
            let _ = child.kill();
            true
        }
    };
    let output = child.wait_with_output().expect("the proxy's output");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (started, said)
}

fn config_with_upgrade_sock(dir: &std::path::Path, sock: &str, port: u16) -> String {
    format!(
        "listen_addr = [\"127.0.0.1\"]\n\
         listen_ports = [{port}]\n\
         database = \"{db}\"\n\
         pid_file = \"{pid}\"\n\
         upgrade_sock = \"{sock}\"\n",
        db = dir.join("deis-proxy.db").display(),
        pid = dir.join("deis-proxy.pid").display(),
    )
}

#[tokio::test]
async fn an_upgrade_socket_longer_than_an_address_is_refused_at_startup() {
    // A Unix address holds 107 bytes of path. `resolve_paths` makes a
    // relative path absolute before anything measures it, so the length that
    // matters is the resolved one and a configuration file cannot be read to
    // find out whether it is over.
    let dir = tempfile::tempdir().expect("a directory for the configuration");
    let mut sock = dir.path().join("x".repeat(120));
    sock.set_extension("sock");
    let (started, said) = start_and_collect(
        &config_with_upgrade_sock(dir.path(), &sock.display().to_string(), Proxy::pick_port()),
        dir.path(),
    )
    .await;

    assert!(
        !started,
        "the proxy started on an upgrade_sock no Unix address can hold. It would serve, and the \
         next upgrade would be a full outage: the replacement panics on ENAMETOOLONG after this \
         process has already been told to hand its listeners over. It said:\n{said}"
    );
    assert!(
        said.contains("upgrade_sock") && said.contains("107"),
        "the refusal does not name the field and the limit, so an operator cannot act on it. It \
         said:\n{said}"
    );
}

#[tokio::test]
async fn an_upgrade_socket_with_no_directory_is_refused_at_startup() {
    // The second way the same panic is reached, and the nastier one: the path
    // is short, the process starts perfectly, and nothing looks wrong until
    // somebody upgrades.
    let dir = tempfile::tempdir().expect("a directory for the configuration");
    let sock = dir.path().join("no-such-directory").join("upgrade.sock");
    let (started, said) = start_and_collect(
        &config_with_upgrade_sock(dir.path(), &sock.display().to_string(), Proxy::pick_port()),
        dir.path(),
    )
    .await;

    assert!(
        !started,
        "the proxy started on an upgrade_sock whose directory does not exist. It said:\n{said}"
    );
    assert!(
        said.contains("upgrade_sock"),
        "the refusal does not name the field, so an operator cannot act on it. It said:\n{said}"
    );
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

#[tokio::test]
async fn an_upgrade_whose_socket_can_be_bound_still_hands_the_listeners_over() {
    // The other side of the same change: proving the socket can be bound
    // means binding it and letting it go again, microseconds before pingora
    // binds it for real. If that probe took the path away from the handover,
    // every upgrade would fail instead of every broken one, which would be a
    // worse bug than the one it fixes.
    let backend = Backend::start().await;
    let mut proxy = Proxy::launch(ProxyOptions {
        upstream: backend.addr,
        ..Default::default()
    })
    .await;
    let addr = proxy.addr;

    Command::new("kill")
        .arg("-USR1")
        .arg(proxy.pid().to_string())
        .status()
        .expect("SIGUSR1 is delivered");

    let handed_over = "replacement process is ready; handing over listening sockets";
    let failed = "keeping the current process";
    let deadline = Instant::now() + OUTCOME_TIMEOUT;
    let mut log = String::new();
    while Instant::now() < deadline {
        log = proxy.log_text();
        if log.contains(handed_over) || log.contains(failed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        log.contains(handed_over),
        "the upgrade did not reach the handover. Its log said:\n{}",
        proxy.log_tail()
    );

    // The replacement now owns the listener; the log names its pid, and it
    // outlives this test's `Proxy` so the test has to stop it itself.
    let replacement = log
        .split("replacement process started")
        .nth(1)
        .and_then(|rest| rest.split("pid=").nth(1))
        .and_then(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|digits| digits.parse::<u32>().ok())
        })
        .unwrap_or_else(|| panic!("the log does not name the replacement's pid:\n{log}"));

    // Serving across the handover is the property being asserted: the old
    // process keeps accepting for pingora's five-second close timeout while
    // the replacement takes the socket over, so no request in between is
    // refused.
    let deadline = Instant::now() + OUTCOME_TIMEOUT;
    let mut answers = 0;
    while Instant::now() < deadline && proxy.is_running() {
        let answer = harness::request_to(addr, "/across-the-handover", "example.test").await;
        assert!(
            answer.starts_with("HTTP/1.1 200 OK"),
            "a request was refused while the listeners were being handed over; the proxy \
             answered:\n{answer}\nits log said:\n{}",
            proxy.log_tail()
        );
        answers += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        answers > 0,
        "no request was made across the handover, so nothing was observed"
    );

    let answer = harness::request_to(addr, "/after-the-handover", "example.test").await;
    assert!(
        answer.starts_with("HTTP/1.1 200 OK"),
        "the replacement is not serving on the listener it was handed; it answered:\n{answer}\n\
         the old process's log said:\n{}",
        proxy.log_tail()
    );

    let _ = Command::new("kill").arg(replacement.to_string()).status();
    drop(proxy);
    backend.stop();
}
