//! `deis secret set|list|rm` - an environment's secret values.
//!
//! Three properties hold here and are worth stating, because each of them
//! is a place where the obvious implementation would have leaked something:
//!
//! * A value can be written and never read back. `list` prints names and
//!   timestamps; there is no `get`, because the control plane has no
//!   endpoint that returns a value to a human caller.
//! * `set` always reads the value from redirected stdin, so plaintext never
//!   reaches shell history, argv, another user's `ps` output or a prompt that
//!   echoes it.
//! * A secret is not part of the immutable bundle, so changing one produces
//!   no new version. It reaches the running code only because the control
//!   plane re-delivers the current version with the new environment - which
//!   is why every write here prints where that delivery landed, and why a
//!   node that did not take it fails the command.

use std::io::Read;

use anyhow::{Context, Result, bail};
use colored::Colorize;

use crate::commands::deploy::{fail_on_incomplete_delivery, print_delivery};
use crate::commands::{Session, escaped};
use crate::render::{Output, Table, age, timestamp};
use crate::settings::Overrides;

pub async fn set(
    app: Option<String>,
    environment: Option<String>,
    name: String,
    output: Output,
) -> Result<()> {
    let session = Session::open(&Overrides {
        app,
        environment,
        // Not a storage command: there is no namespace inside a tenant to
        // choose, only the tenant itself.
        binding: None,
    })?;
    let app = session.app()?.to_string();
    let environment = session.environment()?.to_string();
    check_secret_name(&name)?;

    let value = read_value_from_stdin()?;
    deis_wire::validate_secret_value(&value).map_err(anyhow::Error::new)?;

    let response = session
        .control
        .put_secret(&app, &environment, &name, &value)
        .await?;

    if output.json {
        output.emit(&serde_json::json!({
            "app": app,
            "environment": environment,
            "secret": response.secret,
            "delivery": response.delivery,
        }))?;
    } else {
        println!(
            "\n  {} {} of {} stored (never printed, never logged)",
            "✓".green(),
            name.cyan(),
            environment.cyan()
        );
        println!(
            "  {}",
            "a secret creates no new version, so the running one was delivered again with it:"
                .dimmed()
        );
        print_delivery(&response.delivery);
    }

    fail_on_incomplete_delivery(Some(&response.delivery), &environment)
}

pub async fn list(app: Option<String>, environment: Option<String>, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app,
        environment,
        // Not a storage command: there is no namespace inside a tenant to
        // choose, only the tenant itself.
        binding: None,
    })?;
    let app = session.app()?.to_string();
    let environment = session.environment()?.to_string();

    let secrets = session.control.list_secrets(&app, &environment).await?;

    if output.json {
        // Names and timestamps, exactly as they came back. There is no
        // field for a value anywhere in this CLI's types.
        return output.emit(&serde_json::json!({
            "app": app,
            "environment": environment,
            "secrets": secrets,
        }));
    }

    println!(
        "\n{} {} {} {}",
        "Secrets of".bold(),
        app.cyan(),
        "in".bold(),
        environment.cyan()
    );

    if secrets.is_empty() {
        println!(
            "\n  {}",
            format!("none set. `deis secret set <NAME> -e {environment}` adds one.").dimmed()
        );
        return Ok(());
    }

    let mut table = Table::new(&["NAME", "REVISION", "UPDATED", "AGE"]);
    for secret in &secrets {
        // A secret's *name* - never its value, which this command cannot
        // read back at all. The name was chosen by whoever set it, which for
        // a scoped credential is not necessarily the operator reading this.
        table.row(vec![
            escaped(&secret.name),
            secret
                .revision
                .as_deref()
                .map(escaped)
                .unwrap_or_else(|| "-".to_string()),
            timestamp(secret.updated_at),
            age(secret.updated_at),
        ]);
    }
    println!();
    table.print();
    println!(
        "\n  {}",
        "Values are not shown and cannot be read back; set one again to change it.".dimmed()
    );
    Ok(())
}

pub async fn remove(
    app: Option<String>,
    environment: Option<String>,
    name: String,
    output: Output,
) -> Result<()> {
    let session = Session::open(&Overrides {
        app,
        environment,
        // Not a storage command: there is no namespace inside a tenant to
        // choose, only the tenant itself.
        binding: None,
    })?;
    let app = session.app()?.to_string();
    let environment = session.environment()?.to_string();

    let response = session
        .control
        .delete_secret(&app, &environment, &name)
        .await?;

    if output.json {
        output.emit(&serde_json::json!({
            "app": app,
            "environment": environment,
            "deleted": response.deleted,
            "delivery": response.delivery,
        }))?;
    } else {
        println!(
            "\n  {} {} removed from {}",
            "✓".green(),
            name.cyan(),
            environment.cyan()
        );
        println!(
            "  {}",
            format!(
                "code reading context.env.{name} now sees nothing; the running version was \
                 delivered again without it:"
            )
            .dimmed()
        );
        print_delivery(&response.delivery);
    }

    fail_on_incomplete_delivery(Some(&response.delivery), &environment)
}

/// Read the value from stdin, keeping it off the command line.
///
/// Exactly one trailing newline is removed, because `echo value |` adds one
/// and nobody means it; anything else - inner newlines, leading or trailing
/// spaces - is part of the value, since PEM keys and base64 blobs both have
/// shapes that a helpful trim would quietly break.
fn read_value_from_stdin() -> Result<String> {
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!(
            "refusing to read a secret from an echoing terminal; pipe it through stdin, for \
             example `printf %s \"$VALUE\" | deis secret set NAME`"
        );
    }
    let mut value = String::new();
    std::io::stdin()
        .take((deis_wire::ENVIRONMENT_VALUE_BYTES_LIMIT + 1) as u64)
        .read_to_string(&mut value)
        .context("reading the secret value from stdin")?;
    if let Some(stripped) = value.strip_suffix('\n') {
        value = stripped.strip_suffix('\r').unwrap_or(stripped).to_string();
    }
    if value.len() > deis_wire::ENVIRONMENT_VALUE_BYTES_LIMIT {
        bail!("secret value exceeds the v1 UTF-8 byte limit");
    }
    Ok(value)
}

/// Refuse a name that cannot become an environment variable.
///
/// The value is injected as `context.env.<name>`, so a name with a `=`, a
/// space or a NUL in it produces an application that starts and then cannot
/// see the secret at all - the slowest possible way to find a typo.
fn check_secret_name(name: &str) -> Result<()> {
    let mut characters = name.chars();
    let usable = characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_');
    if usable {
        return Ok(());
    }
    bail!(
        "{name:?} cannot be a secret name exposed through context.env: it must start with a \
         letter or '_', followed only by letters, digits or '_'"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_that_could_not_reach_context_env_is_refused_here() {
        check_secret_name("API_KEY").unwrap();
        check_secret_name("_private2").unwrap();
        for bad in ["", "API KEY", "A=B", "a\nb", "sentry.dsn", "api-key"] {
            let error = check_secret_name(bad)
                .expect_err("{bad:?} must be refused")
                .to_string();
            assert!(error.contains("context.env"), "{error}");
        }
    }
}
