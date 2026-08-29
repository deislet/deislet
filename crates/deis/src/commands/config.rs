//! `deis config` - where the platform is and which credential to present.
//!
//! Two files, and the split between them is the point:
//!
//! * `deis.toml` in the project holds addresses and defaults. It belongs in
//!   the repository, so everyone who checks the project out deploys to the
//!   same place without being told.
//! * `credentials.toml` under the user's config directory holds tokens,
//!   keyed by the control plane they belong to, written `0600`. It is
//!   outside every project, so no `git add .` can ever pick it up.
//!
//! `deis config set control.token` therefore writes to the second file and
//! refuses to be talked into the first. Everything either file holds can
//! also be overridden for one shell with an environment variable; `deis
//! config list` prints which of the three won for each setting, so a value
//! that is not taking effect can be traced to the thing that is shadowing
//! it.

use anyhow::{Context, Result, bail};
use colored::Colorize;

use crate::commands::project_dir;
use crate::render::{Output, Table};
use crate::settings::{
    self, CredentialEntry, DefaultsSection, EndpointSection, Overrides, PROJECT_FILE, Settings,
    Source,
};

/// Every setting that can be named on the command line.
const KEYS: &[&str] = &[
    "control.endpoint",
    "control.token",
    "telemetry.endpoint",
    "defaults.app",
    "defaults.environment",
];

pub fn set(key: &str, value: &str, output: Output) -> Result<()> {
    let key = check_key(key)?;
    let value = value.trim();
    if value.is_empty() {
        bail!("{key} was given an empty value. Use `deis config unset {key}` to remove it.");
    }

    let dir = project_dir()?;

    if key == "control.token" {
        let settings = Settings::load(&dir, &Overrides::default())?;
        let endpoint = settings.control_endpoint().context(
            "a token belongs to one control plane, so the address has to be known before the \
             token can be stored against it",
        )?;
        let mut credentials = settings::load_credentials()?;
        credentials.endpoints.insert(
            endpoint.to_string(),
            CredentialEntry {
                token: value.to_string(),
            },
        );
        let path = settings::save_credentials(&credentials)?;

        if output.json {
            return output.emit(&serde_json::json!({
                "key": key,
                "stored": true,
                "endpoint": endpoint,
                "file": path,
            }));
        }
        println!(
            "\n  {} token for {} stored in {}",
            "✓".green(),
            endpoint.cyan(),
            path.display().to_string().cyan()
        );
        println!(
            "  {}",
            "That file is owner-readable only and lives outside the project, so it is never \
             committed. Never put a token in deis.toml."
                .dimmed()
        );
        return Ok(());
    }

    let mut project = settings::load_project_file(&dir)?;
    match key {
        "control.endpoint" => {
            check_http_endpoint(key, value)?;
            project
                .control
                .get_or_insert_with(EndpointSection::default)
                .endpoint = Some(value.to_string());
        }
        "telemetry.endpoint" => {
            project
                .telemetry
                .get_or_insert_with(EndpointSection::default)
                .endpoint = Some(value.to_string());
        }
        "defaults.app" => {
            settings::check_name("application", value)?;
            project
                .defaults
                .get_or_insert_with(DefaultsSection::default)
                .app = Some(value.to_string());
        }
        "defaults.environment" => {
            settings::check_name("environment", value)?;
            project
                .defaults
                .get_or_insert_with(DefaultsSection::default)
                .environment = Some(value.to_string());
        }
        _ => unreachable!("check_key accepted an unknown key"),
    }
    let path = settings::save_project_file(&dir, &project)?;

    if output.json {
        return output.emit(&serde_json::json!({
            "key": key,
            "value": value,
            "file": path,
        }));
    }
    println!(
        "\n  {} {} = {} in {}",
        "✓".green(),
        key.cyan(),
        value,
        path.display().to_string().dimmed()
    );
    if is_shadowed(key) {
        println!(
            "  {}",
            format!(
                "note: {} is set in this shell and takes precedence over the file.",
                variable_for(key).unwrap_or("an environment variable")
            )
            .yellow()
        );
    }
    Ok(())
}

