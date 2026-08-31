//! Starting a real deis-runtime and handing it a bundle.
//!
//! This is the whole point of `deis dev`: the process a developer tests
//! against is the same binary that serves production, loading the same
//! bundle through the same `ApplyDeployment` call a control plane makes. A
//! second, simplified implementation of routing, assets and the isolate
//! would only be able to agree with the real one by accident, and the
//! disagreements would be found in production.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use colored::Colorize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tonic::transport::{Channel, Endpoint};

/// Generated client for `deis.management`, the same management plane the
/// control plane calls.
pub mod proto {
    tonic::include_proto!("deis.management");
}

use proto::runtime_management_client::RuntimeManagementClient;
use proto::{ApplyDeploymentRequest, HealthCheckRequest};

/// Environment variable naming the deis-runtime binary to start.
pub const ENV_RUNTIME_BIN: &str = "DEIS_RUNTIME_BIN";

/// How long the node is given to come up before it is called broken.
///
/// Generous because the first start of a debug build has to map V8 and
/// compile the system application.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// Gap between connection attempts while waiting for the node to listen.
///
/// There is no event to wait on here - a process that has not bound its
/// port yet notifies nobody - so the connection attempt itself is the wait
/// and this only keeps the retry from becoming a busy loop.
const CONNECT_RETRY_GAP: Duration = Duration::from_millis(100);

/// A deis-runtime started by, and owned by, this `deis dev`.
pub struct DevNode {
    child: Child,
    client: RuntimeManagementClient<Channel>,
    /// Deleted on shutdown: a dev node's SQLite files and unpacked bundles
    /// are worth nothing once the process is gone, and leaving them behind
    /// would make the next `deis dev` start from someone else's state.
    data_dir: PathBuf,
}

impl DevNode {
    /// Start a node on `port` and wait until its management plane answers.
    ///
    /// The second half of the answer fires when the node's process has gone
    /// away. It is handed back separately rather than kept on [`DevNode`] so
    /// that a caller can wait on it in the same `select!` that also reloads
    /// the node, which two borrows of one value could not do.
    pub async fn start(
        binary: &Path,
        port: u16,
        debug: bool,
    ) -> Result<(Self, oneshot::Receiver<()>)> {
        let data_dir = std::env::temp_dir().join(format!("deis-dev-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).with_context(|| {
            format!(
                "creating the dev node's data directory {}",
                data_dir.display()
            )
        })?;

        let mut command = Command::new(binary);
        command
            .env("DEIS_RUNTIME_NODE_ID", "deis-dev")
            .env("DEIS_RUNTIME_HTTP_PORT", port.to_string())
            .env("DEIS_RUNTIME_DATA_DIR", &data_dir)
            // Nothing is collecting logs locally, so the node keeps them in
            // its own output where a developer can see them. Shipping them
            // to an address that is not there would only cost retries.
            .env("DEIS_RUNTIME_LOGGING_ENABLED", "false")
            // A config file left over from running the node by hand would
            // otherwise silently point this one at a real control plane.
            .env_remove("DEIS_RUNTIME_CONFIG")
            .env_remove("DEIS_RUNTIME_CONTROL_ENDPOINT")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A `deis dev` that is killed must not leave a node holding the
            // port and serving a stale bundle.
            .kill_on_drop(true);

        if std::env::var_os("RUST_LOG").is_none() {
            command.env(
                "RUST_LOG",
                if debug {
                    "deis_runtime=debug,warn"
                } else {
                    "deis_runtime=info,warn"
                },
            );
        }

        let mut child = command.spawn().with_context(|| {
            format!(
                "starting the deis-runtime binary {}. It is what actually runs the application; \
                 without it `deis dev` has nothing to serve from.",
                binary.display()
            )
        })?;

        let exited = forward_output(&mut child, debug);

        let client = connect(port, &mut child, binary).await?;

        Ok((
            Self {
                child,
                client,
                data_dir,
            },
            exited,
        ))
    }

