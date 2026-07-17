//! Command Line Interface for Deflux
//!
//! This module handles all command line argument parsing and related operations.

use anyhow::{anyhow, Context, Result};
use std::env;
use std::path::PathBuf;
use std::process;

/// CLI configuration structure
#[derive(Debug, Clone)]
pub struct CliConfig {
    /// Configuration file path
    pub config_file: PathBuf,
    /// Whether to run as a daemon
    pub daemon: bool,
    /// Action to perform
    pub action: CliAction,
}

/// Available CLI actions
#[derive(Debug, Clone)]
pub enum CliAction {
    /// Start the server normally
    Start,
    /// Print help information
    Help,
    /// Print version information
    Version,
    /// Test configuration file syntax
    TestConfig,
    /// Send signal to main process
    Signal(SignalType),
}

/// Signal types for process control
#[derive(Debug, Clone)]
pub enum SignalType {
    /// Fast shutdown - immediately terminate all processes
    Stop,
    /// Graceful shutdown - wait for requests to complete
    Quit,
    /// Hot reload configuration - start new process, graceful switch
    Reload,
    /// Reopen log files - for log rotation
    Reopen,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            config_file: PathBuf::from("config.toml"),
            daemon: false,
            action: CliAction::Start,
        }
    }
}

impl CliConfig {
    /// Parse command line arguments
    pub fn parse() -> Result<Self> {
        let args: Vec<String> = env::args().collect();
        let mut config = Self::default();
        
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "-?" | "-h" | "--help" => {
                    config.action = CliAction::Help;
                    return Ok(config);
                }
                "-v" | "--version" => {
                    config.action = CliAction::Version;
                    return Ok(config);
                }
                "-t" | "--test" => {
                    config.action = CliAction::TestConfig;
                }
                "-d" | "--daemon" => {
                    config.daemon = true;
                }
                "-c" | "--config" => {
                    i += 1;
                    if i >= args.len() {
                        return Err(anyhow!("Option -c requires a configuration file path"));
                    }
                    config.config_file = PathBuf::from(&args[i]);
                }
                "-s" | "--signal" => {
                    i += 1;
                    if i >= args.len() {
                        return Err(anyhow!("Option -s requires a signal name"));
                    }
                    let signal = Self::parse_signal(&args[i])?;
                    config.action = CliAction::Signal(signal);
                    return Ok(config);
                }
                arg if arg.starts_with('-') => {
                    return Err(anyhow!("Unknown option: {}", arg));
                }
                // Reject any positional arguments
                positional_arg => {
                    return Err(anyhow!("Unexpected argument: {}. Use -c to specify config file.", positional_arg));
                }
            }
            i += 1;
        }

        Ok(config)
    }

    /// Parse signal type from string
    fn parse_signal(signal: &str) -> Result<SignalType> {
        match signal.to_lowercase().as_str() {
            "stop" => Ok(SignalType::Stop), // SIGTERM stop, kill -15, will exit immediately
            "quit" => Ok(SignalType::Quit), // SIGQUIT quit, kill -3, will wait for requests to complete
            "reload" => Ok(SignalType::Reload), // SIGUSR1 reload, kill -10, will reload configuration
            "reopen" => Ok(SignalType::Reopen), // SIGUSR2 reopen, kill -12, will reopen log files
            _ => Err(anyhow!(
                "Invalid signal '{}'. Valid signals: stop, quit, reload, reopen",
                signal
            )),
        }
    }

    /// Execute the CLI action
    pub async fn execute(&self) -> Result<()> {
        match &self.action {
            CliAction::Help => {
                Self::print_help();
                process::exit(0);
            }
            CliAction::Version => {
                Self::print_version();
                process::exit(0);
            }
            CliAction::TestConfig => {
                self.test_config().await?;
                process::exit(0);
            }
            CliAction::Signal(signal) => {
                self.send_signal(signal).await?;
                process::exit(0);
            }
            CliAction::Start => {
                // Normal startup - no action needed here
                Ok(())
            }
        }
    }

    /// Print help information
    fn print_help() {
        println!("Deflux - Managed Load Balancer Server v{}", env!("CARGO_PKG_VERSION"));
        println!();
        println!("USAGE:");
        println!("    deflux [OPTIONS]");
        println!();
        println!("OPTIONS:");
        println!("    -?, -h, --help         Print this help message");
        println!("    -v, --version          Print version information");
        println!("    -d, --daemon           Run as a daemon (in the background)");
        println!("    -c, --config <FILE>    Specify configuration file (default: config.toml)");
        println!("    -t, --test             Test configuration file syntax and exit");
        println!("    -s, --signal <SIGNAL>  Send signal to main process");
        println!();
        println!("SIGNALS:");
        println!("    stop      Fast shutdown - immediately terminate all processes");
        println!("    quit      Graceful shutdown - wait for requests to complete");
        println!("    reload    Hot reload configuration - start new process, graceful switch");
        println!("    reopen    Reopen log files - for log rotation");
        println!();
        println!("EXAMPLES:");
        println!("    deflux");
        println!("    deflux -c /etc/deflux/config.toml");
        println!("    deflux -t -c /etc/deflux/config.toml");
        println!("    deflux -s reload");
        println!("    deflux -s quit");
    }

    /// Print version information
    fn print_version() {
        println!("Deflux v{}", env!("CARGO_PKG_VERSION"));
        // Note: RUSTC_VERSION and TARGET are not always available at compile time
        #[cfg(debug_assertions)]
        println!("Debug build");
        #[cfg(not(debug_assertions))]
        println!("Release build");
    }

    /// Test configuration file syntax
    async fn test_config(&self) -> Result<()> {
        use crate::config::LocalConfig;
        
        println!("Testing configuration file: {}", self.config_file.display());
        
        // Test if file exists
        if !self.config_file.exists() {
            return Err(anyhow!(
                "Configuration file does not exist: {}",
                self.config_file.display()
            ));
        }

        // Try to parse the configuration file
        let _config = LocalConfig::from_toml(self.config_file.clone())
            .with_context(|| {
                format!(
                    "Failed to parse configuration file: {}",
                    self.config_file.display()
                )
            })?;

        println!("Configuration file syntax is OK");
        Ok(())
    }

    /// Send signal to main process
    async fn send_signal(&self, signal: &SignalType) -> Result<()> {
        use std::fs;

        // Try to read PID file (common convention)
        let pid_file = if PathBuf::from("/var/run/deflux.pid").exists() {
            PathBuf::from("/var/run/deflux.pid")
        } else {
            PathBuf::from("deflux.pid")
        };

        if !pid_file.exists() {
            return Err(anyhow!(
                "PID file not found: {}. Is Deflux running?",
                pid_file.display()
            ));
        }

        let pid_str = fs::read_to_string(&pid_file)
            .with_context(|| format!("Failed to read PID file: {}", pid_file.display()))?;

        let pid: u32 = pid_str.trim().parse()
            .with_context(|| format!("Invalid PID in file: {}", pid_str.trim()))?;

        println!("Sending {} signal to process {}", Self::signal_name(signal), pid);

        // On Unix systems, we would use the signal syscall
        // For now, this is a placeholder implementation
        #[cfg(unix)]
        {
            self.send_unix_signal(pid, signal).await?;
        }

        #[cfg(not(unix))]
        {
            return Err(anyhow!("Signal sending is only supported on Unix systems"));
        }

        println!("Signal sent successfully");
        Ok(())
    }

    /// Send Unix signal to process
    #[cfg(unix)]
    async fn send_unix_signal(&self, pid: u32, signal: &SignalType) -> Result<()> {
        use nix::sys::signal::{self, Signal};
        use nix::unistd::Pid;

        let unix_signal = match signal {
            SignalType::Stop => Signal::SIGTERM,
            SignalType::Quit => Signal::SIGQUIT,
            SignalType::Reload => Signal::SIGUSR1, // Common convention for reload
            SignalType::Reopen => Signal::SIGUSR2, // Common convention for reopen
        };

        let pid = Pid::from_raw(pid as i32);
        signal::kill(pid, unix_signal)
            .with_context(|| format!("Failed to send signal to process {}", pid))?;

        Ok(())
    }

    /// Get human-readable signal name
    fn signal_name(signal: &SignalType) -> &'static str {
        match signal {
            SignalType::Stop => "STOP",
            SignalType::Quit => "QUIT",
            SignalType::Reload => "RELOAD",
            SignalType::Reopen => "REOPEN",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signal_parsing() {
        assert!(matches!(
            CliConfig::parse_signal("stop").unwrap(),
            SignalType::Stop
        ));
        assert!(matches!(
            CliConfig::parse_signal("QUIT").unwrap(),
            SignalType::Quit
        ));
        assert!(matches!(
            CliConfig::parse_signal("reload").unwrap(),
            SignalType::Reload
        ));
        assert!(matches!(
            CliConfig::parse_signal("reopen").unwrap(),
            SignalType::Reopen
        ));
        
        assert!(CliConfig::parse_signal("invalid").is_err());
    }

    #[test]
    fn test_default_config() {
        let config = CliConfig::default();
        assert_eq!(config.config_file, PathBuf::from("config.toml"));
        assert!(matches!(config.action, CliAction::Start));
    }
} 