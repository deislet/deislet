//! `deis blob list|head|get|put|delete` - an environment's object storage.
//!
//! Bodies are bytes, exactly like a KV value; see `crate::commands::kv` for
//! the reasoning behind reading them from `--file` and refusing to print them
//! raw when they are not text.
//!
//! # The two commands that can refuse to do anything
//!
//! `put` and `delete` take `--if-match` / `--if-none-match`, which make the
//! write conditional on the object's etag - the sha256 `head` prints and `put`
//! answers with. A conditional write that lost is not an error here: it exits
//! non-zero with its own code so a shell script can tell "somebody wrote first,
//! read it again" from "the store is down", and every other failure keeps the
//! usual exit code 1. See [`PRECONDITION_EXIT_CODE`].

use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};
use colored::Colorize;

use crate::commands::Session;
use crate::platform::storage::{BlobCondition, BlobRange, Conditional, DEFAULT_MAX_BLOB_BYTES};
use crate::render::{
    BytesForDisplay, Output, base64_encode, bytes_for_display, confirm_destructive, timestamp,
};
use crate::settings::Overrides;

/// What `deis blob put` and `deis blob delete` exit with when a `--if-match`
/// or `--if-none-match` did not hold.
///
/// Its own code, and nothing else uses it: a script retrying a
/// read-modify-write has to retry exactly this outcome and must not retry a
/// refused token or an unreachable store, and matching on the text of an error
/// message would start doing the wrong thing the first time somebody reworded
/// one.
pub const PRECONDITION_EXIT_CODE: u8 = 3;

/// A conditional write that did not happen because the object was not in the
/// state the caller required.
///
/// An error rather than a quiet zero exit, because the write the operator
/// asked for did not happen; a distinct type rather than a message, because
/// `main` gives it [`PRECONDITION_EXIT_CODE`] and every other failure the
/// ordinary one. Deciding that at the top rather than by calling
/// `process::exit` down here keeps one place that ends this process.
#[derive(Debug)]
pub struct PreconditionNotMet {
    pub key: String,
    pub condition: String,
    /// The etag the object carries now; `None` when it does not exist.
    pub etag: Option<String>,
}

impl std::fmt::Display for PreconditionNotMet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "blob key {:?} does not satisfy {}, so nothing was written. It is {} now - read it \
             again and retry against that.",
            self.key,
            self.condition,
            match &self.etag {
                Some(etag) => format!("sha256 {etag}"),
                None => "absent".to_string(),
            }
        )
    }
}

impl std::error::Error for PreconditionNotMet {}

/// What a `--if-match` / `--if-none-match` pair means, or an error when both
/// were given.
///
/// Two conditions on one write have no defined order, so the write is not
/// attempted at all rather than resolved in whichever order this code happens
/// to check them.
fn condition(
    if_match: Option<String>,
    if_none_match: Option<String>,
) -> Result<Option<BlobCondition>> {
    match (if_match, if_none_match) {
        (Some(_), Some(_)) => anyhow::bail!(
            "--if-match and --if-none-match are two conditions on one write and cannot be given \
             together; nothing was sent"
        ),
        (Some(etag), None) => Ok(Some(BlobCondition::IfMatch(etag))),
        (None, Some(etag)) => Ok(Some(BlobCondition::IfNoneMatch(etag))),
        (None, None) => Ok(None),
    }
}

/// Report a conditional write that lost.
///
/// `--json` still gets its one document on stdout - a script reads the outcome
/// there - and the error carries the same facts to stderr and to the exit
/// code.
fn report_refused(
    output: Output,
    tenant: &str,
    key: &str,
    what: &str,
    condition: String,
    etag: Option<String>,
) -> Result<()> {
    if output.json {
        output.emit(&serde_json::json!({
            "tenant": tenant,
            "key": key,
            what: false,
            "precondition": condition,
            "etag": etag,
        }))?;
    }
    Err(PreconditionNotMet {
        key: key.to_string(),
        condition,
        etag,
    }
    .into())
}

/// The byte range `--offset` / `--length` / `--suffix` name, or `None` for the
/// whole object.
fn range(
    offset: Option<u64>,
    length: Option<u64>,
    suffix: Option<u64>,
) -> Result<Option<BlobRange>> {
    match (offset, length, suffix) {
        (None, None, None) => Ok(None),
        (None, None, Some(suffix)) => Ok(Some(BlobRange::Suffix(suffix))),
        (_, _, Some(_)) => anyhow::bail!(
            "--suffix asks for the last bytes of the object, so it cannot be combined with \
             --offset or --length"
        ),
        (offset, length, None) => Ok(Some(BlobRange::Span {
            offset: offset.unwrap_or(0),
            length,
        })),
    }
}

