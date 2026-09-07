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

use crate::commands::{Session, escape_control, escaped};
use crate::platform::{EnvironmentView, FleetView};
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
        // A credential scoped to one environment is refused the others, and
        // that is a row in the report rather than the end of the command.
        let current = session
            .control
            .current_deployment(&app, &environment.name)
            .await?;
        deployed.push((environment.name.clone(), current));
    }

    // The fleet is a platform-operator view; an application credential is
    // refused it. That is a section of the report that says so, not the end
    // of a command whose subject is the application.
    let fleet = session.control.list_nodes().await?;
    let fleet_visible = matches!(fleet, FleetView::Nodes(_));
    let mut reports = Vec::new();
    if let FleetView::Nodes(nodes) = fleet {
        for node in nodes {
            let apps = session.control.node_status(&node.id).await?;
            reports.push(NodeReport { node, apps });
        }
    }

    if output.json {
        return output.emit(&serde_json::json!({
            "app": app,
            "control_endpoint": session.control.base_url(),
            "environments": deployed
                .iter()
                .map(|(name, current)| match current {
                    EnvironmentView::Running(deployment) => serde_json::json!({
                        "name": name,
                        "visible": true,
                        "deployed": true,
                        "version_number": deployment.version.number,
                        "version_id": deployment.version.id,
                        "sha256": deployment.version.bundle_sha,
                        "deployed_at": deployment.deployment.created_at,
                        "note": deployment.deployment.note,
                    }),
                    EnvironmentView::NeverDeployed => serde_json::json!({
                        "name": name, "visible": true, "deployed": false
                    }),
                    // Said as its own field rather than folded into
                    // "deployed": false, because "nothing runs there" and "I
                    // am not allowed to know" are different answers and a
                    // script acting on the first must not act on the second.
                    EnvironmentView::NotPermitted => serde_json::json!({
                        "name": name, "visible": false
                    }),
                })
                .collect::<Vec<_>>(),
            // Said as its own field for the same reason the environment
            // rows are: "no nodes" and "I may not look" are different
            // answers and a script must not act on the first when it got
            // the second.
            "fleet_visible": fleet_visible,
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
    print_nodes(&app, &reports, fleet_visible);
    Ok(())
}

fn print_environments(deployed: &[(String, EnvironmentView)]) {
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
    // An environment name is checked on the way *in* by
    // `deis_wire::conventions`, but this list came back from the control
    // plane rather than from this process's argv, and a renderer that trusts
    // a name because some other component was supposed to have checked it is
    // the shape that put the other cells of this table on this list.
    for (name, current) in deployed {
        match current {
            EnvironmentView::Running(deployment) => table.row(vec![
                escaped(name),
                deployment.version.number.to_string(),
                escaped(&short_digest(&deployment.version.bundle_sha)),
                timestamp(deployment.deployment.created_at),
                age(deployment.deployment.created_at),
                // A deploy note is free text somebody typed at `deis deploy
                // -m`, stored by the control plane and read back here on
                // every operator's terminal.
                deployment
                    .deployment
                    .note
                    .as_deref()
                    .map(escaped)
                    .unwrap_or_else(|| "-".to_string()),
            ]),
            EnvironmentView::NeverDeployed => table.row(vec![
                escaped(name),
                "-".to_string(),
                "-".to_string(),
                "never deployed".to_string(),
                "-".to_string(),
                "-".to_string(),
            ]),
            // Not "never deployed": this environment may well be running
            // something, and what is missing is the authority to look. The
            // row is kept so the reader can see the environment exists.
            EnvironmentView::NotPermitted => table.row(vec![
                escaped(name),
                "-".to_string(),
                "-".to_string(),
                "not visible to this credential".to_string(),
                "-".to_string(),
                "-".to_string(),
            ]),
        }
    }
    table.print();
}

fn print_nodes(app: &str, reports: &[NodeReport], fleet_visible: bool) {
    println!("\n{}", "Nodes".bold());
    if !fleet_visible {
        println!(
            "  {}",
            "the runtime fleet is a platform-operator view and this credential does not hold it; \
             what runs where is above"
                .yellow()
        );
        return;
    }
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
                // A node's id, address and last error are what that node told
                // the control plane about itself. A node is a peer, not a
                // trusted half of this CLI, and `last_error` in particular is
                // whatever string the failure carried.
                escaped(&report.node.id),
                escaped(&report.node.addr),
                health,
                "unknown".to_string(),
                report
                    .node
                    .last_error
                    .as_deref()
                    .map(escaped)
                    .unwrap_or_else(|| "could not be asked".to_string()),
                "-".to_string(),
                "-".to_string(),
            ]);
            continue;
        };

        let mine: Vec<&NodeAppStatus> = apps.iter().filter(|s| s.app_id == app).collect();
        if mine.is_empty() {
            table.row(vec![
                escaped(&report.node.id),
                escaped(&report.node.addr),
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
                escaped(&report.node.id),
                escaped(&report.node.addr),
                health.clone(),
                escaped(&status.environment),
                // `error_message` is the failure a node reports for one app -
                // frequently the application's own thrown error, relayed.
                if status.error_message.is_empty() {
                    escaped(&status.state)
                } else {
                    format!(
                        "{}: {}",
                        escape_control(&status.state),
                        escape_control(&status.error_message)
                    )
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
