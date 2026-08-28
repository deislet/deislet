//! Row types of the product model plus the identifier rules that keep
//! user-supplied names safe to put in a file path or a route table.
//!
//! Every timestamp in this module is Unix epoch seconds, matching the
//! `INTEGER` columns declared in `database::schema`.

use serde::Serialize;

use super::ProductError;

/// Longest accepted app id, environment name or route host label.
pub const MAX_NAME_LEN: usize = 64;
/// Longest accepted secret name. Secrets end up as process environment
/// variable names in the runtime, which is why they get their own limit.
pub const MAX_SECRET_NAME_LEN: usize = 128;
/// Longest accepted route path prefix.
pub const MAX_PATH_PREFIX_LEN: usize = 512;

/// An application. `id` is the app name and is the only tenant dimension
/// the dumb services (deis-store, deis-queue, deis-telemetry) ever see.
#[derive(Debug, Clone, Serialize)]
pub struct App {
    pub id: String,
    pub created_at: i64,
}

/// An immutable version: one uploaded bundle plus its parsed manifest.
///
/// Deliberately not `Serialize`: `bundle_path` is a control-plane local
/// filesystem path and must not leak into an API response. The REST layer
/// builds its own view.
#[derive(Debug, Clone)]
pub struct Version {
    pub id: String,
    pub app_id: String,
    /// Per-app counter starting at 1. This is what a human calls "v3".
    pub number: i64,
    pub bundle_sha: String,
    /// Where the zip lives on the control plane. Internal only.
    pub bundle_path: String,
    /// Verbatim `manifest.json` text as it was found inside the bundle.
    pub manifest: String,
    pub created_at: i64,
}

/// An environment: `production` or any preview name. Deployment pointers,
/// routes and secrets all hang off this, never off the bare app.
#[derive(Debug, Clone, Serialize)]
pub struct Environment {
    pub id: String,
    pub app_id: String,
    pub name: String,
    pub created_at: i64,
}

/// One pointer row. The newest row of an environment is its current
/// version; older rows are the deployment history.
#[derive(Debug, Clone, Serialize)]
pub struct Deployment {
    pub id: String,
    pub app_id: String,
    pub env_id: String,
    pub version_id: String,
    pub created_at: i64,
    pub note: Option<String>,
}

/// A host plus path prefix bound to one environment. `host: None` matches
/// any domain.
#[derive(Debug, Clone, Serialize)]
pub struct Route {
    pub id: String,
    pub app_id: String,
    pub env_id: String,
    pub host: Option<String>,
    pub path_prefix: String,
    pub priority: i64,
}

/// What a caller is allowed to learn about a secret: that it exists and
/// when it last changed. The value never leaves the control plane except
/// as part of a deployment payload to a node.
#[derive(Debug, Clone, Serialize)]
pub struct SecretMeta {
    pub name: String,
    pub updated_at: i64,
}

/// What one node should be running, before the bundle download URL is
/// filled in by the REST layer (which knows the public base address).
#[derive(Debug, Clone)]
pub struct Assignment {
    pub app_id: String,
    pub env_id: String,
    pub environment: String,
    pub version_id: String,
    pub bundle_sha: String,
}

/// Everything deleting an app removed. Returned so the caller can clean up
/// the bundle files on disk and log exactly how much history was destroyed.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DeletedApp {
    pub versions: usize,
    pub environments: usize,
    pub deployments: usize,
    pub routes: usize,
    pub secrets: usize,
    /// Bundle files that are now unreferenced. Not serialized: these are
    /// control-plane local paths.
    #[serde(skip)]
    pub bundle_paths: Vec<String>,
}

/// Current wall clock as the Unix epoch seconds stored in every
/// `created_at` / `updated_at` column of the product tables.
pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// App ids become directory names under the bundle root and appear in
/// route tables, so they are restricted to a conservative slug: lowercase
/// letters, digits, `-` and `_`, starting with a letter or digit.
pub fn validate_app_id(id: &str) -> Result<(), ProductError> {
    validate_slug(id, "app id", MAX_NAME_LEN, false)
}

/// Environment names additionally allow `.` so that names like `pr-42.1`
/// work, but are otherwise as restricted as app ids.
pub fn validate_environment_name(name: &str) -> Result<(), ProductError> {
    validate_slug(name, "environment name", MAX_NAME_LEN, true)
}

fn validate_slug(
    value: &str,
    what: &str,
    max_len: usize,
    allow_dot: bool,
) -> Result<(), ProductError> {
    if value.is_empty() {
        return Err(ProductError::invalid(format!("{what} must not be empty")));
    }
    if value.len() > max_len {
        return Err(ProductError::invalid(format!(
            "{what} is longer than {max_len} characters"
        )));
    }
    let first = value.as_bytes()[0];
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(ProductError::invalid(format!(
            "{what} must start with a lowercase letter or a digit, got {value:?}"
        )));
    }
    for ch in value.chars() {
        let ok = ch.is_ascii_lowercase()
            || ch.is_ascii_digit()
            || ch == '-'
            || ch == '_'
            || (allow_dot && ch == '.');
        if !ok {
            return Err(ProductError::invalid(format!(
                "{what} may only contain lowercase letters, digits, '-'{} and '_', got {value:?}",
                if allow_dot { ", '.'" } else { "" }
            )));
        }
    }
    Ok(())
}

