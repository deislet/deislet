//! `deis status` - what each environment of an application runs, and which
//! nodes are serving it.
//!
//! Two different sources, deliberately printed apart. The environment table
//! comes from the catalog and is the platform's intent: this environment
//! points at this version. The node table comes from asking each node what
//! it currently has loaded, and it is the only place a disagreement between
//! intent and reality shows up - a node that never took the last deployment
//! is still listed here, still serving, with its own counters.

use anyhow::Result;
use colored::Colorize;

use crate::commands::Session;
use crate::platform::{Node, NodeAppStatus};
use crate::render::{Output, Table, age, short_digest, timestamp};
use crate::settings::Overrides;

/// One node's answer, or the fact that it did not give one.
struct NodeReport {
    node: Node,
    /// `None` when the node could not be asked. Its own view is then
    /// unknown, which is not the same as "runs nothing".
    apps: Option<Vec<NodeAppStatus>>,
}

pub async fn execute(app: Option<String>, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app,
        ..Overrides::default()
    })?;
    let app = session.app()?.to_string();

    if session.control.get_app(&app).await?.is_none() {
        anyhow::bail!(
            "the control plane at {} has no application called {app}. `deis deploy` creates it; \
             `deis config list` shows which control plane this is.",
            session.control.base_url()
        );
    }

    let environments = session.control.list_environments(&app).await?;
    let mut deployed = Vec::new();
    for environment in &environments {
        let current = session
            .control
            .current_deployment(&app, &environment.name)
            .await?;
        deployed.push((environment.name.clone(), current));
    }

    let mut reports = Vec::new();
    for node in session.control.list_nodes().await? {
        let apps = session.control.node_status(&node.id).await?;
        reports.push(NodeReport { node, apps });
    }

    if output.json {
        return output.emit(&serde_json::json!({
            "app": app,
            "control_endpoint": session.control.base_url(),
            "environments": deployed
                .iter()
                .map(|(name, current)| match current {
                    Some(deployment) => serde_json::json!({
                        "name": name,
                        "deployed": true,
                        "version_number": deployment.version.number,
                        "version_id": deployment.version.id,
                        "sha256": deployment.version.bundle_sha,
                        "deployed_at": deployment.deployment.created_at,
                        "note": deployment.deployment.note,
                    }),
                    None => serde_json::json!({ "name": name, "deployed": false }),
                })
                .collect::<Vec<_>>(),
            "nodes": reports
                .iter()
                .map(|report| serde_json::json!({
                    "id": report.node.id,
                    "addr": report.node.addr,
                    "healthy": report.node.healthy,
                    "version": report.node.version,
                    "last_error": report.node.last_error,
                    "reachable": report.apps.is_some(),
                    "serving": report
                        .apps
                        .as_ref()
                        .map(|apps| apps
                            .iter()
                            .filter(|status| status.app_id == app)
                            .collect::<Vec<_>>()),
                }))
                .collect::<Vec<_>>(),
        }));
    }

    println!(
        "\n{} {} {} {}",
        "Status of".bold(),
        app.cyan(),
        "on".bold(),
        session.control.base_url().dimmed()
    );

    print_environments(&deployed);
    print_nodes(&app, &reports);
    Ok(())
}

fn print_environments(deployed: &[(String, Option<crate::platform::DeploymentResponse>)]) {
    println!("\n{}", "Environments".bold());
    if deployed.is_empty() {
        println!(
            "  {}",
            "none yet; `deis deploy` creates one on first use".dimmed()
        );
        return;
    }

    let mut table = Table::new(&[
        "ENVIRONMENT",
        "VERSION",
        "DIGEST",
        "DEPLOYED",
        "AGE",
        "NOTE",
    ]);
    for (name, current) in deployed {
        match current {
            Some(deployment) => table.row(vec![
                name.clone(),
                deployment.version.number.to_string(),
                short_digest(&deployment.version.bundle_sha),
                timestamp(deployment.deployment.created_at),
                age(deployment.deployment.created_at),
                deployment
                    .deployment
                    .note
                    .clone()
                    .unwrap_or_else(|| "-".to_string()),
            ]),
            None => table.row(vec![
                name.clone(),
                "-".to_string(),
                "-".to_string(),
                "never deployed".to_string(),
                "-".to_string(),
                "-".to_string(),
            ]),
        }
    }
    table.print();
}

fn print_nodes(app: &str, reports: &[NodeReport]) {
    println!("\n{}", "Nodes".bold());
    if reports.is_empty() {
        println!(
            "  {}",
            "the control plane knows no runtime nodes, so nothing is serving this application"
                .yellow()
        );
        return;
    }

    let mut table = Table::new(&[
        "NODE",
        "ADDRESS",
        "HEALTH",
        "ENVIRONMENT",
        "STATE",
        "REQUESTS",
        "ERRORS",
    ]);
    for report in reports {
        let health = match (report.node.healthy, report.apps.is_some()) {
            (true, _) => "healthy".to_string(),
            // The control plane's cached health can lag a node that has just
            // come back, so a node that answered us is reported as answering
            // rather than as the stale value.
            (false, true) => "answering".to_string(),
            (false, false) => "unreachable".to_string(),
        };

        let Some(apps) = &report.apps else {
            table.row(vec![
                report.node.id.clone(),
                report.node.addr.clone(),
                health,
                "unknown".to_string(),
                report
                    .node
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "could not be asked".to_string()),
                "-".to_string(),
                "-".to_string(),
            ]);
            continue;
        };

        let mine: Vec<&NodeAppStatus> = apps.iter().filter(|s| s.app_id == app).collect();
        if mine.is_empty() {
            table.row(vec![
                report.node.id.clone(),
                report.node.addr.clone(),
                health,
                "-".to_string(),
                format!("not serving {app}"),
                "-".to_string(),
                "-".to_string(),
            ]);
            continue;
        }

        for status in mine {
            table.row(vec![
                report.node.id.clone(),
                report.node.addr.clone(),
                health.clone(),
                status.environment.clone(),
                if status.error_message.is_empty() {
                    status.state.clone()
                } else {
                    format!("{}: {}", status.state, status.error_message)
                },
                status.request_count.to_string(),
                status.error_count.to_string(),
            ]);
        }
    }
    table.print();

    println!(
        "\n  {}",
        "A node reports which environments it runs, not which version of each: compare the two \
         tables by hand until the node status carries a version id."
            .dimmed()
    );
}
