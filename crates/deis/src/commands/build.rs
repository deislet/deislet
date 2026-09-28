//! `deis build` - compile the project for one or more targets.
//!
//! The deislet target is this platform's own: it produces the application
//! bundle `deis deploy` uploads. The cloudflare target produces the payload
//! `deis deploy --target cloudflare` hands to the control plane's Cloudflare
//! provider driver. The frozen deno and tencent targets produce stateless
//! artifacts only when named explicitly; nothing deploys them.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use colored::Colorize;
use deis_build::Compiler;

/// The build target name of this platform's own target.
pub const DEISLET_VENDOR: &str = "deislet";

/// What a `deis build` without `--target` builds: every target .config.json
/// enables, minus the frozen ones (deno, tencent), which still build
/// stateless artifacts when named but are never part of a default build.
fn default_targets(config: &deis_build::ProjectConfig) -> Vec<String> {
    let Some(vendors) = config.vendors.as_ref() else {
        return Vec::new();
    };
    [
        (
            DEISLET_VENDOR,
            vendors.deislet.as_ref().is_some_and(|v| v.enabled),
        ),
        (
            "cloudflare",
            vendors.cloudflare.as_ref().is_some_and(|v| v.enabled),
        ),
    ]
    .into_iter()
    .filter(|(_, enabled)| *enabled)
    .map(|(name, _)| name.to_string())
    .collect()
}

pub async fn execute(targets: Vec<String>, watch: bool, minify: bool) -> Result<()> {
    // Refused rather than ignored. A `--watch` that silently builds once and
    // exits looks exactly like a watch that is running and seeing no
    // changes, which is the slowest possible way to find out it does nothing.
    if watch {
        bail!(
            "`deis build --watch` is not implemented. `deis dev` rebuilds and reloads on every \
             change for this platform; for another target, run `deis build --target <name>` from \
             your own file watcher."
        );
    }

    println!("{}", "Building project...".yellow());

    let start = Instant::now();
    let project_dir = env::current_dir()?;
    let compiler = Compiler::new();
    let vendors = if targets.is_empty() {
        let config = deis_build::load_project_config(&project_dir.to_string_lossy())
            .context("reading .config.json to choose the default build targets")?;
        let defaults = default_targets(&config);
        if defaults.is_empty() {
            bail!(
                ".config.json enables neither the deislet nor the cloudflare target; enable one \
                 under \"vendors\" or name a target with --target"
            );
        }
        defaults
    } else {
        targets
    };

    // First validate
    println!("{}", "Validating project...".dimmed());
    let validation = compiler.validate(&project_dir, false)?;

    if !validation.is_valid() {
        println!("\n{}", "Validation failed".red().bold());
        println!("{}", "\n✗ Fix validation errors before building".red());
        std::process::exit(1);
    }

    println!("{}", "Building...".dimmed());

    let result = compiler
        .compile(&project_dir, vendors.clone(), minify)
        .await?;

    if result.success {
        println!("\n{}", "Build completed".green().bold());

        if let Some(canonical) = &result.canonical_artifact {
            println!(
                "{}",
                format!(
                    "  ✓ Canonical: {} (content {}, {} files)\n    {}",
                    &canonical.artifact_sha256[..12],
                    &canonical.content_root_sha256[..12],
                    canonical.file_count,
                    canonical.output_path
                )
                .green()
            );
        }

        for (i, (vendor, build)) in result.builds.iter().enumerate() {
            println!(
                "\n{}",
                format!(
                    "[{}/{}] {}",
                    i + 1,
                    result.builds.len(),
                    vendor.chars().next().unwrap().to_uppercase().to_string() + &vendor[1..]
                )
                .bold()
            );

            if let Some(derivation) = &build.derivation {
                println!(
                    "{}",
                    format!(
                        "  ✓ Derived from canonical {}{}",
                        &derivation.parent_artifact_sha256[..12],
                        derivation
                            .manifest_sha256
                            .as_ref()
                            .map(|digest| format!(", record {}", &digest[..12]))
                            .unwrap_or_default()
                    )
                    .green()
                );
            }
            println!(
                "{}",
                format!("  ✓ Compiled {} file(s)", build.files.len()).green()
            );
            println!(
                "{}",
                format!(
                    "  ✓ Output: {} ({:.2} KB)",
                    build.output_path,
                    build.size as f64 / 1024.0
                )
                .green()
            );

            if !build.warnings.is_empty() {
                println!(
                    "{}",
                    format!("  ⚠ {} warning(s)", build.warnings.len()).yellow()
                );
            }
        }

        let total_size: usize = result.builds.values().map(|b| b.size).sum();
        let duration = start.elapsed();

        println!(
            "\n{}",
            format!("✓ Build completed in {:.2}s", duration.as_secs_f64()).green()
        );
        println!(
            "{}",
            format!("  Total output: {:.2} KB", total_size as f64 / 1024.0).dimmed()
        );
    } else {
        println!("\n{}", "Build failed".red().bold());

        if !result.errors.is_empty() {
            println!();
            for error in &result.errors {
                println!("  {}:{} - {}", error.file.red(), error.line, error.message);
            }
        }

        std::process::exit(1);
    }

    Ok(())
}

