//! `deis login` - the way in on a fresh install.
//!
//! A control plane that has never had a live credential mints a one-time
//! bootstrap token and writes it, mode 0600, to `<data_dir>/bootstrap.token`.
//! That credential may do exactly one thing: mint the operator's standing
//! token off the bootstrap account every installation carries. This command
//! does that one thing, stores the standing token where every later command
//! finds it, and lets the control plane retire the bootstrap credential -
//! which its first successful use does on its own, revoking the token and
//! deleting its file.
//!
//! The standing token is unscoped, so its account's role is its only limit:
//! the same shape the control plane's own first-login test mints. An operator
//! who wants narrower credentials mints them with `deis token create` and
//! revokes this one.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use colored::Colorize;

use crate::platform::Control;
use crate::render::Output;
use crate::settings::{CredentialEntry, ENV_CONTROL_ENDPOINT, load_credentials, save_credentials};

pub async fn execute(bootstrap: PathBuf, endpoint: Option<String>, output: Output) -> Result<()> {
    let secret = std::fs::read_to_string(&bootstrap)
        .with_context(|| format!("reading {}", bootstrap.display()))?
        .trim()
        .to_string();
    if secret.is_empty() {
        bail!(
            "{} holds no credential; there is nothing to log in with",
            bootstrap.display()
        );
    }

    let endpoint = match endpoint {
        Some(url) => url,
        None => match std::env::var(ENV_CONTROL_ENDPOINT) {
            Ok(value) if !value.is_empty() => value,
            _ => bail!(
                "no control plane address. Set {ENV_CONTROL_ENDPOINT} or pass --endpoint <url>; \
                 this is the address the control plane serves its API on, and the credential \
                 about to be minted is stored against it."
            ),
        },
    };

    // Present the bootstrap credential itself. The control plane narrows it
    // to this one mint and revokes it the moment the mint succeeds.
    let control = Control::new(&endpoint, Some(&secret))?;
    let minted = control
        .create_token("bootstrap", "owner", &[], true)
        .await
        .context("logging in: the control plane refused to trade the bootstrap credential")?;

    // Key the credential by the address it answers to, spelled the way
    // `Settings` looks it up - without any trailing slash.
    let mut credentials = load_credentials()?;
    let base = control.base_url().to_string();
    credentials
        .endpoints
        .entry(base.clone())
        .or_insert_with(|| CredentialEntry {
            token: String::new(),
        })
        .token = minted.secret;
    let stored_at = save_credentials(&credentials)?;

    if output.json {
        return output.emit(&serde_json::json!({
            "control_endpoint": base,
            "account": minted.token.account,
            "token": {
                "id": minted.token.id,
                "name": minted.token.name,
                "unscoped": minted.token.unscoped,
            },
            "credentials_file": stored_at,
        }));
    }

    println!(
        "\n{} Logged in to {} as account {}",
        "✓".green().bold(),
        base.cyan(),
        minted.token.account.cyan()
    );
    println!(
        "  {}",
        format!(
            "the standing credential {} was stored in {}",
            minted.token.name.cyan(),
            stored_at.display().to_string().cyan()
        )
        .dimmed()
    );
    println!(
        "  {}",
        "the bootstrap credential is spent: the control plane revoked it and deleted its file \
         on this login's success. This token is unscoped - its account's role is its only \
         limit; mint scoped credentials with `deis token create` and revoke this one when \
         they exist."
            .dimmed()
    );
    Ok(())
}
