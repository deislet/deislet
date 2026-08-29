//! The Deislet developer CLI.
//!
//! Two halves that deliberately do not mix:
//!
//! * `init` / `dev` / `validate` / `build` work on the project on disk and
//!   need no platform at all. `build --vendor cloudflare|deno|tencent`
//!   produces what that vendor's own tooling deploys; this CLI never pushes
//!   to them.
//! * `deploy` / `versions` / `rollback` / `status` / `secret` / `logs` /
//!   `tail` talk to a Deislet platform - a `deis-control` and, for logs, a
//!   `deis-telemetry`. Where those live and which credential to present is
//!   [`settings`].
//!
//! Flag names are the same everywhere they mean the same thing. In
//! particular the environment is always `-e, --environment` and never
//! `--env`, because "env" in this codebase already means an application's
//! variables (`context.env`), and one flag standing for both would be read
//! wrong by everyone exactly once.

use std::process::ExitCode;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use colored::Colorize;

mod commands;
mod dev;
mod platform;
mod quiet;
mod render;
mod settings;

use render::Output;

#[derive(Parser)]
#[command(name = "deis")]
#[command(about = "Build, deploy and inspect Deislet applications", long_about = None)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// How a command that answers with data should print it.
///
/// Declared on each such command rather than globally, so `--help` offers it
/// exactly where it does something: a flag that is accepted and ignored is a
/// flag someone will build a script on.
#[derive(Args, Clone, Copy, Debug, Default)]
struct Format {
    /// Print machine-readable JSON instead of the human report.
    ///
    /// `deis tail` prints one JSON object per line, because its output has
    /// no end; every other command prints one document and nothing else.
    #[arg(long)]
    json: bool,
}

impl From<Format> for Output {
    fn from(format: Format) -> Self {
        Output { json: format.json }
    }
}

/// Which application and environment a command is about.
///
/// Both fall back through the environment variables and the project file
/// (see [`settings`]), so a project that names itself in `deis.toml` needs
/// neither flag.
#[derive(Args, Clone, Debug, Default)]
struct Target {
    /// Application to act on.
    #[arg(short = 'a', long, value_name = "APP")]
    app: Option<String>,

    /// Environment to act on. Not `--env`: that would read as the
    /// application's variables.
    #[arg(short = 'e', long, value_name = "NAME")]
    environment: Option<String>,
}

/// The same, for a command that is about a whole application.
#[derive(Args, Clone, Debug, Default)]
struct AppTarget {
    /// Application to act on.
    #[arg(short = 'a', long, value_name = "APP")]
    app: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new project.
    Init {
        /// Project name; also the directory it is created in.
        #[arg(default_value = "edge-app")]
        name: String,

        /// Template to use. Only "default" exists; any other name is
        /// refused rather than quietly replaced by it.
        #[arg(short, long, default_value = "default")]
        template: String,
    },

    /// Run the project locally, in a real deis-runtime.
    ///
    /// The bundle is built and handed to a deis-runtime this command starts,
    /// with the same call the control plane makes, so routing, assets,
    /// `context.params` and `context.env` behave as they will in production.
    Dev {
        /// Port to listen on.
        #[arg(short, long, default_value = "8000")]
        port: u16,

        /// Port for the runtime behind it. Chosen by the OS when unset.
        #[arg(long, value_name = "PORT")]
        runtime_port: Option<u16>,

        /// Which deis-runtime binary to start. Found next to this `deis`,
        /// or on PATH, when unset.
        #[arg(long, value_name = "PATH")]
        runtime: Option<std::path::PathBuf>,

        /// Environment name the application runs under locally. Not
        /// `production`, so a `--store` pointed at a real deis-store cannot
        /// write into the production tenant by accident.
        #[arg(short = 'e', long, default_value = "development")]
        environment: String,

        /// Do not rebuild when a file changes.
        #[arg(long)]
        no_watch: bool,

        /// Print the runtime's debug log lines as well.
        #[arg(long)]
        debug: bool,

        /// Minify the bundle, as a deploy does. Off by default so builds are
        /// fast and stack traces point at real lines.
        #[arg(long)]
        minify: bool,

        /// deis-store address, if one is running. Without it every
        /// ctx.services.{kv,database,blob} call fails.
        #[arg(long, value_name = "URL")]
        store: Option<String>,

        /// deis-queue address, if one is running.
        #[arg(long, value_name = "URL")]
        queue: Option<String>,

        /// A secret value for this session, as NAME=VALUE. Repeatable.
        #[arg(short = 's', long = "secret", value_name = "NAME=VALUE")]
        secrets: Vec<String>,
    },

    /// Check the project against the specification.
    Validate {
        /// Treat warnings as errors.
        #[arg(long)]
        strict: bool,
    },

    /// Compile the project for one or more targets.
    Build {
        /// Targets: deislet, cloudflare, deno, tencent, or all.
        #[arg(short, long, default_values_t = vec!["all".to_string()])]
        vendor: Vec<String>,

        /// Not implemented, and refused rather than ignored: `deis dev`
        /// is what rebuilds and reloads on every change.
        #[arg(long)]
        watch: bool,

        /// Keep the output unminified.
        #[arg(long)]
        no_minify: bool,
    },

