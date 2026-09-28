//! `deis deploy` - build the bundle, record it as a version, and point an
//! environment at it.
//!
//! `--target deislet` (the default) deploys to this platform: the version is
//! delivered to this platform's nodes. `--target cloudflare` records the same
//! version, uploads the Cloudflare payload built from the same canonical
//! artifact, and asks the control plane for a provider deployment: the
//! Cloudflare provider driver installed on the control plane runs its steps
//! (preflight, plan, prepare, upload, verify, activate) and this command
//! follows the workflow until it settles. The environment must hold a grant
//! on a Cloudflare provider target (`POST
//! /api/v1/apps/{app}/environments/{env}/provider-target-grants`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use colored::Colorize;
use deis_wire::CanonicalId;

use crate::commands::{Session, build, escaped, project_dir};
use crate::platform::{
    DeliveryReport, DeploymentResponse, ProviderArtifactRef, ProviderTargetGrant,
};
use crate::render::{Output, compatibility_line, short_digest};
use crate::settings::Overrides;

/// Where a deployment runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum DeployTarget {
    /// This platform's own nodes.
    #[default]
    Deislet,
    /// Cloudflare Workers, through the control plane's Cloudflare provider
    /// driver.
    Cloudflare,
}

impl DeployTarget {
    /// The build target that produces this target's payload.
    fn build_target(self) -> &'static str {
        match self {
            Self::Deislet => build::DEISLET_VENDOR,
            Self::Cloudflare => "cloudflare",
        }
    }

    /// The provider backend id a provider target of this kind drives.
    fn backend_id(self) -> &'static str {
        match self {
            Self::Deislet => "deislet",
            Self::Cloudflare => "cloudflare-workers-pages",
        }
    }
}

pub struct Args {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub note: Option<String>,
    /// Deploy this bundle instead of building one.
    pub bundle: Option<PathBuf>,
    /// Use the bundle the last build left behind instead of building again.
    pub no_build: bool,
    pub no_minify: bool,
    pub target: DeployTarget,
    /// Which provider target, by name or id, when the environment holds
    /// grants on more than one of the chosen kind.
    pub provider_target: Option<String>,
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