/// Secret names are injected as environment variables into the isolate,
/// so they follow the shell variable rule: letter or `_` first, then
/// letters, digits and `_`.
pub fn validate_secret_name(name: &str) -> Result<(), ProductError> {
    if name.is_empty() {
        return Err(ProductError::invalid("secret name must not be empty"));
    }
    if name.len() > MAX_SECRET_NAME_LEN {
        return Err(ProductError::invalid(format!(
            "secret name is longer than {MAX_SECRET_NAME_LEN} characters"
        )));
    }
    let first = name.as_bytes()[0];
    if !first.is_ascii_alphabetic() && first != b'_' {
        return Err(ProductError::invalid(format!(
            "secret name must start with a letter or '_', got {name:?}"
        )));
    }
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(ProductError::invalid(format!(
            "secret name may only contain letters, digits and '_', got {name:?}"
        )));
    }
    Ok(())
}

/// Route path prefixes are matched literally against the request path, so
/// they must be absolute and free of the traversal segments that would let
/// one environment shadow another.
pub fn validate_path_prefix(prefix: &str) -> Result<(), ProductError> {
    if !prefix.starts_with('/') {
        return Err(ProductError::invalid(format!(
            "route path_prefix must start with '/', got {prefix:?}"
        )));
    }
    if prefix.len() > MAX_PATH_PREFIX_LEN {
        return Err(ProductError::invalid(format!(
            "route path_prefix is longer than {MAX_PATH_PREFIX_LEN} characters"
        )));
    }
    if prefix.contains("..") {
        return Err(ProductError::invalid(
            "route path_prefix must not contain '..'",
        ));
    }
    if prefix.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ProductError::invalid(
            "route path_prefix must not contain whitespace or control characters",
        ));
    }
    Ok(())
}

/// Route hosts are compared against the request `Host` header. Only the
/// characters a hostname can legally carry are accepted, plus a leading
/// `*.` for a wildcard domain.
pub fn validate_host(host: &str) -> Result<(), ProductError> {
    if host.is_empty() {
        return Err(ProductError::invalid(
            "route host must not be empty; omit it to match any domain",
        ));
    }
    if host.len() > 253 {
        return Err(ProductError::invalid(
            "route host is longer than 253 characters",
        ));
    }
    let body = host.strip_prefix("*.").unwrap_or(host);
    if body.is_empty() {
        return Err(ProductError::invalid(format!(
            "route host {host:?} is not a hostname"
        )));
    }
    let valid = body
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b':');
    if !valid {
        return Err(ProductError::invalid(format!(
            "route host may only contain letters, digits, '-', '.', ':' and an optional leading '*.', got {host:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_rules() {
        assert!(validate_app_id("my-app").is_ok());
        assert!(validate_app_id("app_1").is_ok());
        assert!(validate_app_id("").is_err());
        assert!(validate_app_id("-leading").is_err());
        assert!(validate_app_id("Upper").is_err());
        assert!(validate_app_id("has space").is_err());
        // A traversal attempt must never reach the bundle directory.
        assert!(validate_app_id("../etc").is_err());
        assert!(validate_app_id("a/b").is_err());
        assert!(validate_app_id(&"a".repeat(MAX_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn environment_names_allow_dots_but_not_slashes() {
        assert!(validate_environment_name("production").is_ok());
        assert!(validate_environment_name("pr-42.1").is_ok());
        assert!(validate_environment_name("pr/42").is_err());
    }

    #[test]
    fn secret_names_follow_environment_variable_rules() {
        assert!(validate_secret_name("API_KEY").is_ok());
        assert!(validate_secret_name("_private2").is_ok());
        assert!(validate_secret_name("2FA").is_err());
        assert!(validate_secret_name("api-key").is_err());
    }

    #[test]
    fn path_prefix_rules() {
        assert!(validate_path_prefix("/").is_ok());
        assert!(validate_path_prefix("/api/users").is_ok());
        assert!(validate_path_prefix("api").is_err());
        assert!(validate_path_prefix("/a/../b").is_err());
        assert!(validate_path_prefix("/a b").is_err());
    }

    #[test]
    fn host_rules() {
        assert!(validate_host("example.com").is_ok());
        assert!(validate_host("*.example.com").is_ok());
        assert!(validate_host("localhost:8080").is_ok());
        assert!(validate_host("").is_err());
        assert!(validate_host("bad host").is_err());
    }
}