    /// Load a bundle, the same call and the same arguments the control plane
    /// makes when it deploys a version.
    pub async fn apply(
        &mut self,
        app: &str,
        environment: &str,
        bundle_url: &str,
        bundle_sha: &str,
        env: HashMap<String, String>,
    ) -> Result<()> {
        let response = self
            .client
            .apply_deployment(ApplyDeploymentRequest {
                app_id: app.to_string(),
                // A dev node holds one build at a time, and the digest is
                // what actually distinguishes two of them.
                version_id: format!("dev-{bundle_sha}"),
                environment: environment.to_string(),
                bundle_url: bundle_url.to_string(),
                bundle_sha: bundle_sha.to_string(),
                env,
                // No per-application ceilings locally: in production the
                // control plane fills these in from the manifest it stored
                // with the version, and a dev node has no catalog to read
                // one out of. It therefore runs on its own defaults, which
                // are the same defaults an application with no declared
                // limits gets in production.
                limits: None,
            })
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "the dev node refused the bundle: {}\nIts own output above says why.",
                    e.message()
                )
            })?
            .into_inner();

        if !response.success {
            bail!(
                "the dev node could not load the bundle: {}\nThis is the same failure a real \
                 deployment would hit, so it is worth fixing before deploying.",
                response.message
            );
        }
        Ok(())
    }

    /// How the node ended, once it has. `None` while it is still running.
    pub fn exit_status(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// Stop the node and remove its scratch state.
    pub async fn shutdown(mut self) {
        // SIGKILL rather than a graceful drain: nothing here needs to be
        // finished, and a dev server that takes seconds to answer Ctrl-C is
        // one people learn to kill twice.
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        if let Err(e) = std::fs::remove_dir_all(&self.data_dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "{} could not remove the dev node's data directory {}: {e}",
                "warning:".yellow(),
                self.data_dir.display()
            );
        }
    }
}

/// Print whatever the node says, so an application's `console.log` and the
/// node's own errors land in the terminal the developer is looking at.
///
/// Filtered by level unless `debug`, because deis-runtime installs no
/// `EnvFilter`: it emits every span and event its dependencies produce, down
/// to one TRACE line per TCP connection, and `RUST_LOG` does not narrow it.
/// Unfiltered, a single request buries the application's own output in
/// several hundred lines of HTTP/2 internals.
///
/// The returned receiver fires when both streams have reached end of file,
/// which is the moment the process is gone. That is the signal `deis dev`
/// waits on: a node that died has to be reported, not left behind a front
/// door that answers 502 to everything.
fn forward_output(child: &mut Child, debug: bool) -> oneshot::Receiver<()> {
    let stdout = child.stdout.take().map(|stream| {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                print_node_line(&line, debug);
            }
        })
    });
    let stderr = child.stderr.take().map(|stream| {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                print_node_line(&line, debug);
            }
        })
    });

    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        if let Some(handle) = stdout {
            let _ = handle.await;
        }
        if let Some(handle) = stderr {
            let _ = handle.await;
        }
        let _ = tx.send(());
    });
    rx
}

/// Print one line from the node, unless it is below the level a developer
/// asked for.
fn print_node_line(line: &str, debug: bool) {
    if !debug && matches!(level_of(line), Some("TRACE" | "DEBUG")) {
        return;
    }
    println!("{} {line}", "node │".dimmed());
}

/// The severity of one of the node's log lines, if it is one.
///
/// deis-runtime writes `<timestamp> <LEVEL> <thread> <message>`, coloured, so
/// the escape sequences are dropped and the first token that names a level is
/// taken. A line that names none - the raw stdout copy of a `console.log`, or
/// a panic - has no level and is always printed.
fn level_of(line: &str) -> Option<&'static str> {
    const LEVELS: [&str; 5] = ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"];
    for token in strip_ansi(line).split_whitespace().take(3) {
        if let Some(level) = LEVELS.iter().find(|level| **level == token) {
            return Some(level);
        }
    }
    None
}

/// Drop ANSI escape sequences, so a coloured level still reads as a word.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // CSI sequences end at the first byte in @-~; anything else is a
        // short escape whose single next character is consumed.
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

