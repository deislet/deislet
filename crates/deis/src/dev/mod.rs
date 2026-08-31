//! `deis dev` - run the project locally in the real runtime.
//!
//! What happens here is what happens on a deployment, with the control plane
//! taken out of the middle:
//!
//! 1. `deis build --vendor deislet` produces the bundle.
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
//! queue, and `ctx.services.*` fails exactly as it would on a node that was
//! never given their addresses.

pub mod node;
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
    /// `NAME=VALUE` pairs standing in for the secrets the control plane
    /// would have decrypted and sent.
    pub secrets: Vec<String>,
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

    let secrets = parse_secrets(&options.secrets)?;
    let env = deployment_env(&bundle, &secrets, &options);
    report_gaps(&bundle, &secrets, &options);

    let bundles = proxy::BundleServer::start(bundle.bytes.clone()).await?;
    let runtime_port = match options.runtime_port {
        Some(port) => port,
        None => free_port()?,
    };

    let (mut node, mut node_exited) =
        node::DevNode::start(&binary, runtime_port, options.debug).await?;
    node.apply(
        &app,
        &options.environment,
        &bundles.url,
        &bundle.digest,
        env,
        &bundle.manifest,
    )
    .await?;

    let addr = front_listener
        .local_addr()
        .context("reading the address the dev server is listening on")?;
    let mut front = tokio::spawn(proxy::serve_front_door(
        front_listener,
        runtime_port,
        app.clone(),
        options.environment.clone(),
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

    let env = deployment_env(&bundle, secrets, options);
    bundles.replace(bundle.bytes.clone()).await;

    match node
        .apply(
            app,
            &options.environment,
            &bundles.url,
            &bundle.digest,
            env,
            &bundle.manifest,
        )
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

/// What `context.env` will hold.
///
/// Built in the same order the control plane builds it, so the precedence a
/// developer sees locally is the one that will apply in production: the
/// manifest's plain variables first, then secrets, then the platform's own
/// reserved keys, which win.
fn deployment_env(
    bundle: &Bundle,
    secrets: &BTreeMap<String, String>,
    options: &Options,
) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = bundle
        .manifest
        .env
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    for (name, value) in secrets {
        env.insert(name.clone(), value.clone());
    }
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
/// bug report about `ctx.services.kv` being broken.
fn report_gaps(bundle: &Bundle, secrets: &BTreeMap<String, String>, options: &Options) {
    let declared: Vec<&str> = bundle
        .manifest
        .services
        .iter()
        .filter(|(_, enabled)| **enabled)
        .map(|(name, _)| name.as_str())
        .collect();

    let wants_queue = declared.contains(&"queue") || !bundle.manifest.queues.is_empty();
    let wants_store = declared.iter().any(|s| *s != "queue");

    if wants_store && options.store.is_none() {
        println!(
            "  {} this app declares {} but no --store address was given, so every \
             ctx.services.{{kv,database,blob}} call will fail. Start deis-store and pass --store \
             http://127.0.0.1:8090.",
            "!".yellow(),
            declared
                .iter()
                .filter(|s| **s != "queue")
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if wants_queue && options.queue.is_none() {
        println!(
            "  {} this app uses a queue but no --queue address was given, so ctx.services.queue \
             will fail and nothing will be consumed. Start deis-queue and pass --queue \
             http://127.0.0.1:8091.",
            "!".yellow()
        );
    }

    for name in &bundle.manifest.secrets {
        if !secrets.contains_key(name) {
            println!(
                "  {} secret {name} has no local value, so context.env.{name} will be missing. \
                 Pass --secret {name}=<value>.",
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

/// Read `NAME=VALUE` pairs.
fn parse_secrets(raw: &[String]) -> Result<BTreeMap<String, String>> {
    let mut secrets = BTreeMap::new();
    for entry in raw {
        let Some((name, value)) = entry.split_once('=') else {
            bail!(
                "--secret {entry:?} is not a NAME=VALUE pair. Write it as --secret \
                 API_KEY=local-value."
            );
        };
        let name = name.trim();
        if name.is_empty() {
            bail!("--secret {entry:?} has no name before the \"=\".");
        }
        secrets.insert(name.to_string(), value.to_string());
    }
    Ok(secrets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_read_as_name_value_pairs() {
        let secrets =
            parse_secrets(&["API_KEY=abc".to_string(), "URL=http://x/?a=b".to_string()]).unwrap();
        assert_eq!(secrets["API_KEY"], "abc");
        // Only the first "=" separates; a value may contain more.
        assert_eq!(secrets["URL"], "http://x/?a=b");
    }

    #[test]
    fn a_secret_without_a_value_says_the_shape_it_wanted() {
        let error = parse_secrets(&["API_KEY".to_string()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("NAME=VALUE"), "{error}");
    }
}