pub struct GetArgs {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub key: String,
    pub file: Option<PathBuf>,
    /// First byte to read, counted from the start of the object.
    pub offset: Option<u64>,
    /// How many bytes at most, from `offset`. Absent runs to the end.
    pub length: Option<u64>,
    /// The last n bytes; cannot be combined with the two above.
    pub suffix: Option<u64>,
    /// Largest read this call will hold in memory. See
    /// `crate::platform::storage::StorageSession::blob_get`.
    pub max_bytes: usize,
}

pub async fn get(args: GetArgs, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app: args.app,
        environment: args.environment,
    })?;
    let tenant = session.tenant()?;
    let range = range(args.offset, args.length, args.suffix)?;
    let storage = session.storage().await?;

    let Some(blob) = storage.blob_get(&args.key, range, args.max_bytes).await? else {
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
            // The object's etag as well as its digest: the value to hand back
            // to `--if-match` on the next write. One string, two names, and
            // the name it is printed under here is the one `deis blob head`
            // and `deis blob list` also print.
            "sha256": blob.info.sha256,
            "size": blob.info.size,
            "content_type": blob.info.content_type,
            "created_at": blob.info.created_at,
            "bytes_read": blob.data.len(),
            "range": blob.range.map(|r| serde_json::json!({
                "offset": r.offset,
                "length": r.length,
            })),
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
    if let Some(served) = blob.range {
        // Spelled the way a `Content-Range` header is, because that is what an
        // operator checking a partial download compares it against.
        eprintln!(
            "{}",
            format!(
                "range: bytes {}-{}/{}",
                served.offset,
                served.offset + served.length - 1,
                blob.info.size
            )
            .dimmed()
        );
    }
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
    /// Write only if the object carries this etag, or `*` for any existing
    /// object.
    pub if_match: Option<String>,
    /// Write only if it does not carry this etag, or `*` for a key that must
    /// not exist at all.
    pub if_none_match: Option<String>,
}

pub async fn put(args: PutArgs, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app: args.app,
        environment: args.environment,
    })?;
    let tenant = session.tenant()?;
    let condition = condition(args.if_match.clone(), args.if_none_match.clone())?;

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
    let described = condition.as_ref().map(describe_condition);
    let stored = match storage
        .blob_put(&args.key, args.content_type.clone(), condition, data)
        .await?
    {
        Conditional::Done(stored) => stored,
        Conditional::Refused { condition, etag } => {
            return report_refused(output, &tenant, &args.key, "stored", condition, etag);
        }
    };

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
            "stored": true,
            "bytes": bytes,
            "sha256": stored.sha256,
            "content_type": args.content_type,
            "precondition": described,
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

/// How a condition reads back to the person who asked for it.
fn describe_condition(condition: &BlobCondition) -> String {
    match condition {
        BlobCondition::IfMatch(etag) => format!("If-Match: {etag}"),
        BlobCondition::IfNoneMatch(etag) => format!("If-None-Match: {etag}"),
    }
}

pub struct DeleteArgs {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub key: String,
    pub yes: bool,
    /// Delete only if the object carries this etag, or `*` for any existing
    /// object.
    pub if_match: Option<String>,
    /// Delete only if it does not carry this etag, or `*` for a key that must
    /// not exist at all.
    pub if_none_match: Option<String>,
}

