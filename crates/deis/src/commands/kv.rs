//! `deis kv get|put|delete|list` - an environment's key-value store.
//!
//! Before this existed, looking at what an application had stored in KV
//! meant driving `deis-store`'s gRPC directly, and doing that now also means
//! minting a capability token by hand. These four subcommands are that same
//! call, made through the credential the rest of this CLI already has.
//!
//! A value is whatever bytes an application wrote - there is no reason to
//! expect UTF-8. `put` takes them from `--file` (or a positional argument,
//! for the common case where the value really is short text); `get` writes
//! them to `--file`, or prints them only when they are valid UTF-8, on the
//! same reasoning `deis secret set` reads a value from stdin: text a
//! terminal can safely display, and nothing else, ever goes to it as raw
//! bytes.

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};
use colored::Colorize;

use crate::commands::Session;
use crate::render::{
    BytesForDisplay, Output, Table, age, base64_encode, bytes_for_display, confirm_destructive,
    timestamp,
};
use crate::settings::Overrides;

pub struct GetArgs {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub key: String,
    /// Write the value's bytes here instead of showing them.
    pub file: Option<PathBuf>,
}

pub async fn get(args: GetArgs, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app: args.app,
        environment: args.environment,
    })?;
    let tenant = session.tenant()?;
    let storage = session.storage().await?;

    let Some(value) = storage.kv_get(&args.key).await? else {
        if output.json {
            return output.emit(&serde_json::json!({
                "tenant": tenant,
                "key": args.key,
                "found": false,
            }));
        }
        println!(
            "{}",
            format!("no such key {:?} in {tenant}", args.key).dimmed()
        );
        return Ok(());
    };

    if let Some(path) = &args.file {
        std::fs::write(path, &value.value).with_context(|| {
            format!(
                "writing {} bytes of kv key {:?} to {}",
                value.value.len(),
                args.key,
                path.display()
            )
        })?;
    }

    if output.json {
        return output.emit(&serde_json::json!({
            "tenant": tenant,
            "key": args.key,
            "found": true,
            "bytes": value.value.len(),
            "metadata": value.metadata,
            "expires_at": value.expires_at,
            "file": args.file,
            // Omitted once the bytes are already on disk: doubling a
            // multi-megabyte value into base64 in the document too would be
            // pure waste for a caller who asked for a file.
            "value_base64": args.file.is_none().then(|| base64_encode(&value.value)),
        }));
    }

    match &args.file {
        Some(path) => println!(
            "\n  {} {} bytes written to {}",
            "✓".green(),
            value.value.len(),
            path.display().to_string().cyan()
        ),
        // Nothing asked to keep it on disk: the value itself is the whole of
        // stdout when it is text, so `deis kv get key > out.txt` captures
        // exactly the value and nothing else.
        None => match bytes_for_display(&value.value) {
            BytesForDisplay::Text(text) => println!("{text}"),
            BytesForDisplay::Binary { len, preview_hex } => println!(
                "{}",
                format!(
                    "{len} bytes, not valid UTF-8 (starts {preview_hex}...). Use --file to save \
                     it."
                )
                .dimmed()
            ),
        },
    }
    if let Some(metadata) = &value.metadata {
        eprintln!("{}", format!("metadata: {metadata}").dimmed());
    }
    if let Some(expires_at) = value.expires_at {
        eprintln!(
            "{}",
            format!("expires {} ({})", timestamp(expires_at), age(expires_at)).dimmed()
        );
    }
    Ok(())
}

pub struct PutArgs {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub key: String,
    /// The value, as text. `clap` refuses this together with `file`.
    pub value: Option<String>,
    /// The value, as raw bytes. What binary content goes in through.
    pub file: Option<PathBuf>,
    pub ttl_seconds: Option<i64>,
    pub metadata: Option<String>,
}

pub async fn put(args: PutArgs, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app: args.app,
        environment: args.environment,
    })?;
    let tenant = session.tenant()?;

    let value = read_value(args.value, args.file.as_deref())?;
    let bytes = value.len();
    let storage = session.storage().await?;
    storage
        .kv_put(&args.key, value, args.ttl_seconds, args.metadata.clone())
        .await?;

    if output.json {
        return output.emit(&serde_json::json!({
            "tenant": tenant,
            "key": args.key,
            "bytes": bytes,
            "ttl_seconds": args.ttl_seconds,
            "metadata": args.metadata,
        }));
    }
    println!(
        "\n  {} {} bytes stored under {} in {}",
        "✓".green(),
        bytes,
        args.key.cyan(),
        tenant.cyan()
    );
    Ok(())
}

