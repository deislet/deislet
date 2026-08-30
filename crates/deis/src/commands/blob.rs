//! `deis blob get|put|delete` - an environment's object storage.
//!
//! # Why there is no `deis blob list`
//!
//! `BlobStore` in `crates/deis-store/proto/blob.proto` has exactly three
//! RPCs: `Get`, `Put`, `Delete`. There is no `List`. The task this module
//! closes named `list` as one of the four to add, on the belief - stated in
//! the gap review this answers - that every RPC these commands need already
//! existed; for blobs that turned out not to be true, and the instruction
//! that mattered more was "read the proto, do not assume, and do not add
//! server-side RPCs: you do not own that crate". So this is three
//! subcommands, not four, and `deis-store`'s owner is who can turn this into
//! four - see the workspace-level report for the exact ask.
//!
//! Bodies are bytes, exactly like a KV value; see `crate::commands::kv` for
//! the reasoning behind reading them from `--file` and refusing to print
//! them raw when they are not text.

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};
use colored::Colorize;

use crate::commands::Session;
use crate::platform::storage::DEFAULT_MAX_BLOB_BYTES;
use crate::render::{
    BytesForDisplay, Output, base64_encode, bytes_for_display, confirm_destructive, timestamp,
};
use crate::settings::Overrides;

pub struct GetArgs {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub key: String,
    pub file: Option<PathBuf>,
    /// Largest object this call will hold in memory. See
    /// `crate::platform::storage::StorageSession::blob_get`.
    pub max_bytes: usize,
}

pub async fn get(args: GetArgs, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app: args.app,
        environment: args.environment,
    })?;
    let tenant = session.tenant()?;
    let storage = session.storage().await?;

    let Some(blob) = storage.blob_get(&args.key, args.max_bytes).await? else {
        if output.json {
            return output.emit(&serde_json::json!({
                "tenant": tenant,
                "key": args.key,
                "found": false,
            }));
        }
        println!(
            "{}",
            format!("no such blob key {:?} in {tenant}", args.key).dimmed()
        );
        return Ok(());
    };

    if let Some(path) = &args.file {
        std::fs::write(path, &blob.data).with_context(|| {
            format!(
                "writing {} bytes of blob key {:?} to {}",
                blob.data.len(),
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
            "sha256": blob.info.sha256,
            "size": blob.info.size,
            "content_type": blob.info.content_type,
            "created_at": blob.info.created_at,
            "file": args.file,
            "value_base64": args.file.is_none().then(|| base64_encode(&blob.data)),
        }));
    }

    match &args.file {
        Some(path) => println!(
            "\n  {} {} bytes written to {}",
            "✓".green(),
            blob.data.len(),
            path.display().to_string().cyan()
        ),
        None => match bytes_for_display(&blob.data) {
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
    eprintln!("{}", format!("sha256: {}", blob.info.sha256).dimmed());
    if let Some(content_type) = &blob.info.content_type {
        eprintln!("{}", format!("content-type: {content_type}").dimmed());
    }
    eprintln!(
        "{}",
        format!("stored {}", timestamp(blob.info.created_at)).dimmed()
    );
    Ok(())
}

pub struct PutArgs {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub key: String,
    /// The object's bytes. Absent means read them from stdin, which is what
    /// lets `deis blob put key --content-type image/png < file.png` work
    /// without a temporary file.
    pub file: Option<PathBuf>,
    pub content_type: Option<String>,
}

pub async fn put(args: PutArgs, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app: args.app,
        environment: args.environment,
    })?;
    let tenant = session.tenant()?;

    let data = match &args.file {
        Some(path) => std::fs::read(path)
            .with_context(|| format!("reading the object to store from {}", path.display()))?,
        None => {
            let mut bytes = Vec::new();
            std::io::stdin()
                .read_to_end(&mut bytes)
                .context("reading the object to store from stdin")?;
            bytes
        }
    };
    let bytes = data.len();

    let storage = session.storage().await?;
    let stored = storage
        .blob_put(&args.key, args.content_type.clone(), data)
        .await?;

    // deis-store answers with the size it actually wrote; a mismatch means
    // the upload was corrupted or truncated somewhere between this process
    // and the object it says exists, which is worth failing loudly over
    // rather than reporting the byte count this command merely sent.
    if stored.size != bytes as u64 {
        anyhow::bail!(
            "deis-store reports {} bytes stored for key {:?}, but this command sent {bytes}. The \
             object was written under that key regardless - do not trust its contents; put it \
             again.",
            stored.size,
            args.key
        );
    }

    if output.json {
        return output.emit(&serde_json::json!({
            "tenant": tenant,
            "key": args.key,
            "bytes": bytes,
            "sha256": stored.sha256,
            "content_type": args.content_type,
        }));
    }
    println!(
        "\n  {} {} bytes stored under {} in {} ({})",
        "✓".green(),
        bytes,
        args.key.cyan(),
        tenant.cyan(),
        format!("sha256 {}", &stored.sha256[..stored.sha256.len().min(12)]).dimmed()
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

    confirm_destructive(
        &tenant,
        &format!("delete blob key {:?}", args.key),
        args.yes,
    )?;

    let storage = session.storage().await?;
    storage.blob_delete(&args.key).await?;

    if output.json {
        return output.emit(&serde_json::json!({
            "tenant": tenant,
            "key": args.key,
            "deleted": true,
        }));
    }
    println!(
        "\n  {} {} deleted from {}",
        "✓".green(),
        args.key.cyan(),
        tenant.cyan()
    );
    Ok(())
}

/// The default for `--max-size`, exposed so `main.rs` can show it in
/// `--help` instead of a bare number nobody can trace back to anything.
pub fn default_max_bytes() -> usize {
    DEFAULT_MAX_BLOB_BYTES
}
