//! The Deislet developer CLI.
//!
//! Two halves that deliberately do not mix:
//!
//! * `init` / `dev` / `validate` / `build` work on the project on disk and
//!   need no platform at all. `build --vendor cloudflare|deno|tencent`
//!   produces what that vendor's own tooling deploys; this CLI never pushes
//!   to them.
//! * `deploy` / `versions` / `rollback` / `status` / `secret` / `logs` /
//!   `tail` / `trace` / `kv` / `db` / `blob` talk to a Deislet platform - a
//!   `deis-control` and, for logs and traces, a `deis-telemetry`, and for
//!   `kv`/`db`/`blob`, also a `deis-store` reached *through* `deis-control` (see
//!   [`commands::Session::storage`]). Where those live and which credential
//!   to present is [`settings`].
//!
//! Flag names are the same everywhere they mean the same thing. In
//! particular the environment is always `-e, --environment` and never
//! `--env`, because "env" in this codebase already means an application's
//! variables (`context.env`), and one flag standing for both would be read
//! wrong by everyone exactly once.

use std::process::ExitCode;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use colored::Colorize;

mod commands;
mod dev;
mod platform;
mod quiet;
mod render;
mod settings;

use render::Output;

#[derive(Parser)]
#[command(name = "deis")]
#[command(about = "Build, deploy and inspect Deislet applications", long_about = None)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// How a command that answers with data should print it.
///
/// Declared on each such command rather than globally, so `--help` offers it
/// exactly where it does something: a flag that is accepted and ignored is a
/// flag someone will build a script on.
#[derive(Args, Clone, Copy, Debug, Default)]
struct Format {
    /// Print machine-readable JSON instead of the human report.
    ///
    /// `deis tail` prints one JSON object per line, because its output has
    /// no end; every other command prints one document and nothing else.
    #[arg(long)]
    json: bool,
}

impl From<Format> for Output {
    fn from(format: Format) -> Self {
        Output { json: format.json }
    }
}

/// Which application and environment a command is about.
///
/// Both fall back through the environment variables and the project file
/// (see [`settings`]), so a project that names itself in `deis.toml` needs
/// neither flag.
#[derive(Args, Clone, Debug, Default)]
struct Target {
    /// Application to act on.
    #[arg(short = 'a', long, value_name = "APP")]
    app: Option<String>,

    /// Environment to act on. Not `--env`: that would read as the
    /// application's variables.
    #[arg(short = 'e', long, value_name = "NAME")]
    environment: Option<String>,
}

/// The same, plus which namespace inside the tenant the command acts in.
///
/// Only `deis kv`, `deis db` and `deis blob` take it, because a binding is a
/// namespace inside one tenant's storage and nothing else in this CLI is about
/// storage. Leaving it out means the tenant's *unnamed* namespace, which is
/// what `context.services.kv`, `.database` and `.blob` reach and where an
/// application that declares no binding keeps everything it has.
#[derive(Args, Clone, Debug, Default)]
struct DataTarget {
    #[command(flatten)]
    target: Target,

    /// Named binding to act in. Omitted means the namespace
    /// `context.services.*` uses.
    #[arg(long, value_name = "NAME")]
    binding: Option<String>,
}

/// The same, for a command that is about a whole application.
#[derive(Args, Clone, Debug, Default)]
struct AppTarget {
    /// Application to act on.
    #[arg(short = 'a', long, value_name = "APP")]
    app: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new project.
    Init {
        /// Project name; also the directory it is created in.
        #[arg(default_value = "edge-app")]
        name: String,

        /// Template to use. Only "default" exists; any other name is
        /// refused rather than quietly replaced by it.
        #[arg(short, long, default_value = "default")]
        template: String,
    },

    /// Run the project locally, in a real deis-runtime.
    ///
    /// The bundle is built and handed to a deis-runtime this command starts,
    /// with the same call the control plane makes, so routing, assets,
    /// `context.params` and `context.env` behave as they will in production.
    Dev {
        /// Port to listen on.
        #[arg(short, long, default_value = "8000")]
        port: u16,

        /// Port for the runtime behind it. Chosen by the OS when unset.
        #[arg(long, value_name = "PORT")]
        runtime_port: Option<u16>,

        /// Which deis-runtime binary to start. Found next to this `deis`,
        /// or on PATH, when unset.
        #[arg(long, value_name = "PATH")]
        runtime: Option<std::path::PathBuf>,

        /// Environment name the application runs under locally. Not
        /// `production`, so a `--store` pointed at a real deis-store cannot
        /// write into the production tenant by accident.
        #[arg(short = 'e', long, default_value = "development")]
        environment: String,

        /// Do not rebuild when a file changes.
        #[arg(long)]
        no_watch: bool,

        /// Print the runtime's debug log lines as well.
        #[arg(long)]
        debug: bool,

        /// Minify the bundle, as a deploy does. Off by default so builds are
        /// fast and stack traces point at real lines.
        #[arg(long)]
        minify: bool,

        /// deis-store address, if one is running. Without it every
        /// ctx.services.{kv,database,blob} call fails.
        #[arg(long, value_name = "URL")]
        store: Option<String>,

        /// deis-queue address, if one is running.
        #[arg(long, value_name = "URL")]
        queue: Option<String>,

        /// A secret value for this session, as NAME=VALUE. Repeatable.
        #[arg(short = 's', long = "secret", value_name = "NAME=VALUE")]
        secrets: Vec<String>,
    },

    /// Check the project against the specification.
    Validate {
        /// Treat warnings as errors.
        #[arg(long)]
        strict: bool,
    },

