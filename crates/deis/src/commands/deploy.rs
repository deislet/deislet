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
use crate::render::{Output, compatibility_line, short_digest};
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
        binding: None,
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
        None if args.no_build => build::last_deislet_bundle(&dir)?,
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

    let config_values = deployment_config_values(&dir, &bundle.manifest)?;
    let lineage_transport = if bundle.lineage.is_some() {
        Some((bundle.canonical_transport()?, bundle.provider_envelope()?))
    } else {
        None
    };

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
    let (upload, artifact_record) = if let Some((canonical_transport, provider_envelope)) =
        lineage_transport
    {
        let lineage = bundle.lineage.as_ref().expect("checked above");
        let artifact = session
            .control
            .upload_canonical_artifact(&app, canonical_transport, &lineage.artifact_sha256)
            .await?;
        if artifact.artifact.canonical_output.artifact_sha256 != lineage.artifact_sha256
            || artifact.artifact.canonical_output.content_root_sha256 != lineage.content_root_sha256
        {
            bail!("the control plane recorded a different canonical artifact identity");
        }
        output.line(format!(
            "  {} canonical artifact {} {}",
            if artifact.created {
                "✓".green()
            } else {
                "•".yellow()
            },
            short_digest(&lineage.artifact_sha256),
            if artifact.created {
                "recorded"
            } else {
                "reused"
            }
        ));
        let upload = session
            .control
            .upload_version_with_lineage(
                &app,
                artifact.artifact.artifact_id,
                provider_envelope,
                &bundle.digest,
            )
            .await?;
        if upload.version.canonical_artifact_id != Some(artifact.artifact.artifact_id)
            || upload.version.provider_derivation_sha256.as_deref()
                != Some(lineage.derivation_manifest_sha256.as_str())
        {
            bail!("the control plane version does not preserve the uploaded artifact lineage");
        }
        (upload, Some(artifact.artifact))
    } else {
        output.line(format!(
            "  {} this manually supplied bundle has no canonical/derivation evidence; using the legacy compatibility upload",
            "•".yellow()
        ));
        (
            session
                .control
                .upload_version(&app, bundle.bytes.clone(), &bundle.digest)
                .await?,
            None,
        )
    };

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
        .deploy(
            &app,
            &environment,
            &upload.version.id,
            args.note.as_deref(),
            &config_values,
        )
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
                "compatibility": deployment.version.compatibility,
                "canonical_artifact_id": deployment.version.canonical_artifact_id,
                "provider_derivation_sha256": deployment.version.provider_derivation_sha256,
            },
            "canonical_artifact": artifact_record,
            "deployment": deployment.deployment,
            "delivery": deployment.delivery,
            "plan": deployment.plan,
        }))?;
    } else {
        print_result(&deployment);
    }

    // A version that reached no node is not deployed, whatever the catalog
    // now says, so the exit code has to say so too.
    fail_on_incomplete_delivery(deployment.delivery.as_ref(), &environment)
}

