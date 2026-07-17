use crate::compiler::Compiler;
use anyhow::Result;
use colored::Colorize;
use std::env;
use std::time::Instant;

pub async fn execute(vendors: Vec<String>, _watch: bool, minify: bool) -> Result<()> {
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