pub fn get(key: &str, output: Output) -> Result<()> {
    let key = check_key(key)?;
    let dir = project_dir()?;
    let settings = Settings::load(&dir, &Overrides::default())?;

    let (value, source) = match key {
        "control.endpoint" => described(settings.control_endpoint.as_ref()),
        "telemetry.endpoint" => described(settings.telemetry_endpoint.as_ref()),
        "defaults.app" => described(settings.app.as_ref()),
        "defaults.environment" => (
            Some(settings.environment.value.clone()),
            Some(settings.environment.source.clone()),
        ),
        // Deliberately not the value. A token that is echoed lands in the
        // terminal's scrollback and in whatever captured the command's
        // output; the question this answers is "is one configured, and from
        // where", which is the one that is ever actually being asked.
        "control.token" => (
            settings.token().map(|_| "set".to_string()),
            settings.token_source().cloned(),
        ),
        _ => unreachable!("check_key accepted an unknown key"),
    };

    if output.json {
        return output.emit(&serde_json::json!({
            "key": key,
            "value": value,
            "source": source.as_ref().map(|s| s.to_string()),
        }));
    }

    match value {
        Some(value) => {
            let source = source
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            println!("{value}  {}", format!("({source})").dimmed());
        }
        None => {
            println!("{}", format!("{key} is not set").dimmed());
            if key == "control.token" {
                println!(
                    "{}",
                    "Store one with `deis config set control.token <value>`; it goes to the \
                     per-user credentials file, never into the project."
                        .dimmed()
                );
            }
        }
    }
    Ok(())
}

pub fn unset(key: &str, output: Output) -> Result<()> {
    let key = check_key(key)?;
    let dir = project_dir()?;

    if key == "control.token" {
        let settings = Settings::load(&dir, &Overrides::default())?;
        let endpoint = settings.control_endpoint().context(
            "a token is stored against one control plane, so the address has to be known before \
             the right one can be removed",
        )?;
        let mut credentials = settings::load_credentials()?;
        let removed = credentials.endpoints.remove(endpoint).is_some();
        let path = settings::save_credentials(&credentials)?;

        if output.json {
            return output.emit(&serde_json::json!({
                "key": key,
                "removed": removed,
                "endpoint": endpoint,
                "file": path,
            }));
        }
        if removed {
            println!(
                "\n  {} token for {} removed from {}",
                "✓".green(),
                endpoint.cyan(),
                path.display().to_string().dimmed()
            );
        } else {
            println!(
                "\n  {}",
                format!("no token was stored for {endpoint}; nothing to remove").dimmed()
            );
        }
        if std::env::var(settings::ENV_CONTROL_TOKEN).is_ok() {
            println!(
                "  {}",
                format!(
                    "note: {} is still set in this shell and is what commands will use.",
                    settings::ENV_CONTROL_TOKEN
                )
                .yellow()
            );
        }
        return Ok(());
    }

    let mut project = settings::load_project_file(&dir)?;
    let removed = match key {
        "control.endpoint" => project
            .control
            .as_mut()
            .and_then(|c| c.endpoint.take())
            .is_some(),
        "telemetry.endpoint" => project
            .telemetry
            .as_mut()
            .and_then(|t| t.endpoint.take())
            .is_some(),
        "defaults.app" => project
            .defaults
            .as_mut()
            .and_then(|d| d.app.take())
            .is_some(),
        "defaults.environment" => project
            .defaults
            .as_mut()
            .and_then(|d| d.environment.take())
            .is_some(),
        _ => unreachable!("check_key accepted an unknown key"),
    };
    let path = settings::save_project_file(&dir, &project)?;

    if output.json {
        return output.emit(&serde_json::json!({
            "key": key,
            "removed": removed,
            "file": path,
        }));
    }
    if removed {
        println!(
            "\n  {} {} removed from {}",
            "✓".green(),
            key.cyan(),
            path.display().to_string().dimmed()
        );
    } else {
        println!(
            "\n  {}",
            format!("{key} was not set in {}; nothing to remove", path.display()).dimmed()
        );
    }
    Ok(())
}

pub fn list(output: Output) -> Result<()> {
    let dir = project_dir()?;
    let settings = Settings::load(&dir, &Overrides::default())?;
    let project_path = dir.join(PROJECT_FILE);
    let credentials_path = settings::credentials_path();

    let rows: Vec<(&str, Option<String>, Option<Source>)> = vec![
        (
            "control.endpoint",
            settings.control_endpoint.as_ref().map(|s| s.value.clone()),
            settings.control_endpoint.as_ref().map(|s| s.source.clone()),
        ),
        (
            "control.token",
            settings.token().map(|_| "set".to_string()),
            settings.token_source().cloned(),
        ),
        (
            "telemetry.endpoint",
            settings
                .telemetry_endpoint
                .as_ref()
                .map(|s| s.value.clone()),
            settings
                .telemetry_endpoint
                .as_ref()
                .map(|s| s.source.clone()),
        ),
        (
            "defaults.app",
            settings.app.as_ref().map(|s| s.value.clone()),
            settings.app.as_ref().map(|s| s.source.clone()),
        ),
        (
            "defaults.environment",
            Some(settings.environment.value.clone()),
            Some(settings.environment.source.clone()),
        ),
    ];

    if output.json {
        return output.emit(&serde_json::json!({
            "project_file": project_path,
            "credentials_file": credentials_path,
            "settings": rows
                .iter()
                .map(|(key, value, source)| serde_json::json!({
                    "key": key,
                    "value": value,
                    "source": source.as_ref().map(|s| s.to_string()),
                    "variable": variable_for(key),
                }))
                .collect::<Vec<_>>(),
        }));
    }

    println!("\n{}", "Configuration".bold());
    let mut table = Table::new(&["SETTING", "VALUE", "FROM", "OVERRIDE WITH"]);
    for (key, value, source) in &rows {
        table.row(vec![
            (*key).to_string(),
            value.clone().unwrap_or_else(|| "-".to_string()),
            source
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "not set".to_string()),
            variable_for(key).unwrap_or("-").to_string(),
        ]);
    }
    println!();
    table.print();

    println!("\n{}", "Files".bold());
    println!(
        "  {:<20} {} {}",
        "project",
        project_path.display(),
        if project_path.exists() {
            "".to_string()
        } else {
            "(does not exist yet)".dimmed().to_string()
        }
    );
    println!(
        "  {:<20} {} {}",
        "credentials",
        credentials_path.display(),
        if credentials_path.exists() {
            "".to_string()
        } else {
            "(does not exist yet)".dimmed().to_string()
        }
    );
    println!(
        "\n  {}",
        "The project file is meant to be committed. Tokens go in the credentials file only - a \
         token committed once has to be rotated, so `deis` refuses to write one into the project."
            .dimmed()
    );
    Ok(())
}

