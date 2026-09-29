//! `deis dev` - run the project locally in the real runtime.
//!
//! What happens here is what happens on a deployment, with the control plane
//! taken out of the middle:
//!
//! 1. `deis build --target deislet` produces the bundle.
//! 2. A local HTTP server holds it, the way the control plane holds a version.
//! 3. A real `deis-runtime` is started and told to load it with the same
//!    `ApplyDeployment` call the control plane makes.
//! 4. Requests to `--port` are forwarded to that node with the two headers
//!    deis-proxy would have stamped on.
//!
//! So the routing, the asset index, the isolate, `context.params` and
//! `context.env` are the production ones, not a local imitation of them.
//! The parts that are genuinely absent are named in the banner rather than
//! faked: without `--store` and `--queue` there is no KV, database, blob or
//! queue, and the bindings that need them fail exactly as they would on a node
//! that was never given their addresses.

pub mod node;
pub mod pki;
pub mod proxy;
pub mod watcher;

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use colored::Colorize;

use crate::commands::build::{self, Bundle};

/// How long changes are collected before a rebuild starts.
///
/// Saving one file in an editor produces several events, and a formatter
/// running on save produces several more. Without this the first of them
/// would start a build of a half-written tree.
const COALESCE: Duration = Duration::from_millis(200);

pub struct Options {
    /// Port the developer's requests arrive on.
    pub port: u16,
    /// Port the node listens on. Chosen by the operating system when unset.
    pub runtime_port: Option<u16>,
    /// Which deis-runtime binary to start.
    pub runtime: Option<PathBuf>,
    pub environment: String,
    pub watch: bool,
    pub debug: bool,
    pub minify: bool,
    /// deis-store address, if one is running locally.
    pub store: Option<String>,
    /// deis-queue address, if one is running locally.
    pub queue: Option<String>,
    /// Restricted file containing local `NAME=VALUE` secret values.
    pub secrets_file: Option<PathBuf>,
    /// Read the same document from stdin, keeping plaintext out of argv and
    /// the process environment.
    pub secrets_stdin: bool,
}