/// A built application bundle, ready to be uploaded as a version.
pub struct Bundle {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    /// sha256 of `bytes`, hex. The control plane is given this so a truncated
    /// upload is refused instead of becoming a version no node can load.
    pub digest: String,
    /// The manifest inside the archive, parsed with the shared reader.
    pub manifest: deis_bundle::Manifest,
    /// Exact provider-neutral parent and derivation sidecar. A manually
    /// supplied historical bundle has no such local evidence and therefore
    /// uses the explicit compatibility upload path.
    pub lineage: Option<BundleLineage>,
}

#[derive(Debug, Clone)]
pub struct BundleLineage {
    pub canonical_root: PathBuf,
    pub artifact_sha256: String,
    pub content_root_sha256: String,
    pub derivation_manifest_path: PathBuf,
    pub derivation_manifest_sha256: String,
}

impl Bundle {
    /// Deterministic transport for the canonical directory. This zip is only
    /// an HTTP carrier; Control expands and validates the upstream directory
    /// contract before storing it.
    pub fn canonical_transport(&self) -> Result<Vec<u8>> {
        let lineage = self
            .lineage
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("this bundle has no retained canonical parent"))?;
        zip_directory_files(&lineage.canonical_root)
            .context("packing the canonical Build Output for upload")
    }

    /// Transport the exact derivation record beside the native payload it
    /// binds. The outer zip is never recorded as an artifact identity.
    pub fn provider_envelope(&self) -> Result<Vec<u8>> {
        let lineage = self
            .lineage
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("this bundle has no retained provider derivation"))?;
        let derivation = fs::read(&lineage.derivation_manifest_path).with_context(|| {
            format!(
                "reading provider derivation {}",
                lineage.derivation_manifest_path.display()
            )
        })?;
        if deis_build::canonical_artifact::sha256(&derivation) != lineage.derivation_manifest_sha256
        {
            bail!("the provider derivation identity changed before upload");
        }
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o644);
        writer.start_file(".deislet/provider-derivation.json", options)?;
        writer.write_all(&derivation)?;
        writer.start_file("payload/bundle.zip", options)?;
        writer.write_all(&self.bytes)?;
        Ok(writer.finish()?.into_inner())
    }
}

/// Build the project for this platform and read the bundle back.
///
/// Validation runs first: a bundle that would be refused at upload is better
/// refused here, next to the code that produced it.
pub async fn deislet_bundle(project_dir: &Path, minify: bool) -> Result<Bundle> {
    Ok(build_for_deploy(project_dir, None, minify).await?.0)
}

/// A provider payload an external provider driver deploys, with the
/// derivation record binding it to the canonical parent.
#[derive(Debug, Clone)]
pub struct ProviderPayload {
    pub provider: String,
    pub payload_root: PathBuf,
    pub payload_root_sha256: String,
    pub derivation_manifest_path: PathBuf,
    pub derivation_manifest_sha256: String,
}

