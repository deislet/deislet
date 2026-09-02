//! Where the control plane is, where deis-telemetry is, and which credential
//! to present.
//!
//! Three places, in this order of precedence:
//!
//! 1. a flag on the command (`--app`, `-e/--environment`);
//! 2. an environment variable ([`ENV_CONTROL_ENDPOINT`] and friends);
//! 3. the project file [`PROJECT_FILE`], which belongs in the repository.
//!
//! The credential is deliberately not in that list. It is read from
//! [`ENV_CONTROL_TOKEN`] or from a per-user credentials file outside the
//! project ([`credentials_path`]), and `deis config set control.token` writes
//! it there with owner-only permissions. A token written into the project file
//! would be committed by the next `git add .`, and a token committed once is a
//! token that has to be rotated - so this crate never writes one into a file
//! inside the project, and refuses if asked.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Project file, next to `.config.json`. Safe to commit: it holds addresses
/// and defaults, never a credential.
pub const PROJECT_FILE: &str = "deis.toml";

/// Per-user credentials file, inside [`config_home`] and outside every
/// project.
pub const CREDENTIALS_FILE: &str = "credentials.toml";

pub const ENV_CONTROL_ENDPOINT: &str = "DEIS_CONTROL_ENDPOINT";
pub const ENV_CONTROL_TOKEN: &str = "DEIS_CONTROL_TOKEN";
pub const ENV_TELEMETRY_ENDPOINT: &str = "DEIS_TELEMETRY_ENDPOINT";
/// The shared secret deis-telemetry's own `auth_secret` is set to, from which
/// `deis logs`, `deis tail` and `deis trace` derive the token they present.
///
/// A secret rather than a ready-made token because that is what the only other
/// holder of this credential takes: a node reads the same string as
/// `logging.auth_secret` and derives with the same function over the same
/// subject (`deis_runtime::telemetry::TelemetryCredential`). The control plane
/// cannot hand this one out the way it hands out a storage token - its
/// storage-token route mints a *tenant* credential from deis-store's secret,
/// and a telemetry token names the channel, not a tenant - so configuration is
/// the only source there is.
pub const ENV_TELEMETRY_AUTH_SECRET: &str = "DEIS_TELEMETRY_AUTH_SECRET";
pub const ENV_QUEUE_ENDPOINT: &str = "DEIS_QUEUE_ENDPOINT";
pub const ENV_APP: &str = "DEIS_APP";
pub const ENV_ENVIRONMENT: &str = "DEIS_ENVIRONMENT";
/// Overrides [`config_home`] whole. Set by the tests so they never touch the
/// real user's credentials, and usable by anyone juggling two platforms.
pub const ENV_CONFIG_HOME: &str = "DEIS_CONFIG_HOME";

/// Where one resolved value came from, so `deis config list` and every error
/// message can say which file or variable to edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A flag on the command line.
    Flag,
    /// An environment variable, named.
    Env(&'static str),
    /// The project file.
    Project,
    /// The credentials file.
    Credentials,
    /// The project's `.config.json` - only the application name comes from
    /// there, because that is where `deis build` already reads it.
    BuildConfig,
    /// Nothing said otherwise.
    Default,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Flag => write!(f, "command line"),
            Self::Env(name) => write!(f, "${name}"),
            Self::Project => write!(f, "{PROJECT_FILE}"),
            Self::Credentials => write!(f, "{CREDENTIALS_FILE}"),
            Self::BuildConfig => write!(f, ".config.json"),
            Self::Default => write!(f, "default"),
        }
    }
}

/// A resolved value together with where it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub value: String,
    pub source: Source,
}

impl Setting {
    fn new(value: impl Into<String>, source: Source) -> Self {
        Self {
            value: value.into(),
            source,
        }
    }
}