    /// Compile the project for one or more targets.
    Build {
        /// Targets: deislet, cloudflare, deno, tencent, or all.
        #[arg(short, long, default_values_t = vec!["all".to_string()])]
        vendor: Vec<String>,

        /// Not implemented, and refused rather than ignored: `deis dev`
        /// is what rebuilds and reloads on every change.
        #[arg(long)]
        watch: bool,

        /// Keep the output unminified.
        #[arg(long)]
        no_minify: bool,
    },

    /// Build, upload as a version, and point an environment at it.
    ///
    /// This deploys to this platform. For Cloudflare, Deno or EdgeOne run
    /// `deis build --vendor <name>` and then that vendor's own tool.
    Deploy {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        format: Format,

        /// Note recorded with the deployment, for the history.
        #[arg(long)]
        note: Option<String>,

        /// Deploy this bundle instead of building one.
        #[arg(long, value_name = "PATH", conflicts_with = "no_build")]
        bundle: Option<std::path::PathBuf>,

        /// Use the bundle the last build left behind.
        #[arg(long)]
        no_build: bool,

        /// Keep the bundle unminified.
        #[arg(long)]
        no_minify: bool,
    },

    /// List an application's versions and which environments run them.
    Versions {
        #[command(flatten)]
        target: AppTarget,

        #[command(flatten)]
        format: Format,

        /// Show at most this many, newest first.
        #[arg(long, default_value = "20")]
        limit: usize,
    },

    /// Move an environment's pointer back to an earlier version.
    Rollback {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        format: Format,

        /// Version number to go back to. Omitted means the previous
        /// distinct version this environment ran.
        #[arg(long, value_name = "N")]
        to: Option<i64>,

        /// Note recorded with the rollback.
        #[arg(long)]
        note: Option<String>,
    },

    /// What each environment of an application runs, and which nodes serve it.
    Status {
        #[command(flatten)]
        target: AppTarget,

        #[command(flatten)]
        format: Format,
    },

    /// Follow an application's logs until interrupted.
    Tail {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        format: Format,

        /// Minimum severity: trace, debug, info, warn, error, fatal.
        #[arg(short, long, value_name = "LEVEL")]
        level: Option<String>,

        /// Do not filter by environment.
        #[arg(long)]
        all_environments: bool,
    },

    /// Read stored logs for a time range.
    Logs {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        format: Format,

        /// Minimum severity: trace, debug, info, warn, error, fatal.
        #[arg(short, long, value_name = "LEVEL")]
        level: Option<String>,

        /// Start of the range: a duration back from now (30s, 15m, 2h, 3d)
        /// or an RFC 3339 instant.
        #[arg(long, default_value = "1h", value_name = "WHEN")]
        since: String,

        /// End of the range, same forms. Omitted means now.
        #[arg(long, value_name = "WHEN")]
        until: Option<String>,

        /// Only lines whose message contains this, case-insensitively.
        #[arg(long, value_name = "TEXT")]
        contains: Option<String>,

        /// Only lines carrying this trace id.
        #[arg(long, value_name = "ID")]
        trace_id: Option<String>,

        /// Show at most this many, newest first.
        #[arg(long, default_value = "200")]
        limit: usize,

        /// Do not filter by environment.
        #[arg(long)]
        all_environments: bool,
    },

    /// List an application's invocations: how each one ended, and why.
    ///
    /// One row per invocation, from what the nodes report to
    /// deis-telemetry. `deis logs` shows what an application said; this
    /// shows what the platform decided - which invocations it cut off for
    /// running too long, hammering the op layer or eating the heap, and
    /// which threw on their own.
    Trace {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        format: Format,

        /// Only invocations that ended this way. Repeatable.
        ///
        /// One of ok, app-error, timeout, rate-limited, memory-limit,
        /// cpu-limit, internal-error, or `failed` for every one but ok.
        #[arg(long, value_name = "OUTCOME")]
        outcome: Vec<String>,

        /// Only invocations of this kind: http, queue, cron, websocket.
        /// Repeatable.
        #[arg(long, value_name = "KIND")]
        kind: Vec<String>,

        /// Only invocations that answered this HTTP status (500) or class
        /// (5xx). Applied by this CLI, not by deis-telemetry.
        #[arg(long, value_name = "STATUS")]
        status: Option<String>,

        /// Start of the range: a duration back from now (30s, 15m, 2h, 3d)
        /// or an RFC 3339 instant. Default 1h.
        #[arg(long, value_name = "WHEN", conflicts_with = "follow")]
        since: Option<String>,

        /// End of the range, same forms. Omitted means now.
        #[arg(long, value_name = "WHEN", conflicts_with = "follow")]
        until: Option<String>,

        /// Only the invocation carrying this trace id. The same id
        /// `deis logs --trace-id` takes.
        #[arg(long, value_name = "ID", conflicts_with = "follow")]
        trace_id: Option<String>,

        /// Only invocations whose target contains this, case-insensitively.
        #[arg(long, value_name = "TEXT", conflicts_with = "follow")]
        target_contains: Option<String>,

        /// Only invocations that ran at least this long, in milliseconds.
        #[arg(long, value_name = "MS", conflicts_with = "follow")]
        min_duration: Option<u64>,

        /// Show at most this many, newest first. Default 50.
        #[arg(long, conflicts_with = "follow")]
        limit: Option<usize>,

        /// Follow invocations as they happen instead of reading stored
        /// ones. Only what the nodes report from now on appears.
        #[arg(long)]
        follow: bool,

        /// Do not filter by environment.
        #[arg(long)]
        all_environments: bool,
    },