impl ProviderPayload {
    /// The upload envelope: the exact derivation record beside the payload
    /// tree under `payload/`. The outer zip is only a carrier; the Control
    /// re-derives the payload's file-set digest from the unpacked tree.
    pub fn envelope(&self) -> Result<Vec<u8>> {
        let derivation = fs::read(&self.derivation_manifest_path).with_context(|| {
            format!(
                "reading provider derivation {}",
                self.derivation_manifest_path.display()
            )
        })?;
        if deis_build::canonical_artifact::sha256(&derivation) != self.derivation_manifest_sha256 {
            bail!("the provider derivation identity changed before upload");
        }
        let mut files = walkdir::WalkDir::new(&self.payload_root)
            .follow_links(false)
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()?;
        files.retain(|entry| entry.file_type().is_file());
        files.sort_by(|left, right| left.path().cmp(right.path()));
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o644);
        writer.start_file(".deislet/provider-derivation.json", options)?;
        writer.write_all(&derivation)?;
        for entry in files {
            let relative = entry
                .path()
                .strip_prefix(&self.payload_root)?
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("provider payload path is not UTF-8"))?
                .replace('\\', "/");
            writer.start_file(format!("payload/{relative}"), options)?;
            writer.write_all(&fs::read(entry.path())?)?;
        }
        Ok(writer.finish()?.into_inner())
    }
}

/// Build the Deislet bundle and, when `provider` is named, that provider's
/// payload, from one compile so both derive from the same canonical parent.
///
/// The Deislet version is what a provider deployment's artifact names, so it
/// is always built: a Cloudflare deploy records a version exactly as a
/// self-hosted one does, without moving any environment pointer.
pub async fn build_for_deploy(
    project_dir: &Path,
    provider: Option<&str>,
    minify: bool,
) -> Result<(Bundle, Option<ProviderPayload>)> {
    let compiler = Compiler::new();
    let validation = compiler
        .validate(project_dir, false)
        .context("validating the project before building it")?;
    if !validation.is_valid() {
        for error in &validation.errors {
            eprintln!("  {}:{} {}", error.file, error.line, error.message);
        }
        bail!(
            "the project does not validate, so nothing was built or deployed. Run `deis validate` \
             for the full report and fix the errors above."
        );
    }

    let mut targets = vec![DEISLET_VENDOR.to_string()];
    targets.extend(provider.map(str::to_string));
    let result = compiler
        .compile(project_dir, targets, minify)
        .await
        .context("building the application bundle")?;

    // Deployment is a new trust boundary. Recompute both the canonical parent
    // and the provider payload lineage here rather than trusting the checks
    // performed earlier in the build call: a concurrent or manual mutation
    // between packaging and upload must stop before bytes leave this machine.
    let canonical = result.canonical_artifact.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "the build returned no retained Edge Canon parent, so its provider payload cannot be deployed"
        )
    })?;
    let canonical_summary =
        deis_build::canonical_artifact::validate(Path::new(&canonical.output_path), None, &[])
            .context("revalidating the canonical artifact before deployment")?;
    if canonical_summary.artifact_sha256 != canonical.artifact_sha256
        || canonical_summary.content_root_sha256 != canonical.content_root_sha256
    {
        bail!("the canonical artifact identity changed after the build completed");
    }
    let verified = |target: &str| -> Result<(
        PathBuf,
        deis_build::provider_derivation::ProviderDerivationSummary,
    )> {
        let build = result.builds.get(target).ok_or_else(|| {
            anyhow::anyhow!(
                "the build produced no {target} target. Enable it in .config.json:\n\n  \
                 \"vendors\": {{ \"{target}\": {{ \"enabled\": true }} }}"
            )
        })?;
        let derivation = build.derivation.as_ref().ok_or_else(|| {
            anyhow::anyhow!("the {target} build has no canonical parent derivation record")
        })?;
        let derivation_path = derivation.manifest_path.as_deref().ok_or_else(|| {
            anyhow::anyhow!("the {target} build has no retained provider derivation manifest")
        })?;
        let derived = deis_build::provider_derivation::validate(
            Path::new(derivation_path),
            Path::new(&build.output_path),
            Some(target),
            Some(&canonical_summary.standard_version),
            Some(&canonical.artifact_sha256),
        )
        .with_context(|| format!("revalidating the {target} provider payload before deployment"))?;
        if derivation.manifest_sha256.as_deref() != Some(derived.manifest_sha256.as_str())
            || derivation.payload_root_sha256.as_deref()
                != Some(derived.payload_root_sha256.as_str())
        {
            bail!("the {target} provider derivation identity changed after the build completed");
        }
        Ok((PathBuf::from(&build.output_path), derived))
    };

    let (deislet_root, deislet) = verified(DEISLET_VENDOR)?;
    let path = deislet_root.join(deis_build::adapter_deislet::BUNDLE_FILE_NAME);
    let mut bundle = read_bundle(&path)?;
    bundle.lineage = Some(BundleLineage {
        canonical_root: canonical_summary.root.clone(),
        artifact_sha256: canonical_summary.artifact_sha256.clone(),
        content_root_sha256: canonical_summary.content_root_sha256.clone(),
        derivation_manifest_path: deislet.manifest_path,
        derivation_manifest_sha256: deislet.manifest_sha256,
    });
    let payload = match provider {
        Some(provider) => {
            let (payload_root, derived) = verified(provider)?;
            Some(ProviderPayload {
                provider: provider.to_string(),
                payload_root,
                payload_root_sha256: derived.payload_root_sha256,
                derivation_manifest_path: derived.manifest_path,
                derivation_manifest_sha256: derived.manifest_sha256,
            })
        }
        None => None,
    };
    Ok((bundle, payload))
}

