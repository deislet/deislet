//! `deis audit` - what was done to this platform, by whom, and how it ended.
//!
//! The columns are the questions asked after an incident, in the order they
//! are asked: when, who, what, to which application and environment, and what
//! happened. A refused call is printed as loudly as an accepted one, because
//! it is the line that shows a credential still deployed somewhere it should
//! not be.
//!
//! Reading it needs `admin:*/*`. There is no command that deletes a line and
//! no endpoint behind one: the log is append-only in the control plane, and
//! removing anything from it means having the database file.

use anyhow::Result;
use colored::Colorize;

use crate::commands::Session;
use crate::platform::AuditFilter;
use crate::render::{Output, Table, timestamp};
use crate::settings::Overrides;

// Each argument is one filter the control plane understands; grouping them
// would only move the same list into a struct the caller has to build.
#[allow(clippy::too_many_arguments)]
pub async fn execute(
    app: Option<String>,
    environment: Option<String>,
    account: Option<String>,
    action: Option<String>,
    outcome: Option<String>,
    since_minutes: Option<i64>,
    limit: usize,
    output: Output,
) -> Result<()> {
    // Deliberately not resolved through the project file the way `deis
    // deploy` resolves them. An operator reading this log is asking about the
    // whole platform, and a checkout that happens to name an application in
    // deis.toml must not silently narrow the answer to it. Only what was
    // typed on the command line filters anything.
    let session = Session::open(&Overrides::default())?;

    let filter = AuditFilter {
        app,
        environment,
        account,
        action,
        outcome,
        since: since_minutes.map(|minutes| chrono::Utc::now().timestamp() - minutes * 60),
        limit: Some(limit.max(1)),
    };

    let entries = session.control.read_audit(&filter).await?;

    if output.json {
        return output.emit(&serde_json::json!({
            "control_endpoint": session.control.base_url(),
            "total": entries.len(),
            "entries": entries,
        }));
    }

    println!(
        "\n{} {}",
        "Audit log of".bold(),
        session.control.base_url().dimmed()
    );

    if entries.is_empty() {
        println!(
            "\n  {}",
            "nothing recorded for that query. The log holds every change and every refusal; a \
             plain successful read is not an event and is not in it."
                .yellow()
        );
        return Ok(());
    }

    let mut table = Table::new(&["WHEN", "WHO", "ACTION", "TARGET", "FROM", "OUTCOME"]);
    for entry in &entries {
        let who = match (&entry.actor_account, &entry.actor_credential) {
            (Some(account), Some(credential)) => format!("{account} ({credential})"),
            (Some(account), None) => account.clone(),
            (None, Some(credential)) => format!("key {credential}"),
            (None, None) => "anonymous".to_string(),
        };
        let target = match (&entry.app_id, &entry.environment, &entry.target) {
            (Some(app), Some(env), Some(name)) => format!("{app}/{env} {name}"),
            (Some(app), Some(env), None) => format!("{app}/{env}"),
            (Some(app), None, Some(name)) => format!("{app} {name}"),
            (Some(app), None, None) => app.clone(),
            (None, _, Some(name)) => name.clone(),
            (None, _, None) => "-".to_string(),
        };
        let outcome = match entry.outcome.as_str() {
            "accepted" => entry.outcome.green().to_string(),
            "refused" => format!("{} ({})", entry.outcome.red(), entry.status),
            _ => format!("{} ({})", entry.outcome.yellow(), entry.status),
        };
        table.row(vec![
            timestamp(entry.at),
            who,
            entry.action.clone(),
            target,
            entry.peer.clone().unwrap_or_else(|| "-".to_string()),
            outcome,
        ]);
    }
    println!();
    table.print();

    // Why a refusal was refused does not fit in a column and is the whole
    // point of the line, so it is printed underneath.
    for entry in entries.iter().filter(|e| e.outcome == "refused") {
        if let Some(detail) = &entry.detail {
            println!(
                "  {} {}",
                format!("{}:", entry.action).dimmed(),
                detail.dimmed()
            );
        }
    }
    println!();
    Ok(())
}
