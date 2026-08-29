//! `deis deploy` - build the bundle, record it as a version, and point an
//! environment at it.
//!
//! This deploys to *this* platform and nothing else. Building for Cloudflare,
//! Deno or EdgeOne is `deis build --vendor <name>` followed by that vendor's
//! own tooling, which owns the credentials and the deployment model for it;
//! a `--vendor` flag here would have suggested this command could push to
//! them, which it never could.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use colored::Colorize;

use crate::commands::{Session, build, project_dir};
use crate::platform::{DeliveryReport, DeploymentResponse};
use crate::render::{Output, short_digest};
use crate::settings::Overrides;

pub struct Args {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub note: Option<String>,
    /// Deploy this bundle instead of building one.
    pub bundle: Option<PathBuf>,
    /// Use the bundle the last build left behind instead of building again.
    pub no_build: bool,
    pub no_minify: bool,
}

pub async fn execute(args: Args, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app: args.app.clone(),
        environment: args.environment.clone(),
    })?;
    let app = session.app()?.to_string();
    let environment = session.environment()?.to_string();
    let dir = project_dir()?;

    output.line(format!(
        "\n{} {} {} {}",
        "Deploying".bold(),
        app.cyan(),
        "to".bold(),
        environment.cyan()
    ));
    output.line(format!(
        "  control plane  {}",
        session.control.base_url().dimmed()
    ));

    // Build (or read) the bundle first: an application that does not compile
    // must not create an app, an environment or a version.
    let bundle = match &args.bundle {
        Some(path) => build::read_bundle(path)?,
        None if args.no_build => {
            let path = default_bundle_path(&dir)?;
            build::read_bundle(&path)?
        }
        None => {
            // The compiler reports its progress on stdout; in --json mode
            // that would land in front of the document. See crate::quiet.
            let _quiet = output.json.then(crate::quiet::StdoutToStderr::start);
            build::deislet_bundle(&dir, !args.no_minify).await?
        }
    };

    if bundle.manifest.app != app {
        bail!(
            "the bundle was built for application {:?} but this deploy is for {:?}. The control \
             plane refuses a bundle whose manifest names another application, so change \"name\" \
             in .config.json or drop --app.",
            bundle.manifest.app,
            app
        );
    }

    output.line(format!(
        "  {} built {} ({}, sha256 {})",
        "✓".green(),
        bundle.path.display(),
        human_size(bundle.bytes.len()),
        short_digest(&bundle.digest)
    ));

    let app_created = session.control.ensure_app(&app).await?;
    if app_created {
        output.line(format!("  {} created application {app}", "✓".green()));
    }

    let environment_created = session
        .control
        .ensure_environment(&app, &environment)
        .await?;
    if environment_created {
        output.line(format!(
            "  {} created environment {environment}",
            "✓".green()
        ));
    }

    let size = bundle.bytes.len();
    let upload = session
        .control
        .upload_version(&app, bundle.bytes, &bundle.digest)
        .await?;

    if upload.created {
        output.line(format!(
            "  {} uploaded {} as version {}",
            "✓".green(),
            human_size(size),
            upload.version.number
        ));
    } else {
        // Same bytes as a version already recorded. Versions are immutable and
        // content addressed, so this is not an error - but saying "uploaded"
        // would hide that the number did not move.
        output.line(format!(
            "  {} identical to version {}, which is reused unchanged",
            "•".yellow(),
            upload.version.number
        ));
    }

    let deployment = session
        .control
        .deploy(&app, &environment, &upload.version.id, args.note.as_deref())
        .await?;

    if output.json {
        // Deliberately not the whole `DeploymentResponse`: it carries the
        // bundle manifest, which is kilobytes of detail a deploy script has
        // no use for and which would bury the four fields it does need.
        output.emit(&serde_json::json!({
            "app": app,
            "environment": environment,
            "app_created": app_created,
            "environment_created": environment_created,
            "version_created": upload.created,
            "bundle": {
                "path": bundle.path,
                "bytes": size,
                "sha256": bundle.digest,
            },
            "version": {
                "number": deployment.version.number,
                "id": deployment.version.id,
                "sha256": deployment.version.bundle_sha,
                "created_at": deployment.version.created_at,
            },
            "deployment": deployment.deployment,
            "delivery": deployment.delivery,
        }))?;
    } else {
        print_result(&deployment);
    }

    // A version that reached no node is not deployed, whatever the catalog
    // now says, so the exit code has to say so too.
    fail_on_incomplete_delivery(deployment.delivery.as_ref(), &environment)
}

