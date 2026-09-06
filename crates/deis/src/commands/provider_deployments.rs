//! `deis provider-deployments` - what the provider drivers were asked to do
//! in one environment, and the two ways to intervene.
//!
//! A provider deployment is not the pointer move `deis deployments` lists. It
//! is a durable workflow that drives somebody else's cloud through the
//! driver's steps, so it can be *in progress* for minutes, it can be asked to
//! stop, and once it has finished it can be undone - none of which is true of
//! a pointer that moves in one write. Those three facts are the four verbs
//! here:
//!
//! - `list` reads the newest page, newest first, exactly as `deis
//!   deployments` does and for the same reason: the history has no ceiling.
//! - `get` reads one workflow whole, steps included.
//! - `cancel` asks a workflow that is still moving to stop. It is fenced on
//!   the revision the caller just read, so a workflow that finished a moment
//!   ago is refused rather than half-cancelled.
//! - `rollback` records a *new* operation that drives a finished deployment's
//!   provider back to an earlier one's desired state.
//!
//! Cancel and rollback are deliberately not one verb with a flag. Cancelling
//! stops work that has not happened yet and cleans up what is reversible;
//! rolling back undoes work that has already taken effect, by doing more
//! work. An operator who confuses them at three in the morning gets a
//! different outcome, not a differently spelled one.

use anyhow::{Context, Result};
use colored::Colorize;
use deis_wire::CanonicalId;

use crate::commands::{Session, escaped, escaped_or_dash};
use crate::platform::{
    API_MAJOR, Control, MAX_LIST_LIMIT, ProviderDeployment, ProviderDeploymentCancel,
    ProviderOperationSummary,
};
use crate::render::{Output, Table, age, short_digest, timestamp};
use crate::settings::Overrides;

/// Open a session for one application environment.
///
/// Every verb here is scoped to exactly one environment - a provider
/// deployment belongs to one - so the same three lines are not written four
/// times.
fn session(app: Option<String>, environment: Option<String>) -> Result<(Session, String, String)> {
    let session = Session::open(&Overrides {
        app,
        environment,
        // Not a storage command: there is no namespace inside a tenant to
        // choose here, only the tenant itself.
        binding: None,
    })?;
    let app = session.app()?.to_string();
    let environment = session.environment()?.to_string();
    Ok((session, app, environment))
}

/// Read an operation id an operator typed.
///
/// Parsed rather than passed through, and parsed *here* rather than in the
/// client: the id becomes a path segment, and one that has already been read
/// as a canonical identity cannot carry a slash or a query string into a URL.
/// The message says where to find a real one, because "not a UUIDv7" on its
/// own tells somebody who copied the wrong column nothing.
fn operation_id(value: &str) -> Result<CanonicalId> {
    value.parse::<CanonicalId>().with_context(|| {
        format!(
            "{} is not a provider deployment operation id; `deis provider-deployments list` \
             prints them in the OPERATION column",
            escaped(value)
        )
    })
}

pub async fn list(
    app: Option<String>,
    environment: Option<String>,
    limit: usize,
    output: Output,
) -> Result<()> {
    let (session, app, environment) = session(app, environment)?;
    let history = session
        .control
        .list_provider_deployments(&app, &environment, limit)
        .await?;

    if output.json {
        // The workflows exactly as the control plane serves them: a script
        // correlating one with an audit line or a provider's own console
        // needs the identities, not the rendering.
        return output.emit(&serde_json::json!({
            "app": app,
            "environment": environment,
            "control_endpoint": session.control.base_url(),
            "total": history.total,
            "shown": history.deployments.len(),
            "providerDeployments": history.deployments,
        }));
    }

    println!(
        "\n{} {} {} {}",
        "Provider deployments of".bold(),
        app.cyan(),
        "in".bold(),
        environment.cyan()
    );

    if history.deployments.is_empty() {
        println!(
            "\n  {}",
            format!(
                "no provider driver has been asked to do anything in {environment} yet, so there \
                 is nothing to cancel or roll back."
            )
            .yellow()
        );
        return Ok(());
    }

    let mut table = Table::new(&[
        "WHEN",
        "AGE",
        "OPERATION",
        "KIND",
        "STATE",
        "CANCEL",
        "ROLLBACK OF",
    ]);
    for workflow in &history.deployments {
        table.row(row(workflow));
    }
    println!();
    table.print();

    if let Some(advice) = deeper_history(history.deployments.len(), history.total) {
        println!("\n  {}", advice.dimmed());
    }
    println!(
        "  {}",
        "`deis provider-deployments get <operation-id>` reads one whole, steps included.".dimmed()
    );
    Ok(())
}