    /// Manage an environment's secrets. Values go in and never come back.
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },

    /// Read and write an environment's key-value store.
    Kv {
        #[command(subcommand)]
        action: KvAction,
    },

    /// Run SQL against an environment's database.
    Db {
        #[command(subcommand)]
        action: DbAction,
    },

    /// Read and write an environment's blob objects.
    Blob {
        #[command(subcommand)]
        action: BlobAction,
    },

    /// Look at what an environment's queues gave up on, and put it back.
    ///
    /// Producing and consuming messages is the application's job; this is
    /// the half of a queue only a person can do.
    Queue {
        #[command(subcommand)]
        action: QueueAction,
    },

    /// Show or change where the platform is and which credential to use.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Manage the accounts that may call this platform.
    ///
    /// An account is what an audit line blames, and its role is the ceiling
    /// on everything its tokens may do. Needs a credential carrying
    /// `admin:*/*`.
    Account {
        #[command(subcommand)]
        action: AccountAction,
    },

    /// Mint, list and revoke the credentials accounts call with.
    ///
    /// A minted secret is printed once and stored only as a digest, so a
    /// control plane cannot show it again. Needs `admin:*/*`.
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },

    /// Read what was done to this platform, by whom, and how it ended.
    ///
    /// Every deploy, rollback, secret change, token mint, account change and
    /// route change is here, refusals included. Needs `admin:*/*`.
    Audit {
        #[command(flatten)]
        format: Format,

        /// Only this application.
        #[arg(short = 'a', long, value_name = "APP")]
        app: Option<String>,

        /// Only this environment.
        #[arg(short = 'e', long, value_name = "NAME")]
        environment: Option<String>,

        /// Only what this account did.
        #[arg(long, value_name = "NAME")]
        account: Option<String>,

        /// Only this operation, as the log names it: deploy, rollback,
        /// secret.write, token.mint, storage_token.mint.
        #[arg(long, value_name = "NAME")]
        action: Option<String>,

        /// Only accepted, refused or failed calls.
        #[arg(long, value_name = "OUTCOME")]
        outcome: Option<String>,

        /// Only the last this many minutes.
        #[arg(long, value_name = "MINUTES")]
        since: Option<i64>,

        /// Most lines to print, newest first.
        #[arg(long, default_value = "50")]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum AccountAction {
    /// List every account and the role that caps its credentials.
    List {
        #[command(flatten)]
        format: Format,
    },

    /// Create an account.
    Create {
        #[command(flatten)]
        format: Format,

        /// Name, as every audit line will print it.
        name: String,

        /// owner, maintainer, deployer or viewer. Required rather than
        /// defaulted: the difference between viewer and owner is the point.
        #[arg(long)]
        role: String,
    },

    /// Change an account's role, or switch it off and back on.
    Update {
        #[command(flatten)]
        format: Format,

        /// Account to change.
        name: String,

        /// New role. Applies to every token this account already holds, at
        /// their next request.
        #[arg(long)]
        role: Option<String>,

        /// Refuse every token of this account without revoking any of them.
        #[arg(long)]
        disable: bool,

        /// Undo `--disable`, restoring exactly the same credentials.
        #[arg(long)]
        enable: bool,
    },
}

#[derive(Subcommand)]
enum TokenAction {
    /// Mint a credential for an account. Its secret is printed once.
    Create {
        #[command(flatten)]
        format: Format,

        /// Which account it belongs to.
        #[arg(long)]
        account: String,

        /// What it is for, in a word: ci-preview, alice-laptop. Printed in
        /// every listing, so it is how a person decides which one to revoke.
        name: String,

        /// One scope, as `<verb>:<app>/<environment>` - for instance
        /// `deploy:myapp/preview`. Repeatable. Leave it out and the token
        /// gets its account's role over every application.
        #[arg(long = "scope", value_name = "SCOPE")]
        scopes: Vec<String>,

        /// Mint a credential subject to no scope at all - the old flat
        /// api_keys behaviour. Refused together with --scope.
        #[arg(long)]
        unscoped: bool,
    },

    /// List the credentials this platform knows about.
    List {
        #[command(flatten)]
        format: Format,

        /// Only this account's.
        #[arg(long)]
        account: Option<String>,

        /// Include credentials that no longer work.
        #[arg(long)]
        include_revoked: bool,
    },

    /// Revoke a credential. It is refused from its very next request.
    Revoke {
        #[command(flatten)]
        format: Format,

        /// The token's id, as `deis token list` prints it.
        id: String,
    },
}

#[derive(Subcommand)]
enum KvAction {
    /// Read one value.
    Get {
        #[command(flatten)]
        target: DataTarget,

        #[command(flatten)]
        format: Format,

        /// Key to read.
        key: String,

        /// Write the value's bytes here instead of showing them. Without
        /// this, a value that is not valid UTF-8 is described (size and a
        /// hex preview) rather than printed, so it never corrupts the
        /// terminal.
        #[arg(long, value_name = "PATH")]
        file: Option<std::path::PathBuf>,
    },

    /// Write one value.
    Put {
        #[command(flatten)]
        target: DataTarget,

        #[command(flatten)]
        format: Format,

        /// Key to write.
        key: String,

        /// The value, as text. Omit it and pass --file for anything that is
        /// not text, or pipe the value in on stdin.
        #[arg(conflicts_with = "file")]
        value: Option<String>,

        /// The value, as raw bytes read from a file. What binary content
        /// goes in through.
        #[arg(long, value_name = "PATH")]
        file: Option<std::path::PathBuf>,

        /// Lifetime in seconds from now. Omitted means the value never
        /// expires.
        #[arg(long, value_name = "SECONDS")]
        ttl: Option<i64>,

        /// An opaque string stored and returned alongside the value.
        /// Never parsed by the store.
        #[arg(long)]
        metadata: Option<String>,
    },

