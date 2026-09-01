//! `deis db query|execute` - SQL against an environment's database.
//!
//! `query` runs a `SELECT` and shows what came back; `execute` runs anything
//! else. The split mirrors deis-store's own `Database` service, which has
//! separate `Query` and `Execute` RPCs rather than one that returns rows only
//! sometimes - see `crates/deis-store/proto/storage.proto`.
//!
//! `execute` is the command described in the section of the gap review this
//! closes as "exactly the command someone runs at 2am": one line of SQL
//! against a live tenant's only database file, with no undo. It refuses to
//! run without a confirmation that requires reading and typing back the
//! tenant it is about to hit - see [`crate::render::confirm_destructive`] -
//! unless `-y`/`--yes` was given, which is what a script or a migration
//! runner should pass instead.

use anyhow::{Result, anyhow};
use colored::Colorize;

use crate::commands::Session;
#[cfg(test)]
use crate::render::destructive_subject;
use crate::render::{Output, Table, confirm_destructive};
use crate::settings::Overrides;

pub struct QueryArgs {
    pub app: Option<String>,
    pub environment: Option<String>,
    /// Which namespace inside the tenant to act in. `None` is the unnamed one:
    /// what `context.services.*` reaches, and where an application that
    /// declares no binding keeps everything it has.
    pub binding: Option<String>,
    pub sql: String,
    /// One per `?` placeholder, in order. Each is parsed as JSON when it
    /// parses (`42`, `true`, `null`, `"already-quoted"`) and taken as a plain
    /// string otherwise (`alice`), so a caller only has to think about
    /// quoting for the values that need it.
    pub params: Vec<String>,
}

pub async fn query(args: QueryArgs, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app: args.app,
        environment: args.environment,
        binding: args.binding,
    })?;
    let tenant = session.namespace()?;
    let storage = session.storage().await?;

    let result = storage
        .db_query(&args.sql, params_to_json(&args.params))
        .await?;

    let rows = result
        .rows
        .as_array()
        .ok_or_else(|| {
            anyhow!(
                "deis-store answered a query with rows that are not a JSON array: {}",
                result.rows
            )
        })?
        .clone();

    if output.json {
        return output.emit(&serde_json::json!({
            "tenant": tenant,
            "columns": result.columns,
            "rows": rows,
        }));
    }

    if rows.is_empty() {
        println!("\n  {}", "0 rows".dimmed());
        return Ok(());
    }

    let mut table = Table::new(
        &result
            .columns
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    );
    for row in &rows {
        table.row(
            result
                .columns
                .iter()
                .map(|column| cell(row.get(column)))
                .collect(),
        );
    }
    println!();
    table.print();
    println!("\n  {}", format!("{} row(s)", rows.len()).dimmed());
    Ok(())
}

pub struct ExecuteArgs {
    pub app: Option<String>,
    pub environment: Option<String>,
    /// Which namespace inside the tenant to act in. `None` is the unnamed one:
    /// what `context.services.*` reaches, and where an application that
    /// declares no binding keeps everything it has.
    pub binding: Option<String>,
    pub sql: String,
    pub params: Vec<String>,
    pub yes: bool,
}

pub async fn execute(args: ExecuteArgs, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app: args.app,
        environment: args.environment,
        binding: args.binding,
    })?;
    let tenant = session.namespace()?;

    confirm_destructive(&tenant, &execute_action(&args.sql), args.yes)?;

    let storage = session.storage().await?;
    let (rows_affected, last_insert_id) = storage
        .db_execute(&args.sql, params_to_json(&args.params))
        .await?;

    if output.json {
        return output.emit(&serde_json::json!({
            "tenant": tenant,
            "sql": args.sql,
            "rows_affected": rows_affected,
            "last_insert_id": last_insert_id,
        }));
    }
    println!(
        "\n  {} {} row(s) affected in {}",
        "✓".green(),
        rows_affected,
        tenant.cyan()
    );
    if let Some(id) = last_insert_id {
        println!("  {}", format!("last insert id: {id}").dimmed());
    }
    Ok(())
}

/// What `deis db execute` is about to do, in the words the confirmation puts
/// in front of the operator.
///
/// A phrase rather than a `format!` at the call site so that the sentence a
/// person reads immediately before changing a live database is somewhere a
/// test can read it too. It ends at "the database", because
/// [`crate::render::confirm_destructive`] is what adds "on `<tenant>`".
fn execute_action(sql: &str) -> String {
    format!("run `{sql}` against the database")
}

/// One table cell, rendered for a person rather than for a machine: a bare
/// string with no surrounding quotes, `NULL` spelled out rather than left
/// blank (blank is what an *empty string* should look like), and everything
/// else - numbers, booleans, and the JSON arrays deis-store uses for blob
/// columns - as compact JSON.
fn cell(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => "NULL".to_string(),
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// Assemble `--param` values into the JSON-encoded array `Query`/`Execute`
/// expect, or `None` when there are none - an empty array and "no params
/// given" are the same thing to deis-store, but `None` is what lets a
/// statement with no placeholders skip the field entirely.
fn params_to_json(params: &[String]) -> Option<String> {
    if params.is_empty() {
        return None;
    }
    let values: Vec<serde_json::Value> = params
        .iter()
        .map(|p| serde_json::from_str(p).unwrap_or_else(|_| serde_json::Value::String(p.clone())))
        .collect();
    Some(serde_json::Value::Array(values).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The last thing a person reads before a statement runs against a live
    /// tenant's only database file. It read "About to run `DELETE FROM users`
    /// against the database of on demo.production." - two prepositions, one
    /// of them left over from a phrase that used to end differently - which
    /// is a sentence nobody can check their own intent against.
    #[test]
    fn the_confirmation_is_a_sentence_a_person_can_read_back() {
        let subject = destructive_subject(
            &execute_action("DELETE FROM users WHERE id = 7"),
            "demo.production",
        );
        assert_eq!(
            format!("About to {subject}."),
            "About to run `DELETE FROM users WHERE id = 7` against the database on \
             demo.production."
        );
    }

    #[test]
    fn no_params_is_none_not_an_empty_array() {
        assert_eq!(params_to_json(&[]), None);
    }

    #[test]
    fn each_param_is_parsed_as_json_when_it_parses() {
        let json = params_to_json(&[
            "42".to_string(),
            "true".to_string(),
            "null".to_string(),
            "\"already-quoted\"".to_string(),
        ])
        .unwrap();
        assert_eq!(json, r#"[42,true,null,"already-quoted"]"#);
    }

    #[test]
    fn a_param_that_is_not_json_is_taken_as_a_plain_string() {
        let json = params_to_json(&["alice".to_string()]).unwrap();
        assert_eq!(json, r#"["alice"]"#);
    }

    #[test]
    fn null_and_absent_are_both_shown_as_null_not_blank() {
        assert_eq!(cell(None), "NULL");
        assert_eq!(cell(Some(&serde_json::Value::Null)), "NULL");
    }

    #[test]
    fn a_string_cell_has_no_surrounding_quotes() {
        assert_eq!(cell(Some(&serde_json::json!("alice"))), "alice");
    }

    #[test]
    fn a_blob_cell_renders_as_compact_json() {
        assert_eq!(cell(Some(&serde_json::json!([0, 255, 16]))), "[0,255,16]");
    }
}
