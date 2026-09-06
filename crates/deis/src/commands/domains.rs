//! `deis domains list` - who may claim which hostname.
//!
//! A hostname is the one namespace on this platform that is global. An
//! application id belongs to whoever created the application and an
//! environment name belongs to its application, but `shop.example.com`
//! belongs to whoever registered the domain, which is a fact about the world
//! that a deploy scope cannot spell. So the names are written down, and the
//! control plane refuses every route claim the registry does not cover.
//!
//! This command is the read of that registry, and reading it needs `admin:*/*`
//! for the same reason writing it does: granting a name decides which tenant
//! receives somebody's cookies, so the list itself is a platform-level answer
//! rather than an application-level one. Withdrawing a grant is deliberately
//! not here - it silently breaks whatever is already serving on that name, and
//! that is not a keystroke away from a listing.

use anyhow::Result;
use colored::Colorize;

use crate::commands::{Session, escaped, escaped_or_dash};
use crate::platform::EVERY_ENVIRONMENT;
use crate::render::{Output, Table, timestamp};
use crate::settings::Overrides;

pub async fn list(output: Output) -> Result<()> {
    // Deliberately not resolved through the project file the way `deis
    // deploy` resolves an application. The registry is global, and a checkout
    // that happens to name an application in deis.toml must not silently look
    // like the whole platform's grant list.
    let session = Session::open(&Overrides::default())?;

    let grants = session.control.list_domain_grants().await?;

    if output.json {
        return output.emit(&serde_json::json!({
            "control_endpoint": session.control.base_url(),
            "total": grants.len(),
            "grants": grants,
        }));
    }

    println!(
        "\n{} {}",
        "Domain grants on".bold(),
        session.control.base_url().dimmed()
    );

    if grants.is_empty() {
        println!(
            "\n  {}",
            "the registry is empty, so no application may claim any hostname and every route \
             creation is refused. An administrator grants a name before an application can serve \
             it."
            .yellow()
        );
        return Ok(());
    }

    let mut table = Table::new(&["PATTERN", "APPLICATION", "ENVIRONMENT", "GRANTED", "NOTE"]);
    for grant in &grants {
        // Every cell here is text an administrator typed and the control
        // plane stored: a pattern, an application id, an environment name and
        // a free-text note nothing ever interprets. It is read back on the
        // terminal of whoever audits the registry next, which is exactly the
        // reader worth protecting from a note that repaints the table.
        table.row(vec![
            escaped(&grant.pattern),
            escaped(&grant.app_id),
            escaped(&grant.environment),
            timestamp(grant.created_at),
            escaped_or_dash(grant.note.as_deref()),
        ]);
    }
    println!();
    table.print();
    println!(
        "\n  {}",
        format!(
            "PATTERN {EVERY_ENVIRONMENT} covers every name including the route that names none, \
             `*.example.com` every name strictly under the zone, and `(no-host)` only the route \
             that names no host. ENVIRONMENT {EVERY_ENVIRONMENT} means every environment of that \
             application; anything else is one environment's name."
        )
        .dimmed()
    );
    Ok(())
}
