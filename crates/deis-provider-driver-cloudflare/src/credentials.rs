//! Credential parsing: `EDGE_CANON_PROVIDER_CREDENTIAL` → Cloudflare env vars.
//!
//! The environment variable carries a JSON object `{"api_token": "...",
//! "account_id": "..."}`. The driver reads it once, validates both fields,
//! and hands them to wrangler as `CLOUDFLARE_API_TOKEN` and
//! `CLOUDFLARE_ACCOUNT_ID` in the subprocess environment - never in argv.
//!
//! The token never appears in `Debug` output or an error message: `Debug`
//! prints `[redacted]`, [`CloudflareCredential::redact`] scrubs it from any
//! captured wrangler output, and the value is zeroed when dropped.

use serde::Deserialize;
use zeroize::Zeroize;

/// Parsed from `EDGE_CANON_PROVIDER_CREDENTIAL`.
#[derive(Clone, Deserialize)]
pub struct CloudflareCredential {
    api_token: String,
    account_id: String,
}

impl CloudflareCredential {
    /// Parse and validate the credential string.
    ///
    /// Returns an error if the JSON is malformed or either field is absent,
    /// empty, or carries whitespace/control characters. The error messages
    /// name only the field that is wrong, never a value.
    pub fn from_env_value(value: &str) -> Result<Self, CredentialError> {
        let credential: Self = serde_json::from_str(value).map_err(|error| {
            // serde_json's message can quote the offending input; keep only
            // the position so a malformed token is not echoed.
            CredentialError::InvalidJson {
                message: format!("line {} column {}", error.line(), error.column()),
            }
        })?;
        for (field, value) in [
            ("api_token", &credential.api_token),
            ("account_id", &credential.account_id),
        ] {
            if value.is_empty() {
                return Err(CredentialError::MissingField { field });
            }
            if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(CredentialError::MalformedField { field });
            }
        }
        Ok(credential)
    }

    pub fn api_token(&self) -> &str {
        &self.api_token
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// `text` with every occurrence of the API token replaced.
    pub fn redact(&self, text: &str) -> String {
        if self.api_token.is_empty() {
            return text.to_owned();
        }
        text.replace(&self.api_token, "[redacted]")
    }
}

impl std::fmt::Debug for CloudflareCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudflareCredential")
            .field("api_token", &"[redacted]")
            .field("account_id", &self.account_id)
            .finish()
    }
}

impl Drop for CloudflareCredential {
    fn drop(&mut self) {
        self.api_token.zeroize();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CredentialError {
    InvalidJson { message: String },
    MissingField { field: &'static str },
    MalformedField { field: &'static str },
    NotSet,
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidJson { message } => {
                write!(
                    f,
                    "EDGE_CANON_PROVIDER_CREDENTIAL is not valid JSON ({message})"
                )
            }
            Self::MissingField { field } => {
                write!(
                    f,
                    "EDGE_CANON_PROVIDER_CREDENTIAL is missing the required field {field:?}"
                )
            }
            Self::MalformedField { field } => {
                write!(
                    f,
                    "EDGE_CANON_PROVIDER_CREDENTIAL field {field:?} contains whitespace or \
                     control characters"
                )
            }
            Self::NotSet => {
                write!(f, "EDGE_CANON_PROVIDER_CREDENTIAL is not set")
            }
        }
    }
}

impl std::error::Error for CredentialError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_credential() {
        let value = r#"{"api_token": "tok", "account_id": "acct-1"}"#;
        let cred = CloudflareCredential::from_env_value(value).unwrap();
        assert_eq!(cred.api_token(), "tok");
        assert_eq!(cred.account_id(), "acct-1");
    }

    #[test]
    fn rejects_empty_api_token() {
        let value = r#"{"api_token": "", "account_id": "acct-1"}"#;
        let err = CloudflareCredential::from_env_value(value).unwrap_err();
        assert!(err.to_string().contains("api_token"));
    }

    #[test]
    fn rejects_missing_account_id() {
        let value = r#"{"api_token": "tok"}"#;
        let err = CloudflareCredential::from_env_value(value).unwrap_err();
        assert!(matches!(err, CredentialError::InvalidJson { .. }));
    }

    #[test]
    fn rejects_invalid_json_without_echoing_it() {
        let err = CloudflareCredential::from_env_value("secret-token-value").unwrap_err();
        assert!(matches!(err, CredentialError::InvalidJson { .. }));
        assert!(!err.to_string().contains("secret-token-value"));
    }

    #[test]
    fn rejects_a_token_with_a_newline() {
        let value = r#"{"api_token": "tok\n", "account_id": "acct-1"}"#;
        let err = CloudflareCredential::from_env_value(value).unwrap_err();
        assert_eq!(err, CredentialError::MalformedField { field: "api_token" });
    }

    #[test]
    fn extra_fields_are_accepted() {
        let value = r#"{"api_token": "tok", "account_id": "acct-1", "extra": true}"#;
        let cred = CloudflareCredential::from_env_value(value).unwrap();
        assert_eq!(cred.api_token(), "tok");
    }

    #[test]
    fn debug_and_redact_hide_the_token() {
        let value = r#"{"api_token": "s3cr3t-token", "account_id": "acct-1"}"#;
        let cred = CloudflareCredential::from_env_value(value).unwrap();
        assert!(!format!("{cred:?}").contains("s3cr3t-token"));
        assert_eq!(
            cred.redact("auth with s3cr3t-token failed"),
            "auth with [redacted] failed"
        );
    }
}
