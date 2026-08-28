//! Command line interface.

use std::env;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::process;

use anyhow::{Context, Result, anyhow};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::config::{ConfigManager, SystemConfig};

#[derive(Debug, Clone)]
pub struct CliConfig {
    /// Configuration file path, made absolute at parse time so that log
    /// lines and relaunches never depend on the working directory.
    pub config_file: PathBuf,
    /// Detach and run in the background.
    pub daemon: bool,
    /// Take over the listening sockets of a running instance.
    pub upgrade: bool,
    /// Inherited pipe to write one byte to once startup has succeeded and
    /// the upgrade socket is about to be opened. Set by the process that
    /// spawned this one for an upgrade; see [`crate::restart`].
    pub ready_fd: Option<RawFd>,
    pub action: CliAction,
}

#[derive(Debug, Clone)]
pub enum CliAction {
    Start,
    Help,
    Version,
    /// Parse the configuration file and exit.
    TestConfig,
    /// Signal the running instance named by the configuration's PID file.
    Signal(SignalType),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalType {
    /// Exit immediately, dropping in-flight requests.
    Stop,
    /// Stop accepting connections, exit once in-flight requests finish.
    Quit,
    /// Start a replacement process and hand the listening sockets to it.
    Reload,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            config_file: PathBuf::from("config.toml"),
            daemon: false,
            upgrade: false,
            ready_fd: None,
            action: CliAction::Start,
        }
    }
}

impl CliConfig {
    pub fn parse() -> Result<Self> {
        Self::parse_from(env::args().skip(1))
    }