    /// Delete one key.
    ///
    /// Destructive and confirmed: without -y/--yes this asks the tenant to
    /// be typed back before anything is deleted.
    Delete {
        #[command(flatten)]
        target: DataTarget,

        #[command(flatten)]
        format: Format,

        /// Key to delete.
        key: String,

        /// Skip the confirmation prompt. For scripts.
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// List keys under a prefix.
    List {
        #[command(flatten)]
        target: DataTarget,

        #[command(flatten)]
        format: Format,

        /// Only keys starting with this. Empty lists every key.
        #[arg(long, default_value = "")]
        prefix: String,

        /// Show at most this many.
        #[arg(long, default_value = "100")]
        limit: i32,

        /// Resume a listing from a previous page's `next_cursor`.
        #[arg(long)]
        cursor: Option<String>,
    },
}

#[derive(Subcommand)]
enum DbAction {
    /// Run a SELECT and show what came back.
    Query {
        #[command(flatten)]
        target: DataTarget,

        #[command(flatten)]
        format: Format,

        /// The SQL to run. Quote it as one shell argument.
        sql: String,

        /// One per `?` placeholder, in order. Repeatable. Each is parsed as
        /// JSON when it parses (42, true, null, "already-quoted") and taken
        /// as a plain string otherwise (alice).
        #[arg(long = "param", value_name = "VALUE")]
        params: Vec<String>,
    },

