use anyhow::Result;
use colored::Colorize;
use deis_build::Compiler;
use std::env;

pub async fn execute(strict: bool) -> Result<()> {
    println!("{}", "Validating project...".yellow());

    let project_dir = env::current_dir()?;
    let compiler = Compiler::new();

    let result = compiler.validate(&project_dir, strict)?;

    if result.is_valid() {
        println!("\n{}", "✓ Validation passed".green().bold());

        if !result.warnings.is_empty() {
            println!(
                "\n{}",
                format!("⚠ {} warning(s):", result.warnings.len()).yellow()
            );
            for warning in &result.warnings {
                println!(
                    "\n  {}:{}",
                    warning.file.yellow(),
                    warning.line.to_string().yellow()
                );
                println!("  {}", warning.message.dimmed());
                if let Some(suggestion) = &warning.suggestion {
                    println!("  {}: {}", "Suggestion".cyan(), suggestion.cyan());
                }
            }
        }

        println!("\n{}", "✓ Project structure valid".green());
        println!("{}", "✓ .config.json valid".green());
        println!("{}", "✓ All handlers valid".green());
    } else {
        println!("\n{}", "✗ Validation failed".red().bold());

        println!(
            "\n{}",
            format!(
                "✗ {} error(s), {} warning(s)",
                result.errors.len(),
                result.warnings.len()
            )
            .red()
        );

        if !result.errors.is_empty() {
            println!("\n{}", "Errors:".red().bold());
            for error in &result.errors {
                println!("\n  {}:{}", error.file.red(), error.line.to_string().red());
                println!("  {}: {}", "Error".red(), error.message.red());
                if let Some(suggestion) = &error.suggestion {
                    println!("  {}: {}", "Suggestion".cyan(), suggestion.cyan());
                }
            }
        }

        println!("\n{}", "Fix the errors above and run validate again.".red());
        std::process::exit(1);
    }

    Ok(())
}