/// The value plus its source, for a setting that may be absent.
fn described(setting: Option<&settings::Setting>) -> (Option<String>, Option<Source>) {
    (
        setting.map(|s| s.value.clone()),
        setting.map(|s| s.source.clone()),
    )
}

/// Accept a key, or list the ones that exist.
///
/// Returned as a `&'static str` so the rest of the module matches on the
/// canonical spelling rather than on whatever the user typed.
fn check_key(key: &str) -> Result<&'static str> {
    let wanted = key.trim().to_ascii_lowercase();
    match KEYS.iter().find(|k| **k == wanted) {
        Some(key) => Ok(key),
        None => bail!(
            "{key:?} is not a setting. The settings are:\n  {}",
            KEYS.join("\n  ")
        ),
    }
}

/// Which environment variable overrides a setting for one shell.
fn variable_for(key: &str) -> Option<&'static str> {
    match key {
        "control.endpoint" => Some(settings::ENV_CONTROL_ENDPOINT),
        "control.token" => Some(settings::ENV_CONTROL_TOKEN),
        "telemetry.endpoint" => Some(settings::ENV_TELEMETRY_ENDPOINT),
        "defaults.app" => Some(settings::ENV_APP),
        "defaults.environment" => Some(settings::ENV_ENVIRONMENT),
        _ => None,
    }
}

/// Whether an environment variable is currently hiding the file this write
/// just changed. Saying so turns "I set it and nothing happened" into one
/// line of output instead of an afternoon.
fn is_shadowed(key: &str) -> bool {
    variable_for(key)
        .and_then(|name| std::env::var(name).ok())
        .is_some_and(|value| !value.trim().is_empty())
}

/// The control plane is reached with an HTTP client, which needs a scheme.
/// A bare `host:port` parses as a URL whose scheme is the host, and the
/// failure surfaces much later as an unhelpful transport error.
fn check_http_endpoint(key: &str, value: &str) -> Result<()> {
    let parsed = url::Url::parse(value).map_err(|e| {
        anyhow::anyhow!(
            "{value:?} is not a usable address for {key}: {e}\nWrite it with a scheme, for \
             example http://127.0.0.1:8085."
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!(
            "{value:?} is not a usable address for {key}: its scheme is {:?}, but the control \
             plane speaks HTTP. Write it as http://host:port or https://host:port.",
            parsed.scheme()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_key_lists_the_ones_that_exist() {
        let error = check_key("control.url").unwrap_err().to_string();
        for key in KEYS {
            assert!(error.contains(key), "{error}");
        }
    }

    #[test]
    fn keys_are_matched_case_insensitively_and_trimmed() {
        assert_eq!(check_key(" Control.Endpoint ").unwrap(), "control.endpoint");
    }

    /// A bare host:port would reach the HTTP client as a URL whose scheme is
    /// the host name, and the error a person then sees says nothing about
    /// the missing `http://`.
    #[test]
    fn an_address_without_a_scheme_is_refused_here() {
        let error = check_http_endpoint("control.endpoint", "127.0.0.1:8085")
            .unwrap_err()
            .to_string();
        assert!(error.contains("http://127.0.0.1:8085"), "{error}");
        check_http_endpoint("control.endpoint", "https://control.example").unwrap();

        let error = check_http_endpoint("control.endpoint", "grpc://control.example")
            .unwrap_err()
            .to_string();
        assert!(error.contains("speaks HTTP"), "{error}");
    }
}