    /// Run anything that is not a SELECT: INSERT, UPDATE, DELETE, DDL.
    ///
    /// Destructive and confirmed: without -y/--yes this asks the tenant to
    /// be typed back before the statement runs. This is the command a
    /// mistaken WHERE clause turns into an incident, so read what is on
    /// screen before typing it back.
    Execute {
        #[command(flatten)]
        target: DataTarget,

        #[command(flatten)]
        format: Format,

        /// The SQL to run. Quote it as one shell argument.
        sql: String,

        /// One per `?` placeholder, in order. Same parsing as `deis db
        /// query`.
        #[arg(long = "param", value_name = "VALUE")]
        params: Vec<String>,

        /// Skip the confirmation prompt. For scripts and migration runners.
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum BlobAction {
    /// List objects, newest page first.
    ///
    /// One page unless --all is given: an environment can hold far more
    /// objects than fit on a terminal, so the walk is asked for rather than
    /// assumed.
    List {
        #[command(flatten)]
        target: DataTarget,

        #[command(flatten)]
        format: Format,

        /// Only keys starting with this. Omit it to list everything.
        #[arg(default_value = "")]
        prefix: String,

        /// Roll every key up at this separator, so one folder is one line.
        /// `--delimiter /` walks a key space one level at a time.
        #[arg(long, value_name = "SEP")]
        delimiter: Option<String>,

        /// Objects per page. deis-store caps this at its own ceiling, so a
        /// larger number pages rather than fails.
        #[arg(long, value_name = "COUNT")]
        limit: Option<i32>,

        /// Resume from the cursor a previous page printed.
        #[arg(long, value_name = "CURSOR")]
        cursor: Option<String>,

        /// Keep paging until the listing ends.
        #[arg(long)]
        all: bool,
    },

    /// Show one object's size, media type and etag without downloading it.
    Head {
        #[command(flatten)]
        target: DataTarget,

        #[command(flatten)]
        format: Format,

        /// Key to describe.
        key: String,
    },

    /// Read one object.
    Get {
        #[command(flatten)]
        target: DataTarget,

        #[command(flatten)]
        format: Format,

        /// Key to read.
        key: String,

        /// Write the object's bytes here instead of showing them.
        #[arg(long, value_name = "PATH")]
        file: Option<std::path::PathBuf>,

        /// First byte to read, counted from the start of the object.
        #[arg(long, value_name = "BYTES")]
        offset: Option<u64>,

        /// How many bytes to read from --offset. Omit it to read to the end.
        #[arg(long, value_name = "BYTES")]
        length: Option<u64>,

        /// Read the last n bytes instead. Cannot be combined with --offset or
        /// --length.
        #[arg(long, value_name = "BYTES")]
        suffix: Option<u64>,

        /// Largest read this command will hold in memory. Raise it, or ask
        /// for a byte range, for an object bigger than the default.
        #[arg(long, value_name = "BYTES", default_value_t = commands::blob::default_max_bytes())]
        max_size: usize,
    },

    /// Write one object.
    Put {
        #[command(flatten)]
        target: DataTarget,

        #[command(flatten)]
        format: Format,

        /// Key to write.
        key: String,

        /// The object's bytes. Omit it to pipe the object in on stdin.
        #[arg(long, value_name = "PATH")]
        file: Option<std::path::PathBuf>,

        /// Media type recorded alongside the object, e.g. image/png.
        #[arg(long, value_name = "TYPE")]
        content_type: Option<String>,

        /// Write only if the object still carries this etag - the sha256
        /// `deis blob head` prints. `*` accepts any existing object. A write
        /// that loses exits 3 and stores nothing.
        #[arg(long, value_name = "ETAG")]
        if_match: Option<String>,

        /// Write only if the object does not carry this etag. `*` accepts
        /// only a key that does not exist, which is how a script creates one
        /// exactly once.
        #[arg(long, value_name = "ETAG", conflicts_with = "if_match")]
        if_none_match: Option<String>,
    },

    /// Delete one object.
    ///
    /// Destructive and confirmed: without -y/--yes this asks the tenant to
    /// be typed back before anything is deleted.
    Delete {
        #[command(flatten)]
        target: DataTarget,

        #[command(flatten)]
        format: Format,

        /// Key to delete.
        key: String,

        /// Skip the confirmation prompt. For scripts.
        #[arg(short = 'y', long)]
        yes: bool,

        /// Delete only if the object still carries this etag. `*` accepts any
        /// existing object. A delete that loses exits 3 and removes nothing.
        #[arg(long, value_name = "ETAG")]
        if_match: Option<String>,

        /// Delete only if the object does not carry this etag. `*` accepts
        /// only a key that does not exist.
        #[arg(long, value_name = "ETAG", conflicts_with = "if_match")]
        if_none_match: Option<String>,
    },
}

#[derive(Subcommand)]
enum QueueAction {
    /// List the messages a queue gave up on: what is stuck, and why.
    ///
    /// One page unless --all is given. Every queue of the environment unless
    /// --queue names one, because the question this answers is usually "what
    /// is stuck?" rather than "what is stuck in this one queue".
    DeadLetters {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        format: Format,

        /// Only this queue. Omitted reads every queue of the environment.
        #[arg(long, value_name = "NAME")]
        queue: Option<String>,

        /// Show at most this many. deis-queue caps it, and also stops a page
        /// early when the bodies would be too large to send.
        #[arg(long)]
        limit: Option<i32>,

        /// Resume from a previous page's cursor.
        #[arg(long)]
        cursor: Option<String>,

        /// Walk every page instead of stopping after one.
        #[arg(long)]
        all: bool,
    },

    /// Put named dead letters back on their queue, with their attempts reset.
    ///
    /// For after the cause has been fixed: a replayed message is delivered
    /// again as many times as a new one, so replaying before the fix simply
    /// spends the attempts again. There is no --all and there will not be
    /// one - deis-queue refuses a replay that names no message, because
    /// putting a whole queue of poison messages back cannot be undone.
    Replay {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        format: Format,

        /// Which queue the messages belong to.
        #[arg(long, value_name = "NAME")]
        queue: String,

        /// Ids from `deis queue dead-letters`. At least one.
        #[arg(required = true, value_name = "ID")]
        ids: Vec<String>,

        /// Hold them back this many seconds before a consumer may have them.
        #[arg(long, value_name = "SECONDS")]
        delay: Option<i32>,
    },
}

#[derive(Subcommand)]
enum SecretAction {
    /// Store a secret. The value is read from stdin when it is not given,
    /// which keeps it out of the shell history.
    Set {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        format: Format,

        /// Secret name, as `context.env` sees it.
        name: String,

        /// Secret value. Omit it to be prompted, or to pipe it in.
        value: Option<String>,
    },

    /// List the secret names of an environment, with when each was last set.
    /// There is no command that prints a value: the control plane has no
    /// endpoint that returns one.
    List {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        format: Format,
    },

    /// Remove a secret.
    Rm {
        #[command(flatten)]
        target: Target,

        #[command(flatten)]
        format: Format,

        /// Secret name.
        name: String,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Set one setting. `control.token` goes to the per-user credentials
    /// file; everything else goes to the project file.
    Set {
        /// One of: control.endpoint, control.token, telemetry.endpoint,
        /// defaults.app, defaults.environment.
        key: String,
        /// Value to store.
        value: String,

        #[command(flatten)]
        format: Format,
    },

    /// Print one setting and where it came from.
    Get {
        /// Setting name, as in `deis config set`.
        key: String,

        #[command(flatten)]
        format: Format,
    },

    /// Remove one setting from the file that holds it.
    Unset {
        /// Setting name, as in `deis config set`.
        key: String,

        #[command(flatten)]
        format: Format,
    },

    /// Show every setting, where it came from, and which files hold them.
    List {
        #[command(flatten)]
        format: Format,
    },
}

fn main() -> ExitCode {
    // Progress and diagnostics belong on stderr: stdout is where `--json`
    // puts the one document a script reads.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("{} could not start an async runtime: {e}", "error:".red());
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{} {e}", "error:".red().bold());
            for cause in e.chain().skip(1) {
                eprintln!("  {} {cause}", "caused by".dimmed());
            }
            // A conditional write that lost gets its own exit code, because it
            // is the one failure a script answers by reading the object again
            // and retrying - and the only way to tell it from a refused token
            // or an unreachable store without matching on the text of a
            // message somebody may reword.
            if e.downcast_ref::<commands::blob::PreconditionNotMet>()
                .is_some()
            {
                ExitCode::from(commands::blob::PRECONDITION_EXIT_CODE)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Init { name, template } => commands::init::execute(name, template).await,

        Commands::Dev {
            port,
            runtime_port,
            runtime,
            environment,
            no_watch,
            debug,
            minify,
            store,
            queue,
            secrets,
        } => {
            commands::dev::execute(commands::dev::Args {
                port,
                runtime_port,
                runtime,
                environment,
                watch: !no_watch,
                debug,
                minify,
                store,
                queue,
                secrets,
            })
            .await
        }

        Commands::Validate { strict } => commands::validate::execute(strict).await,

        Commands::Build {
            vendor,
            watch,
            no_minify,
        } => commands::build::execute(vendor, watch, !no_minify).await,

        Commands::Deploy {
            target,
            format,
            note,
            bundle,
            no_build,
            no_minify,
        } => {
            commands::deploy::execute(
                commands::deploy::Args {
                    app: target.app,
                    environment: target.environment,
                    note,
                    bundle,
                    no_build,
                    no_minify,
                },
                format.into(),
            )
            .await
        }

        Commands::Versions {
            target,
            format,
            limit,
        } => commands::versions::execute(target.app, limit, format.into()).await,

        Commands::Rollback {
            target,
            format,
            to,
            note,
        } => {
            commands::rollback::execute(
                commands::rollback::Args {
                    app: target.app,
                    environment: target.environment,
                    to,
                    note,
                },
                format.into(),
            )
            .await
        }

        Commands::Status { target, format } => {
            commands::status::execute(target.app, format.into()).await
        }

        Commands::Tail {
            target,
            format,
            level,
            all_environments,
        } => {
            commands::logs::tail(
                commands::logs::TailArgs {
                    app: target.app,
                    environment: target.environment,
                    level,
                    all_environments,
                },
                format.into(),
            )
            .await
        }

        Commands::Logs {
            target,
            format,
            level,
            since,
            until,
            contains,
            trace_id,
            limit,
            all_environments,
        } => {
            commands::logs::query(
                commands::logs::QueryArgs {
                    app: target.app,
                    environment: target.environment,
                    level,
                    since,
                    until,
                    contains,
                    trace_id,
                    limit,
                    all_environments,
                },
                format.into(),
            )
            .await
        }

        Commands::Trace {
            target,
            format,
            outcome,
            kind,
            status,
            since,
            until,
            trace_id,
            target_contains,
            min_duration,
            limit,
            follow,
            all_environments,
        } => {
            commands::trace::execute(
                commands::trace::Args {
                    app: target.app,
                    environment: target.environment,
                    all_environments,
                    outcome,
                    kind,
                    status,
                    since,
                    until,
                    trace_id,
                    target: target_contains,
                    min_duration_ms: min_duration,
                    limit,
                    follow,
                },
                format.into(),
            )
            .await
        }

        Commands::Account { action } => match action {
            AccountAction::List { format } => commands::account::list(format.into()).await,
            AccountAction::Create { format, name, role } => {
                commands::account::create(name, role, format.into()).await
            }
            AccountAction::Update {
                format,
                name,
                role,
                disable,
                enable,
            } => commands::account::update(name, role, disable, enable, format.into()).await,
        },

        Commands::Token { action } => match action {
            TokenAction::Create {
                format,
                account,
                name,
                scopes,
                unscoped,
            } => commands::token::create(account, name, scopes, unscoped, format.into()).await,
            TokenAction::List {
                format,
                account,
                include_revoked,
            } => commands::token::list(account, include_revoked, format.into()).await,
            TokenAction::Revoke { format, id } => commands::token::revoke(id, format.into()).await,
        },

        Commands::Audit {
            format,
            app,
            environment,
            account,
            action,
            outcome,
            since,
            limit,
        } => {
            commands::audit::execute(
                app,
                environment,
                account,
                action,
                outcome,
                since,
                limit,
                format.into(),
            )
            .await
        }

        Commands::Secret { action } => match action {
            SecretAction::Set {
                target,
                format,
                name,
                value,
            } => {
                commands::secret::set(target.app, target.environment, name, value, format.into())
                    .await
            }
            SecretAction::List { target, format } => {
                commands::secret::list(target.app, target.environment, format.into()).await
            }
            SecretAction::Rm {
                target,
                format,
                name,
            } => {
                commands::secret::remove(target.app, target.environment, name, format.into()).await
            }
        },

        Commands::Kv { action } => match action {
            KvAction::Get {
                target,
                format,
                key,
                file,
            } => {
                commands::kv::get(
                    commands::kv::GetArgs {
                        app: target.target.app,
                        environment: target.target.environment,
                        binding: target.binding,
                        key,
                        file,
                    },
                    format.into(),
                )
                .await
            }
            KvAction::Put {
                target,
                format,
                key,
                value,
                file,
                ttl,
                metadata,
            } => {
                commands::kv::put(
                    commands::kv::PutArgs {
                        app: target.target.app,
                        environment: target.target.environment,
                        binding: target.binding,
                        key,
                        value,
                        file,
                        ttl_seconds: ttl,
                        metadata,
                    },
                    format.into(),
                )
                .await
            }
            KvAction::Delete {
                target,
                format,
                key,
                yes,
            } => {
                commands::kv::delete(
                    commands::kv::DeleteArgs {
                        app: target.target.app,
                        environment: target.target.environment,
                        binding: target.binding,
                        key,
                        yes,
                    },
                    format.into(),
                )
                .await
            }
            KvAction::List {
                target,
                format,
                prefix,
                limit,
                cursor,
            } => {
                commands::kv::list(
                    commands::kv::ListArgs {
                        app: target.target.app,
                        environment: target.target.environment,
                        binding: target.binding,
                        prefix,
                        limit,
                        cursor,
                    },
                    format.into(),
                )
                .await
            }
        },

        Commands::Db { action } => match action {
            DbAction::Query {
                target,
                format,
                sql,
                params,
            } => {
                commands::db::query(
                    commands::db::QueryArgs {
                        app: target.target.app,
                        environment: target.target.environment,
                        binding: target.binding,
                        sql,
                        params,
                    },
                    format.into(),
                )
                .await
            }
            DbAction::Execute {
                target,
                format,
                sql,
                params,
                yes,
            } => {
                commands::db::execute(
                    commands::db::ExecuteArgs {
                        app: target.target.app,
                        environment: target.target.environment,
                        binding: target.binding,
                        sql,
                        params,
                        yes,
                    },
                    format.into(),
                )
                .await
            }
        },

        Commands::Blob { action } => match action {
            BlobAction::List {
                target,
                format,
                prefix,
                delimiter,
                limit,
                cursor,
                all,
            } => {
                commands::blob::list(
                    commands::blob::ListArgs {
                        app: target.target.app,
                        environment: target.target.environment,
                        binding: target.binding,
                        prefix,
                        delimiter,
                        limit,
                        cursor,
                        all,
                    },
                    format.into(),
                )
                .await
            }
            BlobAction::Head {
                target,
                format,
                key,
            } => {
                commands::blob::head(
                    commands::blob::HeadArgs {
                        app: target.target.app,
                        environment: target.target.environment,
                        binding: target.binding,
                        key,
                    },
                    format.into(),
                )
                .await
            }
            BlobAction::Get {
                target,
                format,
                key,
                file,
                offset,
                length,
                suffix,
                max_size,
            } => {
                commands::blob::get(
                    commands::blob::GetArgs {
                        app: target.target.app,
                        environment: target.target.environment,
                        binding: target.binding,
                        key,
                        file,
                        offset,
                        length,
                        suffix,
                        max_bytes: max_size,
                    },
                    format.into(),
                )
                .await
            }
            BlobAction::Put {
                target,
                format,
                key,
                file,
                content_type,
                if_match,
                if_none_match,
            } => {
                commands::blob::put(
                    commands::blob::PutArgs {
                        app: target.target.app,
                        environment: target.target.environment,
                        binding: target.binding,
                        key,
                        file,
                        content_type,
                        if_match,
                        if_none_match,
                    },
                    format.into(),
                )
                .await
            }
            BlobAction::Delete {
                target,
                format,
                key,
                yes,
                if_match,
                if_none_match,
            } => {
                commands::blob::delete(
                    commands::blob::DeleteArgs {
                        app: target.target.app,
                        environment: target.target.environment,
                        binding: target.binding,
                        key,
                        yes,
                        if_match,
                        if_none_match,
                    },
                    format.into(),
                )
                .await
            }
        },

        Commands::Queue { action } => match action {
            QueueAction::DeadLetters {
                target,
                format,
                queue,
                limit,
                cursor,
                all,
            } => {
                commands::queue::dead_letters(
                    commands::queue::DeadLettersArgs {
                        app: target.app,
                        environment: target.environment,
                        queue,
                        limit,
                        cursor,
                        all,
                    },
                    format.into(),
                )
                .await
            }
            QueueAction::Replay {
                target,
                format,
                queue,
                ids,
                delay,
            } => {
                commands::queue::replay(
                    commands::queue::ReplayArgs {
                        app: target.app,
                        environment: target.environment,
                        queue,
                        ids,
                        delay,
                    },
                    format.into(),
                )
                .await
            }
        },

        Commands::Config { action } => match action {
            ConfigAction::Set { key, value, format } => {
                commands::config::set(&key, &value, format.into())
            }
            ConfigAction::Get { key, format } => commands::config::get(&key, format.into()),
            ConfigAction::Unset { key, format } => commands::config::unset(&key, format.into()),
            ConfigAction::List { format } => commands::config::list(format.into()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    /// The environment is `-e/--environment` on every command that has one.
    /// `--env` must not be accepted anywhere: it reads as the application's
    /// variables, which is a different thing entirely.
    #[test]
    fn the_environment_flag_has_one_spelling_everywhere() {
        for argv in [
            vec!["deis", "deploy", "-e", "preview"],
            vec!["deis", "rollback", "--environment", "preview"],
            vec!["deis", "tail", "-e", "preview"],
            vec!["deis", "logs", "--environment", "preview"],
            vec!["deis", "secret", "list", "-e", "preview"],
            vec!["deis", "kv", "get", "-e", "preview", "mykey"],
            vec![
                "deis",
                "db",
                "query",
                "--environment",
                "preview",
                "select 1",
            ],
            vec!["deis", "blob", "get", "-e", "preview", "mykey"],
            vec!["deis", "queue", "dead-letters", "-e", "preview"],
        ] {
            Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }

        for argv in [
            vec!["deis", "deploy", "--env", "preview"],
            vec!["deis", "tail", "--env", "preview"],
            vec!["deis", "kv", "get", "--env", "preview", "mykey"],
        ] {
            assert!(
                Cli::try_parse_from(&argv).is_err(),
                "{argv:?}: --env must not be accepted; it reads as context.env"
            );
        }
    }

    /// Deploying targets this platform only. A `--vendor` flag here would
    /// have suggested it could push to Cloudflare or Deno, which it cannot.
    #[test]
    fn deploy_takes_no_vendor() {
        assert!(
            Cli::try_parse_from(["deis", "deploy", "--vendor", "cloudflare"]).is_err(),
            "deploy must not accept --vendor"
        );
        assert!(
            Cli::try_parse_from(["deis", "build", "--vendor", "cloudflare"]).is_ok(),
            "build is where a vendor is chosen"
        );
    }

    /// `--json` is offered by the commands that answer with data, and by no
    /// others: a flag accepted and ignored is a flag someone writes a script
    /// against.
    #[test]
    fn json_is_offered_exactly_where_it_is_honoured() {
        for argv in [
            vec!["deis", "versions", "--json"],
            vec!["deis", "status", "--json"],
            vec!["deis", "deploy", "--json"],
            vec!["deis", "rollback", "--json"],
            vec!["deis", "logs", "--json"],
            vec!["deis", "tail", "--json"],
            vec!["deis", "secret", "list", "--json"],
            vec!["deis", "config", "list", "--json"],
            vec!["deis", "kv", "get", "mykey", "--json"],
            vec!["deis", "kv", "list", "--json"],
            vec!["deis", "db", "query", "select 1", "--json"],
            vec!["deis", "blob", "get", "mykey", "--json"],
            vec!["deis", "queue", "dead-letters", "--json"],
            vec!["deis", "queue", "replay", "--queue", "jobs", "m1", "--json"],
        ] {
            Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }

        for argv in [
            vec!["deis", "init", "--json"],
            vec!["deis", "dev", "--json"],
            vec!["deis", "build", "--json"],
            vec!["deis", "validate", "--json"],
        ] {
            assert!(
                Cli::try_parse_from(&argv).is_err(),
                "{argv:?}: this command has no JSON form, so the flag must not be accepted"
            );
        }
    }

    /// `-y`/`--yes` exists exactly on the calls that delete or run arbitrary
    /// SQL, and nowhere else: a skip-confirmation flag on a command with
    /// nothing to confirm is a flag someone will pass out of habit and then
    /// rely on for a command where it used to matter.
    #[test]
    fn yes_is_offered_exactly_where_a_confirmation_exists() {
        for argv in [
            vec!["deis", "kv", "delete", "mykey", "-y"],
            vec!["deis", "db", "execute", "delete from t", "--yes"],
            vec!["deis", "blob", "delete", "mykey", "-y"],
        ] {
            Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }

        for argv in [
            vec!["deis", "kv", "get", "mykey", "-y"],
            vec!["deis", "kv", "put", "mykey", "value", "-y"],
            vec!["deis", "kv", "list", "-y"],
            vec!["deis", "db", "query", "select 1", "-y"],
            vec!["deis", "blob", "get", "mykey", "-y"],
            vec!["deis", "blob", "put", "mykey", "-y"],
            // A replay deletes nothing and overwrites nothing: it moves
            // named messages back onto their queue, and naming them is the
            // deliberate act. What guards it is that there is no way to name
            // them all at once.
            vec!["deis", "queue", "replay", "--queue", "jobs", "m1", "-y"],
        ] {
            assert!(
                Cli::try_parse_from(&argv).is_err(),
                "{argv:?}: this command deletes and changes nothing, so -y must not be accepted"
            );
        }
    }

    /// Every verb `BlobStore` answers has to be reachable from the command
    /// line, or the RPC is delivered to nobody. This is the parse-level half
    /// of that: `scripts/demo.sh` runs the commands against a live store.
    #[test]
    fn blob_offers_every_verb_the_store_answers() {
        for argv in [
            vec!["deis", "blob", "list"],
            vec!["deis", "blob", "list", "uploads/", "--delimiter", "/"],
            vec![
                "deis", "blob", "list", "--limit", "10", "--cursor", "=k", "--all",
            ],
            vec!["deis", "blob", "head", "mykey"],
            vec![
                "deis", "blob", "get", "mykey", "--offset", "10", "--length", "20",
            ],
            vec!["deis", "blob", "get", "mykey", "--suffix", "64"],
            vec!["deis", "blob", "put", "mykey", "--if-none-match", "*"],
            vec!["deis", "blob", "put", "mykey", "--if-match", "abc"],
            vec!["deis", "blob", "delete", "mykey", "-y", "--if-match", "abc"],
        ] {
            assert!(
                Cli::try_parse_from(&argv).is_ok(),
                "{argv:?} must parse: deis-store answers this and nothing else offers it"
            );
        }
    }

    /// The two calls deis-queue answers for an operator have to be reachable
    /// from the command line, or the dead letter table is somewhere work goes
    /// to disappear: the platform stops retrying, says so in one log line,
    /// and nothing afterwards can show what is in there or put it back.
    ///
    /// `ListDeadLetters` and `ReplayDeadLetters` were implemented, registered
    /// and tested with no caller anywhere - this is the parse-level half of
    /// giving them one, and `scripts/demo.sh` runs both against a live queue.
    #[test]
    fn queue_offers_both_calls_deis_queue_answers_for_an_operator() {
        for argv in [
            vec!["deis", "queue", "dead-letters"],
            vec!["deis", "queue", "dead-letters", "--queue", "jobs"],
            vec![
                "deis",
                "queue",
                "dead-letters",
                "--limit",
                "10",
                "--cursor",
                "1700000000:m1",
                "--all",
            ],
            vec!["deis", "queue", "replay", "--queue", "jobs", "m1"],
            vec![
                "deis", "queue", "replay", "--queue", "jobs", "m1", "m2", "--delay", "30",
            ],
        ] {
            assert!(
                Cli::try_parse_from(&argv).is_ok(),
                "{argv:?} must parse: deis-queue answers this and nothing else offers it"
            );
        }
    }

    /// A replay names the messages it means, one by one, all the way down to
    /// deis-queue - which refuses one that names none. Putting a whole queue
    /// of poison messages back cannot be undone, and each of them comes back
    /// with a fresh set of delivery attempts to spend.
    #[test]
    fn a_replay_that_names_no_message_is_refused_at_the_command_line() {
        assert!(
            Cli::try_parse_from(["deis", "queue", "replay", "--queue", "jobs"]).is_err(),
            "a replay must name at least one message id"
        );
        assert!(
            Cli::try_parse_from(["deis", "queue", "replay", "--queue", "jobs", "--all"]).is_err(),
            "there is no --all, and adding one would be a way to replay poison by accident"
        );
    }

    /// The two conditions are mutually exclusive: a write cannot require both
    /// that the etag matches and that it does not, and there is no order in
    /// which checking them both would mean anything.
    #[test]
    fn blob_refuses_both_conditions_at_once() {
        for verb in ["put", "delete"] {
            assert!(
                Cli::try_parse_from([
                    "deis",
                    "blob",
                    verb,
                    "mykey",
                    "-y",
                    "--if-match",
                    "a",
                    "--if-none-match",
                    "b",
                ])
                .is_err(),
                "deis blob {verb} must refuse two preconditions at once"
            );
        }
    }

    /// A KV value comes from exactly one place per call - the CLI must not
    /// let a positional value and `--file` both be given, since only one of
    /// them could ever win and silently picking one would hide the mistake.
    #[test]
    fn kv_put_refuses_a_value_and_a_file_together() {
        assert!(
            Cli::try_parse_from(["deis", "kv", "put", "mykey", "text", "--file", "v.bin"]).is_err(),
            "a value argument and --file must be mutually exclusive"
        );
    }
}