    pub fn parse_from<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut config = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_ref() {
                "-?" | "-h" | "--help" => {
                    config.action = CliAction::Help;
                    return Ok(config);
                }
                "-v" | "--version" => {
                    config.action = CliAction::Version;
                    return Ok(config);
                }
                "-t" | "--test" => config.action = CliAction::TestConfig,
                "-d" | "--daemon" => config.daemon = true,
                "-u" | "--upgrade" => config.upgrade = true,
                "-c" | "--config" => {
                    let path = args
                        .next()
                        .ok_or_else(|| anyhow!("option -c requires a configuration file path"))?;
                    config.config_file = PathBuf::from(path.as_ref());
                }
                "-s" | "--signal" => {
                    let name = args
                        .next()
                        .ok_or_else(|| anyhow!("option -s requires a signal name"))?;
                    config.action = CliAction::Signal(Self::parse_signal(name.as_ref())?);
                }
                "--ready-fd" => {
                    let fd = args
                        .next()
                        .ok_or_else(|| anyhow!("option --ready-fd requires a descriptor"))?;
                    let fd: RawFd = fd
                        .as_ref()
                        .parse()
                        .map_err(|_| anyhow!("invalid --ready-fd value: {}", fd.as_ref()))?;
                    config.ready_fd = Some(fd);
                }
                other if other.starts_with('-') => return Err(anyhow!("unknown option: {other}")),
                other => {
                    return Err(anyhow!(
                        "unexpected argument: {other}; use -c to specify the config file"
                    ));
                }
            }
        }
        // Absolute, but without resolving symlinks: a deployment that
        // re-points a config.toml link must see the new target on reload.
        config.config_file = std::path::absolute(&config.config_file).with_context(|| {
            format!(
                "resolving configuration file path {}",
                config.config_file.display()
            )
        })?;
        Ok(config)
    }

    fn parse_signal(name: &str) -> Result<SignalType> {
        match name.to_lowercase().as_str() {
            "stop" => Ok(SignalType::Stop),
            "quit" => Ok(SignalType::Quit),
            "reload" => Ok(SignalType::Reload),
            _ => Err(anyhow!(
                "invalid signal '{name}'; valid signals: stop, quit, reload"
            )),
        }
    }

    /// Carry out any action other than starting the server, then exit.
    pub fn execute(&self) -> Result<()> {
        match &self.action {
            CliAction::Help => {
                Self::print_help();
                process::exit(0);
            }
            CliAction::Version => {
                println!("Deflux v{}", env!("CARGO_PKG_VERSION"));
                process::exit(0);
            }
            CliAction::TestConfig => {
                self.load_config()?;
                println!("Configuration file syntax is OK");
                process::exit(0);
            }
            CliAction::Signal(signal) => {
                self.send_signal(*signal)?;
                process::exit(0);
            }
            CliAction::Start => Ok(()),
        }
    }

    fn load_config(&self) -> Result<SystemConfig> {
        if !self.config_file.exists() {
            return Err(anyhow!(
                "configuration file does not exist: {}",
                self.config_file.display()
            ));
        }
        ConfigManager::load_system_config(&self.config_file).with_context(|| {
            format!(
                "failed to parse configuration file {}",
                self.config_file.display()
            )
        })
    }

    fn send_signal(&self, signal: SignalType) -> Result<()> {
        let config = self.load_config()?;
        let pid_file = config.pid_file();
        let pid_text = std::fs::read_to_string(pid_file).with_context(|| {
            format!("reading PID file {pid_file}; is deflux running as a daemon?")
        })?;
        let pid: i32 = pid_text
            .trim()
            .parse()
            .with_context(|| format!("invalid PID in {pid_file}: {}", pid_text.trim()))?;

        let unix_signal = match signal {
            SignalType::Stop => Signal::SIGINT,
            SignalType::Quit => Signal::SIGTERM,
            SignalType::Reload => Signal::SIGUSR1,
        };
        println!("Sending {unix_signal} to process {pid}");
        kill(Pid::from_raw(pid), unix_signal)
            .with_context(|| format!("sending {unix_signal} to process {pid}"))?;
        Ok(())
    }

    fn print_help() {
        println!(
            "Deflux - managed multi-site reverse proxy v{}",
            env!("CARGO_PKG_VERSION")
        );
        println!();
        println!("USAGE:");
        println!("    deflux [OPTIONS]");
        println!();
        println!("OPTIONS:");
        println!("    -?, -h, --help         Print this help message");
        println!("    -v, --version          Print version information");
        println!("    -c, --config <FILE>    Configuration file (default: config.toml)");
        println!("    -d, --daemon           Run in the background");
        println!(
            "    -u, --upgrade          Take over the listening sockets of a running instance"
        );
        println!("    -t, --test             Check the configuration file and exit");
        println!(
            "    -s, --signal <SIGNAL>  Signal the running daemon (uses pid_file from the config)"
        );
        println!();
        println!("SIGNALS:");
        println!("    stop      Exit immediately");
        println!("    quit      Stop accepting connections, exit when in-flight requests finish");
        println!("    reload    Start a replacement process and hand the listening sockets to it");
        println!();
        println!("EXAMPLES:");
        println!("    deflux -c /etc/deflux/config.toml");
        println!("    deflux -c /etc/deflux/config.toml -d");
        println!("    deflux -c /etc/deflux/config.toml -s reload");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_signals() {
        assert_eq!(CliConfig::parse_signal("stop").unwrap(), SignalType::Stop);
        assert_eq!(CliConfig::parse_signal("QUIT").unwrap(), SignalType::Quit);
        assert_eq!(
            CliConfig::parse_signal("reload").unwrap(),
            SignalType::Reload
        );
        assert!(CliConfig::parse_signal("reopen").is_err());
    }

    #[test]
    fn parses_flags() {
        let config = CliConfig::parse_from(["-c", "/tmp/x.toml", "--upgrade", "-d"]).unwrap();
        assert_eq!(config.config_file, PathBuf::from("/tmp/x.toml"));
        assert!(config.upgrade);
        assert!(config.daemon);
        assert!(config.ready_fd.is_none());
        assert!(matches!(config.action, CliAction::Start));
    }

    #[test]
    fn parses_ready_fd() {
        let config = CliConfig::parse_from(["--ready-fd", "7"]).unwrap();
        assert_eq!(config.ready_fd, Some(7));
        assert!(CliConfig::parse_from(["--ready-fd", "seven"]).is_err());
        assert!(CliConfig::parse_from(["--ready-fd"]).is_err());
    }

    #[test]
    fn makes_relative_config_paths_absolute() {
        let config = CliConfig::parse_from(["-c", "etc/deflux.toml"]).unwrap();
        assert!(config.config_file.is_absolute());
        assert!(config.config_file.ends_with("etc/deflux.toml"));

        let config = CliConfig::parse_from(Vec::<&str>::new()).unwrap();
        assert!(config.config_file.is_absolute());
        assert!(config.config_file.ends_with("config.toml"));
    }

    #[test]
    fn rejects_positional_and_unknown() {
        assert!(CliConfig::parse_from(["config.toml"]).is_err());
        assert!(CliConfig::parse_from(["--bogus"]).is_err());
        assert!(CliConfig::parse_from(["-c"]).is_err());
    }

    #[test]
    fn default_config() {
        let config = CliConfig::default();
        assert_eq!(config.config_file, PathBuf::from("config.toml"));
        assert!(matches!(config.action, CliAction::Start));
    }
}
