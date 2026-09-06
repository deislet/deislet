//! `deis routes list` - which requests one environment answers.
//!
//! The order is the whole point of the table, so the rows are printed exactly
//! as the control plane serves them: highest priority first, then by path
//! prefix. That is the order the edge tries them in, which means the first row
//! that matches a request is the row that wins, and a reader can answer "why
//! did this URL go there" by reading downwards. Sorting these client-side
//! would produce a table that is easier to scan and answers a different
//! question than the one it looks like it answers.
//!
//! Read-only. A route decides which tenant receives somebody's traffic - and,
//! through cookies, sometimes their session - so creating and withdrawing one
//! is not something to make easy to do by accident from a list command; the
//! hostname half of that decision is not even the application's to make (see
//! [`crate::commands::domains`]).

use anyhow::Result;
use colored::Colorize;

use crate::commands::{Session, escaped};
use crate::render::{Output, Table};
use crate::settings::Overrides;

pub async fn list(app: Option<String>, environment: Option<String>, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app,
        environment,
        // Not a storage command: there is no namespace inside a tenant to
        // choose, only the tenant itself.
        binding: None,
    })?;
    let app = session.app()?.to_string();
    let environment = session.environment()?.to_string();

    let routes = session.control.list_routes(&app, &environment).await?;

    if output.json {
        return output.emit(&serde_json::json!({
            "app": app,
            "environment": environment,
            "control_endpoint": session.control.base_url(),
            "total": routes.len(),
            "routes": routes,
        }));
    }

    println!(
        "\n{} {} {} {}",
        "Routes of".bold(),
        app.cyan(),
        "in".bold(),
        environment.cyan()
    );

    if routes.is_empty() {
        println!(
            "\n  {}",
            format!(
                "no routes, so nothing at the edge sends any request to {environment}. A \
                 deployment still runs; it is simply not reachable by hostname."
            )
            .yellow()
        );
        return Ok(());
    }

    let mut table = Table::new(&["PRIORITY", "HOST", "PATH", "ROUTE"]);
    for route in &routes {
        table.row(vec![
            route.priority.to_string(),
            // A host and a path prefix are strings somebody posted into the
            // routing table, stored by the control plane and read back onto
            // every operator's terminal. The route id is the control plane's
            // own, and goes through the same escaping for the same reason
            // every other cell in this CLI does: a renderer that trusts a
            // value because some other component was supposed to have checked
            // it is the shape that produces the next finding.
            host_cell(route.host.as_deref()),
            escaped(&route.path_prefix),
            escaped(&route.id),
        ]);
    }
    println!();
    table.print();
    println!(
        "\n  {}",
        "Highest priority first, then by path: the edge tries them in this order and the first \
         match wins. `(no-host)` answers for every name no more specific route claims, which is \
         the widest claim on this platform - `deis domains list` shows who is allowed to make it."
            .dimmed()
    );
    Ok(())
}

/// The HOST cell of one route.
///
/// A route with no host is not a route with a missing field, so it is not a
/// dash: it answers for every name no more specific route claims, which makes
/// it the *widest* claim on the platform rather than the mildest. It is
/// printed with the same spelling the grant registry uses for the pattern that
/// covers it, `(no-host)`, so the two tables can be read against each other.
fn host_cell(host: Option<&str>) -> String {
    match host {
        Some(host) => escaped(host),
        None => "(no-host)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{escape_control, escaped_or_dash};

    #[test]
    fn a_route_with_no_host_reads_as_the_widest_claim_not_as_a_missing_value() {
        assert_eq!(host_cell(None), "(no-host)");
        // The dash spelling is deliberately not what this cell uses, so a
        // reader never mistakes the widest claim on the platform for a field
        // the control plane forgot to fill in.
        assert_ne!(host_cell(None), escaped_or_dash(None));
    }

    #[test]
    fn a_hostile_hostname_cannot_repaint_the_table() {
        // A host is posted into the routing table by whoever holds a
        // deploy scope on some environment, and read back on every
        // operator's terminal - including operators of other applications.
        let forged = "shop.example.com\n0         evil.example.com  /  route-x";
        let rendered = host_cell(Some(forged));
        assert_eq!(rendered.lines().count(), 1, "{rendered:?}");
        assert!(rendered.contains("shop.example.com\\n0"), "{rendered:?}");
        assert!(!host_cell(Some("\u{1b}[2Jgone")).contains('\u{1b}'));
        // Same rule, same function, wherever the string came from.
        assert_eq!(escape_control(forged), rendered);
    }
}
