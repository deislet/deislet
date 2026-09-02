//! `deis account` - the people and pipelines that may call this platform.
//!
//! An account is the thing an audit line blames, and its role is the ceiling
//! on everything its tokens may ever do. That is why the role leads the
//! table and why changing it is its own verb: narrowing an account narrows
//! every credential it already holds, at their next request, without anyone
//! having to hunt those credentials down.
//!
//! Nothing here can be done without `admin:*/*`, so a refusal is expected
//! when a developer's own token runs it; the control plane's message names
//! the scope that would have worked.

use anyhow::Result;
use colored::Colorize;

use crate::commands::{Session, escaped};
use crate::render::{Output, Table, timestamp};
use crate::settings::Overrides;

pub async fn list(output: Output) -> Result<()> {
    let session = Session::open(&Overrides::default())?;
    let accounts = session.control.list_accounts().await?;

    if output.json {
        return output.emit(&serde_json::json!({
            "control_endpoint": session.control.base_url(),
            "total": accounts.len(),
            "accounts": accounts,
        }));
    }

    println!(
        "\n{} {}",
        "Accounts on".bold(),
        session.control.base_url().dimmed()
    );

    if accounts.is_empty() {
        println!(
            "\n  {}",
            "no accounts yet. Until one exists, this platform authenticates only against the \
             api_keys in its configuration file, and every one of those is unscoped."
                .yellow()
        );
        return Ok(());
    }

    let mut table = Table::new(&["NAME", "ROLE", "STATE", "CREATED"]);
    for account in &accounts {
        // Same reasoning as the token table: this is the list an operator
        // reads before deciding who to disable.
        table.row(vec![
            escaped(&account.name),
            escaped(&account.role),
            match account.disabled_at {
                Some(_) => "disabled".red().to_string(),
                None => "active".to_string(),
            },
            timestamp(account.created_at),
        ]);
    }
    println!();
    table.print();
    println!();
    Ok(())
}

pub async fn create(name: String, role: String, output: Output) -> Result<()> {
    let session = Session::open(&Overrides::default())?;
    let account = session.control.create_account(&name, &role).await?;

    if output.json {
        return output.emit(&serde_json::to_value(&account)?);
    }

    println!(
        "\n{} {} {} {}",
        "Created account".green().bold(),
        account.name.cyan(),
        "with role".bold(),
        account.role.cyan()
    );
    println!(
        "  {}",
        format!(
            "It has no credential yet. `deis token create --account {} --scope <verb>:<app>/<env>` \
             mints one, and its secret is shown once.",
            account.name
        )
        .dimmed()
    );
    println!();
    Ok(())
}

pub async fn update(
    name: String,
    role: Option<String>,
    disable: bool,
    enable: bool,
    output: Output,
) -> Result<()> {
    if disable && enable {
        anyhow::bail!("--disable and --enable ask for opposite things; pass one of them");
    }
    if role.is_none() && !disable && !enable {
        anyhow::bail!(
            "nothing to change: pass --role to move the ceiling, or --disable / --enable to \
             switch the account off and back on"
        );
    }
    let disabled = if disable {
        Some(true)
    } else if enable {
        Some(false)
    } else {
        None
    };

    let session = Session::open(&Overrides::default())?;
    let account = session
        .control
        .update_account(&name, role.as_deref(), disabled)
        .await?;

    if output.json {
        return output.emit(&serde_json::to_value(&account)?);
    }

    println!(
        "\n{} {} {} {} {}",
        "Account".bold(),
        account.name.cyan(),
        "is now".bold(),
        account.role.cyan(),
        match account.disabled_at {
            Some(_) => "and switched off".red().to_string(),
            None => "and active".to_string(),
        }
    );
    println!(
        "  {}",
        "Every token of this account is measured against this from its next request; none of \
         them had to be reissued."
            .dimmed()
    );
    println!();
    Ok(())
}