/// Resolve only the non-secret values selected at deployment time. The
/// bundle's declaration remains authoritative: a stale project file cannot
/// silently attach values to a canonical artifact built from another set of
/// declarations.
pub(crate) fn deployment_config_values(
    project_dir: &Path,
    manifest: &deis_bundle::Manifest,
) -> Result<std::collections::BTreeMap<String, serde_json::Value>> {
    let Some(declarations) = manifest.environment.as_ref() else {
        return Ok(Default::default());
    };
    let config = deis_build::load_project_config(&project_dir.to_string_lossy())
        .context("reading deployment-time environment config from .config.json")?;
    let supplied =
        deis_build::resolve_environment_declarations(&config, &declarations.standard_version)
            .context("validating deployment-time environment declarations")?;
    if supplied.identity()? != declarations.identity()? {
        bail!(
            "EC_ARTIFACT_DERIVATION_INPUT_MISMATCH: .config.json environment declarations differ from the bundle being deployed"
        );
    }
    deis_build::resolve_environment_config_values(&config, declarations)
        .context("validating deployment-time environment values")
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

    // Printed on every deploy, dated or not. Which day's platform behaviour an
    // application is running under decides answers a developer will otherwise
    // debug from the outside, and the one deploy where it is cheapest to
    // notice is the one that has just happened.
    let compatibility = deployment.version.compatibility.as_ref();
    let dated = compatibility.and_then(|c| c.date()).is_some();
    let line = compatibility_line(compatibility);
    if dated {
        println!("  {} {}", "✓".green(), line);
    } else {
        println!("  {} {}", "•".yellow(), line.yellow());
    }

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
#[cfg(test)]
pub fn default_bundle_path(project_dir: &Path) -> Result<PathBuf> {
    let config = deis_build::load_project_config(&project_dir.to_string_lossy())
        .context("reading .config.json to find where the last build wrote its bundle")?;
    let out_dir = config
        .build
        .as_ref()
        .map(|b| b.out_dir.clone())
        .unwrap_or_else(|| "./dist".to_string());
    let output_base = project_dir.join(out_dir);
    let payload = deis_build::provider_derivation::resolve_current(
        &output_base,
        build::DEISLET_VENDOR,
    )
    .context(
        "resolving the last fully verified deislet build; run `deis build --vendor deislet` first",
    )?;
    Ok(payload.join(deis_build::adapter_deislet::BUNDLE_FILE_NAME))
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
    use sha2::{Digest, Sha256};

    fn report(
        delivered: &[&str],
        failed: &[(&str, &str)],
        skipped: Option<&str>,
    ) -> DeliveryReport {
        DeliveryReport {
            reason: "deployment".to_string(),
            phase: "activate".to_string(),
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

    #[test]
    fn schema_two_deploy_reads_typed_config_but_never_a_secret_value() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join(".config.json"),
            r#"{
                "version":"1.0",
                "name":"demo",
                "runtime":"standard-v1",
                "language":"javascript",
                "environment":{
                    "declarations":[
                        {"name":"OPTIONS","kind":"config","valueType":"json","required":true},
                        {"name":"TOKEN","kind":"secret","valueType":"string","required":true}
                    ],
                    "values":{"OPTIONS":{"retries":3}}
                }
            }"#,
        )
        .unwrap();
        let config = deis_build::load_project_config(&project.path().to_string_lossy()).unwrap();
        let declarations = deis_build::resolve_environment_declarations(
            &config,
            "edge-canon.next@a41a4caadeaf8e5751ebe57ef5be673d18384654",
        )
        .unwrap();
        let manifest = deis_bundle::Manifest::parse(
            &serde_json::json!({
                "schema": 2,
                "app": "demo",
                "entry": "worker/index.js",
                "environment": declarations,
            })
            .to_string(),
        )
        .unwrap();

        let values = deployment_config_values(project.path(), &manifest).unwrap();
        assert_eq!(
            values.get("OPTIONS"),
            Some(&serde_json::json!({"retries": 3}))
        );
        assert!(!values.contains_key("TOKEN"));
    }

    #[test]
    fn no_build_resolves_the_verified_content_addressed_bundle() {
        const STANDARD: &str = "edge-canon.next@a41a4caadeaf8e5751ebe57ef5be673d18384654";

        let project = tempfile::tempdir().expect("project");
        std::fs::write(
            project.path().join(".config.json"),
            r#"{
                "version":"1.0",
                "name":"selector-test",
                "runtime":"standard-v1",
                "language":"javascript",
                "build":{"outDir":"./artifacts","minify":true,"sourcemap":false}
            }"#,
        )
        .expect("config");

        let source = project.path().join("provider-source");
        std::fs::create_dir_all(&source).expect("source");
        std::fs::write(
            source.join(deis_build::adapter_deislet::BUNDLE_FILE_NAME),
            b"immutable bundle bytes",
        )
        .expect("bundle");
        let output_base = project.path().join("artifacts");
        let payload = deis_build::provider_derivation::publish_payload(
            &output_base.join("providers/deislet"),
            &source,
        )
        .expect("payload");
        let parent = hex::encode(Sha256::digest(b"parent"));
        let content = hex::encode(Sha256::digest(b"content"));
        let derivation = deis_build::provider_derivation::publish(
            &output_base.join(".deislet/derivations/deislet"),
            &payload.root,
            build::DEISLET_VENDOR,
            STANDARD,
            &parent,
            &content,
        )
        .expect("derivation");
        deis_build::provider_derivation::publish_current(
            &output_base,
            build::DEISLET_VENDOR,
            STANDARD,
            &parent,
            &content,
            &derivation.payload_root_sha256,
            &derivation.manifest_sha256,
        )
        .expect("current selector");

        assert_eq!(
            default_bundle_path(project.path()).expect("default bundle"),
            payload
                .root
                .join(deis_build::adapter_deislet::BUNDLE_FILE_NAME)
        );
    }
}
