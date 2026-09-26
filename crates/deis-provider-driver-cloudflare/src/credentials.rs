//! Credential parsing: `EDGE_CANON_PROVIDER_CREDENTIAL` → Cloudflare env vars.
//!
//! The environment variable carries a JSON object `{"api_token": "...",
//! "account_id": "..."}`. The driver reads it once, validates both fields,
//! and sets `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` in the
//! subprocess environment.

use serde::Deserialize;

/// Parsed from `EDGE_CANON_PROVIDER_CREDENTIAL`.
#[derive(Clone, Debug, Deserialize)]
pub struct CloudflareCredential {
    pub api_token: String,
    pub account_id: String,
}

impl CloudflareCredential {
    /// Parse and validate the credential string.
    ///
    /// Returns an error if the JSON is malformed or either field is absent
    /// or empty. The token value is never logged; the returned error
    /// messages mention only the field name that is wrong.
    pub fn from_env_value(value: &str) -> Result<Self, CredentialError> {
        let credential: Self = serde_json::from_str(value).map_err(|error| {
            CredentialError::InvalidJson {
                message: error.to_string(),
            }
        })?;
        if credential.api_token.is_empty() {
            return Err(CredentialError::MissingField {
                field: "api_token",
            });
        }
        if credential.account_id.is_empty() {
            return Err(CredentialError::MissingField {
                field: "account_id",
            });
        }
        Ok(credential)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CredentialError {
    InvalidJson { message: String },
    MissingField { field: &'static str },
    NotSet,
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidJson { message } => {
                write!(f, "EDGE_CANON_PROVIDER_CREDENTIAL is not valid JSON: {message}")
            }
            Self::MissingField { field } => {
                write!(
                    f,
                    "EDGE_CANON_PROVIDER_CREDENTIAL is missing the required field {field:?}"
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
        assert_eq!(cred.api_token, "tok");
        assert_eq!(cred.account_id, "acct-1");
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
        assert!(err.to_string().contains("account_id"));
    }

    #[test]
    fn rejects_invalid_json() {
        let value = "not json";
        let err = CloudflareCredential::from_env_value(value).unwrap_err();
        assert!(matches!(err, CredentialError::InvalidJson { .. }));
    }

    #[test]
    fn extra_fields_are_accepted() {
        let value = r#"{"api_token": "tok", "account_id": "acct-1", "extra": true}"#;
        let cred = CloudflareCredential::from_env_value(value).unwrap();
        assert_eq!(cred.api_token, "tok");
    }
}