/// Read the last completed, fully verified Deislet payload and its retained
/// canonical lineage for `deis deploy --no-build`.
pub fn last_deislet_bundle(project_dir: &Path) -> Result<Bundle> {
    let config = deis_build::load_project_config(&project_dir.to_string_lossy())
        .context("reading .config.json to find the last build")?;
    let output_base = project_dir.join(
        config
            .build
            .as_ref()
            .map(|build| build.out_dir.as_str())
            .unwrap_or("./dist"),
    );
    let selected =
        deis_build::provider_derivation::resolve_current_summary(&output_base, DEISLET_VENDOR)
            .context("resolving the last fully verified Deislet provider payload")?;
    let canonical_root = output_base
        .join("canonical")
        .join(&selected.parent_artifact_sha256);
    let canonical = deis_build::canonical_artifact::validate(
        &canonical_root,
        Some(&selected.standard_version),
        &[],
    )
    .context("revalidating the retained canonical parent")?;
    if canonical.artifact_sha256 != selected.parent_artifact_sha256
        || canonical.content_root_sha256 != selected.parent_content_root_sha256
    {
        bail!("the retained canonical parent differs from the current provider selection");
    }
    let path = selected
        .payload_root
        .join(deis_build::adapter_deislet::BUNDLE_FILE_NAME);
    let mut bundle = read_bundle(&path)?;
    bundle.lineage = Some(BundleLineage {
        canonical_root,
        artifact_sha256: selected.parent_artifact_sha256,
        content_root_sha256: selected.parent_content_root_sha256,
        derivation_manifest_path: selected.derivation_manifest_path,
        derivation_manifest_sha256: selected.derivation_manifest_sha256,
    });
    Ok(bundle)
}

/// Read a bundle off disk and parse its manifest.
pub fn read_bundle(path: &Path) -> Result<Bundle> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "reading the application bundle {}. Build it first with `deis build --target {}`.",
            path.display(),
            DEISLET_VENDOR
        )
    })?;

    let parsed = deis_bundle::read_manifest(&bytes).with_context(|| {
        format!(
            "{} is not a readable application bundle; the control plane would refuse it too",
            path.display()
        )
    })?;

    let digest = digest_of(&bytes);
    Ok(Bundle {
        path: path.to_path_buf(),
        bytes,
        digest,
        manifest: parsed.manifest,
        lineage: None,
    })
}

fn zip_directory_files(root: &Path) -> Result<Vec<u8>> {
    let mut files = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    files.retain(|entry| entry.file_type().is_file());
    files.sort_by(|left, right| left.path().cmp(right.path()));
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    for entry in files {
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("canonical artifact transport contains a non-regular file");
        }
        let relative = entry
            .path()
            .strip_prefix(root)?
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("canonical artifact path is not UTF-8"))?
            .replace('\\', "/");
        if relative != deis_build::canonical_artifact::MANIFEST_PATH {
            deis_build::canonical_artifact::validate_portable_path(&relative)?;
        }
        writer.start_file(relative, options)?;
        writer.write_all(&fs::read(entry.path())?)?;
    }
    Ok(writer.finish()?.into_inner())
}

