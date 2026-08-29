//! `deis build` - compile the project for one or more targets.
//!
//! The deislet target is this platform's own: it produces the application
//! bundle `deis deploy` uploads. The other targets produce what each vendor's
//! own tooling deploys, and this CLI does not deploy them.

use std::env;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use colored::Colorize;
use deis_build::Compiler;

/// The vendor name of this platform's own target.
pub const DEISLET_VENDOR: &str = "deislet";

pub async fn execute(vendors: Vec<String>, watch: bool, minify: bool) -> Result<()> {
    // Refused rather than ignored. A `--watch` that silently builds once and
    // exits looks exactly like a watch that is running and seeing no
    // changes, which is the slowest possible way to find out it does nothing.
    if watch {
        bail!(
            "`deis build --watch` is not implemented. `deis dev` rebuilds and reloads on every \
             change for this platform; for a vendor target, run `deis build --vendor <name>` from \
             your own file watcher."
        );
    }

    println!("{}", "Building project...".yellow());

    let start = Instant::now();
    let project_dir = env::current_dir()?;
    let compiler = Compiler::new();

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
}

/// Build the project for this platform and read the bundle back.
///
/// Validation runs first: a bundle that would be refused at upload is better
/// refused here, next to the code that produced it.
pub async fn deislet_bundle(project_dir: &Path, minify: bool) -> Result<Bundle> {
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

    let result = compiler
        .compile(project_dir, vec![DEISLET_VENDOR.to_string()], minify)
        .await
        .context("building the application bundle")?;

    let build = result.builds.get(DEISLET_VENDOR).ok_or_else(|| {
        anyhow::anyhow!(
            "the build produced no {DEISLET_VENDOR} target. Enable it in .config.json:\n\n  \
             \"vendors\": {{ \"deislet\": {{ \"enabled\": true }} }}"
        )
    })?;

    let path = Path::new(&build.output_path).join(deis_build::adapter_deislet::BUNDLE_FILE_NAME);
    read_bundle(&path)
}

/// Read a bundle off disk and parse its manifest.
pub fn read_bundle(path: &Path) -> Result<Bundle> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "reading the application bundle {}. Build it first with `deis build --vendor {}`.",
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
    })
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
        assert!(error.contains("deis build --vendor deislet"), "{error}");
    }
}