    if args.target != DeployTarget::Deislet {
        if args.bundle.is_some() || args.no_build {
            bail!(
                "--bundle and --no-build deploy a prebuilt Deislet bundle; a {} deployment builds \
                 the bundle and the provider payload together so both derive from one canonical \
                 artifact",
                args.target.build_target()
            );
        }
        if args.note.is_some() {
            bail!(
                "--note is recorded on a Deislet deployment; provider deployments do not carry one"
            );
        }
        return deploy_to_provider(&session, &app, &environment, &dir, &args, output).await;
    }
    if args.provider_target.is_some() {
        bail!("--provider-target chooses among provider targets; pass it with --target cloudflare");
    }

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

/// Deploy through a provider driver: build, record the version and the
/// provider payload, start a provider deployment and follow it.
async fn deploy_to_provider(
    session: &Session,
    app: &str,
    environment: &str,
    dir: &Path,
    args: &Args,
    output: Output,
) -> Result<()> {
    let target = args.target;
    output.line(format!(
        "\n{} {} {} {} {} {}",
        "Deploying".bold(),
        app.cyan(),
        "to".bold(),
        environment.cyan(),
        "on".bold(),
        target.build_target().cyan()
    ));
    output.line(format!(
        "  control plane  {}",
        session.control.base_url().dimmed()
    ));

    let (bundle, payload) = {
        let _quiet = output.json.then(crate::quiet::StdoutToStderr::start);
        build::build_for_deploy(dir, Some(target.build_target()), !args.no_minify).await?
    };
    let payload = payload.expect("a provider build returns its payload");
    let lineage = bundle
        .lineage
        .as_ref()
        .expect("a fresh build always carries its lineage");
    if bundle.manifest.app != app {
        bail!(
            "the bundle was built for application {:?} but this deploy is for {:?}; change \
             \"name\" in .config.json or drop --app",
            bundle.manifest.app,
            app
        );
    }
    output.line(format!(
        "  {} built {} payload {} from canonical {}",
        "✓".green(),
        target.build_target(),
        short_digest(&payload.payload_root_sha256),
        short_digest(&lineage.artifact_sha256)
    ));

    // Resolved before anything is written: an environment that may not
    // deploy to this kind of target should not collect versions it can
    // never deploy there.
    session.control.ensure_app(app).await?;
    session.control.ensure_environment(app, environment).await?;
    let grant = choose_provider_target(
        &session
            .control
            .list_provider_target_grants(app, environment)
            .await?,
        target,
        args.provider_target.as_deref(),
        app,
        environment,
    )?;

    let artifact = session
        .control
        .upload_canonical_artifact(app, bundle.canonical_transport()?, &lineage.artifact_sha256)
        .await?;
    if artifact.artifact.canonical_output.artifact_sha256 != lineage.artifact_sha256
        || artifact.artifact.canonical_output.content_root_sha256 != lineage.content_root_sha256
    {
        bail!("the control plane recorded a different canonical artifact identity");
    }
    let upload = session
        .control
        .upload_version_with_lineage(
            app,
            artifact.artifact.artifact_id,
            bundle.provider_envelope()?,
            &bundle.digest,
        )
        .await?;
    if upload.version.canonical_artifact_id != Some(artifact.artifact.artifact_id)
        || upload.version.provider_derivation_sha256.as_deref()
            != Some(lineage.derivation_manifest_sha256.as_str())
    {
        bail!("the control plane version does not preserve the uploaded artifact lineage");
    }
    let version_id = upload.version.canonical_id.ok_or_else(|| {
        anyhow::anyhow!("the control plane answered the version without its canonical id")
    })?;
    output.line(format!(
        "  {} version {} ({})",
        "✓".green(),
        upload.version.number,
        if upload.created { "recorded" } else { "reused" }
    ));
    let stored = session
        .control
        .upload_provider_payload(app, artifact.artifact.artifact_id, payload.envelope()?)
        .await?;
    if stored.payload_root_sha256 != payload.payload_root_sha256
        || stored.manifest_sha256 != payload.derivation_manifest_sha256
        || stored.provider != payload.provider
    {
        bail!("the control plane recorded a different provider payload identity");
    }
    output.line(format!(
        "  {} {} payload {}",
        "✓".green(),
        stored.provider,
        if stored.created {
            "stored"
        } else {
            "already stored"
        }
    ));

    let accepted = session
        .control
        .begin_provider_deployment(
            app,
            environment,
            grant.provider_target_id,
            &ProviderArtifactRef {
                artifact_version_id: version_id,
                canonical_artifact_sha256: lineage.artifact_sha256.clone(),
                provider_payload_sha256: payload.payload_root_sha256.clone(),
            },
        )
        .await?;
    output.line(format!(
        "  {} provider deployment {} started on target {}",
        "✓".green(),
        accepted.operation_id.to_string().cyan(),
        escaped(&grant.target_name)
    ));
    output.line(format!(
        "\n  waiting for {} to finish...",
        accepted.operation_id
    ));
    let settled = session
        .control
        .wait_for_provider_deployment(app, environment, accepted.operation_id)
        .await?;
    let succeeded = settled.operation.state == deis_wire::OperationState::Succeeded.as_str();

    if output.json {
        output.emit(&serde_json::json!({
            "app": app,
            "environment": environment,
            "target": target.build_target(),
            "providerTargetId": grant.provider_target_id,
            "version": {
                "number": upload.version.number,
                "id": version_id,
                "created": upload.created,
            },
            "artifact": {
                "artifactVersionId": version_id,
                "canonicalArtifactSha256": lineage.artifact_sha256,
                "providerPayloadSha256": payload.payload_root_sha256,
            },
            "providerDeployment": settled,
        }))?;
    } else if succeeded {
        println!(
            "\n  {} {} of {} is deployed on {} ({})",
            "✓".green(),
            environment.cyan(),
            app.cyan(),
            target.build_target(),
            accepted.operation_id
        );
    }
    if !succeeded {
        let reason = settled
            .operation
            .error
            .as_ref()
            .map(|error| format!(": {} {}", error.code, error.message))
            .unwrap_or_default();
        bail!(
            "provider deployment {} ended {}{reason}; `deis provider-deployments get {}` shows \
             every step",
            accepted.operation_id,
            settled.operation.state,
            accepted.operation_id
        );
    }
    Ok(())
}

/// Pick the one provider target of `target`'s kind this environment may
/// deploy to, or the one `requested` names.
fn choose_provider_target(
    grants: &[ProviderTargetGrant],
    target: DeployTarget,
    requested: Option<&str>,
    app: &str,
    environment: &str,
) -> Result<ProviderTargetGrant> {
    let usable: Vec<&ProviderTargetGrant> = grants
        .iter()
        .filter(|grant| {
            grant.backend_id == target.backend_id()
                && grant.status == "active"
                && grant.target_status == "active"
        })
        .collect();
    let chosen: Vec<&ProviderTargetGrant> = match requested {
        Some(requested) => {
            let id = requested.parse::<CanonicalId>().ok();
            usable
                .iter()
                .copied()
                .filter(|grant| {
                    Some(grant.provider_target_id) == id || grant.target_name == requested
                })
                .collect()
        }
        None => usable.clone(),
    };
    match chosen.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!(
            "{environment} of {app} holds no active grant on a {} provider target{}. An operator \
             registers the target (POST /api/v1/providers/installations/{{id}}/targets) and grants \
             it to this environment (POST \
             /api/v1/apps/{app}/environments/{environment}/provider-target-grants)",
            target.build_target(),
            requested
                .map(|requested| format!(" named {}", escaped(requested)))
                .unwrap_or_default()
        ),
        many => bail!(
            "{environment} of {app} may deploy to {} {} provider targets ({}); choose one with \
             --provider-target <name or id>",
            many.len(),
            target.build_target(),
            many.iter()
                .map(|grant| escaped(&grant.target_name))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
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
        "resolving the last fully verified deislet build; run `deis build --target deislet` first",
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

    fn grant(name: &str, backend: &str) -> ProviderTargetGrant {
        ProviderTargetGrant {
            grant_id: CanonicalId::new_v7(),
            provider_target_id: CanonicalId::new_v7(),
            backend_id: backend.to_string(),
            target_name: name.to_string(),
            target_status: "active".to_string(),
            status: "active".to_string(),
        }
    }

    #[test]
    fn the_one_cloudflare_grant_is_chosen_and_others_are_ignored() {
        let grants = [
            grant("edge", "cloudflare-workers-pages"),
            grant("eo", "tencent-edgeone-makers"),
        ];
        let chosen = choose_provider_target(
            &grants,
            DeployTarget::Cloudflare,
            None,
            "demo",
            "production",
        )
        .unwrap();
        assert_eq!(chosen.target_name, "edge");
    }

    #[test]
    fn no_grant_says_how_to_get_one_and_many_ask_which() {
        let error = choose_provider_target(&[], DeployTarget::Cloudflare, None, "demo", "prod")
            .unwrap_err()
            .to_string();
        assert!(error.contains("provider-target-grants"), "{error}");

        let grants = [
            grant("eu", "cloudflare-workers-pages"),
            grant("us", "cloudflare-workers-pages"),
        ];
        let error = choose_provider_target(&grants, DeployTarget::Cloudflare, None, "demo", "prod")
            .unwrap_err()
            .to_string();
        assert!(error.contains("--provider-target"), "{error}");
        let by_name = choose_provider_target(
            &grants,
            DeployTarget::Cloudflare,
            Some("us"),
            "demo",
            "prod",
        )
        .unwrap();
        assert_eq!(by_name.target_name, "us");
        let id = grants[0].provider_target_id.to_string();
        let by_id =
            choose_provider_target(&grants, DeployTarget::Cloudflare, Some(&id), "demo", "prod")
                .unwrap();
        assert_eq!(by_id.target_name, "eu");
    }

    #[test]
    fn a_revoked_or_disabled_target_is_not_chosen() {
        let mut revoked = grant("old", "cloudflare-workers-pages");
        revoked.status = "revoked".to_string();
        let mut disabled = grant("off", "cloudflare-workers-pages");
        disabled.target_status = "disabled".to_string();
        assert!(
            choose_provider_target(
                &[revoked, disabled],
                DeployTarget::Cloudflare,
                None,
                "demo",
                "prod"
            )
            .is_err()
        );
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
