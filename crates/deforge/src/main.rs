use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;
mod compiler;
mod parser;

#[derive(Parser)]
#[command(name = "deforge")]
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
        /// Target vendors (cloudflare, deno, tencent, or all)
        #[arg(short, long, default_values_t = vec!["all".to_string()])]
        vendor: Vec<String>,

        /// Watch for changes and rebuild
        #[arg(long)]
        watch: bool,

        /// Disable minification
        #[arg(long)]
        no_minify: bool,
    },
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
    }

    Ok(())
}