/// Hex sha256 of a bundle, computed the same way the control plane and every
/// node compute it.
pub fn digest_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// The digest travels to the control plane, which rejects an upload that
    /// does not hash to it, so it has to be the plain sha256 of the bytes.
    #[test]
    fn the_digest_is_the_sha256_of_the_bytes() {
        assert_eq!(
            digest_of(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_missing_bundle_says_how_to_produce_one() {
        // Matched rather than unwrapped: a `Bundle` holds the whole archive,
        // so making it printable to satisfy `unwrap_err` would put megabytes
        // of zip in a panic message.
        let Err(error) = read_bundle(Path::new("/nonexistent/bundle.zip")) else {
            panic!("a bundle that is not on disk must not read as one that is");
        };
        let error = error.to_string();
        assert!(error.contains("deis build --target deislet"), "{error}");
    }

    #[test]
    fn a_default_build_leaves_out_frozen_targets() {
        let config: deis_build::ProjectConfig = serde_json::from_value(serde_json::json!({
            "version": "1.0",
            "name": "demo",
            "runtime": "standard-v1",
            "language": "javascript",
            "vendors": {
                "deislet": {"enabled": true},
                "cloudflare": {"enabled": true},
                "deno": {"enabled": true},
                "tencent": {"enabled": true}
            }
        }))
        .unwrap();
        assert_eq!(default_targets(&config), ["deislet", "cloudflare"]);
    }

    #[test]
    fn canonical_transport_preserves_the_directory_contract() {
        let root = tempfile::tempdir().expect("canonical root");
        fs::create_dir_all(root.path().join(".edge-canon")).expect("metadata directory");
        fs::create_dir_all(root.path().join("worker")).expect("worker directory");
        fs::write(root.path().join(".edge-canon/output.json"), b"{}").expect("canonical manifest");
        fs::write(root.path().join("worker/index.js"), b"export default {}")
            .expect("worker source");

        let bytes = zip_directory_files(root.path()).expect("transport");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
        assert_eq!(archive.len(), 2);
        let mut manifest = String::new();
        archive
            .by_name(".edge-canon/output.json")
            .expect("manifest entry")
            .read_to_string(&mut manifest)
            .expect("manifest bytes");
        assert_eq!(manifest, "{}");
        assert!(archive.by_name("worker/index.js").is_ok());
    }

    #[test]
    fn provider_envelope_binds_the_exact_derivation_and_payload() {
        let root = tempfile::tempdir().expect("lineage root");
        let derivation_path = root.path().join("derivation.json");
        let derivation = br#"{"schemaVersion":1}"#;
        fs::write(&derivation_path, derivation).expect("derivation");
        let bytes = b"native payload".to_vec();
        let bundle = Bundle {
            path: root.path().join("bundle.zip"),
            digest: digest_of(&bytes),
            bytes: bytes.clone(),
            manifest: deis_bundle::Manifest::parse(
                r#"{"schema":1,"app":"demo","entry":"worker/index.js"}"#,
            )
            .expect("manifest"),
            lineage: Some(BundleLineage {
                canonical_root: root.path().to_path_buf(),
                artifact_sha256: "a".repeat(64),
                content_root_sha256: "b".repeat(64),
                derivation_manifest_path: derivation_path,
                derivation_manifest_sha256: deis_build::canonical_artifact::sha256(derivation),
            }),
        };

        let transport = bundle.provider_envelope().expect("provider envelope");
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(transport)).expect("provider zip");
        assert_eq!(archive.len(), 2);
        let mut actual_derivation = Vec::new();
        archive
            .by_name(".deislet/provider-derivation.json")
            .expect("derivation entry")
            .read_to_end(&mut actual_derivation)
            .expect("derivation bytes");
        assert_eq!(actual_derivation, derivation);
        let mut actual_payload = Vec::new();
        archive
            .by_name("payload/bundle.zip")
            .expect("payload entry")
            .read_to_end(&mut actual_payload)
            .expect("payload bytes");
        assert_eq!(actual_payload, bytes);
    }
}
