//! `deis rollback` - point an environment back at an earlier version.
//!
//! A rollback is a pointer move, so it is fast and it is itself reversible:
//! the control plane inserts a new deployment row aimed at an older version
//! and deletes nothing. What this command adds on top is the sentence a
//! person needs afterwards - which version it moved *from* - because the
//! control plane's answer only describes where the environment ended up.

use anyhow::{Result, bail};
use colored::Colorize;

use crate::commands::Session;
use crate::commands::deploy::{fail_on_incomplete_delivery, print_delivery};
use crate::render::{Output, short_digest};
use crate::settings::Overrides;

pub struct Args {
    pub app: Option<String>,
    pub environment: Option<String>,
    /// Version number to return to. `None` means the previous distinct
    /// version this environment ran.
    pub to: Option<i64>,
    pub note: Option<String>,
}

pub async fn execute(args: Args, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app: args.app,
        environment: args.environment,
    })?;
    let app = session.app()?.to_string();
    let environment = session.environment()?.to_string();

    // Read where the environment is before moving it. Doing this first is
    // the whole point: after the rollback the previous pointer is only
    // findable in the history, and "rolled back from what?" is the first
    // question anyone asks.
    let before = match session
        .control
        .current_deployment(&app, &environment)
        .await?
    {
        crate::platform::EnvironmentView::Running(deployment) => *deployment,
        crate::platform::EnvironmentView::NeverDeployed => bail!(
            "environment {environment} of app {app} has never been deployed, so there is nothing \
             to roll back to. Deploy a version first with `deis deploy -e {environment}`."
        ),
        // The refusal is worth its own sentence: the environment may well be
        // deployed, and what is missing is authority rather than a version.
        crate::platform::EnvironmentView::NotPermitted => bail!(
            "this credential may not look at {environment} of app {app}, so it cannot roll it \
             back either. It would need a scope covering deploy:{app}/{environment}; ask an owner \
             of the platform for one."
        ),
    };

    if let Some(number) = args.to
        && number == before.version.number
    {
        bail!(
            "{environment} of app {app} already runs version {number}, so this rollback would \
             move nothing. Run `deis versions -a {app}` to see which numbers exist."
        );
    }

    output.line(format!(
        "\n{} {} {} {}",
        "Rolling back".bold(),
        app.cyan(),
        "in".bold(),
        environment.cyan()
    ));

    let after = session
        .control
        .rollback(&app, &environment, args.to, args.note.as_deref())
        .await?;

    if output.json {
        output.emit(&serde_json::json!({
            "app": app,
            "environment": environment,
            "from": {
                "number": before.version.number,
                "id": before.version.id,
                "sha256": before.version.bundle_sha,
            },
            "to": {
                "number": after.version.number,
                "id": after.version.id,
                "sha256": after.version.bundle_sha,
            },
            "deployment": after.deployment,
            "delivery": after.delivery,
        }))?;
    } else {
        println!(
            "\n  {} {} moved from version {} to version {} (sha256 {})",
            "✓".green(),
            environment.cyan(),
            before.version.number.to_string().bold(),
            after.version.number.to_string().bold(),
            short_digest(&after.version.bundle_sha)
        );
        if after.version.number > before.version.number {
            // The control plane accepts any target, so `--to` can move an
            // environment forward. Saying so keeps the word "rollback" from
            // describing something that was not one.
            println!(
                "  {} version {} is newer than {}, so this moved the environment forward",
                "•".yellow(),
                after.version.number,
                before.version.number
            );
        }
        match &after.delivery {
            Some(delivery) => print_delivery(delivery),
            None => println!(
                "  {}",
                "the control plane did not say where this was delivered".yellow()
            ),
        }
    }

    fail_on_incomplete_delivery(after.delivery.as_ref(), &environment)
}