pub async fn run(options: Options) -> Result<()> {
    let dir = crate::commands::project_dir()?;
    crate::settings::check_name("environment", &options.environment)?;

    // Located before anything is built: "there is no deis-runtime here" is
    // worth saying before a minute of compilation, not after.
    let binary = node::locate_runtime(options.runtime.as_deref())?;

    // Bound before anything is built or started, so a port clash is reported
    // at once rather than after a compile, and so the address the banner
    // names is already this process's: announcing a URL and binding it a
    // moment later means the first request a developer makes can be refused.
    let front_listener =
        proxy::bind_front_door(SocketAddr::from(([127, 0, 0, 1], options.port))).await?;

    println!("\n{}", "Starting the local runtime".bold());
    println!("  runtime  {}", binary.display().to_string().dimmed());

    let bundle = build::deislet_bundle(&dir, options.minify).await?;
    let app = bundle.manifest.app.clone();
    crate::settings::check_name("application", &app)?;

    let secrets =
        load_secret_values(options.secrets_file.as_deref(), options.secrets_stdin).await?;
    let env = deployment_env(&options);
    let config_values = crate::commands::deploy::deployment_config_values(&dir, &bundle.manifest)?;
    report_gaps(&bundle, &secrets, &options);

    let bundles = proxy::BundleServer::start(bundle.bytes.clone()).await?;
    let runtime_port = match options.runtime_port {
        Some(port) => port,
        None => free_port()?,
    };

    let (mut node, mut node_exited) =
        node::DevNode::start(&binary, runtime_port, options.debug).await?;
    node.apply(node::DevDeployment {
        app: &app,
        environment: &options.environment,
        bundle_url: &bundles.url,
        bundle_sha: &bundle.digest,
        env,
        config_values,
        local_secrets: &secrets,
        manifest: &bundle.manifest,
    })
    .await?;

    let addr = front_listener
        .local_addr()
        .context("reading the address the dev server is listening on")?;
    let mut front = tokio::spawn(proxy::serve_front_door(
        front_listener,
        runtime_port,
        app.clone(),
        options.environment.clone(),
        node.proxy().clone(),
    ));

    print_banner(&app, &options, &bundle, addr, runtime_port);

    let mut changes = match options.watch {
        true => Some(watcher::watch(&dir, &source_roots(&dir))?),
        false => {
            println!("  {}", "not watching for changes (--no-watch)".dimmed());
            None
        }
    };

    loop {
        // `changes` is optional, so its branch needs a future that simply
        // never resolves when nothing is being watched.
        let next_change = async {
            match changes.as_mut() {
                Some(changes) => changes.rx.recv().await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            biased;

            signal = tokio::signal::ctrl_c() => {
                signal.context("waiting for Ctrl-C")?;
                println!("\n  {}", "stopping".dimmed());
                break;
            }

            _ = &mut node_exited => {
                let status = node.exit_status();
                node.shutdown().await;
                bail!(
                    "the deis-runtime this dev server was serving from has stopped{}. Its output \
                     above says why; nothing is serving {app} any more.",
                    match status {
                        Some(status) => format!(" ({status})"),
                        None => String::new(),
                    }
                );
            }

            result = &mut front => {
                node.shutdown().await;
                return match result {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(e),
                    Err(e) => Err(anyhow::anyhow!("the dev server task ended unexpectedly: {e}")),
                };
            }

            change = next_change => {
                if change.is_none() {
                    // The watcher is gone; carry on serving what is loaded
                    // rather than exiting on a developer who is still using
                    // the server.
                    changes = None;
                    println!(
                        "  {}",
                        "the file watcher stopped; changes will not be picked up until this is \
                         restarted"
                            .yellow()
                    );
                    continue;
                }
                if let Some(changes) = changes.as_mut() {
                    coalesce(changes).await;
                }
                reload(&mut node, &bundles, &dir, &options, &app, &secrets).await;
            }
        }
    }

    node.shutdown().await;
    Ok(())
}

/// Rebuild and hand the result to the node.
///
/// A failure here is printed and swallowed on purpose: a dev server that
/// exits on the first syntax error is a dev server that gets restarted all
/// day. The previously loaded bundle keeps serving until a build succeeds.
async fn reload(
    node: &mut node::DevNode,
    bundles: &proxy::BundleServer,
    dir: &std::path::Path,
    options: &Options,
    app: &str,
    secrets: &BTreeMap<String, String>,
) {
    println!("\n  {}", "change detected, rebuilding".bold());

    let bundle = match build::deislet_bundle(dir, options.minify).await {
        Ok(bundle) => bundle,
        Err(e) => {
            println!("  {} {e}", "✗".red());
            println!("  {}", "the previous build is still being served".dimmed());
            return;
        }
    };

    if bundle.manifest.app != app {
        // The node keys a loaded application by (app, environment), so
        // applying this would leave the old one loaded and serving beside a
        // new one nothing forwards to.
        println!(
            "  {} .config.json now names the application {:?}, but this server was started for \
             {app:?}. Restart `deis dev` to serve the renamed application.",
            "✗".red(),
            bundle.manifest.app
        );
        return;
    }

    let env = deployment_env(options);
    let config_values =
        match crate::commands::deploy::deployment_config_values(dir, &bundle.manifest) {
            Ok(values) => values,
            Err(error) => {
                println!("  {} {error}", "✗".red());
                println!("  {}", "the previous build is still being served".dimmed());
                return;
            }
        };
    bundles.replace(bundle.bytes.clone()).await;

    match node
        .apply(node::DevDeployment {
            app,
            environment: &options.environment,
            bundle_url: &bundles.url,
            bundle_sha: &bundle.digest,
            env,
            config_values,
            local_secrets: secrets,
            manifest: &bundle.manifest,
        })
        .await
    {
        Ok(()) => println!(
            "  {} reloaded {} route(s) and {} asset(s), sha256 {}",
            "✓".green(),
            bundle.manifest.routes.len(),
            bundle.manifest.assets.len(),
            crate::render::short_digest(&bundle.digest)
        ),
        Err(e) => {
            println!("  {} {e}", "✗".red());
            println!("  {}", "the previous build is still being served".dimmed());
        }
    }
}

/// Swallow the rest of one editor save.
async fn coalesce(changes: &mut watcher::Changes) {
    while tokio::time::timeout(COALESCE, changes.rx.recv())
        .await
        .is_ok_and(|change| change.is_some())
    {}
}

/// The platform's wiring the dev node is sent beside the snapshot, as the
/// control plane sends it: the storage addresses. The application's config
/// values and secrets reach the isolate through the snapshot the dev node
/// builds from the project file and the local secrets.
fn deployment_env(options: &Options) -> HashMap<String, String> {
    let mut env = HashMap::new();
    if let Some(store) = &options.store {
        env.insert(
            deis_wire::conventions::STORE_ENDPOINT_ENV.to_string(),
            store.clone(),
        );
    }
    if let Some(queue) = &options.queue {
        env.insert(
            deis_wire::conventions::QUEUE_ENDPOINT_ENV.to_string(),
            queue.clone(),
        );
    }
    env
}

/// Say plainly which parts of the platform are not present, and what the
/// symptom will be. A dev server that stays quiet about this turns into a
/// bug report about `env.KV` being broken.
fn report_gaps(bundle: &Bundle, secrets: &BTreeMap<String, String>, options: &Options) {
    let store_bindings: Vec<&str> = bundle
        .manifest
        .bindings
        .iter()
        .filter(|(_, binding)| binding.kind.needs_store())
        .map(|(name, _)| name)
        .collect();
    let wants_queue = !bundle.manifest.queues.is_empty()
        || bundle
            .manifest
            .bindings
            .iter()
            .any(|(_, binding)| binding.kind == deis_bundle::BindingKind::Queue);

    if !store_bindings.is_empty() && options.store.is_none() {
        println!(
            "  {} this app declares the storage binding(s) {} but no --store address was given, \
             so they are not on env. Start deis-store and pass --store http://127.0.0.1:8090.",
            "!".yellow(),
            store_bindings.join(", ")
        );
    }
    if wants_queue && options.queue.is_none() {
        println!(
            "  {} this app uses a queue but no --queue address was given, so every send will \
             fail and nothing will be consumed. Start deis-queue and pass --queue \
             http://127.0.0.1:8091.",
            "!".yellow()
        );
    }

    let declared_secrets = bundle
        .manifest
        .environment
        .declarations
        .iter()
        .filter(|declaration| declaration.kind == deis_wire::EnvironmentKind::Secret)
        .map(|declaration| declaration.name.as_str());
    for name in declared_secrets {
        if !secrets.contains_key(name) {
            println!(
                "  {} secret {name} has no local value, so context.env.{name} will be missing. \
                 pass a restricted NAME=VALUE document with --secrets-file, or pipe it to \
                 --secrets-stdin.",
                "!".yellow()
            );
        }
    }

    if !bundle.manifest.cron.is_empty() {
        println!(
            "  {} {} cron expression(s) are declared; nothing triggers them locally, because \
             scheduling is the control plane's job. Call the handler directly to test it.",
            "!".yellow(),
            bundle.manifest.cron.len()
        );
    }
}

fn print_banner(
    app: &str,
    options: &Options,
    bundle: &Bundle,
    addr: SocketAddr,
    runtime_port: u16,
) {
    println!("\n{}", "Serving".bold());
    println!("  {}  http://{addr}", "→".green());
    println!(
        "  application  {} {} {}",
        app.cyan(),
        "environment".dimmed(),
        options.environment.cyan()
    );
    println!(
        "  loaded       {} route(s), {} asset(s), sha256 {}",
        bundle.manifest.routes.len(),
        bundle.manifest.assets.len(),
        crate::render::short_digest(&bundle.digest)
    );
    println!(
        "  node         {}",
        format!("http://127.0.0.1:{runtime_port} (real deis-runtime)").dimmed()
    );

    if !bundle.manifest.routes.is_empty() {
        println!("\n{}", "Routes".bold());
        for route in &bundle.manifest.routes {
            let methods = match route.methods.is_empty() {
                true => "ANY".to_string(),
                false => route.methods.join(","),
            };
            println!("  {:<8} {}", methods.dimmed(), route.pattern);
        }
    }
    println!();
}

/// Directories whose contents end up in the bundle.
fn source_roots(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(config) = deis_build::load_project_config(&dir.to_string_lossy()) {
        roots.push(dir.join(&config.function_root));
        if let Some(assets) = config.assets.as_ref().map(|a| a.dir.clone()) {
            roots.push(dir.join(assets));
        }
    }
    if roots.is_empty() {
        // Better to watch the obvious place than to watch nothing: without
        // this a project whose configuration failed to re-read would stop
        // reloading with no explanation.
        roots.push(dir.join("functions"));
    }
    roots
}

/// A port nothing is listening on.
///
/// Asked of the operating system and then released, so there is a moment in
/// which something else could take it. The node reports that as a bind
/// failure on startup and `deis dev` passes it on, which is a better trade
/// than making the developer choose a port by hand every time.
fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .context("asking the operating system for a free port for the dev node")?;
    Ok(listener
        .local_addr()
        .context("reading the port the operating system gave out")?
        .port())
}