/// One row of the list, as its own function so what an operator reads can be
/// asserted without a terminal.
///
/// `kind` and `state` come off the wire and go through the same escaping every
/// other operator-facing cell in this CLI does: they are short controlled
/// vocabularies today, and a control plane is free to grow one tomorrow.
fn row(workflow: &ProviderDeployment) -> Vec<String> {
    vec![
        timestamp(workflow.operation.created_at.timestamp()),
        age(workflow.operation.created_at.timestamp()),
        workflow.operation.operation_id.to_string(),
        escaped(&workflow.kind),
        escaped(&workflow.operation.state),
        // Not a yes/no: the time a cancel was asked for is what tells an
        // operator whether a workflow that is still `running` has been asked
        // to stop and is taking its time, or was never asked at all.
        workflow
            .cancel_requested_at
            .map(|at| age(at.timestamp()))
            .unwrap_or_else(|| "-".to_string()),
        workflow
            .rolled_back_from_operation_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "-".to_string()),
    ]
}

pub async fn get(
    app: Option<String>,
    environment: Option<String>,
    operation: &str,
    output: Output,
) -> Result<()> {
    let (session, app, environment) = session(app, environment)?;
    let id = operation_id(operation)?;
    let read = session
        .control
        .get_provider_deployment(&app, &environment, id)
        .await?;

    if output.json {
        return output.emit(&serde_json::json!({
            "app": app,
            "environment": environment,
            "control_endpoint": session.control.base_url(),
            // The revision this read observed, spelled as the control plane
            // spelled it: a script that wants to cancel this workflow itself
            // sends exactly this back as If-Match.
            "etag": read.etag,
            "providerDeployment": read.workflow,
        }));
    }

    println!(
        "\n{} {}",
        "Provider deployment".bold(),
        read.workflow.operation.operation_id.to_string().cyan()
    );
    println!("  in {} of {}\n", environment.cyan(), app.cyan());
    describe(&read.workflow);
    Ok(())
}

pub async fn cancel(
    app: Option<String>,
    environment: Option<String>,
    operation: &str,
    output: Output,
) -> Result<()> {
    let (session, app, environment) = session(app, environment)?;
    let id = operation_id(operation)?;
    // A refusal here - 409 for a workflow that has already finished, 412 for
    // one that moved between the read and the cancel - arrives as the control
    // plane's own structured error, code and request id included, and is
    // printed by the client rather than reworded. Rewording it would lose the
    // one string an operator can search the control plane's log for.
    let cancelled: ProviderDeploymentCancel = session
        .control
        .cancel_provider_deployment(&app, &environment, id)
        .await?;

    if output.json {
        return output.emit(&serde_json::json!({
            "app": app,
            "environment": environment,
            "control_endpoint": session.control.base_url(),
            "observedState": cancelled.observed.operation.state,
            "observedRevision": cancelled.observed.operation.revision,
            "operation": cancelled.operation,
        }));
    }

    println!(
        "\n{} {}",
        "Cancel requested for provider deployment".bold(),
        cancelled.operation.operation_id.to_string().cyan()
    );
    println!(
        "  was  {} (revision {})",
        escaped(&cancelled.observed.operation.state),
        cancelled.observed.operation.revision
    );
    println!(
        "  now  {} (revision {})",
        escaped(&cancelled.operation.state).yellow(),
        cancelled.operation.revision
    );
    println!(
        "\n  {}",
        "A cancel stops what has not run yet and undoes what is reversible; steps that already \
         took effect stay in effect - undoing those is `deis provider-deployments rollback`."
            .dimmed()
    );
    println!(
        "  {}",
        format!(
            "`deis provider-deployments get {}` says how far it has got.",
            cancelled.operation.operation_id
        )
        .dimmed()
    );
    Ok(())
}

