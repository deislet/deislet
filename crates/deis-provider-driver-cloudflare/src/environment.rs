//! The driver process's inputs besides the request: where it keeps tools and
//! state, which Node.js runs wrangler, and the deployment's config values,
//! secret values and data-destruction consent.
//!
//! | Variable | Meaning |
//! |---|---|
//! | `EDGE_CANON_PROVIDER_CREDENTIAL` | `{"api_token","account_id"}` (see [`crate::credentials`]) |
//! | `EDGE_CANON_PROVIDER_CONFIG` | optional JSON object `{NAME: "value"}` filling `[vars]` placeholders |
//! | `EDGE_CANON_PROVIDER_SECRETS` | optional JSON object `{NAME: "value"}`, sent to wrangler on stdin only |
//! | `DEIS_PROVIDER_PURGE_DATA` | the environment id whose KV/D1/R2/queue resources `delete`/`cleanup` may destroy; anything else destroys nothing |
//! | `DEIS_PROVIDER_TOOLS_DIR` | toolchain cache (default `$HOME/.deis/tools`) |
//! | `DEIS_PROVIDER_NODE` | absolute path of the Node.js executable (default: `node` on `PATH`) |
//!
//! The artifact directory and the state directory (journals, wrangler's
//! private home) come from the envelope's
//! [`deis_provider_driver::DriverWorkspace`], never from the environment.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use deis_provider_driver::DriverWorkspace;
use zeroize::Zeroize;

pub const CONFIG_ENV: &str = deis_provider_driver::DRIVER_PROCESS_CONFIG_ENV;
pub const SECRETS_ENV: &str = deis_provider_driver::DRIVER_PROCESS_SECRETS_ENV;
pub const PURGE_ENV: &str = deis_provider_driver::DRIVER_PROCESS_PURGE_ENV;
pub const TOOLS_DIR_ENV: &str = "DEIS_PROVIDER_TOOLS_DIR";
pub const NODE_ENV: &str = "DEIS_PROVIDER_NODE";

/// Secret values for the Worker. `Debug` never prints a value, and values
/// are zeroed on drop.
#[derive(Clone, Default)]
pub struct SecretValues(BTreeMap<String, String>);

impl SecretValues {
    pub fn new(values: BTreeMap<String, String>) -> Result<Self, String> {
        for name in values.keys() {
            if !is_env_name(name) {
                return Err(format!(
                    "secret name {name:?} is not a valid Worker binding name"
                ));
            }
        }
        Ok(Self(values))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// The JSON object wrangler's `--secrets-file` reads, as bytes for stdin.
    pub fn to_stdin_json(&self) -> Vec<u8> {
        serde_json::to_vec(&self.0).expect("a string map serialises")
    }

    /// `text` with every secret value replaced.
    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for value in self.0.values().filter(|value| value.len() >= 4) {
            out = out.replace(value.as_str(), "[redacted]");
        }
        out
    }
}

impl std::fmt::Debug for SecretValues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
    }
}

impl Drop for SecretValues {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            value.zeroize();
        }
    }
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Everything the driver reads from its environment, resolved once.
#[derive(Clone, Debug)]
pub struct DriverEnvironment {
    pub artifact_dir: PathBuf,
    pub tools_dir: PathBuf,
    pub state_dir: PathBuf,
    pub node: Option<PathBuf>,
    pub search_path: Option<OsString>,
    pub config: BTreeMap<String, String>,
    pub secrets: SecretValues,
    /// The environment id named by `DEIS_PROVIDER_PURGE_DATA`.
    pub purge_environment_id: Option<String>,
}

impl DriverEnvironment {
    pub fn from_process(workspace: &DriverWorkspace) -> Result<Self, String> {
        let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
        let home = var("HOME").map(PathBuf::from);
        let under_home = |suffix: &str, explicit: &str| -> Result<PathBuf, String> {
            match var(explicit) {
                Some(dir) => Ok(PathBuf::from(dir)),
                None => home
                    .as_ref()
                    .map(|home| home.join(".deis").join(suffix))
                    .ok_or_else(|| format!("neither {explicit} nor HOME is set")),
            }
        };
        let artifact_dir = crate::protocol::artifact_work_dir(workspace);
        let config = match std::env::var(CONFIG_ENV) {
            Ok(json) if !json.is_empty() => parse_string_map(CONFIG_ENV, &json)?,
            _ => BTreeMap::new(),
        };
        let secrets = match std::env::var(SECRETS_ENV) {
            Ok(mut json) if !json.is_empty() => {
                let parsed = parse_string_map(SECRETS_ENV, &json);
                json.zeroize();
                SecretValues::new(parsed?)?
            }
            _ => SecretValues::default(),
        };
        Ok(Self {
            artifact_dir,
            tools_dir: under_home("tools", TOOLS_DIR_ENV)?,
            state_dir: workspace.state_directory.clone(),
            node: var(NODE_ENV).map(PathBuf::from),
            search_path: var("PATH"),
            config,
            secrets,
            purge_environment_id: std::env::var(PURGE_ENV).ok().filter(|v| !v.is_empty()),
        })
    }

    /// wrangler's private `HOME`.
    pub fn wrangler_home(&self) -> PathBuf {
        self.state_dir.join("cloudflare").join("wrangler-home")
    }

    pub fn artifact_dir(&self) -> &Path {
        &self.artifact_dir
    }
}

fn parse_string_map(variable: &str, json: &str) -> Result<BTreeMap<String, String>, String> {
    // The error never quotes the input: it may be a secret.
    serde_json::from_str(json).map_err(|error| {
        format!(
            "{variable} must be a JSON object of strings (error at line {} column {})",
            error.line(),
            error.column()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_lists_names_only() {
        let secrets = SecretValues::new(BTreeMap::from([(
            "API_KEY".to_owned(),
            "very-secret".to_owned(),
        )]))
        .unwrap();
        let debug = format!("{secrets:?}");
        assert!(debug.contains("API_KEY"));
        assert!(!debug.contains("very-secret"));
        assert_eq!(secrets.redact("got very-secret"), "got [redacted]");
    }

    #[test]
    fn secret_names_must_be_binding_names() {
        assert!(
            SecretValues::new(BTreeMap::from([("bad-name".to_owned(), "v".to_owned())])).is_err()
        );
    }

    #[test]
    fn map_parse_errors_do_not_echo_input() {
        let error = parse_string_map(SECRETS_ENV, "{\"A\": 1, \"hunter2\"").unwrap_err();
        assert!(!error.contains("hunter2"));
    }
}