pub async fn delete(args: DeleteArgs, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app: args.app,
        environment: args.environment,
    })?;
    let tenant = session.tenant()?;
    let condition = condition(args.if_match.clone(), args.if_none_match.clone())?;

    confirm_destructive(
        &tenant,
        &format!("delete blob key {:?}", args.key),
        args.yes,
    )?;

    let storage = session.storage().await?;
    let described = condition.as_ref().map(describe_condition);
    if let Conditional::Refused { condition, etag } =
        storage.blob_delete(&args.key, condition).await?
    {
        return report_refused(output, &tenant, &args.key, "deleted", condition, etag);
    }

    if output.json {
        return output.emit(&serde_json::json!({
            "tenant": tenant,
            "key": args.key,
            "deleted": true,
            "precondition": described,
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

pub struct HeadArgs {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub key: String,
}

/// What the store knows about one object, without moving its body.
///
/// The whole point is that this costs one metadata row: learning an object's
/// size used to mean downloading it and throwing the bytes away.
pub async fn head(args: HeadArgs, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app: args.app,
        environment: args.environment,
    })?;
    let tenant = session.tenant()?;
    let storage = session.storage().await?;

    let Some(info) = storage.blob_head(&args.key).await? else {
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

    if output.json {
        return output.emit(&serde_json::json!({
            "tenant": tenant,
            "key": info.key,
            "found": true,
            "sha256": info.sha256,
            "size": info.size,
            "content_type": info.content_type,
            "created_at": info.created_at,
        }));
    }
    println!("\n  {} in {}\n", info.key.cyan(), tenant.cyan());
    println!("  {:<14}{}", "size".dimmed(), info.size);
    println!("  {:<14}{}", "sha256".dimmed(), info.sha256);
    println!(
        "  {:<14}{}",
        "content-type".dimmed(),
        info.content_type.as_deref().unwrap_or("-")
    );
    println!("  {:<14}{}", "stored".dimmed(), timestamp(info.created_at));
    Ok(())
}

pub struct ListArgs {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub prefix: String,
    /// Roll keys up at this separator, so one folder is one line.
    pub delimiter: Option<String>,
    /// Objects per page asked of deis-store. It clamps this to its own
    /// ceiling, so a large value is a request rather than a promise.
    pub limit: Option<i32>,
    /// Resume from the cursor a previous page answered with.
    pub cursor: Option<String>,
    /// Keep asking for pages until the scan ends, instead of stopping after
    /// one. Off by default: a tenant can hold far more objects than fit on a
    /// terminal, and a command that walked the whole key space by default
    /// would be a surprise, not a convenience.
    pub all: bool,
}

pub async fn list(args: ListArgs, output: Output) -> Result<()> {
    let session = Session::open(&Overrides {
        app: args.app,
        environment: args.environment,
    })?;
    let tenant = session.tenant()?;
    let storage = session.storage().await?;

    let mut objects = Vec::new();
    let mut prefixes = Vec::new();
    let mut cursor = args.cursor;
    let mut pages = 0usize;
    loop {
        let page = storage
            .blob_list(&args.prefix, args.delimiter.clone(), args.limit, cursor)
            .await?;
        pages += 1;
        objects.extend(page.objects);
        prefixes.extend(page.prefixes);
        cursor = page.next_cursor;
        // The cursor is the end signal, not a short page: a scan can fill a
        // page exactly and still have nothing behind it, so stopping on a
        // short page would be right only by luck.
        if cursor.is_none() || !args.all {
            break;
        }
    }

    if output.json {
        return output.emit(&serde_json::json!({
            "tenant": tenant,
            "prefix": args.prefix,
            "delimiter": args.delimiter,
            "pages": pages,
            "objects": objects
                .iter()
                .map(|object| serde_json::json!({
                    "key": object.key,
                    "sha256": object.sha256,
                    "size": object.size,
                    "content_type": object.content_type,
                    "created_at": object.created_at,
                }))
                .collect::<Vec<_>>(),
            "prefixes": prefixes,
            "cursor": cursor,
        }));
    }

    if objects.is_empty() && prefixes.is_empty() {
        println!(
            "{}",
            format!("no objects under {:?} in {tenant}", args.prefix).dimmed()
        );
        return Ok(());
    }

    println!();
    for prefix in &prefixes {
        println!("  {:>10}  {}", "<folder>".dimmed(), prefix.cyan());
    }
    for object in &objects {
        println!(
            "  {:>10}  {}  {}",
            object.size,
            object.key.cyan(),
            format!(
                "{} {}",
                &object.sha256[..object.sha256.len().min(12)],
                timestamp(object.created_at)
            )
            .dimmed()
        );
    }
    let counted = objects.len() + prefixes.len();
    match &cursor {
        // Named rather than merely reported: this is what a person passes to
        // --cursor, and without it a truncated listing looks like a complete
        // one.
        Some(cursor) => println!(
            "\n  {}",
            format!(
                "{counted} shown, more to come. Continue with --cursor {cursor} , or ask for \
                 --all"
            )
            .dimmed()
        ),
        None => println!(
            "\n  {}",
            format!("{counted} shown, end of the listing").dimmed()
        ),
    }
    Ok(())
}

/// The default for `--max-size`, exposed so `main.rs` can show it in
/// `--help` instead of a bare number nobody can trace back to anything.
pub fn default_max_bytes() -> usize {
    DEFAULT_MAX_BLOB_BYTES
}