pub async fn rollback(
    app: Option<String>,
    environment: Option<String>,
    operation: &str,
    wait: bool,
    output: Output,
) -> Result<()> {
    let (session, app, environment) = session(app, environment)?;
    let id = operation_id(operation)?;
    let accepted = session
        .control
        .rollback_provider_deployment(&app, &environment, id)
        .await?;

    // Waiting is the operator's choice, so the unwaited answer is printed
    // first when a person is reading: a rollback that takes ten minutes
    // should not leave a blank terminal that never named the operation it
    // started. `--json` prints one document at the end and nothing before it.
    if output.human() {
        println!(
            "\n{} {}",
            "Rollback of provider deployment".bold(),
            id.to_string().cyan()
        );
        println!(
            "  new operation  {}",
            accepted.operation_id.to_string().cyan()
        );
        println!("  state          {}", escaped(&accepted.state));
    }

    let settled = if wait {
        Some(
            wait_for(
                &session.control,
                &app,
                &environment,
                accepted.operation_id,
                output,
            )
            .await?,
        )
    } else {
        None
    };

    if output.json {
        return output.emit(&serde_json::json!({
            "app": app,
            "environment": environment,
            "control_endpoint": session.control.base_url(),
            "rolledBackFrom": id.to_string(),
            "operation": accepted,
            "waited": wait,
            "providerDeployment": settled,
        }));
    }

    match &settled {
        Some(workflow) => {
            println!("\n{}", "It finished:".bold());
            println!();
            describe(workflow);
        }
        None => println!(
            "\n  {}",
            format!(
                "`deis provider-deployments get {}` follows it; --wait does the following here.",
                accepted.operation_id
            )
            .dimmed()
        ),
    }
    Ok(())
}

/// Follow one workflow to a terminal state, saying so first when a person is
/// watching an otherwise silent terminal.
async fn wait_for(
    control: &Control,
    app: &str,
    environment: &str,
    id: CanonicalId,
    output: Output,
) -> Result<ProviderDeployment> {
    output.line(format!("\n  waiting for {id} to finish..."));
    control
        .wait_for_provider_deployment(app, environment, id)
        .await
}

