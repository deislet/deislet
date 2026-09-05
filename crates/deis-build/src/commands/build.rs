use crate::compiler::Compiler;
use anyhow::{bail, Result};
use colored::Colorize;
use std::env;
use std::time::Instant;

pub async fn execute(vendors: Vec<String>, watch: bool, minify: bool) -> Result<()> {
    // Refused rather than ignored, the same way `deis build --watch` refuses
    // it. This binary took the flag as `_watch`, built once and exited, which
    // looks exactly like a watcher that is running and seeing no changes - the
    // slowest possible way to discover that nothing is rebuilding.
    if watch {
        bail!(
            "`deis-build build --watch` is not implemented. `deis dev` rebuilds and reloads on every \
             change for this platform; for a vendor target, run `deis-build build --vendor <name>` \
             from your own file watcher."
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
        // The errors themselves, not only that there were some. This used to
        // print two lines that named nothing, so an author whose build stopped
        // here had to guess which file, and then run `deis-build validate`
        // just to be told - which is the same work twice for information this
        // command already had in its hand.
        println!("\n{}", "Validation failed".red().bold());
        for error in &validation.errors {
            println!("\n  {}:{}", error.file.red(), error.line.to_string().red());
            println!("  {}: {}", "Error".red(), error.message.red());
            if let Some(suggestion) = &error.suggestion {
                println!("  {}: {}", "Suggestion".cyan(), suggestion.cyan());
            }
        }
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