/// Print the version, the digest and where it landed.
fn print_result(deployment: &DeploymentResponse) {
    println!(
        "\n  {} {} now runs version {} (sha256 {})",
        "✓".green(),
        deployment.environment.cyan(),
        deployment.version.number.to_string().bold(),
        short_digest(&deployment.version.bundle_sha)
    );

    match &deployment.delivery {
        Some(delivery) => print_delivery(delivery),
        None => println!(
            "  {}",
            "the control plane did not say where this was delivered".yellow()
        ),
    }
}

/// Say which nodes took the version and which did not.
///
/// A node in `failed` is still serving the previous version, so it is printed
/// as prominently as the successes rather than summarised away.
pub fn print_delivery(delivery: &DeliveryReport) {
    for node in &delivery.delivered {
        println!("      {} {node}", "✓".green());
    }
    for failure in &delivery.failed {
        println!(
            "      {} {}: {}",
            "✗".red(),
            failure.node.red(),
            failure.error
        );
    }
    if let Some(reason) = &delivery.skipped {
        println!("      {} nothing was pushed: {reason}", "•".yellow());
    }
    if delivery.delivered.is_empty() && delivery.failed.is_empty() && delivery.skipped.is_none() {
        println!("      {} no node was asked", "•".yellow());
    }
}

/// Turn a partial delivery into a non-zero exit.
pub fn fail_on_incomplete_delivery(
    delivery: Option<&DeliveryReport>,
    environment: &str,
) -> Result<()> {
    let Some(delivery) = delivery else {
        return Ok(());
    };
    if delivery.failed.is_empty() {
        return Ok(());
    }
    let nodes: Vec<&str> = delivery.failed.iter().map(|f| f.node.as_str()).collect();
    bail!(
        "the pointer for {environment} moved, but {} node(s) did not take the version and keep \
         serving what they had: {}. They pick it up when they can reach the control plane again, \
         or on their next restart; check those nodes before treating this deploy as finished.",
        nodes.len(),
        nodes.join(", ")
    )
}

/// Where a previous build left the bundle, honouring `build.outDir`.
pub fn default_bundle_path(project_dir: &Path) -> Result<PathBuf> {
    let config = deis_build::load_project_config(&project_dir.to_string_lossy())
        .context("reading .config.json to find where the last build wrote its bundle")?;
    let out_dir = config
        .build
        .as_ref()
        .map(|b| b.out_dir.clone())
        .unwrap_or_else(|| "./dist".to_string());
    Ok(project_dir
        .join(out_dir)
        .join(build::DEISLET_VENDOR)
        .join(deis_build::adapter_deislet::BUNDLE_FILE_NAME))
}

fn human_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::NodeFailure;

    fn report(
        delivered: &[&str],
        failed: &[(&str, &str)],
        skipped: Option<&str>,
    ) -> DeliveryReport {
        DeliveryReport {
            reason: "deployment".to_string(),
            app_id: "demo".to_string(),
            environment: "production".to_string(),
            version_id: "v".to_string(),
            delivered: delivered.iter().map(|s| s.to_string()).collect(),
            failed: failed
                .iter()
                .map(|(node, error)| NodeFailure {
                    node: node.to_string(),
                    error: error.to_string(),
                })
                .collect(),
            skipped: skipped.map(|s| s.to_string()),
        }
    }

    /// A node that did not take the version has to reach the exit code:
    /// a green deploy that half the fleet ignored is the failure this is for.
    #[test]
    fn a_node_that_refused_the_version_fails_the_command() {
        let delivery = report(&["node-a"], &[("node-b", "connection refused")], None);
        let error = fail_on_incomplete_delivery(Some(&delivery), "production")
            .unwrap_err()
            .to_string();
        assert!(error.contains("node-b"), "{error}");
        assert!(error.contains("keep serving what they had"), "{error}");
    }

    /// A control plane with no nodes configured is a valid setup - nodes pull
    /// their assignments when they start - so it must not fail the command.
    #[test]
    fn a_delivery_with_no_nodes_at_all_is_not_a_failure() {
        let delivery = report(&[], &[], Some("no runtime nodes are configured"));
        fail_on_incomplete_delivery(Some(&delivery), "production").unwrap();
    }

    #[test]
    fn a_full_delivery_succeeds() {
        let delivery = report(&["node-a", "node-b"], &[], None);
        fail_on_incomplete_delivery(Some(&delivery), "production").unwrap();
    }

    #[test]
    fn sizes_read_in_the_unit_a_person_would_use() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }
}
