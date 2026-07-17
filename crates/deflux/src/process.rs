use std::fs::File;
use std::os::unix::process::parent_id;
use anyhow::{Context, Result};
use daemonize_me::{Daemon, Group, User};
use tracing::info;
use serde::{Deserialize, Serialize};
use crate::config::{ConfigError, Validatable};

/// Daemon configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonConfig {
    /// PID file path
    #[serde(default = "default_pid_file")]
    pub pid_file: String,
    /// Stdout redirection file path
    #[serde(default = "default_stdout_file")]
    pub stdout_file: String,
    /// Stderr redirection file path
    #[serde(default = "default_stderr_file")]
    pub stderr_file: String,
    /// Working directory
    #[serde(default = "default_work_dir")]
    pub work_dir: String,
    /// File permission mask
    #[serde(default = "default_umask")]
    pub umask: u16,
    /// Whether to change PID file owner
    #[serde(default = "default_chown_pid")]
    pub chown_pid: bool,
}

fn default_pid_file() -> String {
    "/tmp/deflux.pid".to_string()
}

fn default_stdout_file() -> String {
    "/tmp/deflux.out".to_string()
}

fn default_stderr_file() -> String {
    "/tmp/deflux.err".to_string()
}

fn default_work_dir() -> String {
    "/".to_string()
}

fn default_umask() -> u16 {
    0o022
}

fn default_chown_pid() -> bool {
    false
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            pid_file: default_pid_file(),
            stdout_file: default_stdout_file(),
            stderr_file: default_stderr_file(),
            work_dir: default_work_dir(),
            umask: default_umask(),
            chown_pid: default_chown_pid(),
        }
    }
}

impl Validatable for DaemonConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        // Validate path is absolute
        if !self.pid_file.starts_with('/') {
            return Err(ConfigError::ValidationError {
                message: "PID file path must be absolute".to_string(),
            });
        }
        if !self.stdout_file.starts_with('/') {
            return Err(ConfigError::ValidationError {
                message: "Stdout file path must be absolute".to_string(),
            });
        }
        if !self.stderr_file.starts_with('/') {
            return Err(ConfigError::ValidationError {
                message: "Stderr file path must be absolute".to_string(),
            });
        }
        if !self.work_dir.starts_with('/') {
            return Err(ConfigError::ValidationError {
                message: "Work directory must be absolute".to_string(),
            });
        }
        Ok(())
    }
}

/// Check if the current process is running in daemon mode
pub fn is_daemon() -> bool {
    // Check if the parent process is init (PID 1)
    parent_id() == 1
}

/// Convert the current process to a daemon
///
/// # Arguments
///
/// * `daemon_config` - Daemon configuration
/// * `username` - Optional runtime username. If provided, groupname must also be provided.
/// * `groupname` - Optional runtime group name. If provided, username must also be provided.
///
/// # Errors
///
/// If daemonization fails, an error is returned.
pub fn daemonize(daemon_config: &DaemonConfig, username: Option<&str>, groupname: Option<&str>) -> Result<()> {
    if is_daemon() {
        info!("Already running in daemon mode, skipping daemonization.");
        return Ok(());
    }

    // Validate configuration
    daemon_config.validate().map_err(|e| anyhow::anyhow!("Invalid daemon config: {}", e))?;

    let stdout = File::create(&daemon_config.stdout_file)
        .with_context(|| format!("Failed to create stdout log file: {}", daemon_config.stdout_file))?;
    let stderr = File::create(&daemon_config.stderr_file)
        .with_context(|| format!("Failed to create stderr log file: {}", daemon_config.stderr_file))?;

    let mut daemon = Daemon::new()
        .pid_file(&daemon_config.pid_file, Some(daemon_config.chown_pid))
        .umask(daemon_config.umask)
        .work_dir(&daemon_config.work_dir)
        .stdout(stdout)
        .stderr(stderr);

    // According to daemonize-me documentation, both user and group must be provided together
    if let (Some(user), Some(group)) = (username, groupname) {
        daemon = daemon
            .user(User::try_from(user).context("Invalid user specified")?)
            .group(Group::try_from(group).context("Invalid group specified")?);
    } else if username.is_some() || groupname.is_some() {
        return Err(anyhow::anyhow!(
            "Both user and group must be specified for privilege drop, or neither."
        ));
    }

    daemon
        .start()
        .map_err(|e| anyhow::anyhow!("Failed to start daemon: {}", e))?;

    info!("Process successfully daemonized.");
    Ok(())
}
