pub mod build;
pub mod config;
pub mod deploy;
pub mod dev;
pub mod init;
pub mod logs;
pub mod rollback;
pub mod secret;
pub mod status;
pub mod validate;
pub mod versions;

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::platform::Control;
use crate::settings::{Overrides, Settings};

/// The project directory a command works in: wherever it was run.
///
/// Deliberately not a search up the tree. `deis build` has always used the
/// working directory, and a deploy that silently picked a different project
/// than the build in the same shell would be worse than an error saying to
/// change directory.
pub fn project_dir() -> Result<PathBuf> {
    std::env::current_dir().context("reading the working directory")
}

/// Resolve every setting for a command running in the working directory.
///
/// This is all a command that talks only to deis-telemetry needs. `deis
/// logs` and `deis tail` deliberately stop here: reading an application's
/// logs is not a control-plane operation, and a checkout that has never had
/// a control plane configured must still be able to look at what its
/// application is doing.
pub fn resolve(overrides: &Overrides) -> Result<Settings> {
    Settings::load(&project_dir()?, overrides)
}

/// Everything a command that talks to the control plane needs: the resolved
/// settings and a client pointed at the control plane they name.
pub struct Session {
    pub settings: Settings,
    pub control: Control,
}

impl Session {
    /// Resolve settings and open a client. Fails with an actionable message
    /// when the control plane is not configured.
    pub fn open(overrides: &Overrides) -> Result<Self> {
        let settings = resolve(overrides)?;
        let control = Control::new(settings.control_endpoint()?, settings.token())?;
        Ok(Self { settings, control })
    }

    /// The application this command is about, checked against the naming rule
    /// the storage services enforce.
    pub fn app(&self) -> Result<&str> {
        self.settings.checked_app()
    }

    /// The environment this command is about, checked the same way.
    pub fn environment(&self) -> Result<&str> {
        self.settings.checked_environment()
    }
}