    /// Build, upload as a version, and point an environment at it.
    ///
    /// This deploys to this platform. For Cloudflare, Deno or EdgeOne run
    /// `deis build --vendor <name>` and then that vendor's own tool.
    Deploy {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        format: Format,

        /// Note recorded with the deployment, for the history.
        #[arg(long)]
        note: Option<String>,

        /// Deploy this bundle instead of building one.
        #[arg(long, value_name = "PATH", conflicts_with = "no_build")]
        bundle: Option<std::path::PathBuf>,

        /// Use the bundle the last build left behind.
        #[arg(long)]
        no_build: bool,

        /// Keep the bundle unminified.
        #[arg(long)]
        no_minify: bool,
    },

    /// List an application's versions and which environments run them.
    Versions {
        #[command(flatten)]
        target: AppTarget,

        #[command(flatten)]
        format: Format,

        /// Show at most this many, newest first.
        #[arg(long, default_value = "20")]
        limit: usize,
    },

    /// Move an environment's pointer back to an earlier version.
    Rollback {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        format: Format,

        /// Version number to go back to. Omitted means the previous
        /// distinct version this environment ran.
        #[arg(long, value_name = "N")]
        to: Option<i64>,

        /// Note recorded with the rollback.
        #[arg(long)]
        note: Option<String>,
    },

    /// What each environment of an application runs, and which nodes serve it.
    Status {
        #[command(flatten)]
        target: AppTarget,

        #[command(flatten)]
        format: Format,
    },

    /// Follow an application's logs until interrupted.
    Tail {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        format: Format,

        /// Minimum severity: trace, debug, info, warn, error, fatal.
        #[arg(short, long, value_name = "LEVEL")]
        level: Option<String>,

        /// Do not filter by environment.
        #[arg(long)]
        all_environments: bool,
    },

    /// Read stored logs for a time range.
    Logs {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        format: Format,

        /// Minimum severity: trace, debug, info, warn, error, fatal.
        #[arg(short, long, value_name = "LEVEL")]
        level: Option<String>,

        /// Start of the range: a duration back from now (30s, 15m, 2h, 3d)
        /// or an RFC 3339 instant.
        #[arg(long, default_value = "1h", value_name = "WHEN")]
        since: String,

        /// End of the range, same forms. Omitted means now.
        #[arg(long, value_name = "WHEN")]
        until: Option<String>,

        /// Only lines whose message contains this, case-insensitively.
        #[arg(long, value_name = "TEXT")]
        contains: Option<String>,

        /// Only lines carrying this trace id.
        #[arg(long, value_name = "ID")]
        trace_id: Option<String>,

        /// Show at most this many, newest first.
        #[arg(long, default_value = "200")]
        limit: usize,

        /// Do not filter by environment.
        #[arg(long)]
        all_environments: bool,
    },

    /// Manage an environment's secrets. Values go in and never come back.
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },

    /// Show or change where the platform is and which credential to use.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum SecretAction {
    /// Store a secret. The value is read from stdin when it is not given,
    /// which keeps it out of the shell history.
    Set {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        format: Format,

        /// Secret name, as `context.env` sees it.
        name: String,

        /// Secret value. Omit it to be prompted, or to pipe it in.
        value: Option<String>,
    },

    /// List the secret names of an environment, with when each was last set.
    /// There is no command that prints a value: the control plane has no
    /// endpoint that returns one.
    List {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        format: Format,
    },

    /// Remove a secret.
    Rm {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        format: Format,

        /// Secret name.
        name: String,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Set one setting. `control.token` goes to the per-user credentials
    /// file; everything else goes to the project file.
    Set {
        /// One of: control.endpoint, control.token, telemetry.endpoint,
        /// defaults.app, defaults.environment.
        key: String,
        /// Value to store.
        value: String,

        #[command(flatten)]
        format: Format,
    },

    /// Print one setting and where it came from.
    Get {
        /// Setting name, as in `deis config set`.
        key: String,

        #[command(flatten)]
        format: Format,
    },

    /// Remove one setting from the file that holds it.
    Unset {
        /// Setting name, as in `deis config set`.
        key: String,

        #[command(flatten)]
        format: Format,
    },

    /// Show every setting, where it came from, and which files hold them.
    List {
        #[command(flatten)]
        format: Format,
    },
}

