use anyhow::Result;
use clap::{Parser, Subcommand};

// The binary drives the library rather than declaring `mod compiler;` and
// friends of its own. Doing both compiled the whole compiler twice - once into
// `deis_build` and once into this binary - so anything the CLI happened not to
// call was dead code in one of the two copies and live in the other. That is
// what the `#[allow(dead_code)]` scattered over `CompilerOptions` was hiding,
// and hiding it is how two fields nobody read survived.
use deis_build::commands;

#[derive(Parser)]
#[command(name = "deis-build")]
#[command(about = "Universal Edge Function Compiler - Transform standard code to platform-specific deployables", long_about = None)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Validate project against specification
    Validate {
        /// Enable strict mode
        #[arg(long)]
        strict: bool,
    },

    /// Build project for target platforms
    Build {
        /// Target vendors (cloudflare, deno, tencent, deislet, or all)
        #[arg(short, long, default_values_t = vec!["all".to_string()])]
        vendor: Vec<String>,

        /// Watch for changes and rebuild
        #[arg(long)]
        watch: bool,

        /// Disable minification
        #[arg(long)]
        no_minify: bool,
    },

    /// Print the Node/npm build ceilings enforced before and during install
    NodeNpmLimits,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Validate { strict } => {
            commands::validate::execute(strict).await?;
        }
        Commands::Build {
            vendor,
            watch,
            no_minify,
        } => {
            commands::build::execute(vendor, watch, !no_minify).await?;
        }
        Commands::NodeNpmLimits => {
            println!(
                "{}",
                serde_json::to_string_pretty(&deis_build::node_npm_build_limits())?
            );
        }
    }

    Ok(())
}