/// One workflow printed whole: the operation, what it is bound to, and every
/// step with the child operation that ran it.
fn describe(workflow: &ProviderDeployment) {
    let operation = &workflow.operation;
    println!("  kind             {}", escaped(&workflow.kind));
    println!(
        "  state            {}{}",
        escaped(&operation.state),
        match operation.phase.is_empty() {
            true => String::new(),
            false => format!(" (phase {})", escaped(&operation.phase)),
        }
    );
    println!("  revision         {}", operation.revision);
    println!("  progress         {}", progress(operation));
    println!(
        "  created          {}  ({})",
        timestamp(operation.created_at.timestamp()),
        age(operation.created_at.timestamp())
    );
    if let Some(finished) = operation.finished_at {
        println!(
            "  finished         {}  ({})",
            timestamp(finished.timestamp()),
            age(finished.timestamp())
        );
    }
    match workflow.cancel_requested_at {
        Some(at) => println!(
            "  cancel asked     {}  ({})",
            timestamp(at.timestamp()),
            age(at.timestamp())
        ),
        None => println!(
            "  cancellable      {}",
            if operation.cancellable { "yes" } else { "no" }
        ),
    }
    println!(
        "  provider target  {} (revision {})",
        workflow.provider_target_id, workflow.target_revision
    );
    if let Some(grant) = workflow.provider_target_grant_id {
        println!(
            "  target grant     {}{}",
            grant,
            workflow
                .target_grant_revision
                .map(|revision| format!(" (revision {revision})"))
                .unwrap_or_default()
        );
    }
    if let Some(digest) = &workflow.desired_state_digest {
        println!("  desired state    {}", short_digest(&escaped(digest)));
    }
    if let Some(source) = workflow.rolled_back_from_operation_id {
        println!("  rollback of      {source}");
    }
    if !workflow.rollback_operation_ids.is_empty() {
        println!(
            "  rolled back by   {}",
            workflow
                .rollback_operation_ids
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if let Some(error) = &operation.error {
        println!(
            "  {} {}: {}",
            "error           ".red(),
            escaped(&error.code),
            escaped(&error.message)
        );
        if let Some(remediation) = &error.remediation {
            println!("                   {}", escaped(remediation));
        }
    }
    // A dispatch failure is not the operation's own error: it is the control
    // plane failing to hand the step to a driver, which is a different thing
    // to chase and is why it is printed on its own line rather than folded in.
    if workflow.dispatch_failure_count > 0 {
        println!(
            "  dispatch retries {} (last: {} {})",
            workflow.dispatch_failure_count,
            escaped_or_dash(workflow.last_dispatch_error_code.as_deref()),
            escaped_or_dash(workflow.last_dispatch_error_message.as_deref())
        );
    }

    if workflow.steps.is_empty() {
        return;
    }
    println!("\n  {}", "Steps".bold());
    let mut table = Table::new(&["#", "STEP", "CHILD OPERATION", ""]);
    for step in &workflow.steps {
        table.row(vec![
            step.sequence.to_string(),
            escaped(&step.operation),
            step.child_operation_id.to_string(),
            // Which step the workflow is on now. `next_step_index` counts
            // the steps already settled, so the step at that index is the
            // one currently in flight.
            match step.sequence == workflow.next_step_index {
                true => "<- current".to_string(),
                false => String::new(),
            },
        ]);
    }
    table.print();
}

/// Progress as a person reads it, or why there is no number.
///
/// `None` from the control plane means it has no honest denominator to divide
/// by, which is a different statement from "0%", and printing the latter for
/// the former is how a stuck workflow gets mistaken for one that has not
/// started.
fn progress(operation: &ProviderOperationSummary) -> String {
    match operation.progress {
        Some(basis_points) => format!("{:.1}%", f64::from(basis_points) / 100.0),
        None => "not measurable".to_string(),
    }
}

/// What to say under the table about the rows that were not printed, or
/// `None` when there are none.
///
/// The same three situations `deis deployments` distinguishes, and for the
/// same reason: a bigger `--limit` reaches the rest, or it reaches only the
/// newest page-sized slice, or no `--limit` reaches further at all and the
/// honest pointer is the API's own page cursor.
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
             {API_MAJOR}/apps/<app>/environments/<environment>/provider-deployments?limit={MAX_LIST_LIMIT}&cursor=<nextCursor>"
        )
    };
    Some(format!("{shown} of {total} shown; {advice}."))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEPLOYMENT_ID: &str = "018f2518-7e38-7f80-8000-0000000000a1";
    const ROLLBACK_ID: &str = "018f2518-7e38-7f80-8000-0000000000a2";

    fn workflow(kind: &str, state: &str) -> ProviderDeployment {
        serde_json::from_value(serde_json::json!({
            "operation": {
                "operationId": DEPLOYMENT_ID,
                "kind": kind,
                "state": state,
                "phase": "apply",
                "revision": 7,
                "progress": 4200,
                "cancellable": true,
                "createdAt": "2026-09-06T10:00:00Z",
                "updatedAt": "2026-09-06T10:04:00Z"
            },
            "kind": kind,
            "providerTargetId": "018f2518-7e38-7f80-8000-0000000000b1",
            "targetRevision": 3
        }))
        .expect("the fixture is the shape the control plane serves")
    }

    #[test]
    fn a_row_names_the_operation_its_kind_and_its_state() {
        let row = row(&workflow("deploy", "running"));
        assert_eq!(row.len(), 7);
        assert_eq!(row[2], DEPLOYMENT_ID);
        assert_eq!(row[3], "deploy");
        assert_eq!(row[4], "running");
        // Never asked to stop, and not a rollback of anything: two dashes
        // rather than two empty cells, so a reader can tell "nothing here"
        // from a column that failed to render.
        assert_eq!(row[5], "-");
        assert_eq!(row[6], "-");
    }

    /// The two columns that only a rollback fills, and the one that only a
    /// cancelled workflow fills. A reader scanning the list for "which of
    /// these did somebody already try to stop" is reading exactly these.
    #[test]
    fn a_cancelled_rollback_names_what_it_undoes_and_when_it_was_stopped() {
        let mut workflow = workflow("rollback", "cancelling");
        workflow.rolled_back_from_operation_id = Some(ROLLBACK_ID.parse().unwrap());
        workflow.cancel_requested_at = Some(chrono::Utc::now() - chrono::Duration::minutes(3));

        let row = row(&workflow);
        assert_eq!(row[3], "rollback");
        assert_eq!(row[4], "cancelling");
        assert_eq!(row[5], "3m ago");
        assert_eq!(row[6], ROLLBACK_ID);
    }

    /// Everything in a row came off the wire, so a control plane that serves
    /// a state carrying an escape sequence must not get to repaint the
    /// operator's terminal through this table.
    #[test]
    fn a_state_carrying_terminal_control_bytes_is_escaped_before_it_is_printed() {
        let row = row(&workflow("deploy", "running\u{1b}[2Jrunning"));
        assert!(!row[4].contains('\u{1b}'), "{:?}", row[4]);
        assert!(row[4].contains("\\u{1b}"), "{:?}", row[4]);
    }

    /// "no honest denominator" and "nothing has happened yet" are different
    /// facts about a workflow, and printing 0.0% for the first is how a
    /// running deployment gets read as a stuck one.
    #[test]
    fn missing_progress_is_said_rather_than_shown_as_zero() {
        let mut workflow = workflow("deploy", "running");
        assert_eq!(progress(&workflow.operation), "42.0%");
        workflow.operation.progress = None;
        assert_eq!(progress(&workflow.operation), "not measurable");
    }

    /// The id becomes a URL path segment, so anything that is not a canonical
    /// identity is refused here rather than sent - and the refusal says where
    /// to find a real one.
    #[test]
    fn an_operation_id_that_is_not_a_canonical_identity_is_refused() {
        assert_eq!(
            operation_id(DEPLOYMENT_ID).unwrap().to_string(),
            DEPLOYMENT_ID
        );
        for bad in [
            "../../apps/other",
            "018f2518-7e38-7f80-8000-0000000000a1/cancel",
            "550e8400-e29b-41d4-a716-446655440000",
            "",
        ] {
            let error = operation_id(bad).unwrap_err().to_string();
            assert!(error.contains("provider-deployments list"), "{error}");
        }
    }

    #[test]
    fn a_history_that_fits_says_nothing_further() {
        assert_eq!(deeper_history(20, 20), None);
        assert_eq!(deeper_history(20, 3), None);
    }

    #[test]
    fn a_history_within_one_page_names_the_limit_that_reaches_it() {
        assert_eq!(
            deeper_history(20, 137).expect("137 rows do not fit in 20"),
            "20 of 137 shown; pass --limit 137 for the rest."
        );
    }

    /// At the page ceiling no `--limit` reaches further, so the line must
    /// point at the API's own cursor instead of naming a flag value that
    /// would change nothing.
    #[test]
    fn a_full_page_of_a_longer_history_points_at_the_api_cursor() {
        let advice =
            deeper_history(MAX_LIST_LIMIT, 4_000).expect("4000 rows do not fit in one page");
        assert!(advice.starts_with("1000 of 4000 shown;"), "{advice}");
        assert!(!advice.contains("pass --limit"), "{advice}");
        assert!(advice.contains("cursor=<nextCursor>"), "{advice}");
        assert!(
            advice.contains("/api/v1/apps/<app>/environments/<environment>/provider-deployments"),
            "{advice}"
        );
    }
}