/// The project file's contents.
///
/// Every field is optional: a project that only ever runs `deis build` needs
/// no file at all, and one that deploys to the default control plane only
/// needs the endpoint.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProjectFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<EndpointSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<EndpointSection>,
    /// Where deis-queue listens. In the project file beside the other two
    /// addresses rather than fetched from the control plane, because the
    /// control plane does not hand this one out: its storage-token route
    /// answers with deis-store's address and the tenant's capability token,
    /// and that token is the credential deis-queue checks too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<EndpointSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<DefaultsSection>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EndpointSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Present only so a token that was pasted here is caught and refused
    /// instead of being used as if it were safe there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct DefaultsSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
}

/// The credentials file: one entry per control-plane address, so a developer
/// with a staging and a production platform keeps both without editing
/// anything between commands.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Credentials {
    #[serde(default)]
    pub endpoints: BTreeMap<String, CredentialEntry>,
    /// One entry per deis-telemetry address, keyed the same way `endpoints` is
    /// keyed by a control-plane address.
    ///
    /// Its own map rather than a field on [`CredentialEntry`] because the two
    /// addresses are independent: a project can name a deis-telemetry and no
    /// control plane, and `deis logs` is the command that then has to work.
    /// Keying this by the telemetry address is also what lets the log reader
    /// resolve its own credential from the one string it is given.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub telemetry: BTreeMap<String, TelemetryEntry>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CredentialEntry {
    pub token: String,
}

/// What this CLI holds for one deis-telemetry.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TelemetryEntry {
    /// The same string that service's `auth_secret` is set to. See
    /// [`ENV_TELEMETRY_AUTH_SECRET`].
    pub auth_secret: String,
}

/// Directory holding the per-user credentials file.
pub fn config_home() -> PathBuf {
    if let Ok(dir) = std::env::var(ENV_CONFIG_HOME)
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("deis");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config").join("deis")
}

/// Full path of the credentials file.
pub fn credentials_path() -> PathBuf {
    config_home().join(CREDENTIALS_FILE)
}

