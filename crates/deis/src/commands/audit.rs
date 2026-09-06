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

use crate::commands::{Session, escape_control, escaped};
use crate::platform::{AuditEntry, AuditFilter};
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

    // The request id is the handle an operator searches the control plane's
    // log with, and the one the refused caller was shown. Lines from before
    // it was recorded have none; when no line on this page has one the
    // column is left out rather than printed as a column of dashes.
    let with_request_ids = entries.iter().any(|entry| entry.request_id.is_some());
    let mut table = Table::new(&headers(with_request_ids));
    for entry in &entries {
        // Every cell of this table is a string deis-control recorded off a
        // request. The audit log's whole job is to hold what *refused*
        // callers did, so it is the one table in this CLI guaranteed to
        // contain text chosen by someone who was trying to get somewhere they
        // were not allowed - which makes an unescaped cell here the highest
        // value one in the tree, not the lowest.
        let who = who_cell(entry);
        let target = target_cell(entry);
        let outcome = match entry.outcome.as_str() {
            "accepted" => entry.outcome.green().to_string(),
            "refused" => format!("{} ({})", entry.outcome.red(), entry.status),
            // The arm a value this CLI does not know about lands in, so it is
            // the arm that prints the string as sent rather than one of two
            // literals.
            _ => format!(
                "{} ({})",
                escape_control(&entry.outcome).yellow(),
                entry.status
            ),
        };
        let mut row = vec![
            timestamp(entry.at),
            who,
            escaped(&entry.action),
            target,
            // The peer address, as deis-control read it off the connection or
            // out of a forwarding header.
            entry
                .peer
                .as_deref()
                .map(escaped)
                .unwrap_or_else(|| "-".to_string()),
            outcome,
        ];
        if with_request_ids {
            row.push(request_cell(entry));
        }
        table.row(row);
    }
    println!();
    table.print();

    // Why a refusal was refused does not fit in a column and is the whole
    // point of the line, so it is printed underneath.
    for entry in entries.iter().filter(|e| e.outcome == "refused") {
        if let Some(detail) = &entry.detail {
            // The refusal's own sentence, printed full width under the
            // table: the longest attacker-influenced string this command
            // shows, and the one with no column to be cut by.
            println!(
                "  {} {}",
                format!("{}:", escape_control(&entry.action)).dimmed(),
                escape_control(detail).dimmed()
            );
        }
    }
    println!();
    Ok(())
}

/// The column headings, with the request id column only when this page
/// has an id to put in it.
fn headers(with_request_ids: bool) -> Vec<&'static str> {
    let mut headers = vec!["WHEN", "WHO", "ACTION", "TARGET", "FROM", "OUTCOME"];
    if with_request_ids {
        headers.push("REQUEST");
    }
    headers
}

/// The request id, as one cell. The control plane mints these itself and
/// never reflects a caller's garbage into the log, but the cell is escaped
/// like every other one here: the rule is by provenance of the table, not
/// by trust in one column.
fn request_cell(entry: &AuditEntry) -> String {
    entry
        .request_id
        .as_deref()
        .map(escaped)
        .unwrap_or_else(|| "-".to_string())
}

/// Who did it, as one cell.
///
/// Both halves come off a request deis-control refused or accepted: an
/// account name, or the name of the credential presented. The audit log's
/// whole job is to hold what refused callers did, so it is the one table in
/// this CLI guaranteed to contain text chosen by someone trying to get
/// somewhere they were not allowed - which makes an unescaped cell here the
/// highest-value one in the tree, not the lowest.
fn who_cell(entry: &AuditEntry) -> String {
    match (&entry.actor_account, &entry.actor_credential) {
        (Some(account), Some(credential)) => format!(
            "{} ({})",
            escape_control(account),
            escape_control(credential)
        ),
        (Some(account), None) => escaped(account),
        (None, Some(credential)) => format!("key {}", escape_control(credential)),
        (None, None) => "anonymous".to_string(),
    }
}

