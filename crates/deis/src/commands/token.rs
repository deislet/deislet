//! `deis token` - the credentials themselves.
//!
//! Three things this command is careful about, because each of them is a way
//! an operator loses control of their platform:
//!
//! * a minted secret is printed once, to stdout, with the reason said out
//!   loud. The control plane keeps a SHA-256 and cannot show it again;
//! * the listing marks every credential that answers to no scope at all, and
//!   counts them, because retiring those is what adopting scopes means;
//! * revoking says what stopped working, since something out there is still
//!   configured with it.

use anyhow::Result;
use colored::Colorize;

use crate::commands::{Session, escaped};
use crate::render::{Output, Table, age, timestamp};
use crate::settings::Overrides;

pub async fn create(
    account: String,
    name: String,
    scopes: Vec<String>,
    unscoped: bool,
    output: Output,
) -> Result<()> {
    if unscoped && !scopes.is_empty() {
        anyhow::bail!(
            "--unscoped and --scope ask for opposite things: an unscoped credential is the old \
             flat api_keys behaviour, which no scope list narrows"
        );
    }

    let session = Session::open(&Overrides::default())?;
    let minted = session
        .control
        .create_token(&account, &name, &scopes, unscoped)
        .await?;

    if output.json {
        // The secret is in this document because there is nowhere else it
        // will ever be; a script that mints a token has to be able to read it.
        return output.emit(&serde_json::json!({
            "id": minted.token.id,
            "account": minted.token.account,
            "name": minted.token.name,
            "scopes": minted.token.scopes,
            "unscoped": minted.token.unscoped,
            "created_at": minted.token.created_at,
            "secret": minted.secret,
        }));
    }

    println!(
        "\n{} {} {} {}",
        "Minted token".green().bold(),
        minted.token.name.cyan(),
        "for".bold(),
        minted.token.account.cyan()
    );
    println!("  {}  {}", "scopes:".bold(), minted.token.scopes);
    println!("  {}  {}", "id:    ".bold(), minted.token.id.dimmed());
    println!("\n  {}\n", minted.secret.yellow().bold());
    println!(
        "  {}",
        "This is the only time it will be shown. The control plane stores a SHA-256 of it and \
         cannot recover it; if it is lost, revoke this token and mint another."
            .dimmed()
    );
    println!(
        "  {}",
        "Store it with `deis config set control.token <value>`, or hand it to the pipeline that \
         needs it."
            .dimmed()
    );
    if minted.token.unscoped {
        println!(
            "\n  {}",
            "This credential is UNSCOPED: it can do anything its account's role allows, to every \
             application. Prefer --scope."
                .red()
        );
    }
    println!();
    Ok(())
}

pub async fn list(account: Option<String>, include_revoked: bool, output: Output) -> Result<()> {
    let session = Session::open(&Overrides::default())?;
    let response = session
        .control
        .list_tokens(account.as_deref(), include_revoked)
        .await?;

    if output.json {
        return output.emit(&serde_json::json!({
            "control_endpoint": session.control.base_url(),
            "total": response.tokens.len(),
            "unscoped": response.unscoped,
            "tokens": response.tokens,
        }));
    }

    println!(
        "\n{} {}",
        "Credentials on".bold(),
        session.control.base_url().dimmed()
    );

    if response.tokens.is_empty() {
        println!("\n  {}", "no credentials recorded.".yellow());
        return Ok(());
    }

    let mut table = Table::new(&[
        "ID",
        "ACCOUNT",
        "NAME",
        "ORIGIN",
        "SCOPES",
        "LAST USED",
        "STATE",
    ]);
    for token in &response.tokens {
        // Name, origin and scope list are strings the control plane stored
        // as they arrived. A credential able to mint a token is able to
        // choose all three, and this table is what an operator reads when
        // deciding which credential to revoke - the worst place for a cell
        // that can be made to read as a different row.
        table.row(vec![
            escaped(&token.id),
            escaped(&token.account),
            escaped(&token.name),
            escaped(&token.origin),
            if token.unscoped {
                "unscoped (full authority)".red().to_string()
            } else {
                escaped(&token.scopes)
            },
            match token.last_used_at {
                Some(at) => age(at),
                None => "never".dimmed().to_string(),
            },
            match token.revoked_at {
                Some(at) => format!("revoked {}", timestamp(at)).dimmed().to_string(),
                None => "live".to_string(),
            },
        ]);
    }
    println!();
    table.print();

    // The number this whole model exists to let an operator drive to zero.
    if response.unscoped > 0 {
        println!(
            "\n  {}",
            format!(
                "{} live credential(s) are unscoped: each may deploy any application, rotate any \
                 secret and mint any tenant's storage token. Replace them with scoped tokens and \
                 revoke these; an entry that came from api_keys is retired by taking it out of \
                 the control plane's configuration file.",
                response.unscoped
            )
            .yellow()
        );
    }
    println!();
    Ok(())
}

pub async fn revoke(id: String, output: Output) -> Result<()> {
    let session = Session::open(&Overrides::default())?;
    let token = session.control.revoke_token(&id).await?;

    if output.json {
        return output.emit(&serde_json::to_value(&token)?);
    }

    println!(
        "\n{} {} {} {}",
        "Revoked".red().bold(),
        token.name.cyan(),
        "of account".bold(),
        token.account.cyan()
    );
    println!(
        "  {}",
        "It is refused from its very next request. Anything still presenting it - a pipeline, a \
         script, a laptop - now gets 401 until it is given a new one, and the refusals appear in \
         `deis audit --outcome refused`."
            .dimmed()
    );
    println!();
    Ok(())
}
