use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;
mod dev;

#[derive(Parser)]
#[command(name = "denictl")]
#[command(about = "Universal Edge Function CLI - Write once, deploy anywhere", long_about = None)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new Edge Function project
    Init {
        /// Project name
        #[arg(default_value = "edge-app")]
        name: String,

        /// Template to use
        #[arg(short, long, default_value = "default")]
        template: String,
    },

    /// Start local development server
    Dev {
        /// Port to listen on
        #[arg(short, long, default_value = "8000")]
        port: u16,

        /// Disable file watching
        #[arg(long)]
        no_watch: bool,

        /// Enable debug logging
        #[arg(long)]
        debug: bool,
    },

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

    /// Deploy to target platforms
    Deploy {
        /// Target vendors
        #[arg(short, long, default_values_t = vec!["all".to_string()])]
        vendor: Vec<String>,

        /// Environment (staging or production)
        #[arg(short, long, default_value = "staging")]
        env: String,

        /// Version tag
        #[arg(short, long)]
        tag: Option<String>,

        /// Promote to production after deploy
        #[arg(long)]
        promote: bool,
    },

    /// Manage CLI configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Set configuration value
    Set {
        /// Configuration key
        key: String,
        /// Configuration value
        value: String,
    },
    /// Get configuration value
    Get {
        /// Configuration key
        key: String,
    },
    /// List all configuration
    List,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init { name, template } => {
            commands::init::execute(name, template).await?;
        }
        Commands::Dev {
            port,
            no_watch,
            debug,
        } => {
            commands::dev::execute(port, !no_watch, debug).await?;
        }
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
        Commands::Deploy {
            vendor,
            env,
            tag,
            promote,
        } => {
            commands::deploy::execute(vendor, env, tag, promote).await?;
        }
        Commands::Config { action } => match action {
            ConfigAction::Set { key, value } => {
                commands::config::set(key, value).await?;
            }
            ConfigAction::Get { key } => {
                commands::config::get(key).await?;
            }
            ConfigAction::List => {
                commands::config::list().await?;
            }
        },
    }

    Ok(())
}
