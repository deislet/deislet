//! `deis deployments` - every time one environment's pointer moved, newest
//! first.
//!
//! Three commands read three different questions and are deliberately not one
//! command: `deis status` says what an environment runs *now*, `deis versions`
//! says what an application has ever built, and neither says when a particular
//! environment changed or what whoever changed it wrote down. That last one is
//! where an incident starts - the row above the change an operator suspects is
//! usually the change they are looking for - so it is its own verb, scoped to
//! one environment, with `-e` like every other environment-scoped command.
//!
//! One page, on purpose. An environment gains a row on every deploy, every
//! rollback and every secret change, so its history has no ceiling; this reads
//! the newest `--limit` rows in a single request and says how many are behind
//! them, rather than walking a year of rows to print twenty.

use anyhow::Result;
use colored::Colorize;

use crate::commands::{Session, escaped, escaped_or_dash};
use crate::platform::MAX_LIST_LIMIT;
use crate::render::{Output, Table, age, timestamp};
use crate::settings::Overrides;

pub async fn execute(
    app: Option<String>,
    environment: Option<String>,
    limit: usize,
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

    let history = session
        .control
        .list_deployments(&app, &environment, limit)
        .await?;

    if output.json {
        // The rows exactly as the control plane serves them, canonical ids
        // included: a script correlating a deployment with an audit line or
        // an operation needs the identity, not the rendering.
        return output.emit(&serde_json::json!({
            "app": app,
            "environment": environment,
            "control_endpoint": session.control.base_url(),
            "total": history.total,
            "shown": history.deployments.len(),
            "deployments": history.deployments,
        }));
    }

    println!(
        "\n{} {} {} {}",
        "Deployments of".bold(),
        app.cyan(),
        "in".bold(),
        environment.cyan()
    );

    if history.deployments.is_empty() {
        println!(
            "\n  {}",
            format!(
                "{environment} has never been deployed, so its pointer has never moved. `deis \
                 deploy -e {environment}` points it at a version."
            )
            .yellow()
        );
        return Ok(());
    }

    let mut table = Table::new(&["WHEN", "AGE", "VERSION", "NOTE"]);
    for deployment in &history.deployments {
        table.row(vec![
            timestamp(deployment.created_at),
            age(deployment.created_at),
            // Ids the control plane assigned, and a note somebody typed at
            // `deis deploy -m`. The note is free text stored by the control
            // plane and read back on every operator's terminal, so it goes
            // through the same escaping every other operator-supplied cell in
            // this CLI does.
            escaped(&deployment.version_id),
            escaped_or_dash(deployment.note.as_deref()),
        ]);
    }
    println!();
    table.print();

    if let Some(advice) = deeper_history(history.deployments.len(), history.total) {
        println!("\n  {}", advice.dimmed());
    }
    println!(
        "  {}",
        format!(
            "VERSION is the version's id; `deis versions -a {app} --json` maps one to the number \
             `deis rollback --to` takes. Deployment and canonical ids are in --json."
        )
        .dimmed()
    );
    Ok(())
}

/// What to say under the table about the rows that were not printed, or
/// `None` when there are none.
///
/// Three different sentences, because there are three different situations
/// and telling them apart is the whole value of the line. When the whole
/// history fits in one page, a bigger `--limit` reaches the rest and the
/// number to type is worth printing. When it does not, `--limit` still
/// reaches the newest page-sized slice, and the line must say exactly that
/// rather than call that slice "the rest". At the ceiling, no `--limit`
/// reaches further: this command reads one page and nothing else in the CLI
/// reads deeper (`deis audit` is the same one-page read), so the honest
/// pointer is the API's page cursor, which is where the older rows are.
fn deeper_history(shown: usize, total: usize) -> Option<String> {
    if total <= shown {
        return None;
    }
    let advice = if total <= MAX_LIST_LIMIT {
        format!("pass --limit {total} for the rest")
    } else if shown < MAX_LIST_LIMIT {
        format!(
            "pass --limit {MAX_LIST_LIMIT} for the newest {MAX_LIST_LIMIT}; this command reads \
             one page, so anything older is only reachable through the API's page cursor"
        )
    } else {
        format!(
            "{MAX_LIST_LIMIT} is the most this command reads (one page), so anything older is \
             only reachable through the API's page cursor: GET \
             /api/apps/<app>/environments/<environment>/deployments?limit={MAX_LIST_LIMIT}&cursor=<nextCursor>"
        )
    };
    Some(format!("{shown} of {total} shown; {advice}."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_history_that_fits_says_nothing_further() {
        assert_eq!(deeper_history(20, 20), None);
        // A total the control plane counted lower than the page it filled is
        // a control-plane bug the reader still must not be nagged about.
        assert_eq!(deeper_history(20, 3), None);
    }

    #[test]
    fn a_history_within_one_page_names_the_limit_that_reaches_it() {
        let advice = deeper_history(20, 137).expect("137 rows do not fit in 20");
        assert_eq!(advice, "20 of 137 shown; pass --limit 137 for the rest.");
    }

    /// `--limit 1000` shows the newest 1000 of 4000, not "the rest", and the
    /// line has to say so: an operator in an incident must not be told that
    /// a flag reaches rows it cannot.
    #[test]
    fn a_history_past_the_page_ceiling_names_the_ceiling_as_the_newest_slice() {
        let advice = deeper_history(20, 4_000).expect("4000 rows do not fit in 20");
        assert!(advice.starts_with("20 of 4000 shown;"), "{advice}");
        assert!(
            advice.contains(&format!(
                "pass --limit {MAX_LIST_LIMIT} for the newest {MAX_LIST_LIMIT}"
            )),
            "{advice}"
        );
        assert!(!advice.contains("for the rest"), "{advice}");
        assert!(advice.contains("API's page cursor"), "{advice}");
    }

    /// The case a naive "pass --limit <total>" would get wrong: the operator
    /// is already reading the largest page there is, so the line must say
    /// where the older rows actually are (the API cursor; no CLI command
    /// reads deeper, `deis audit` included) instead of naming a flag value
    /// that changes nothing or a command with the same ceiling.
    #[test]
    fn a_full_page_of_a_longer_history_points_at_the_api_cursor() {
        let advice =
            deeper_history(MAX_LIST_LIMIT, 4_000).expect("4000 rows do not fit in one page");
        assert!(advice.starts_with("1000 of 4000 shown;"), "{advice}");
        assert!(!advice.contains("pass --limit"), "{advice}");
        assert!(!advice.contains("deis audit"), "{advice}");
        assert!(advice.contains("cursor=<nextCursor>"), "{advice}");
        assert!(
            advice.contains("/api/apps/<app>/environments/<environment>/deployments"),
            "{advice}"
        );
    }
}