const MAX_LOCAL_SECRET_DOCUMENT_BYTES: usize =
    deis_wire::ENVIRONMENT_BINDING_COUNT_LIMIT * (deis_wire::ENVIRONMENT_VALUE_BYTES_LIMIT + 80);

/// Read local secret plaintext through a channel that does not expose it in
/// argv or the process environment. File mode validation is intentionally
/// fail-closed where Rust's standard library can prove it; stdin is the
/// portable path on systems where it cannot inspect an ACL.
async fn load_secret_values(
    path: Option<&std::path::Path>,
    from_stdin: bool,
) -> Result<BTreeMap<String, String>> {
    let bytes = if let Some(path) = path {
        read_secret_file(path).await?
    } else if from_stdin {
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        tokio::io::stdin()
            .take((MAX_LOCAL_SECRET_DOCUMENT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .context("reading local secrets from stdin")?;
        bytes
    } else {
        return Ok(BTreeMap::new());
    };
    if bytes.len() > MAX_LOCAL_SECRET_DOCUMENT_BYTES {
        bail!("local secret document exceeds the supported size limit");
    }
    let document = std::str::from_utf8(&bytes).context("local secret document must be UTF-8")?;
    parse_secret_document(document)
}

/// Read `--secrets-file` on a platform whose access control this can prove.
///
/// Every check runs before the plaintext is opened, because a secret staged in
/// a file anyone can read has already leaked by the time this process gets to
/// it: a symlink could point the read somewhere the developer did not intend,
/// a mode with any group or other bit set means the value is readable by
/// another account on the machine, and an oversized file is rejected from its
/// metadata so that a mistargeted flag costs a `stat` instead of the memory to
/// hold whatever it was pointed at. None of these messages repeat the file's
/// contents, so a failure cannot become the leak it is guarding against.
#[cfg(unix)]
async fn read_secret_file(path: &std::path::Path) -> Result<Vec<u8>> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading local secret file metadata at {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "local secret path {} must be a regular, non-symlink file",
            path.display()
        );
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!(
            "local secret file {} is readable or writable by group/others; run chmod 600 on it",
            path.display()
        );
    }
    if metadata.len() > MAX_LOCAL_SECRET_DOCUMENT_BYTES as u64 {
        bail!("local secret document exceeds the supported size limit");
    }
    tokio::fs::read(path)
        .await
        .with_context(|| format!("reading local secret file {}", path.display()))
}