/// What it was done to, as one cell. Same provenance, same rule.
fn target_cell(entry: &AuditEntry) -> String {
    match (&entry.app_id, &entry.environment, &entry.target) {
        (Some(app), Some(env), Some(name)) => format!(
            "{}/{} {}",
            escape_control(app),
            escape_control(env),
            escape_control(name)
        ),
        (Some(app), Some(env), None) => {
            format!("{}/{}", escape_control(app), escape_control(env))
        }
        (Some(app), None, Some(name)) => {
            format!("{} {}", escape_control(app), escape_control(name))
        }
        (Some(app), None, None) => escaped(app),
        (None, _, Some(name)) => escaped(name),
        (None, _, None) => "-".to_string(),
    }
}

#[cfg(test)]
mod terminal_safety_test {
    use super::*;

    fn entry() -> AuditEntry {
        AuditEntry {
            id: 1,
            at: 1_700_000_000,
            actor_kind: "token".to_string(),
            actor_account: None,
            actor_credential: None,
            action: "app.deploy".to_string(),
            app_id: None,
            environment: None,
            target: None,
            method: "POST".to_string(),
            path: "/api/apps".to_string(),
            peer: None,
            outcome: "refused".to_string(),
            status: 403,
            detail: None,
            request_id: None,
        }
    }

    /// The request id column appears only when there is an id to show, and
    /// a line without one on such a page prints a dash rather than nothing.
    #[test]
    fn the_request_column_is_there_only_when_a_line_carries_an_id() {
        assert_eq!(headers(false).len(), 6);
        assert_eq!(headers(true).last(), Some(&"REQUEST"));
        assert_eq!(request_cell(&entry()), "-");
        let mut with_id = entry();
        with_id.request_id = Some("018f2518-7e38-7f80-8000-000000000001".to_string());
        assert_eq!(
            request_cell(&with_id),
            "018f2518-7e38-7f80-8000-000000000001"
        );
    }

    /// Every arm of both cells, not the one an attacker happened to use
    /// first. A cell that is safe only when the fields it prefers are present
    /// is the shape that produces the next finding.
    #[test]
    fn no_arm_of_the_who_cell_lets_an_escape_sequence_through() {
        let paint = "\u{1b}[2J\u{1b}[31mADMIN";
        let mut both = entry();
        both.actor_account = Some(paint.to_string());
        both.actor_credential = Some(paint.to_string());
        let mut account_only = entry();
        account_only.actor_account = Some(paint.to_string());
        let mut credential_only = entry();
        credential_only.actor_credential = Some(paint.to_string());

        for (name, entry) in [
            ("account and credential", both),
            ("account only", account_only),
            ("credential only", credential_only),
        ] {
            let rendered = who_cell(&entry);
            assert!(!rendered.contains('\u{1b}'), "{name}: {rendered:?}");
            assert_eq!(rendered.lines().count(), 1, "{name}: {rendered:?}");
        }
        assert_eq!(who_cell(&entry()), "anonymous");
    }

    #[test]
    fn no_arm_of_the_target_cell_lets_a_newline_through() {
        let forged = "demo\n2026-09-02  ACCEPTED  someone-else";
        let mut all = entry();
        all.app_id = Some(forged.to_string());
        all.environment = Some(forged.to_string());
        all.target = Some(forged.to_string());
        let mut app_env = entry();
        app_env.app_id = Some(forged.to_string());
        app_env.environment = Some(forged.to_string());
        let mut app_target = entry();
        app_target.app_id = Some(forged.to_string());
        app_target.target = Some(forged.to_string());
        let mut app_only = entry();
        app_only.app_id = Some(forged.to_string());
        let mut target_only = entry();
        target_only.target = Some(forged.to_string());

        for (name, entry) in [
            ("app, env and target", all),
            ("app and env", app_env),
            ("app and target", app_target),
            ("app only", app_only),
            ("target only", target_only),
        ] {
            let rendered = target_cell(&entry);
            assert_eq!(rendered.lines().count(), 1, "{name}: {rendered:?}");
            assert!(rendered.contains("demo\\n2026"), "{name}: {rendered:?}");
        }
        assert_eq!(target_cell(&entry()), "-");
    }

    #[test]
    fn an_ordinary_row_reads_as_it_did() {
        let mut entry = entry();
        entry.actor_account = Some("alice".to_string());
        entry.app_id = Some("demo".to_string());
        entry.environment = Some("production".to_string());
        assert_eq!(who_cell(&entry), "alice");
        assert_eq!(target_cell(&entry), "demo/production");
    }
}