/// Read the credentials file. A missing file is an empty set, not an error:
/// most commands need no credential at all.
pub fn load_credentials() -> Result<Credentials> {
    let path = credentials_path();
    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| {
            format!(
                "{} is not readable as TOML; fix or delete it",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Credentials::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write the credentials file with owner-only permissions.
pub fn save_credentials(credentials: &Credentials) -> Result<PathBuf> {
    let dir = config_home();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(CREDENTIALS_FILE);
    let text = toml::to_string_pretty(credentials).context("serialising the credentials file")?;
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    restrict_permissions(&path)?;
    Ok(path)
}

/// Take the credentials file down to `0600`.
///
/// A token readable by every account on the host is a token that has to be
/// rotated when anyone else logs in, so a failure here is reported rather
/// than passed over.
#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {} to owner-only access", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

/// The secret to derive this deis-telemetry's capability token from, and where
/// it was found.
///
/// Keyed by the telemetry address for the same reason the control-plane token
/// is keyed by the control-plane address: a developer with a staging and a
/// production collector keeps both and edits nothing between commands.
///
/// Resolved from the endpoint alone, so the log reader can obtain its own
/// credential from the one string every caller already hands it. That is
/// deliberate: `deis logs`, `deis tail` and `deis trace` reach deis-telemetry
/// through four separate `connect` calls, and a credential passed in as an
/// argument is a credential one of those four forgets. The project file is not
/// consulted - it is committed, and this is a secret.
///
/// A blank value counts as absent. An empty header is a token that matches
/// nothing, and sending one turns "no credential configured", which works
/// against a loopback collector with no `auth_secret`, into a refusal.
pub fn telemetry_auth_secret(endpoint: &str) -> Result<Option<Setting>> {
    if let Some(setting) = from_env(ENV_TELEMETRY_AUTH_SECRET) {
        return Ok(Some(setting));
    }
    Ok(load_credentials()?
        .telemetry
        .get(endpoint.trim_end_matches('/'))
        .map(|entry| entry.auth_secret.trim())
        .filter(|secret| !secret.is_empty())
        .map(|secret| Setting::new(secret, Source::Credentials)))
}

/// Read the project file out of `dir`. A missing file means "no project
/// settings", which is a perfectly valid state.
pub fn load_project_file(dir: &Path) -> Result<ProjectFile> {
    let path = dir.join(PROJECT_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text)
            .with_context(|| format!("{} is not readable as TOML", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ProjectFile::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write the project file back to `dir`.
pub fn save_project_file(dir: &Path, file: &ProjectFile) -> Result<PathBuf> {
    let path = dir.join(PROJECT_FILE);
    let text = toml::to_string_pretty(file).context("serialising the project file")?;
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// What a command was told on the command line, before the files are read.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub app: Option<String>,
    pub environment: Option<String>,
    /// Which namespace inside the tenant a storage command acts in.
    ///
    /// Only `deis kv`, `deis db` and `deis blob` ever set it, and it is
    /// deliberately not resolved from the project file or an environment
    /// variable the way `app` and `environment` are: those two say *which
    /// tenant* a command is about, and getting one wrong is the same mistake
    /// whatever the command. A binding says which namespace inside that
    /// tenant, it is only meaningful for three commands, and a value picked up
    /// from a file would silently redirect a `deis kv put` that named no
    /// binding at all.
    pub binding: Option<String>,
}

/// Everything the platform commands need to know, already resolved.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The directory the command ran in, which is where the project file and
    /// `.config.json` are looked for.
    pub project_dir: PathBuf,
    pub control_endpoint: Option<Setting>,
    pub telemetry_endpoint: Option<Setting>,
    pub queue_endpoint: Option<Setting>,
    pub app: Option<Setting>,
    pub environment: Setting,
    /// Never printed. [`Settings::token_source`] is what output uses.
    token: Option<Setting>,
}

impl Settings {
    /// Resolve every setting for a command running in `project_dir`.
    pub fn load(project_dir: &Path, overrides: &Overrides) -> Result<Self> {
        let project = load_project_file(project_dir)?;

        // Every section that can carry a `token`, not only `[control]`.
        //
        // `EndpointSection` is the type of all three, so `[telemetry]` and
        // `[queue]` could always hold a `token = ...` line - and until
        // 2026-09-02 only `[control]` was checked, so a credential pasted into
        // either of the other two was read, ignored, and committed. Ignored is
        // the worse half: the person believes the service is configured and
        // it is not. Enumerated from the struct rather than checked in one
        // place, so a fourth section cannot be added without a name here.
        // Each section carries its own instruction, because the three
        // credentials do not come from the same place and generic advice on a
        // security failure is worse than none: deis-queue's token is minted by
        // the control plane per call and there is nothing for a person to set
        // at all.
        for (name, section, elsewhere) in [
            (
                "control",
                &project.control,
                format!(
                    "Store it with `deis config set control.token <value>`, which writes it to \
                     {} with owner-only permissions.",
                    credentials_path().display()
                ),
            ),
            (
                "telemetry",
                &project.telemetry,
                format!(
                    "deis-telemetry is not reached with a token from this file: set \
                     ${ENV_TELEMETRY_AUTH_SECRET} to the same string that service's `auth_secret` \
                     is set to, or put it in {} under a [telemetry.\"<address>\"] entry.",
                    credentials_path().display()
                ),
            ),
            (
                "queue",
                &project.queue,
                "deis-queue needs nothing set here: its capability token is minted by the \
                 control plane for the tenant and fetched fresh on every call, so this line \
                 was never going to be read."
                    .to_string(),
            ),
        ] {
            if section.as_ref().and_then(|s| s.token.as_ref()).is_none() {
                continue;
            }
            bail!(
                "{} contains a token under [{name}]. That file belongs in the repository, so the \
                 token must be treated as leaked: rotate it and delete the `token` line. \
                 {elsewhere}",
                project_dir.join(PROJECT_FILE).display(),
            );
        }

        let control_endpoint = first(&[
            from_env(ENV_CONTROL_ENDPOINT),
            project
                .control
                .as_ref()
                .and_then(|c| c.endpoint.clone())
                .map(|v| Setting::new(v, Source::Project)),
        ]);

        let telemetry_endpoint = first(&[
            from_env(ENV_TELEMETRY_ENDPOINT),
            project
                .telemetry
                .as_ref()
                .and_then(|t| t.endpoint.clone())
                .map(|v| Setting::new(v, Source::Project)),
        ]);

        let queue_endpoint = first(&[
            from_env(ENV_QUEUE_ENDPOINT),
            project
                .queue
                .as_ref()
                .and_then(|q| q.endpoint.clone())
                .map(|v| Setting::new(v, Source::Project)),
        ]);

        let app = first(&[
            overrides.app.clone().map(|v| Setting::new(v, Source::Flag)),
            from_env(ENV_APP),
            project
                .defaults
                .as_ref()
                .and_then(|d| d.app.clone())
                .map(|v| Setting::new(v, Source::Project)),
            build_config_app(project_dir).map(|v| Setting::new(v, Source::BuildConfig)),
        ]);

        let environment = first(&[
            overrides
                .environment
                .clone()
                .map(|v| Setting::new(v, Source::Flag)),
            from_env(ENV_ENVIRONMENT),
            project
                .defaults
                .as_ref()
                .and_then(|d| d.environment.clone())
                .map(|v| Setting::new(v, Source::Project)),
        ])
        .unwrap_or_else(|| {
            Setting::new(deis_wire::conventions::DEFAULT_ENVIRONMENT, Source::Default)
        });

        let token = match from_env(ENV_CONTROL_TOKEN) {
            Some(token) => Some(token),
            None => match control_endpoint.as_ref() {
                Some(endpoint) => load_credentials()?
                    .endpoints
                    .get(endpoint.value.trim_end_matches('/'))
                    .map(|entry| Setting::new(entry.token.clone(), Source::Credentials)),
                None => None,
            },
        };

        Ok(Self {
            project_dir: project_dir.to_path_buf(),
            control_endpoint,
            telemetry_endpoint,
            queue_endpoint,
            app,
            environment,
            token,
        })
    }

    /// The control plane's base URL, or an error that says how to set it.
    pub fn control_endpoint(&self) -> Result<&str> {
        match &self.control_endpoint {
            Some(setting) => Ok(setting.value.trim_end_matches('/')),
            None => bail!(
                "no control plane is configured. Add it to {}:\n\n  [control]\n  endpoint = \
                 \"http://127.0.0.1:8085\"\n\nor set {ENV_CONTROL_ENDPOINT} for this shell.",
                self.project_dir.join(PROJECT_FILE).display()
            ),
        }
    }

    /// Where deis-telemetry listens, or an error that says how to set it.
    pub fn telemetry_endpoint(&self) -> Result<&str> {
        match &self.telemetry_endpoint {
            Some(setting) => Ok(setting.value.trim_end_matches('/')),
            None => bail!(
                "no deis-telemetry address is configured, so there is nowhere to read logs from. \
                 Add it to {}:\n\n  [telemetry]\n  endpoint = \"http://127.0.0.1:8092\"\n\nor set \
                 {ENV_TELEMETRY_ENDPOINT} for this shell.",
                self.project_dir.join(PROJECT_FILE).display()
            ),
        }
    }

    /// Where deis-queue listens, or an error that says how to set it.
    ///
    /// Only the dead letter commands need it. They are an operator reading
    /// and repairing a queue directly, which is why the address is named in
    /// the project the way deis-telemetry's is: a person who can reach the
    /// control plane cannot necessarily route to deis-queue, and the control
    /// plane has no route that would tell them where it is.
    pub fn queue_endpoint(&self) -> Result<&str> {
        match &self.queue_endpoint {
            Some(setting) => Ok(setting.value.trim_end_matches('/')),
            None => bail!(
                "no deis-queue address is configured, so there is nowhere to read the dead \
                 letters from. Add it to {}:\n\n  [queue]\n  endpoint = \
                 \"http://127.0.0.1:8091\"\n\nor set {ENV_QUEUE_ENDPOINT} for this shell.",
                self.project_dir.join(PROJECT_FILE).display()
            ),
        }
    }

    /// Which application the command is about, or an error naming every way
    /// to say so.
    pub fn app(&self) -> Result<&str> {
        match &self.app {
            Some(setting) => Ok(setting.value.as_str()),
            None => bail!(
                "no application named. Pass --app <id>, set {ENV_APP}, add `app` under \
                 [defaults] in {}, or run this from a project whose .config.json has a name.",
                self.project_dir.join(PROJECT_FILE).display()
            ),
        }
    }

    /// Which environment the command is about. Always answers - the platform's
    /// own default is the last resort.
    pub fn environment(&self) -> &str {
        &self.environment.value
    }

    /// The application this command is about, checked against the naming
    /// rule the storage services enforce.
    pub fn checked_app(&self) -> Result<&str> {
        let app = self.app()?;
        check_name("application", app)?;
        Ok(app)
    }

    /// The environment this command is about, checked the same way.
    pub fn checked_environment(&self) -> Result<&str> {
        let environment = self.environment();
        check_name("environment", environment)?;
        Ok(environment)
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_ref().map(|s| s.value.as_str())
    }

    /// Where the token came from, for output that must never show the value.
    pub fn token_source(&self) -> Option<&Source> {
        self.token.as_ref().map(|s| &s.source)
    }
}

/// Reject a name the storage services would refuse.
///
/// A tenant is `<app>.<environment>`, so a dot in either half splits into
/// three parts and every `ctx.services` call of that application fails with a
/// permission error - while the application itself loads and serves traffic,
/// which makes it a slow thing to diagnose. Catching it here costs one string
/// check and turns it into a sentence.
pub fn check_name(field: &str, value: &str) -> Result<()> {
    if deis_wire::conventions::is_valid_tenant_part(value) {
        return Ok(());
    }
    bail!(
        "{field} {value:?} cannot be used: an application and an environment name join into the \
         storage tenant \"<app>.<environment>\", so each may only hold letters, digits, \"-\" and \
         \"_\", must not be empty and must stay under 65 bytes. A name with a dot in it deploys \
         and then fails every kv, database, blob and queue call. Use \"-\" instead, for example \
         \"pr-42-1\"."
    )
}

fn from_env(name: &'static str) -> Option<Setting> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| Setting::new(v.trim().to_string(), Source::Env(name)))
}

/// The first candidate that was actually found.
fn first(candidates: &[Option<Setting>]) -> Option<Setting> {
    candidates.iter().flatten().next().cloned()
}

/// The `name` field of `.config.json`, which is what `deis build` puts in the
/// bundle manifest - so an application deployed from a project defaults to
/// the name that project builds under.
fn build_config_app(project_dir: &Path) -> Option<String> {
    let text = fs::read_to_string(project_dir.join(".config.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("name")?
        .as_str()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// The one lock guarding the process environment in this test binary.
///
/// Here rather than in `settings::tests` because `platform::telemetry`'s tests
/// set the same variables, and two locks would guard nothing: they run in the
/// same binary, so a second `Mutex` is a second queue past the same door. Every
/// test that reads or writes an environment variable takes this.
#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Clear every variable this crate reads, so a test starts from a known state
/// whatever the developer has exported.
///
/// One list for the whole binary: a variable added to the CLI and not added
/// here leaks one developer's shell into a test run, and the failure looks
/// like a flaky test rather than a missing name.
#[cfg(test)]
pub(crate) fn clear_env() {
    for name in [
        ENV_CONTROL_ENDPOINT,
        ENV_CONTROL_TOKEN,
        ENV_TELEMETRY_ENDPOINT,
        ENV_TELEMETRY_AUTH_SECRET,
        ENV_QUEUE_ENDPOINT,
        ENV_APP,
        ENV_ENVIRONMENT,
        ENV_CONFIG_HOME,
        // Read but not owned: it is where the credentials file lives when
        // nothing more specific says.
        "XDG_CONFIG_HOME",
    ] {
        // SAFETY: every test that touches the environment holds `env_lock`,
        // so no other thread in this binary is reading it concurrently.
        unsafe { std::env::remove_var(name) };
    }
}

/// Set one variable. The caller holds [`env_lock`].
#[cfg(test)]
pub(crate) fn set_env(name: &str, value: &str) {
    // SAFETY: as above - the caller holds `env_lock`.
    unsafe { std::env::set_var(name, value) };
}

#[cfg(test)]
mod tests;
