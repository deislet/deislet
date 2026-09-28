//! What a driver process is given besides its request and its credential:
//! the deployment's config values, its secret values and, for a teardown,
//! consent to destroy data.
//!
//! None of it belongs in the serializable [`crate::DriverRequest`]: secret
//! values must never be written to the operation journal, and data
//! destruction is an operator's decision about one operation, not part of a
//! desired state a retry may replay. The launcher asks an
//! [`OperationInputSource`] for them just before it starts the process and
//! hands them over through the process environment, never on argv:
//!
//! | Variable | Meaning |
//! |---|---|
//! | [`DRIVER_PROCESS_CONFIG_ENV`] | JSON object `{NAME: "value"}`, the deployment's config values |
//! | [`DRIVER_PROCESS_SECRETS_ENV`] | JSON object `{NAME: "value"}`, the deployment's secret values |
//! | [`DRIVER_PROCESS_PURGE_ENV`] | the environment id whose provider data this operation may destroy |
//!
//! A variable is set only when there is something to say: no config, no
//! secrets and no consent leave all three unset.

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use deis_wire::CanonicalId;
use zeroize::Zeroizing;

use crate::{DriverExecutionContext, SecretString};

/// The deployment's config values, as a JSON object of strings.
pub const DRIVER_PROCESS_CONFIG_ENV: &str = "EDGE_CANON_PROVIDER_CONFIG";
/// The deployment's secret values, as a JSON object of strings.
pub const DRIVER_PROCESS_SECRETS_ENV: &str = "EDGE_CANON_PROVIDER_SECRETS";
/// Consent to destroy the provider data of one environment, by id. A driver
/// destroys data only for the environment of its own request, and only when
/// this names it.
pub const DRIVER_PROCESS_PURGE_ENV: &str = "DEIS_PROVIDER_PURGE_DATA";

/// Everything one driver operation is given besides its request.
///
/// `Debug` names the secrets, never their values; values are zeroed on
/// drop.
#[derive(Default)]
pub struct OperationInputs {
    config: BTreeMap<String, String>,
    secrets: BTreeMap<String, Zeroizing<String>>,
    purge_environment: Option<CanonicalId>,
}

impl OperationInputs {
    /// Nothing: no config, no secrets, no consent to destroy data.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_config(mut self, config: BTreeMap<String, String>) -> Self {
        self.config = config;
        self
    }

    #[must_use]
    pub fn with_secrets(mut self, secrets: BTreeMap<String, Zeroizing<String>>) -> Self {
        self.secrets = secrets;
        self
    }

    /// Consent to destroy the provider data of `environment_id`.
    #[must_use]
    pub fn with_purge(mut self, environment_id: CanonicalId) -> Self {
        self.purge_environment = Some(environment_id);
        self
    }

    #[must_use]
    pub fn config(&self) -> &BTreeMap<String, String> {
        &self.config
    }

    pub fn secret_names(&self) -> impl Iterator<Item = &str> {
        self.secrets.keys().map(String::as_str)
    }

    #[must_use]
    pub const fn purge_environment(&self) -> Option<CanonicalId> {
        self.purge_environment
    }

    /// [`DRIVER_PROCESS_CONFIG_ENV`]'s value, or `None` with no config.
    pub(crate) fn config_json(&self) -> Option<String> {
        (!self.config.is_empty())
            .then(|| serde_json::to_string(&self.config).expect("a string map serializes"))
    }

    /// [`DRIVER_PROCESS_SECRETS_ENV`]'s value, or `None` with no secrets.
    pub(crate) fn secrets_json(&self) -> Result<Option<SecretString>, OperationInputError> {
        if self.secrets.is_empty() {
            return Ok(None);
        }
        let plain: BTreeMap<&str, &str> = self
            .secrets
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let json = Zeroizing::new(serde_json::to_string(&plain).expect("a string map serializes"));
        SecretString::new(json.as_str()).map(Some).map_err(|_| {
            OperationInputError::invalid(
                "the deployment's secret values do not fit the bound a driver process is given",
            )
        })
    }
}

impl fmt::Debug for OperationInputs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OperationInputs")
            .field("config", &self.config.keys().collect::<Vec<_>>())
            .field("secrets", &self.secrets.keys().collect::<Vec<_>>())
            .field("purge_environment", &self.purge_environment)
            .finish()
    }
}

/// Why the inputs of an operation could not be produced. Nothing is
/// launched then: a deployment that needs a secret it cannot be given must
/// not go out without it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationInputError {
    message: String,
    retryable: bool,
}

impl OperationInputError {
    /// A refusal that will not change on a retry: the inputs do not exist,
    /// or may not be handed to this operation.
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }

    /// A failure that may clear: the database or the audit log was not
    /// writable just now.
    #[must_use]
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }

    #[must_use]
    pub const fn retryable(&self) -> bool {
        self.retryable
    }
}

impl fmt::Display for OperationInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for OperationInputError {}

/// Where a launcher gets an operation's [`OperationInputs`].
///
/// Asked once per process launch, after the operation's workspace is ready
/// and before its credential is leased, with the same fenced context the
/// credential broker is given.
#[async_trait]
pub trait OperationInputSource: Send + Sync {
    async fn inputs(
        &self,
        context: &DriverExecutionContext<'_>,
    ) -> Result<OperationInputs, OperationInputError>;
}

/// A source that gives every operation nothing: for a launcher whose
/// deployments carry no config and no secrets, and for tests.
pub struct NoOperationInputs;

#[async_trait]
impl OperationInputSource for NoOperationInputs {
    async fn inputs(
        &self,
        _context: &DriverExecutionContext<'_>,
    ) -> Result<OperationInputs, OperationInputError> {
        Ok(OperationInputs::none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_names_secrets_but_never_their_values() {
        let inputs = OperationInputs::none().with_secrets(BTreeMap::from([(
            "API_KEY".to_owned(),
            Zeroizing::new("value-that-must-not-appear".to_owned()),
        )]));
        let debug = format!("{inputs:?}");
        assert!(debug.contains("API_KEY"), "{debug}");
        assert!(!debug.contains("value-that-must-not-appear"), "{debug}");
    }

    #[test]
    fn nothing_to_say_sets_nothing() {
        let inputs = OperationInputs::none();
        assert!(inputs.config_json().is_none());
        assert!(inputs.secrets_json().unwrap().is_none());
        assert!(inputs.purge_environment().is_none());
    }
}