/// Refuse `--secrets-file` on a platform whose access control this cannot yet
/// inspect.
///
/// Windows expresses "only the owner may read this" as an ACL rather than as
/// the permission bits the standard library exposes, so there is no check here
/// that could stand in for the Unix one. Running the rest of the reads anyway
/// would be worse than refusing: it would accept a file that any other account
/// on the machine can read while looking like it had been vetted. The whole
/// body is therefore the refusal, and it names the flag that does work, so the
/// developer is not left guessing how to pass a secret on this platform.
/// Because the refusal is the entire function rather than a branch inside the
/// shared one, the checks above stay compiled only where they can be enforced
/// and nothing downstream of them becomes dead code here.
#[cfg(not(unix))]
async fn read_secret_file(_path: &std::path::Path) -> Result<Vec<u8>> {
    bail!(
        "--secrets-file cannot verify this platform's ACL yet; use --secrets-stdin so secret plaintext stays on a protected channel"
    )
}

fn parse_secret_document(document: &str) -> Result<BTreeMap<String, String>> {
    let mut secrets = BTreeMap::new();
    for (index, raw_line) in document.lines().enumerate() {
        let line = raw_line.trim_end_matches('\r');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            bail!("local secret line {} is not a NAME=VALUE pair", index + 1);
        };
        let name = name.trim();
        if name.is_empty() {
            bail!("local secret line {} has no name before the '='", index + 1);
        }
        deis_wire::EnvironmentDeclaration {
            name: name.to_string(),
            kind: deis_wire::EnvironmentKind::Secret,
            value_type: deis_wire::EnvironmentValueType::String,
            required: true,
        }
        .validate()
        .map_err(anyhow::Error::new)?;
        deis_wire::validate_secret_value(value).map_err(anyhow::Error::new)?;
        if secrets
            .insert(name.to_string(), value.to_string())
            .is_some()
        {
            bail!("local secret document contains duplicate name {name}");
        }
    }
    Ok(secrets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_document_is_read_as_name_value_pairs() {
        let secrets =
            parse_secret_document("# local only\nAPI_KEY=abc\nURL=http://x/?a=b\n").unwrap();
        assert_eq!(secrets["API_KEY"], "abc");
        // Only the first "=" separates; a value may contain more.
        assert_eq!(secrets["URL"], "http://x/?a=b");
    }

    #[test]
    fn malformed_secret_lines_do_not_echo_their_contents() {
        let error = parse_secret_document("API_KEY-with-sensitive-suffix")
            .unwrap_err()
            .to_string();
        assert!(error.contains("NAME=VALUE"), "{error}");
        assert!(!error.contains("sensitive-suffix"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn secret_file_requires_owner_only_regular_file() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempfile::tempdir().unwrap();
        let secret_path = directory.path().join("secrets.env");
        std::fs::write(&secret_path, "API_KEY=local-only\n").unwrap();

        std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = load_secret_values(Some(&secret_path), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("chmod 600"), "{error}");
        assert!(!error.contains("local-only"), "{error}");

        std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let secrets = load_secret_values(Some(&secret_path), false).await.unwrap();
        assert_eq!(secrets["API_KEY"], "local-only");

        let symlink_path = directory.path().join("secrets-link.env");
        symlink(&secret_path, &symlink_path).unwrap();
        let error = load_secret_values(Some(&symlink_path), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("non-symlink"), "{error}");
        assert!(!error.contains("local-only"), "{error}");
    }
}
