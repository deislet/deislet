/// Memory mode example - demonstrates how to obtain generated code without writing files
///
/// Usage:
/// ```bash
/// cargo run --example memory_mode_example
/// ```
use deis_build::{BuildArtifacts, Compiler, CompilerOptions, OutputMode};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("=== Build Memory Mode Example ===\n");

    // Example 1: memory-only mode
    println!("[Example 1] Memory-only mode - no files written");
    println!("{}", "-".repeat(50));

    let options = CompilerOptions {
        project_dir: "./examples/hello-world".to_string(),
        target_vendor: Some("cloudflare".to_string()),
        output_mode: OutputMode::Memory, // Return in-memory content only
        ..Default::default()
    };

    let compiler = Compiler::with_options(options);
    match compiler.build().await {
        Ok(result) => {
            if let Some(build) = result.builds.get("cloudflare") {
                println!("✓ Compilation succeeded (memory mode)");
                println!("  File output path: {}", build.output_path);
                println!("  (Note: no files were actually written)");

                if let Some(artifacts) = &build.artifacts {
                    display_artifacts(artifacts);
                } else {
                    println!("  ✗ No artifact content received");
                }
            }
        }
        Err(e) => {
            println!("✗ Compilation failed: {}", e);
            println!("  (The example project may not exist; this is expected)");
        }
    }

    println!("\n");

    // Example 2: write files and return in-memory content at the same time
    println!("[Example 2] Both mode - write files and return in-memory content");
    println!("{}", "-".repeat(50));

    let options = CompilerOptions {
        project_dir: "./examples/hello-world".to_string(),
        target_vendor: Some("deno".to_string()),
        output_mode: OutputMode::Both,
        ..Default::default()
    };

    let compiler = Compiler::with_options(options);
    match compiler.build().await {
        Ok(result) => {
            if let Some(build) = result.builds.get("deno") {
                println!("✓ Compilation succeeded (Both mode)");
                println!("  Files written to: {}", build.output_path);

                if let Some(artifacts) = &build.artifacts {
                    display_artifacts(artifacts);

                    // The in-memory code can be processed further
                    println!("\n  💡 The in-memory code can be used for:");
                    println!("     - Uploading to a CDN");
                    println!("     - Sending to a build service");
                    println!("     - Post-processing analysis");
                    println!("     - ...");
                }
            }
        }
        Err(e) => {
            println!("✗ Compilation failed: {}", e);
            println!("  (The example project may not exist; this is expected)");
        }
    }

    Ok(())
}

/// Display detailed information about BuildArtifacts
fn display_artifacts(artifacts: &BuildArtifacts) {
    println!("\n  📦 Build artifacts:");
    println!("     Entry file: {}", artifacts.entry_filename);
    println!("     Entry code: {} bytes", artifacts.entry_code.len());

    if !artifacts.config_files.is_empty() {
        println!("\n  📄 Config files ({}):", artifacts.config_files.len());
        for (name, content) in &artifacts.config_files {
            println!("     - {}: {} bytes", name, content.len());
        }
    }

    if !artifacts.source_files.is_empty() {
        println!("\n  📝 Source files ({}):", artifacts.source_files.len());
        for (path, content) in artifacts.source_files.iter().take(5) {
            println!("     - {}: {} bytes", path, content.len());
        }
        if artifacts.source_files.len() > 5 {
            println!("     ... {} more files", artifacts.source_files.len() - 5);
        }
    }

    if !artifacts.system_files.is_empty() {
        println!("\n  🔧 System files ({}):", artifacts.system_files.len());
        for (path, content) in &artifacts.system_files {
            println!("     - {}: {} bytes", path, content.len());
        }
    }
}
