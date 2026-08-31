//! `deis versions` - the immutable history of one application.
//!
//! A version number is what a person says out loud ("we are on v7"), so it
//! leads every row; the digest is next to it because two versions built from
//! the same source share it and that is worth seeing. The environments
//! column is the part that cannot be read off the version list alone: it is
//! assembled from each environment's current pointer, which is the only
//! thing that says what is actually running.
//!
//! COMPAT is the day's platform behaviour each version was written against.
//! It is here because a person debugging "why does this behave differently in
//! production" has to be able to see it without opening a bundle, and because
//! `none (oldest)` in this column is the answer to "which of my applications
//! will never get a behaviour fix".

use anyhow::Result;
use colored::Colorize;

use crate::commands::Session;
use crate::render::{Output, Table, age, compatibility_cell, short_digest, timestamp};
use crate::settings::Overrides;

pub async fn execute(app: Option<String>, limit: usize, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app,
        ..Overrides::default()
    })?;
    let app = session.app()?.to_string();

    let mut versions = session.control.list_versions(&app).await?;
    // Newest first, whatever order the control plane answered in: the row a
    // person is looking for is almost always the most recent one.
    versions.sort_by_key(|version| std::cmp::Reverse(version.number));
    let total = versions.len();

    let running = running_versions(&session, &app).await?;

    let shown: Vec<_> = versions.into_iter().take(limit.max(1)).collect();

    if output.json {
        let rows: Vec<serde_json::Value> = shown
            .iter()
            .map(|version| {
                serde_json::json!({
                    "number": version.number,
                    "id": version.id,
                    "sha256": version.bundle_sha,
                    "created_at": version.created_at,
                    "compatibility": version.compatibility,
                    "environments": environments_of(&running, &version.id),
                })
            })
            .collect();
        return output.emit(&serde_json::json!({
            "app": app,
            "control_endpoint": session.control.base_url(),
            "total": total,
            "shown": rows.len(),
            "versions": rows,
        }));
    }

    println!(
        "\n{} {} {} {}",
        "Versions of".bold(),
        app.cyan(),
        "on".bold(),
        session.control.base_url().dimmed()
    );

    if shown.is_empty() {
        println!(
            "\n  {}",
            format!(
                "no versions yet. `deis deploy` builds one and uploads it; until then {app} has \
                 nothing to run."
            )
            .yellow()
        );
        return Ok(());
    }

    // COMPAT is here rather than in `deis status` because it is a property of
    // a version, not of an environment: two environments of one application
    // can be on versions written against different days, and the question
    // "why does this behave differently in production" is answered by
    // comparing this column between two rows.
    let mut table = Table::new(&[
        "VERSION",
        "DIGEST",
        "CREATED",
        "AGE",
        "COMPAT",
        "ENVIRONMENTS",
    ]);
    for version in &shown {
        let environments = environments_of(&running, &version.id);
        table.row(vec![
            version.number.to_string(),
            short_digest(&version.bundle_sha),
            timestamp(version.created_at),
            age(version.created_at),
            compatibility_cell(version.compatibility.as_ref()),
            if environments.is_empty() {
                "-".to_string()
            } else {
                environments.join(", ")
            },
        ]);
    }
    println!();
    table.print();

    if total > shown.len() {
        println!(
            "\n  {}",
            format!(
                "{} of {total} shown; pass --limit {total} for the rest.",
                shown.len()
            )
            .dimmed()
        );
    }
    Ok(())
}

/// Which version each environment of this application currently points at.
///
/// An environment that has never been deployed simply contributes nothing;
/// that is a state, not a failure, and it shows up as an empty column.
async fn running_versions(session: &Session, app: &str) -> Result<Vec<(String, String)>> {
    let environments = session.control.list_environments(app).await?;
    let mut running = Vec::new();
    for environment in environments {
        if let Some(deployment) = session
            .control
            .current_deployment(app, &environment.name)
            .await?
        {
            running.push((environment.name, deployment.version.id));
        }
    }
    Ok(running)
}

fn environments_of(running: &[(String, String)], version_id: &str) -> Vec<String> {
    running
        .iter()
        .filter(|(_, id)| id == version_id)
        .map(|(name, _)| name.clone())
        .collect()
}
