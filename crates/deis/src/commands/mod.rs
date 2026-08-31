pub mod blob;
pub mod build;
pub mod config;
pub mod db;
pub mod deploy;
pub mod dev;
pub mod init;
pub mod kv;
pub mod logs;
pub mod rollback;
pub mod secret;
pub mod status;
pub mod trace;
pub mod validate;
pub mod versions;

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::platform::Control;
use crate::platform::storage::StorageSession;
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
/// logs`, `deis tail` and `deis trace` deliberately stop here: reading an
/// application's logs or its invocation history is not a control-plane
/// operation, and a checkout that has never had a control plane configured
/// must still be able to look at what its application is doing.
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

    /// The storage tenant this command is about: application and environment
    /// joined the one way the platform ever joins them.
    ///
    /// Never build this pair by hand elsewhere - `deis_wire::conventions` is
    /// what deis-control and deis-store also build it with, and a second
    /// copy of the join logic is exactly how the three would drift apart.
    pub fn tenant(&self) -> Result<String> {
        Ok(deis_wire::conventions::tenant_id(
            self.app()?,
            self.environment()?,
        ))
    }

    /// Open a connection to this tenant's KV store, database and blob
    /// storage.
    ///
    /// The address and the capability token both come from the control
    /// plane, fetched fresh on every call: this is what lets an operator who
    /// can already run `deis deploy` also run `deis kv`/`deis db`/`deis
    /// blob` with no separate credential to manage, and what keeps the raw
    /// storage secret off every machine this CLI runs on. See
    /// [`crate::platform::Control::storage_credentials`] for the contract.
    pub async fn storage(&self) -> Result<StorageSession> {
        let app = self.app()?;
        let environment = self.environment()?;
        let tenant = self.tenant()?;

        let credentials = self.control.storage_credentials(app, environment).await?;
        let Some(endpoint) = credentials.store_endpoint else {
            anyhow::bail!(
                "the control plane has no deis-store address configured, so {tenant} has \
                 nowhere to keep kv, database or blob data. This is a platform-level setting an \
                 operator sets on deis-control itself; `deis config` has no lever for it."
            );
        };
        StorageSession::connect(&endpoint, &tenant, credentials.token.as_deref())
    }
}