fn main() -> ExitCode {
    // Progress and diagnostics belong on stderr: stdout is where `--json`
    // puts the one document a script reads.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("{} could not start an async runtime: {e}", "error:".red());
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{} {e}", "error:".red().bold());
            for cause in e.chain().skip(1) {
                eprintln!("  {} {cause}", "caused by".dimmed());
            }
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Init { name, template } => commands::init::execute(name, template).await,

        Commands::Dev {
            port,
            runtime_port,
            runtime,
            environment,
            no_watch,
            debug,
            minify,
            store,
            queue,
            secrets,
        } => {
            commands::dev::execute(commands::dev::Args {
                port,
                runtime_port,
                runtime,
                environment,
                watch: !no_watch,
                debug,
                minify,
                store,
                queue,
                secrets,
            })
            .await
        }

        Commands::Validate { strict } => commands::validate::execute(strict).await,

        Commands::Build {
            vendor,
            watch,
            no_minify,
        } => commands::build::execute(vendor, watch, !no_minify).await,

        Commands::Deploy {
            target,
            format,
            note,
            bundle,
            no_build,
            no_minify,
        } => {
            commands::deploy::execute(
                commands::deploy::Args {
                    app: target.app,
                    environment: target.environment,
                    note,
                    bundle,
                    no_build,
                    no_minify,
                },
                format.into(),
            )
            .await
        }

        Commands::Versions {
            target,
            format,
            limit,
        } => commands::versions::execute(target.app, limit, format.into()).await,

        Commands::Rollback {
            target,
            format,
            to,
            note,
        } => {
            commands::rollback::execute(
                commands::rollback::Args {
                    app: target.app,
                    environment: target.environment,
                    to,
                    note,
                },
                format.into(),
            )
            .await
        }

        Commands::Status { target, format } => {
            commands::status::execute(target.app, format.into()).await
        }

        Commands::Tail {
            target,
            format,
            level,
            all_environments,
        } => {
            commands::logs::tail(
                commands::logs::TailArgs {
                    app: target.app,
                    environment: target.environment,
                    level,
                    all_environments,
                },
                format.into(),
            )
            .await
        }

        Commands::Logs {
            target,
            format,
            level,
            since,
            until,
            contains,
            trace_id,
            limit,
            all_environments,
        } => {
            commands::logs::query(
                commands::logs::QueryArgs {
                    app: target.app,
                    environment: target.environment,
                    level,
                    since,
                    until,
                    contains,
                    trace_id,
                    limit,
                    all_environments,
                },
                format.into(),
            )
            .await
        }

        Commands::Secret { action } => match action {
            SecretAction::Set {
                target,
                format,
                name,
                value,
            } => {
                commands::secret::set(target.app, target.environment, name, value, format.into())
                    .await
            }
            SecretAction::List { target, format } => {
                commands::secret::list(target.app, target.environment, format.into()).await
            }
            SecretAction::Rm {
                target,
                format,
                name,
            } => {
                commands::secret::remove(target.app, target.environment, name, format.into()).await
            }
        },

        Commands::Config { action } => match action {
            ConfigAction::Set { key, value, format } => {
                commands::config::set(&key, &value, format.into())
            }
            ConfigAction::Get { key, format } => commands::config::get(&key, format.into()),
            ConfigAction::Unset { key, format } => commands::config::unset(&key, format.into()),
            ConfigAction::List { format } => commands::config::list(format.into()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    /// The environment is `-e/--environment` on every command that has one.
    /// `--env` must not be accepted anywhere: it reads as the application's
    /// variables, which is a different thing entirely.
    #[test]
    fn the_environment_flag_has_one_spelling_everywhere() {
        for argv in [
            vec!["deis", "deploy", "-e", "preview"],
            vec!["deis", "rollback", "--environment", "preview"],
            vec!["deis", "tail", "-e", "preview"],
            vec!["deis", "logs", "--environment", "preview"],
            vec!["deis", "secret", "list", "-e", "preview"],
        ] {
            Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }

        for argv in [
            vec!["deis", "deploy", "--env", "preview"],
            vec!["deis", "tail", "--env", "preview"],
        ] {
            assert!(
                Cli::try_parse_from(&argv).is_err(),
                "{argv:?}: --env must not be accepted; it reads as context.env"
            );
        }
    }

    /// Deploying targets this platform only. A `--vendor` flag here would
    /// have suggested it could push to Cloudflare or Deno, which it cannot.
    #[test]
    fn deploy_takes_no_vendor() {
        assert!(
            Cli::try_parse_from(["deis", "deploy", "--vendor", "cloudflare"]).is_err(),
            "deploy must not accept --vendor"
        );
        assert!(
            Cli::try_parse_from(["deis", "build", "--vendor", "cloudflare"]).is_ok(),
            "build is where a vendor is chosen"
        );
    }

    /// `--json` is offered by the commands that answer with data, and by no
    /// others: a flag accepted and ignored is a flag someone writes a script
    /// against.
    #[test]
    fn json_is_offered_exactly_where_it_is_honoured() {
        for argv in [
            vec!["deis", "versions", "--json"],
            vec!["deis", "status", "--json"],
            vec!["deis", "deploy", "--json"],
            vec!["deis", "rollback", "--json"],
            vec!["deis", "logs", "--json"],
            vec!["deis", "tail", "--json"],
            vec!["deis", "secret", "list", "--json"],
            vec!["deis", "config", "list", "--json"],
        ] {
            Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }

        for argv in [
            vec!["deis", "init", "--json"],
            vec!["deis", "dev", "--json"],
            vec!["deis", "build", "--json"],
            vec!["deis", "validate", "--json"],
        ] {
            assert!(
                Cli::try_parse_from(&argv).is_err(),
                "{argv:?}: this command has no JSON form, so the flag must not be accepted"
            );
        }
    }
}