/// Wait for the node's management plane to answer.
async fn connect(
    port: u16,
    child: &mut Child,
    binary: &Path,
) -> Result<RuntimeManagementClient<Channel>> {
    let url = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + START_TIMEOUT;

    loop {
        // Checked first: a node that has already exited will never answer,
        // and reporting that now beats waiting out the whole deadline.
        if let Ok(Some(status)) = child.try_wait() {
            bail!(
                "the deis-runtime at {} exited with {status} before it was ready. Its output \
                 above says why; a port already in use is the usual cause, so try `deis dev \
                 --runtime-port <other>`.",
                binary.display()
            );
        }

        if let Ok(endpoint) = Endpoint::from_shared(url.clone())
            && let Ok(channel) = endpoint
                .connect_timeout(Duration::from_secs(2))
                .connect()
                .await
        {
            let mut client = RuntimeManagementClient::new(channel)
                .max_decoding_message_size(deis_wire::conventions::MAX_GRPC_MESSAGE_BYTES)
                .max_encoding_message_size(deis_wire::conventions::MAX_GRPC_MESSAGE_BYTES);
            if client.health_check(HealthCheckRequest {}).await.is_ok() {
                return Ok(client);
            }
        }

        if Instant::now() >= deadline {
            let _ = child.start_kill();
            bail!(
                "the deis-runtime at {} did not answer on port {port} within {}s. Its output \
                 above says how far it got.",
                binary.display(),
                START_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(CONNECT_RETRY_GAP).await;
    }
}

/// Find the deis-runtime binary to start.
///
/// Four places, in this order, because each of them is where somebody
/// actually has it: the flag, the variable, next to this `deis` (which is
/// where `cargo build` and a packaged install both put it), and `PATH`.
pub fn locate_runtime(explicit: Option<&Path>) -> Result<PathBuf> {
    let name = if cfg!(windows) {
        "deis-runtime.exe"
    } else {
        "deis-runtime"
    };

    if let Some(path) = explicit {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        bail!(
            "--runtime {} is not a file. It has to be the deis-runtime binary that will actually \
             run the application.",
            path.display()
        );
    }

    if let Some(value) = std::env::var_os(ENV_RUNTIME_BIN) {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Ok(path);
        }
        bail!(
            "{ENV_RUNTIME_BIN} points at {}, which is not a file.",
            path.display()
        );
    }

    if let Ok(current) = std::env::current_exe()
        && let Some(dir) = current.parent()
    {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    bail!(
        "could not find the {name} binary, and `deis dev` runs the application inside it rather \
         than simulating it.\nBuild it with `cargo build -p deis-runtime`, put it next to this \
         `deis`, or name it with --runtime <path> or ${ENV_RUNTIME_BIN}."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The node's own lines are coloured, so the level has to survive the
    /// escape sequences or the filter would keep every one of them.
    #[test]
    fn the_level_of_a_coloured_line_is_still_readable() {
        let line = "\u{1b}[2m2026-08-29T07:52:17.200511Z\u{1b}[0m \u{1b}[34mDEBUG\u{1b}[0m ThreadId(01) something";
        assert_eq!(level_of(line), Some("DEBUG"));

        let line = "\u{1b}[2m2026-08-29T07:52:17.200511Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m ThreadId(01) ready";
        assert_eq!(level_of(line), Some("INFO"));
    }

    /// An application's own stdout carries no level. Dropping those lines
    /// would hide exactly what a developer started `deis dev` to see.
    #[test]
    fn a_line_with_no_level_is_never_filtered_out() {
        assert_eq!(level_of("hello from the app"), None);
        assert_eq!(level_of("thread 'main' panicked at src/main.rs:1"), None);
    }

    #[test]
    fn an_explicit_runtime_that_is_not_there_says_so_instead_of_falling_back() {
        let error = locate_runtime(Some(Path::new("/nonexistent/deis-runtime")))
            .unwrap_err()
            .to_string();
        assert!(error.contains("/nonexistent/deis-runtime"), "{error}");
        assert!(
            error.contains("has to be the deis-runtime binary"),
            "{error}"
        );
    }
}