pub struct DeleteArgs {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub key: String,
    pub yes: bool,
}

pub async fn delete(args: DeleteArgs, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app: args.app,
        environment: args.environment,
    })?;
    let tenant = session.tenant()?;

    confirm_destructive(&tenant, &format!("delete kv key {:?}", args.key), args.yes)?;

    let storage = session.storage().await?;
    let existed = storage.kv_delete(&args.key).await?;

    if output.json {
        return output.emit(&serde_json::json!({
            "tenant": tenant,
            "key": args.key,
            "existed": existed,
        }));
    }
    if existed {
        println!(
            "\n  {} {} deleted from {}",
            "✓".green(),
            args.key.cyan(),
            tenant.cyan()
        );
    } else {
        println!(
            "\n  {}",
            format!("{} did not exist in {tenant}; nothing to delete", args.key).dimmed()
        );
    }
    Ok(())
}

pub struct ListArgs {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub prefix: String,
    pub limit: i32,
    pub cursor: Option<String>,
}

pub async fn list(args: ListArgs, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app: args.app,
        environment: args.environment,
    })?;
    let tenant = session.tenant()?;
    let storage = session.storage().await?;

    let page = storage
        .kv_list(&args.prefix, args.limit, args.cursor)
        .await?;

    if output.json {
        return output.emit(&serde_json::json!({
            "tenant": tenant,
            "prefix": args.prefix,
            "keys": page.entries.iter().map(|e| serde_json::json!({
                "key": e.key,
                "metadata": e.metadata,
                "expires_at": e.expires_at,
            })).collect::<Vec<_>>(),
            "next_cursor": page.next_cursor,
        }));
    }

    println!(
        "\n{} {} {} {}",
        "Keys in".bold(),
        tenant.cyan(),
        "under prefix".bold(),
        format!("{:?}", args.prefix).cyan()
    );
    if page.entries.is_empty() {
        println!("\n  {}", "none".dimmed());
        return Ok(());
    }
    let mut table = Table::new(&["KEY", "METADATA", "EXPIRES"]);
    for entry in &page.entries {
        table.row(vec![
            entry.key.clone(),
            entry.metadata.clone().unwrap_or_else(|| "-".to_string()),
            entry
                .expires_at
                .map(timestamp)
                .unwrap_or_else(|| "never".to_string()),
        ]);
    }
    println!();
    table.print();
    if let Some(cursor) = &page.next_cursor {
        println!(
            "\n  {}",
            format!("more keys exist; continue with --cursor {cursor}").dimmed()
        );
    }
    Ok(())
}

/// Resolve a value from the three places one may come from, in the order a
/// person expects to be able to override: the positional argument, then
/// `--file`, then stdin.
///
/// Unlike `deis secret set`'s stdin reader, no trailing newline is ever
/// stripped: a KV value is raw bytes with no assumption that it is a line of
/// text, and trimming a byte off a binary value because it happens to equal
/// `\n` would be silent corruption of exactly the values this command exists
/// to handle correctly.
fn read_value(value: Option<String>, file: Option<&std::path::Path>) -> Result<Vec<u8>> {
    match (value, file) {
        (Some(value), None) => Ok(value.into_bytes()),
        (None, Some(path)) => std::fs::read(path)
            .with_context(|| format!("reading the value to store from {}", path.display())),
        (None, None) => {
            let mut bytes = Vec::new();
            std::io::stdin()
                .read_to_end(&mut bytes)
                .context("reading the value to store from stdin")?;
            Ok(bytes)
        }
        // `clap`'s `conflicts_with` on the CLI definition keeps a caller from
        // ever reaching this arm.
        (Some(_), Some(_)) => unreachable!("clap refuses --file together with a value argument"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_positional_value_is_used_verbatim() {
        assert_eq!(
            read_value(Some("hello".to_string()), None).unwrap(),
            b"hello"
        );
    }

    #[test]
    fn a_positional_value_keeps_bytes_a_trim_would_have_removed() {
        // Proves the "no trimming, ever" contract rather than merely
        // asserting it in the doc comment.
        assert_eq!(
            read_value(Some("hello\n".to_string()), None).unwrap(),
            b"hello\n"
        );
    }

    #[test]
    fn a_file_is_read_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("value.bin");
        std::fs::write(&path, [0xff, 0x00, 0x01, 0x02]).unwrap();
        assert_eq!(
            read_value(None, Some(&path)).unwrap(),
            vec![0xff, 0x00, 0x01, 0x02]
        );
    }
}
