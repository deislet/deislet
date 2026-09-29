//! SQL database service, one SQLite file per tenant.
//!
//! `<data_dir>/db/<tenant>.sqlite`. Two environments of one application are
//! two tenants, so a preview deployment gets its own file and cannot drop
//! production's tables.
//!
//! The price is that the tenant id becomes a filename, which is why
//! [`TenantId`] refuses anything the wire conventions do not accept as
//! exactly two plain identifiers.
//!
//! # A separate file is not by itself isolation
//!
//! Giving each tenant its own file only isolates tenants if their SQL cannot
//! name a second file, and plain SQLite very much can. `PRAGMA
//! database_list` hands the caller the absolute path of its own database,
//! which is every other tenant's path with one component changed; `ATTACH
//! DATABASE` then opens that path for reading *and writing*; `VACUUM INTO`
//! writes a copy of the tenant's data anywhere the process can write. All
//! three are ordinary SQL, reachable from any application through its
//! database binding.
//!
//! So the file boundary is held by two things underneath the SQL, both
//! installed on every connection of a tenant's pool by
//! [`SqlitePool::new_guarded`]:
//!
//! * **SQLite's authorizer callback** ([`tenant_sql_authorizer`]), which sits
//!   under the parser and is consulted per action rather than per spelling.
//!   It cannot be talked around by comments, case, whitespace or the
//!   eponymous `pragma_*` table forms, because by the time it runs SQLite has
//!   already decided what the statement *does*.
//! * **`SQLITE_LIMIT_ATTACHED` at 0**, which is the floor under it: with no
//!   attach slots, `ATTACH` fails even if an authorizer decision is ever
//!   loosened by mistake. A floor and nothing more - it reports "too many
//!   attached databases", which tells an application nothing and an operator
//!   less, and it stops the statement at run time rather than while it is
//!   prepared. So the authorizer has to recognise every spelling of an
//!   `ATTACH` itself; anything it lets through lands here silently.
//!
//! ## What a tenant's SQL may do
//!
//! Everything ordinary - `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `CREATE`,
//! `DROP`, `ALTER`, triggers, views, indexes, the built-in functions, and the
//! virtual table modules this build compiles in (FTS3, FTS4, FTS5,
//! `fts5vocab`, R*Tree and `dbstat` - none of which can name a file) -
//! against its own database and its own temporary tables. That list is a
//! claim about behaviour, not about a compile flag, so
//! `every_compiled_in_virtual_table_module_is_usable` creates one table with
//! each of them: tightening the PRAGMA allowlist below breaks a test rather
//! than an application. Specifically refused:
//!
//! * **`ATTACH`**, whatever it names, and whether or not it names it in a
//!   way SQLite can show the authorizer: a filename built by an expression
//!   (`'<path>' || ''`, `(SELECT '<path>')`) reaches the callback with the
//!   name missing, and is refused on the action alone. This is also what
//!   stops `VACUUM INTO`: SQLite implements it by attaching the destination,
//!   so the authorizer sees an `Attach` carrying the caller's path.
//! * **`DETACH`**, for symmetry - nothing but the tenant's own file is ever
//!   attached, so a `DETACH` that made sense would mean the layer above had
//!   already failed.
//! * **`VACUUM`, refused a second time in [`check_sql`]** before it reaches
//!   SQLite at all. Both layers are needed and neither replaces the other:
//!   the authorizer sees `VACUUM INTO` only as its `Attach`, and only while
//!   the statement *runs*, so `Prepare` would otherwise hand back a handle
//!   for one and the refusal would surface at whatever executed it later,
//!   saying `ATTACH` about a statement that never spelled the word. The
//!   keyword check names `VACUUM` at the call that sent it, and reads the
//!   first word SQLite would read rather than the first characters it was
//!   given - past comments and empty statements - because that call is the
//!   only place a `Prepare` can be told. It is still one word rather than a
//!   parse tree, which is why it is the message and the authorizer is the
//!   boundary.
//! * **Every `PRAGMA` outside [`TENANT_PRAGMAS`]**, an allowlist rather than
//!   a list of the dangerous ones, plus [`TENANT_READ_ONLY_PRAGMAS`] which
//!   may be read and not set. `database_list` is the disclosure that started
//!   this, but a denylist would age badly: SQLite adds PRAGMAs between
//!   releases and a list written today silently admits whatever arrives
//!   tomorrow.
//! * **`load_extension()`**, which would run native code inside the store.
//!   SQLite already refuses it - the extension loader is off unless
//!   `sqlite3_enable_load_extension` is called, and nothing here calls it,
//!   because rusqlite's `load_extension` feature is not enabled in
//!   `Cargo.toml`. The authorizer refuses it as well so the caller is told
//!   what was refused rather than getting SQLite's bare "not authorized".
//!
//! A refusal reaches the application as [`StoreError::Refused`] naming the
//! statement or PRAGMA, and is logged at `warn` against the tenant: a tenant
//! attempting `ATTACH` is either a broken library or an attack, and an
//! operator wants to see it either way.
//!
//! "Prepared" statements are prepared per call, not held open. A pooled
//! connection cannot own a live `rusqlite::Statement` across requests, so
//! `Prepare` validates the SQL, stores the text under a handle, and every
//! later use re-prepares it on whichever connection it lands on. Callers get
//! the property they actually want from `Prepare` - the SQL is parsed once,
//! up front, and parameters are always bound rather than interpolated.
//!
//! That validation is a real prepare on a real connection, so the authorizer
//! runs and `Prepare` refuses everything above at the call that sent it,
//! naming it. The one action SQLite does not raise while preparing is the
//! `Attach` inside a vacuum, and there is no way to raise it early: it is
//! raised by *running* the vacuum, which is the thing being refused. So the
//! keyword check carries that one, which is why it has to see the same first
//! word SQLite does.
//!
//! # What a statement may touch, and what it may cost
//!
//! Everything above decides *what* a tenant's SQL may reach. None of it
//! decides how much that may cost, and the two are not the same boundary. A
//! `WITH RECURSIVE` counting to a hundred million is ordinary, allowed SQL
//! against the tenant's own file, and it ran 95.7 seconds on one core;
//! killing the client changed nothing, because a statement runs inside
//! `spawn_blocking` and dropping the future awaiting it does not stop it.
//! `SELECT randomblob(20000000)` took the process from 18 MB resident to
//! 1476 MB, because a blob comes back as a JSON array of byte values and the
//! whole result set was built in memory before anything checked its size.
//! One process holds every tenant's KV, SQL and blob data, so either of
//! those denies the node to everybody.
//!
//! So every request carries a budget, and it is the same budget whichever
//! RPC sent it - `Query`, `Execute`, `Batch`, `Prepare` and the four
//! transaction RPCs all go through it:
//!
//! * **[`DEFAULT_STATEMENT_DEADLINE`]** of wall clock, enforced from inside
//!   SQLite by [`crate::sqlite::within_budget`], which also stops a request
//!   whose caller has gone away. Inside a transaction the budget is the
//!   smaller of this and the time the transaction itself has left, because
//!   otherwise a statement outlives the deadline that is supposed to bound
//!   it - a 19.4 second statement ran inside a 5 second transaction.
//! * **[`DEFAULT_MAX_RESULT_ROWS`]** and **[`DEFAULT_MAX_RESULT_BYTES`]**,
//!   charged as the rows are read rather than after they are all in hand, so
//!   a result set that will not fit is refused before the memory for it is
//!   spent.
//!
//! A ceiling nobody can read is not much better than no ceiling, so each one
//! comes back as a sentence naming the number that was crossed and what to
//! change - a `LIMIT`, a narrower `WHERE`, the blob service instead of a
//! column - and rides inside the response like a SQL typo rather than
//! arriving as a transport failure. Two more ceilings sit on the connection
//! itself, on the size of one value and of one statement; they belong to
//! [`crate::sqlite`] because SQLite is the only thing that can enforce them.
//!
//! What this does not do is bound how *often* a tenant may spend a budget.
//! The pool does that instead, and only per tenant: a tenant's SQL cannot
//! occupy more than its own `tenant_connections` blocking threads, however
//! many requests it sends.
//!
//! # What a value looks like on the wire
//!
//! Parameters arrive as a JSON array and rows go back as JSON objects. That
//! JSON is not an internal encoding: it is the text an application's
//! `JSON.stringify` wrote and the text its `JSON.parse` reads, so every value
//! in it has to be something JavaScript can hold. It also has to be something
//! this service will take back, because read a row, change a field, write it
//! back is the ordinary way an application uses a database, and a form that
//! only travels outwards makes that impossible for every table that has such
//! a column in it.
//!
//! NULL, REAL and TEXT need no decision - `null`, a number and a string say
//! them exactly. Two cases do, and both are decided here, with the same
//! spelling read and written:
//!
//! * **A BLOB is an array of byte values.** `X'DEADBEEF'` comes back as
//!   `[222, 173, 190, 239]`, and an array of whole numbers 0 to 255 binds as
//!   a BLOB. A JSON string could not do it: a string is already TEXT, and
//!   reading base64 out of one would turn a tenant's ordinary text into bytes
//!   the moment it happened to look like base64.
//! * **An INTEGER past [`MAX_SAFE_JSON_INTEGER`] is a decimal string.**
//!   9007199254740993 comes back as `"9007199254740993"`, and a string
//!   spelled exactly the way this service would spell such an integer binds
//!   as INTEGER. A JSON number cannot carry it: JavaScript has one numeric
//!   type and it is a double, so `JSON.parse` turns 9007199254740993 into
//!   9007199254740992, and the application writes that different number back
//!   with nothing anywhere reporting an error. Integers inside the safe range
//!   stay JSON numbers, so an id, a count and a millisecond timestamp are
//!   what they have always been.
//!
//! What this still does not preserve, because JSON has no way to say it and
//! SQLite has no type for it either:
//!
//! * **A boolean** binds as INTEGER 1 or 0 and reads back as 1 or 0. SQLite
//!   has no boolean type to remember which it was.
//! * **A date** is whatever the application turned it into - `JSON.stringify`
//!   renders a `Date` as an ISO-8601 string, so it is stored and returned as
//!   TEXT and has to be parsed again on arrival.
//! * **A REAL with no fractional part** goes out as `2.0` and comes back
//!   through JavaScript as `2`, which binds as INTEGER; a column with REAL
//!   affinity stores it as `2.0` again, one with no affinity keeps the
//!   integer.
//! * **NaN and the infinities** have no JSON spelling at all and are returned
//!   as `null`. A bound parameter cannot produce one - they only come out of
//!   arithmetic inside the statement.
//! * **TEXT that is not valid UTF-8** is returned as an array of its bytes,
//!   which is the BLOB spelling, so writing it back stores a BLOB. The bytes
//!   survive; the storage class does not. Only SQL that bound a BLOB to a
//!   TEXT column, or built one with `CAST`, can put such a value there.
//!
//! # Interactive transactions
//!
//! `Batch` runs several statements atomically, but the caller has to know all
//! of them before the first one runs. That is not what a transaction is for.
//! Reading a row, deciding from what it says, and writing accordingly is the
//! whole point, and it needs the transaction to stay open across a round trip
//! to whoever is deciding. A distributed SQL service cannot offer that. One
//! SQLite file per tenant on one box can, so this one does:
//! `BeginTransaction` pins a connection, `ExecuteInTransaction` and
//! `QueryInTransaction` run on it, `CommitTransaction` or
//! `RollbackTransaction` end it and give the connection back.
//!
//! Four things make it safe rather than a foot-gun, and none of them is
//! optional:
//!
//! * **`BEGIN IMMEDIATE`, never deferred.** A deferred transaction that reads
//!   and then writes takes a read snapshot first and fails the write with
//!   `SQLITE_BUSY_SNAPSHOT` - at *commit* time, after the caller has been
//!   told every statement succeeded. Read-then-write is exactly the workload
//!   this exists for, so the write lock is taken up front, where contention
//!   is still something the caller can be told about honestly.
//! * **The tenant is checked on every use**, as it is for prepared
//!   statements: a handle that leaks out of one tenant opens nothing in
//!   another.
//! * **A hard server-side deadline.** The connection is pinned for the whole
//!   life of the transaction, so a handler that opens one and then awaits
//!   something slow is sitting on its tenant's writer. At the deadline the
//!   server rolls the transaction back itself, releases the connection, and
//!   remembers why: the next call on that handle is told its work was rolled
//!   back for exceeding the deadline, rather than that the handle is unknown.
//! * **A bound on how many may be open at once**, per tenant and in total.
//!   The per-tenant bound is additionally clamped to one below the tenant's
//!   pool size, so a client looping on `BeginTransaction` can never take the
//!   last connection its own ordinary queries need. That clamp is only
//!   meaningful if the pool has a connection to spare, which is why a
//!   tenant's pool is never opened with fewer than
//!   [`MIN_TENANT_CONNECTIONS`]: with one connection, "one below the pool
//!   size" is none at all, and a single abandoned transaction would starve
//!   every plain `Execute` and `Query` until the deadline reaped it.
//!
//! SQLite still allows one writer per file. A second interactive transaction
//! on the same tenant therefore waits at `BEGIN IMMEDIATE` for the first to
//! finish, and gives up with "database is locked" if it waits out the pool's
//! busy timeout. That is inherent, and it is why the per-tenant bound is
//! small: queueing writers behind a writer does not make them any faster.

use crate::MAX_GRPC_MESSAGE_BYTES;
use crate::error::{StoreError, StoreResult};
use crate::proto::storage::{
    BatchRequest, BatchResponse, BeginTransactionRequest, BeginTransactionResponse,
    CommitTransactionRequest, CommitTransactionResponse, ExecuteInTransactionRequest,
    ExecutePreparedRequest, ExecuteRequest, ExecuteResponse, PrepareRequest, PrepareResponse,
    QueryInTransactionRequest, QueryPreparedRequest, QueryRequest, QueryResponse,
    RollbackTransactionRequest, RollbackTransactionResponse, database_server::Database,
};
use crate::sqlite::{self, Durability, PoolSize, PooledConnection, SqlitePool};
use crate::tenant::{TenantId, tenant_of};
use rusqlite::ffi;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::{Value as SqlValue, ValueRef};
use serde_json::{Map as JsonMap, Value as JsonValue};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tonic::{Request, Response, Status};

/// The PRAGMAs a tenant's SQL may run, in either the `PRAGMA x` or the
/// `SELECT * FROM pragma_x` spelling - SQLite reports both to the authorizer
/// as the same `Pragma` action, so one list covers both.
///
/// An allowlist, and short on purpose. What is here is what an application
/// legitimately needs against its own file: reading back the schema it
/// created, checking its own foreign keys, and the two counters SQLite
/// reserves for an application's own use. What is not here divides into two
/// kinds. Some of it discloses the store's layout - `database_list` gives the
/// absolute path of the tenant's file, which is every other tenant's path
/// with one component changed, and `compile_options` describes the build.
/// The rest - journal mode, cache size, checkpoints, `foreign_keys` - are the
/// pool's settings, applied once when a connection is opened; a tenant that
/// changed one would be changing it on a pooled connection that outlives the
/// request, for whichever of its own later requests happened to land there.
const TENANT_PRAGMAS: &[&str] = &[
    "application_id",
    "collation_list",
    "foreign_key_check",
    "foreign_key_list",
    "function_list",
    "index_info",
    "index_list",
    "index_xinfo",
    "table_info",
    "table_list",
    "table_xinfo",
    "user_version",
];

/// PRAGMAs a tenant may read but not set.
///
/// Neither is here because an application asked for it. Both are here
/// because the virtual table modules the docs above promise read them from
/// inside their own implementations, so refusing them does not narrow what a
/// tenant may write - it breaks a module:
///
/// * **`page_size`** is read by R*Tree's `xCreate` to size its index nodes,
///   and refusing it fails `CREATE VIRTUAL TABLE ... USING rtree(...)`
///   outright. FTS3 and FTS4 read it too, at create; they treat a refusal as
///   1024 and carry on, so they worked either way and sized their segment
///   nodes against a guess.
/// * **`data_version`** is read by FTS5 on the first write through a
///   connection, to notice a commit made by another one. Refusing it fails
///   the `INSERT`, which is worse than the R*Tree case: the table is created
///   happily and only breaks when the application starts using it.
///
/// Reading either discloses nothing. Both are properties of the caller's own
/// file, say nothing about any other tenant or about where the file lives,
/// and `page_size` is already legible through `dbstat`'s `pgsize` column,
/// which is allowed. Setting is another matter and stays refused:
/// `PRAGMA page_size = N` rewrites the physical layout of a file that
/// outlives the request, which belongs with the pool's own settings above
/// rather than with what one statement may do. (`data_version` is read-only
/// to SQLite as well, so its write form is refused twice.)
///
/// Split from [`TENANT_PRAGMAS`] rather than folded into it because the
/// read/write distinction is only sound for names with no argument form.
/// `PRAGMA table_info('people')` reaches the authorizer carrying `'people'`
/// as its value, exactly as `PRAGMA user_version = 7` carries `7`, so
/// "has a value" means "is a write" only for the names listed here.
const TENANT_READ_ONLY_PRAGMAS: &[&str] = &["data_version", "page_size"];

/// What a tenant's SQL may do, decided under the parser.
///
/// Installed on every connection of every tenant pool; see the module docs
/// for the reasoning and [`crate::sqlite`] for the mechanism. Refusals leave
/// a sentence behind through [`sqlite::refuse`], because SQLite's own report
/// for a denied statement is the bare string "not authorized".
///
/// Called once per action while a statement is prepared, so it is on the hot
/// path of every query: matching and one short list scan, nothing allocated
/// unless something is actually refused.
fn tenant_sql_authorizer(ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        // `VACUUM INTO '<path>'` arrives here too, as an attach of the file
        // it was asked to write: SQLite implements the copy by attaching the
        // destination. That is the whole of the "write a file the caller
        // names" hole, and this is where it closes.
        AuthAction::Attach { filename } => refuse_attach(Some(filename)),
        // The same ATTACH, spelled so that SQLite cannot hand the filename
        // over: it passes the authorizer a null argument whenever the
        // filename is an expression rather than a string literal - `ATTACH
        // ('<path>' || '')`, `ATTACH (SELECT '<path>')` - and rusqlite has no
        // `Attach` variant to put a missing filename in, so the raw action
        // code arrives here instead. Without this arm those two spellings
        // fall through to the catch-all, are allowed, and are then stopped by
        // `SQLITE_LIMIT_ATTACHED` at run time - which refuses them as "too
        // many attached databases", says nothing about why, and leaves no
        // line behind for an operator watching for exactly this.
        AuthAction::Unknown {
            code: ffi::SQLITE_ATTACH,
            ..
        } => refuse_attach(None),
        AuthAction::Detach { database_name } => refuse_detach(Some(database_name)),
        AuthAction::Unknown {
            code: ffi::SQLITE_DETACH,
            ..
        } => refuse_detach(None),
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } if !is_tenant_pragma(pragma_name, pragma_value) => sqlite::refuse(format!(
            "PRAGMA {pragma_name}, because a tenant may run only {}, and may only read {}",
            TENANT_PRAGMAS.join(", "),
            TENANT_READ_ONLY_PRAGMAS.join(", ")
        )),
        // Already impossible - the extension loader is off unless
        // `sqlite3_enable_load_extension` is called and nothing here calls
        // it, because rusqlite's `load_extension` feature is not enabled in
        // this crate's Cargo.toml. Refused here as well so the caller is told
        // what was refused instead of "not authorized".
        AuthAction::Function { function_name }
            if function_name.eq_ignore_ascii_case("load_extension") =>
        {
            sqlite::refuse(
                "load_extension(), because it would load a shared library and run native \
                 code inside the store"
                    .to_string(),
            )
        }
        // Everything else is ordinary SQL against the tenant's own file. The
        // one thing checked on the way through is which database it names:
        // unreachable while ATTACH is refused, and the point of writing it
        // down is that it stays refused if ATTACH ever is not.
        _ => match ctx.database_name {
            Some(name) if name != "main" && name != "temp" => sqlite::refuse(format!(
                "reading or writing the database {name:?}, because a tenant's SQL reaches \
                 its own file and its own temporary tables, and nothing else"
            )),
            _ => Authorization::Allow,
        },
    }
}

/// Log a refusal against the tenant whose SQL caused it.
///
/// `warn`, not `debug`: a tenant reaching for `ATTACH` or `PRAGMA
/// database_list` is either a library doing something nobody meant it to or
/// somebody trying the other tenants' files, and an operator wants to see it
/// either way. Everything else passes through untouched - a SQL typo is not
/// an operator's problem.
fn note_refusal(tenant: &TenantId, error: &StoreError) {
    if let StoreError::Refused(what) = error {
        tracing::warn!(
            tenant = %tenant,
            refused = %what,
            "refused a statement from this tenant; nothing outside this tenant's own database \
             file was read or written"
        );
    }
}

/// Log a request stopped at one of its cost ceilings, against the tenant that
/// sent it.
///
/// `warn`, and for the same reason [`note_refusal`] is: the store had to stop
/// work a tenant asked for, which is either an application that has outgrown
/// what one request may do or one query away from denying this node to
/// everybody on it, and an operator wants the line either way. Refusals still
/// go through `note_refusal`, which this calls, so one `inspect_err` on a
/// path covers both.
fn note_cost(tenant: &TenantId, error: &StoreError) {
    match error {
        StoreError::Expired(what) => tracing::warn!(
            tenant = %tenant,
            reason = %what,
            "stopped a request from this tenant part-way; nothing it had written was kept, and \
             the connection and blocking thread it held are back"
        ),
        StoreError::TooLarge(what) => tracing::warn!(
            tenant = %tenant,
            reason = %what,
            "stopped building a result set for this tenant; the rows already read are discarded \
             rather than sent, so no memory is held for an answer that could not be delivered"
        ),
        other => note_refusal(tenant, other),
    }
}

/// Refuse an `ATTACH`, whether or not SQLite could say what it named.
///
/// `None` is the expression spelling: SQLite only passes the filename to the
/// authorizer when it is a string literal, so a caller that hides it behind
/// `||` or a subquery is refused on the action alone. Naming the spelling in
/// the message keeps the log line honest about what the operator can and
/// cannot see.
fn refuse_attach(filename: Option<&str>) -> Authorization {
    let named = match filename {
        Some(filename) => format!("this statement named {filename:?}"),
        None => "this statement built the filename from an expression, so SQLite did not pass \
                 it to the authorizer"
            .to_string(),
    };
    sqlite::refuse(format!(
        "ATTACH, because a tenant's SQL reaches its own database file and nothing else; \
         {named}. VACUUM arrives here too, because SQLite attaches the file it writes"
    ))
}

/// Refuse a `DETACH`, whether or not SQLite could say what it named. See
/// [`refuse_attach`] for why the name can be missing.
fn refuse_detach(database_name: Option<&str>) -> Authorization {
    let named = match database_name {
        Some(name) => format!("{name:?} is not a database this connection has open"),
        None => "no database this connection has open could be the one it means".to_string(),
    };
    sqlite::refuse(format!(
        "DETACH, because nothing but this tenant's own file is ever attached, so {named}"
    ))
}

/// Whether this PRAGMA, in this form, is one a tenant may run.
///
/// Case-insensitive, because SQLite hands the authorizer the PRAGMA name as
/// the caller spelled it. `value` is what follows `=` or sits in the
/// parentheses, and it is only consulted for [`TENANT_READ_ONLY_PRAGMAS`],
/// where its absence is what makes the statement a read.
fn is_tenant_pragma(name: &str, value: Option<&str>) -> bool {
    if TENANT_PRAGMAS
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
    {
        return true;
    }
    value.is_none()
        && TENANT_READ_ONLY_PRAGMAS
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(name))
}

/// How many statements the whole process will remember at once. Handles are
/// small (an id and the SQL text) but a client that prepares in a loop and
/// never stops would otherwise grow the map without bound.
const MAX_PREPARED_STATEMENTS: usize = 4096;

/// One remembered statement.
#[derive(Clone, Debug)]
struct PreparedStatement {
    /// Whoever prepared it. Checked on every use, so a leaked handle is
    /// still useless to another tenant.
    tenant: TenantId,
    sql: String,
    /// Monotonic sequence number, used to evict the oldest handle first.
    seq: u64,
}

/// Handles for statements prepared by `Prepare`.
#[derive(Default)]
struct StatementRegistry {
    entries: Mutex<HashMap<String, PreparedStatement>>,
    next_seq: Mutex<u64>,
}

impl StatementRegistry {
    fn insert(&self, tenant: TenantId, sql: String) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let seq = {
            let mut next = self.next_seq.lock().unwrap_or_else(|e| e.into_inner());
            *next += 1;
            *next
        };
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() >= MAX_PREPARED_STATEMENTS
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, s)| s.seq)
                .map(|(id, _)| id.clone())
        {
            entries.remove(&oldest);
            tracing::warn!(
                capacity = MAX_PREPARED_STATEMENTS,
                evicted = %oldest,
                "prepared statement table is full, dropped the oldest handle; a client still \
                 holding it will get an unknown-statement error and has to prepare again"
            );
        }
        entries.insert(id.clone(), PreparedStatement { tenant, sql, seq });
        id
    }

    fn resolve(&self, tenant: &TenantId, id: &str) -> StoreResult<String> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let stmt = entries.get(id).ok_or_else(|| {
            tracing::debug!(
                statement = %id,
                tenant = %tenant,
                "unknown prepared statement handle; it was never prepared, or it was evicted"
            );
            StoreError::NotFound(format!("prepared statement {id} is unknown"))
        })?;
        if &stmt.tenant != tenant {
            tracing::warn!(
                statement = %id,
                owner = %stmt.tenant,
                caller = %tenant,
                "refused a prepared statement belonging to another tenant; the call touched no data"
            );
            return Err(StoreError::Tenant(format!(
                "prepared statement {id} belongs to another tenant"
            )));
        }
        Ok(stmt.sql.clone())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

/// How long one request may spend running SQL before the store stops it.
///
/// Five seconds, and the number is taken from three places that already
/// agree. It is the pool's `BUSY_TIMEOUT`, so a statement that has run
/// longer than SQLite is willing to *wait* for a lock is past the point
/// where the rest of the tenant's traffic is being served normally. It is
/// [`DEFAULT_TRANSACTION_DEADLINE`], so a statement can no longer outlive
/// the transaction that is meant to bound it. And it is well inside the
/// thirty seconds the runtime gives a gRPC call, so the store answers with a
/// sentence the application can read instead of the caller's transport
/// giving up on a connection that is still busy.
///
/// It is a ceiling on the request, not on one statement inside it: a batch
/// holds the same connection and the same blocking thread for its twentieth
/// `INSERT` that it held for its first, and that is the resource being
/// bounded.
pub const DEFAULT_STATEMENT_DEADLINE: Duration = Duration::from_secs(5);

/// How many rows one request may return.
///
/// Ten thousand, which is exactly the page ceiling [`crate::kv`] already
/// puts on a listing: it is this store's existing idea of how much one
/// request carries, and a SQL result has no claim to be larger than a KV
/// page.
///
/// This is the cap that actually bounds memory, which is why it exists
/// beside the byte cap rather than being folded into it. A row is held as a
/// `serde_json` object before it is rendered, and measured, a hundred
/// thousand single-column rows cost 67 MB resident while rendering to 1.5 MB
/// of JSON - a forty-six-fold amplification that a byte count taken on the
/// data itself cannot see.
pub const DEFAULT_MAX_RESULT_ROWS: usize = 10_000;

/// How many bytes of column data one request may return.
///
/// A quarter of [`MAX_GRPC_MESSAGE_BYTES`], so four megabytes. The rows go
/// back as one JSON string in one gRPC message capped at sixteen, and a blob
/// column renders as a JSON array of byte values - up to four characters per
/// byte. A quarter is therefore the largest figure at which a result made
/// entirely of blobs still fits in the message that has to carry it.
///
/// It counts the bytes SQLite hands over, before any of them are turned into
/// JSON: charged that way the ceiling means the same thing whatever the rows
/// contain, and it is charged before the memory is spent rather than after.
pub const DEFAULT_MAX_RESULT_BYTES: usize = MAX_GRPC_MESSAGE_BYTES / 4;

/// What one request is allowed to cost, whatever SQL it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlLimits {
    /// Wall clock for the whole request; see [`DEFAULT_STATEMENT_DEADLINE`].
    pub statement_deadline: Duration,
    /// Rows one request may return; see [`DEFAULT_MAX_RESULT_ROWS`].
    pub max_result_rows: usize,
    /// Bytes of column data one request may return; see
    /// [`DEFAULT_MAX_RESULT_BYTES`].
    pub max_result_bytes: usize,
}

impl Default for SqlLimits {
    fn default() -> Self {
        Self {
            statement_deadline: DEFAULT_STATEMENT_DEADLINE,
            max_result_rows: DEFAULT_MAX_RESULT_ROWS,
            max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
        }
    }
}

impl SqlLimits {
    /// The refusal a caller gets for asking for more rows than it may have.
    fn too_many_rows(&self) -> StoreError {
        StoreError::TooLarge(format!(
            "this query returned more than the {} rows one request may carry, so it was stopped \
             before the rest were read; add a LIMIT and page through with OFFSET, or narrow the \
             WHERE clause",
            self.max_result_rows
        ))
    }

    /// The refusal for asking for more data than one response may carry.
    fn too_many_bytes(&self) -> StoreError {
        StoreError::TooLarge(format!(
            "this query returned more than the {} bytes of column data one request may carry, so \
             it was stopped before the rest were read; select fewer columns or fewer rows, and \
             keep payloads this size in the blob service rather than in a column",
            self.max_result_bytes
        ))
    }
}

/// How much of a result set's byte budget one cell costs: the bytes SQLite
/// hands over, before any of them are turned into JSON.
///
/// Counting the source rather than the rendering is deliberate. It is the
/// same number whatever formatter the response ends up going through, it is
/// known before the memory is spent rather than after, and it does not
/// depend on how `serde_json` spells a float or which characters it escapes.
/// What it does not do is promise the rendered response fits in one gRPC
/// message, which is why [`DEFAULT_MAX_RESULT_BYTES`] is a quarter of that
/// message rather than all of it.
fn column_bytes(value: ValueRef<'_>) -> usize {
    match value {
        ValueRef::Null => 0,
        // The widest either can render to, near enough: what this is
        // bounding is the data, and a number is not where a result set gets
        // large.
        ValueRef::Integer(_) | ValueRef::Real(_) => 8,
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => bytes.len(),
    }
}

/// Read every row a prepared statement returns into JSON, refusing to build
/// a result set larger than one request may carry.
///
/// Both ceilings are checked before the row or the cell is added, so the
/// memory for the one that crossed them is never spent. The one place this
/// lives is shared by the plain and the in-transaction query paths: two
/// copies of a loop like this would be two chances for one of them to have
/// no ceiling at all.
fn collect_rows(
    stmt: &mut rusqlite::Statement<'_>,
    params: Vec<SqlValue>,
    limits: SqlLimits,
) -> StoreResult<ResultSet> {
    let columns: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
    let mut out = Vec::new();
    let mut bytes = 0usize;
    // Cells that had to leave as decimal strings because a JSON number would
    // have lost their low digits. Counted rather than logged one by one: a
    // result set may hold ten thousand of them, and what an operator needs is
    // to know this result set is one an application will see strings in.
    let mut wide_integers = 0usize;
    while let Some(row) = rows.next()? {
        if out.len() >= limits.max_result_rows {
            return Err(limits.too_many_rows());
        }
        let mut object = JsonMap::with_capacity(columns.len());
        for (index, name) in columns.iter().enumerate() {
            let value = row.get_ref(index)?;
            bytes = bytes.saturating_add(column_bytes(value));
            if bytes > limits.max_result_bytes {
                return Err(limits.too_many_bytes());
            }
            if matches!(value, ValueRef::Integer(i) if !fits_json_number(i)) {
                wide_integers += 1;
            }
            object.insert(name.clone(), sql_to_json(value));
        }
        out.push(JsonValue::Object(object));
    }
    if wide_integers > 0 {
        tracing::debug!(
            cells = wide_integers,
            safe_ceiling = MAX_SAFE_JSON_INTEGER,
            "this result set carries integers past 2^53 - 1, which go out as decimal strings; a \
             JSON number would reach the application as a different number"
        );
    }
    Ok(ResultSet {
        columns,
        rows: JsonValue::Array(out),
    })
}

/// How long an interactive transaction may stay open before the server rolls
/// it back on its own.
///
/// Small on purpose. The transaction is holding one of its tenant's few
/// connections and, because it began `IMMEDIATE`, that file's write lock; a
/// handler that means to think for longer than this between statements wants
/// two transactions, not a longer one. It is also the same order as the
/// pool's busy timeout, so a second writer waiting at `BEGIN` outlives the
/// first one's deadline rather than the other way round.
pub const DEFAULT_TRANSACTION_DEADLINE: Duration = Duration::from_secs(5);

/// How many transactions one tenant may hold open at once, before the clamp
/// to its pool size in [`clamp_to_pool`].
///
/// Two, because SQLite gives a file one writer: the second is already only
/// waiting for the first, and a third would be waiting for a wait.
pub const DEFAULT_MAX_OPEN_TRANSACTIONS_PER_TENANT: usize = 2;

/// How many transactions the whole process may hold open at once, across
/// every tenant. Each one is a pinned connection and an open file, so this is
/// the ceiling on what a crowd of tenants can pin between them.
pub const DEFAULT_MAX_OPEN_TRANSACTIONS: usize = 64;

/// The smallest pool a tenant may be given, whatever the configuration says.
///
/// An interactive transaction pins one connection for its whole life, so a
/// pool of one has nothing left to run the tenant's ordinary `Execute` and
/// `Query` on while a transaction is open - and "abandon a transaction and
/// every plain query blocks until the deadline reaps it" is a foot-gun an
/// operator can arm by typing `tenant_connections = 1`. Two is the smallest
/// number for which [`clamp_to_pool`] can do what it claims.
pub const MIN_TENANT_CONNECTIONS: usize = 2;

/// How many finished handles are remembered, so a caller that uses one gets
/// "committed" or "rolled back at the deadline" instead of "unknown".
const REMEMBERED_ENDED_TRANSACTIONS: usize = 1024;

/// What an interactive transaction is allowed to cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionLimits {
    /// Ceiling on how long one may stay open. A caller may ask for less; it
    /// cannot ask for more.
    pub deadline: Duration,
    /// How many one tenant may hold open at once.
    pub max_open_per_tenant: usize,
    /// How many the process may hold open at once, over all tenants.
    pub max_open_total: usize,
}

impl Default for TransactionLimits {
    fn default() -> Self {
        Self {
            deadline: DEFAULT_TRANSACTION_DEADLINE,
            max_open_per_tenant: DEFAULT_MAX_OPEN_TRANSACTIONS_PER_TENANT,
            max_open_total: DEFAULT_MAX_OPEN_TRANSACTIONS,
        }
    }
}

impl TransactionLimits {
    /// How long this transaction actually gets: what the caller asked for,
    /// never more than the ceiling.
    fn grant(&self, requested: Option<Duration>) -> StoreResult<Duration> {
        let Some(requested) = requested else {
            return Ok(self.deadline);
        };
        if requested.is_zero() {
            return Err(StoreError::InvalidArgument(
                "a transaction timeout must be positive; one that has already expired cannot be \
                 opened"
                    .to_string(),
            ));
        }
        Ok(requested.min(self.deadline))
    }
}

/// Hold the per-tenant ceiling one below the tenant's pool size.
///
/// Transactions pin their connection for their whole life, so a ceiling equal
/// to the pool size would let a client that loops on `BeginTransaction` pin
/// every connection and then block its own ordinary queries behind its own
/// transactions. Leaving one back means the plain `Execute`/`Query` path
/// always has somewhere to run.
///
/// One below a pool of one is zero, and this returns exactly that rather than
/// rounding up to one: with a single connection there is no ceiling at which
/// the sentence above is true, so the honest answer is that no transaction
/// may be opened. [`DatabaseService::open`] makes that unreachable by
/// refusing to build a pool smaller than [`MIN_TENANT_CONNECTIONS`], which is
/// where an operator gets told, rather than here, where nobody would see it.
fn clamp_to_pool(limits: TransactionLimits, connections_per_tenant: usize) -> TransactionLimits {
    TransactionLimits {
        max_open_per_tenant: limits
            .max_open_per_tenant
            .min(connections_per_tenant.saturating_sub(1)),
        max_open_total: limits.max_open_total.max(1),
        ..limits
    }
}

/// One open interactive transaction.
struct TransactionEntry {
    /// Whoever opened it. Checked on every use, so a leaked handle is still
    /// useless to another tenant.
    tenant: TenantId,
    /// The pinned connection, `None` once whoever ended the transaction has
    /// taken it back out. Guarded by a std mutex that is only ever locked
    /// inside a blocking task: two statements on one handle are one session
    /// and must run one after the other.
    conn: Mutex<Option<PooledConnection>>,
    /// The task that will roll this back at the deadline, cancelled when the
    /// transaction ends first.
    deadline_task: Mutex<Option<JoinHandle<()>>>,
    /// What the caller was granted, quoted back when the deadline fires.
    granted: Duration,
    /// The instant the deadline task will roll this back. Every statement on
    /// the connection is held to what is left of it, so a statement cannot
    /// outlive the transaction it is running in: before this existed a 19.4
    /// second statement ran inside a 5 second transaction, because the
    /// deadline task could not preempt work already under way.
    ///
    /// Behind a mutex because the instant that counts is only known once
    /// `BEGIN IMMEDIATE` has returned - time spent waiting for the write
    /// lock is not the caller's - and by then the entry is already shared.
    expires_at: Mutex<Instant>,
}

impl TransactionEntry {
    /// Start the clock the statements on this connection are held to.
    fn deadline_starts_now(&self, granted: Duration) {
        *self.expires_at.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now() + granted;
    }

    /// How long a statement on this connection may run: what the request
    /// budget allows, or what the transaction has left, whichever is less.
    ///
    /// `None` means the transaction is already past its deadline and the task
    /// that reaps it has simply not got there yet - there is no time to give.
    fn budget_for(&self, request: Duration) -> Option<Duration> {
        let expires_at = *self.expires_at.lock().unwrap_or_else(|e| e.into_inner());
        let left = expires_at.checked_duration_since(Instant::now())?;
        if left.is_zero() {
            return None;
        }
        Some(left.min(request))
    }

    /// Stop the deadline task; the transaction has ended some other way.
    fn cancel_deadline(&self) {
        if let Some(task) = self
            .deadline_task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            task.abort();
        }
    }
}

/// How a transaction ended, remembered so its handle can answer for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    Committed,
    RolledBack,
    DeadlineExceeded,
    /// A statement inside it was stopped part-way - at the request budget,
    /// or because the caller went away. The transaction goes with it; see
    /// [`DatabaseService::end_after_a_stopped_statement`].
    StatementStopped,
}

/// The tombstone left behind by a finished transaction.
#[derive(Debug, Clone)]
struct EndedTransaction {
    tenant: TenantId,
    how: Ended,
    granted: Duration,
}

impl EndedTransaction {
    /// What a caller that still holds this handle is told.
    fn as_error(&self, id: &str) -> StoreError {
        match self.how {
            // The one message that has to be specific: the caller did not ask
            // for this and its work is gone, so "unknown handle" would send
            // it looking for a bug it does not have.
            Ended::DeadlineExceeded => StoreError::Expired(format!(
                "transaction {id} was rolled back because it exceeded its {} ms deadline; nothing \
                 it wrote was applied. Keep the work between BeginTransaction and \
                 CommitTransaction short, or ask for a longer timeout when opening it",
                self.granted.as_millis()
            )),
            // Also specific, and for the same reason: the caller was told
            // its statement was stopped, and it needs to know that took the
            // transaction with it rather than leaving it open to retry in.
            Ended::StatementStopped => StoreError::Expired(format!(
                "transaction {id} was rolled back because a statement inside it had to be \
                 stopped part-way; nothing it wrote was applied. SQLite rolls an explicit \
                 transaction back when it stops a write inside one, so the handle is closed \
                 rather than left half-open. Open a new transaction and send cheaper statements"
            )),
            Ended::Committed => {
                StoreError::NotFound(format!("transaction {id} was already committed"))
            }
            Ended::RolledBack => {
                StoreError::NotFound(format!("transaction {id} was already rolled back"))
            }
        }
    }
}

/// Everything the process knows about interactive transactions: which are
/// open, and why the recently closed ones closed.
#[derive(Default)]
struct TransactionRegistry {
    state: Mutex<RegistryState>,
}

#[derive(Default)]
struct RegistryState {
    open: HashMap<String, Arc<TransactionEntry>>,
    ended: HashMap<String, EndedTransaction>,
    /// Insertion order of `ended`, so the oldest tombstone is the one that
    /// goes when the table is full.
    ended_order: VecDeque<String>,
}

impl RegistryState {
    fn remember_ended(&mut self, id: String, ended: EndedTransaction) {
        if self.ended.insert(id.clone(), ended).is_none() {
            self.ended_order.push_back(id);
        }
        while self.ended_order.len() > REMEMBERED_ENDED_TRANSACTIONS
            && let Some(oldest) = self.ended_order.pop_front()
        {
            self.ended.remove(&oldest);
        }
    }

    fn open_for(&self, tenant: &TenantId) -> usize {
        self.open.values().filter(|e| &e.tenant == tenant).count()
    }
}

impl TransactionRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take a slot for `tenant`, or say which ceiling is in the way.
    ///
    /// The entry is inserted before its connection exists, so the slot is
    /// held for the whole of `BEGIN IMMEDIATE` - which is exactly when a
    /// client looping on begin would otherwise slip past the count.
    fn admit(
        &self,
        tenant: &TenantId,
        granted: Duration,
        limits: &TransactionLimits,
    ) -> StoreResult<(String, Arc<TransactionEntry>)> {
        let mut state = self.lock();
        if state.open.len() >= limits.max_open_total {
            tracing::warn!(
                tenant = %tenant,
                open = state.open.len(),
                ceiling = limits.max_open_total,
                "refused to open a transaction: the server is already holding as many as it \
                 allows, and each one pins a connection"
            );
            return Err(StoreError::Exhausted(format!(
                "this server already holds {} open transactions, which is its ceiling; commit or \
                 roll one back, or retry in a moment",
                state.open.len()
            )));
        }
        let mine = state.open_for(tenant);
        if mine >= limits.max_open_per_tenant {
            tracing::warn!(
                tenant = %tenant,
                open = mine,
                ceiling = limits.max_open_per_tenant,
                "refused to open a transaction: this tenant is already holding as many as it may, \
                 and more would only queue behind them for the one write lock its file has"
            );
            return Err(StoreError::Exhausted(format!(
                "this tenant already holds {mine} open transactions, which is its ceiling; commit \
                 or roll one back before opening another"
            )));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let entry = Arc::new(TransactionEntry {
            tenant: tenant.clone(),
            conn: Mutex::new(None),
            deadline_task: Mutex::new(None),
            granted,
            // Provisional; `open_transaction` restarts it once `BEGIN
            // IMMEDIATE` has returned. This value only covers the window
            // before that, where nothing runs statements on the entry anyway.
            expires_at: Mutex::new(Instant::now() + granted),
        });
        state.open.insert(id.clone(), Arc::clone(&entry));
        Ok((id, entry))
    }

    /// Give a slot back for a transaction that never opened. No tombstone:
    /// the id was never handed out, so nobody can ask about it.
    fn abandon(&self, id: &str) {
        self.lock().open.remove(id);
    }

    /// The entry behind `id`, if this tenant owns it and it is still open.
    fn resolve(&self, tenant: &TenantId, id: &str) -> StoreResult<Arc<TransactionEntry>> {
        let state = self.lock();
        if let Some(entry) = state.open.get(id) {
            if &entry.tenant != tenant {
                tracing::warn!(
                    transaction = %id,
                    owner = %entry.tenant,
                    caller = %tenant,
                    "refused a transaction handle belonging to another tenant; the call touched \
                     no data"
                );
                return Err(StoreError::Tenant(format!(
                    "transaction {id} belongs to another tenant"
                )));
            }
            return Ok(Arc::clone(entry));
        }
        if let Some(ended) = state.ended.get(id) {
            if &ended.tenant != tenant {
                tracing::warn!(
                    transaction = %id,
                    owner = %ended.tenant,
                    caller = %tenant,
                    "refused a finished transaction handle belonging to another tenant"
                );
                return Err(StoreError::Tenant(format!(
                    "transaction {id} belongs to another tenant"
                )));
            }
            return Err(ended.as_error(id));
        }
        Err(StoreError::NotFound(format!(
            "transaction {id} is unknown; it was never opened, or it ended so long ago that the \
             server no longer remembers it"
        )))
    }

    /// Claim the right to end `id` on behalf of `tenant`.
    ///
    /// Removal from `open` is the arbiter: commit, rollback and the deadline
    /// all go through here, and exactly one of them can win.
    fn claim(&self, tenant: &TenantId, id: &str, how: Ended) -> StoreResult<Arc<TransactionEntry>> {
        let mut state = self.lock();
        let Some(entry) = state.open.get(id) else {
            drop(state);
            return Err(self
                .resolve(tenant, id)
                .err()
                .unwrap_or_else(|| StoreError::NotFound(format!("transaction {id} is unknown"))));
        };
        if &entry.tenant != tenant {
            tracing::warn!(
                transaction = %id,
                owner = %entry.tenant,
                caller = %tenant,
                "refused to end a transaction belonging to another tenant"
            );
            return Err(StoreError::Tenant(format!(
                "transaction {id} belongs to another tenant"
            )));
        }
        let entry = state.open.remove(id).expect("just looked it up");
        state.remember_ended(
            id.to_string(),
            EndedTransaction {
                tenant: entry.tenant.clone(),
                how,
                granted: entry.granted,
            },
        );
        Ok(entry)
    }

    /// The deadline's claim: no tenant to check, and losing the race to a
    /// commit that got there first is ordinary rather than an error.
    fn claim_expired(&self, id: &str) -> Option<Arc<TransactionEntry>> {
        let mut state = self.lock();
        let entry = state.open.remove(id)?;
        state.remember_ended(
            id.to_string(),
            EndedTransaction {
                tenant: entry.tenant.clone(),
                how: Ended::DeadlineExceeded,
                granted: entry.granted,
            },
        );
        Some(entry)
    }

    #[cfg(test)]
    fn open_count(&self) -> usize {
        self.lock().open.len()
    }
}

/// Holds a tenant's transaction slot while `BeginTransaction` is still
/// getting the transaction open.
///
/// Drop is what covers the case no error branch can reach: the caller went
/// away - tonic drops the handler future when the client resets the stream -
/// while the call was still waiting for a connection or for the write lock.
/// The slot would then stay taken for the life of the process, with no
/// transaction behind it and no deadline task to reap it, and the tenant
/// would be permanently one transaction poorer.
struct TransactionSlot {
    registry: Arc<TransactionRegistry>,
    id: String,
    /// Set once the transaction is open and its deadline task is about to be
    /// armed, after which the slot belongs to the transaction.
    handed_over: bool,
}

impl TransactionSlot {
    /// The transaction is open; stop guarding the slot and give up the id.
    fn keep(mut self) -> String {
        self.handed_over = true;
        std::mem::take(&mut self.id)
    }
}

impl Drop for TransactionSlot {
    fn drop(&mut self) {
        if self.handed_over {
            return;
        }
        self.registry.abandon(&self.id);
        tracing::debug!(
            transaction = %self.id,
            "gave a transaction slot back: opening it did not finish, so nothing was left holding \
             it"
        );
    }
}

/// A transaction that is open and the caller's to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionHandle {
    pub id: String,
    /// How long the server will keep it open, counted from the moment `BEGIN`
    /// returned.
    pub expires_in: Duration,
}

/// Take the pinned connection back and end the transaction on it.
///
/// The connection is taken out of the entry *before* the statement runs, so
/// even a `COMMIT` that fails releases it - the pool guard rolls back whatever
/// the failure left open before the connection is reused. Everything happens
/// inside the blocking closure, which means an aborted caller (the deadline
/// task being cancelled, say) cannot strand the connection either.
async fn end_transaction(entry: Arc<TransactionEntry>, sql: &'static str) -> StoreResult<()> {
    tokio::task::spawn_blocking(move || {
        let taken = entry.conn.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(pinned) = taken else {
            return Ok(());
        };
        let result = pinned.execute_batch(sql);
        drop(pinned);
        result.map_err(StoreError::from)
    })
    .await?
}

/// Run one statement on a transaction's pinned connection, held to `granted`.
async fn on_transaction<F, T>(
    entry: Arc<TransactionEntry>,
    id: String,
    granted: Duration,
    f: F,
) -> StoreResult<T>
where
    F: FnOnce(&mut rusqlite::Connection) -> StoreResult<T> + Send + 'static,
    T: Send + 'static,
{
    let (presence, abandoned) = sqlite::CallerPresence::new();
    let task = tokio::task::spawn_blocking(move || {
        let mut guard = entry.conn.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            // Budgeted and translated here as well as in
            // `SqlitePool::with_conn_within`, because a pinned connection
            // runs its statements without going through the pool at all -
            // and a transaction is the one place a statement can hold a
            // tenant's write lock while it overruns.
            Some(pinned) => sqlite::tenant_statement(granted, abandoned, || f(pinned)),
            // Only reachable by racing a commit, a rollback or the deadline:
            // the handle was still open when it was looked up and had ended by
            // the time this statement got the connection.
            None => Err(StoreError::NotFound(format!(
                "transaction {id} ended while this statement was waiting for it"
            ))),
        }
    });
    let outcome = task.await;
    drop(presence);
    outcome?
}

/// Open the transaction on a freshly pinned connection.
async fn begin_immediate(pinned: PooledConnection) -> StoreResult<PooledConnection> {
    tokio::task::spawn_blocking(move || {
        // IMMEDIATE rather than the default DEFERRED. A deferred transaction
        // that reads and then writes has to upgrade its read snapshot to a
        // write lock, which SQLite refuses outright with SQLITE_BUSY_SNAPSHOT
        // instead of waiting out - and it refuses at COMMIT, by which point
        // the caller has been told every statement succeeded. Read-then-write
        // is the whole reason this API exists, so the lock is taken here.
        pinned.execute_batch("BEGIN IMMEDIATE")?;
        Ok(pinned)
    })
    .await?
}

/// SQL service. Pools are created on first use and kept for the process
/// lifetime, one per tenant.
#[derive(Clone)]
pub struct DatabaseService {
    root: PathBuf,
    pools: Arc<Mutex<HashMap<TenantId, SqlitePool>>>,
    connections_per_tenant: usize,
    /// The `synchronous` setting every tenant's file is opened with.
    durability: Durability,
    statements: Arc<StatementRegistry>,
    transactions: Arc<TransactionRegistry>,
    limits: TransactionLimits,
    sql: SqlLimits,
}

impl DatabaseService {
    /// Prepare `<data_dir>/db/`. Nothing is opened until a tenant asks.
    pub fn open(data_dir: &std::path::Path, connections_per_tenant: usize) -> StoreResult<Self> {
        Self::open_with(data_dir, connections_per_tenant, Durability::default())
    }

    pub(crate) fn open_with(
        data_dir: &std::path::Path,
        connections_per_tenant: usize,
        durability: Durability,
    ) -> StoreResult<Self> {
        let root = data_dir.join("db");
        std::fs::create_dir_all(&root)?;
        let connections_per_tenant = if connections_per_tenant < MIN_TENANT_CONNECTIONS {
            tracing::warn!(
                configured = connections_per_tenant,
                using = MIN_TENANT_CONNECTIONS,
                "tenant_connections is below the minimum and has been raised; an interactive \
                 transaction pins a connection for its whole life, so a pool this small would \
                 leave a tenant's ordinary queries with nowhere to run until the transaction's \
                 deadline released it"
            );
            MIN_TENANT_CONNECTIONS
        } else {
            connections_per_tenant
        };
        tracing::info!(
            path = %root.display(),
            connections_per_tenant,
            "database service ready, one file per tenant"
        );
        Ok(Self {
            root,
            pools: Arc::new(Mutex::new(HashMap::new())),
            connections_per_tenant,
            durability,
            statements: Arc::new(StatementRegistry::default()),
            transactions: Arc::new(TransactionRegistry::default()),
            limits: clamp_to_pool(TransactionLimits::default(), connections_per_tenant),
            sql: SqlLimits::default(),
        })
    }

    /// Replace what interactive transactions are allowed to cost.
    ///
    /// The per-tenant ceiling is clamped to the tenant's pool size here, not
    /// where it is configured, because the pool size is what makes a given
    /// ceiling safe or not; see [`clamp_to_pool`].
    pub fn with_transaction_limits(mut self, limits: TransactionLimits) -> Self {
        self.limits = clamp_to_pool(limits, self.connections_per_tenant);
        self
    }

    /// Replace what one request is allowed to cost.
    pub fn with_sql_limits(mut self, limits: SqlLimits) -> Self {
        self.sql = limits;
        self
    }

    /// The ceilings actually in force, after clamping. Logged at startup so
    /// an operator reading about a refused `BeginTransaction` can see what it
    /// was refused against.
    pub fn transaction_limits(&self) -> TransactionLimits {
        self.limits
    }

    /// What one request may cost. Logged at startup for the same reason: an
    /// operator reading that a tenant's query was stopped needs the number it
    /// was stopped against in front of them.
    pub fn sql_limits(&self) -> SqlLimits {
        self.sql
    }

    /// The pool for one tenant, opened on first use.
    fn pool(&self, tenant: &TenantId) -> SqlitePool {
        let mut pools = self.pools.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pool) = pools.get(tenant) {
            return pool.clone();
        }
        // What is joined here is a *namespace*, not a bare tenant: it is
        // `app.environment`, or `app.environment@BINDING` when the call named
        // a binding. That it is a single path component is still a property
        // of the validator rather than of this line - `TenantId` exists only
        // via `split_namespace`, so each of the two or three parts satisfied
        // `is_valid_tenant_part` / `is_valid_binding_name`, whose character
        // set is ASCII letters, digits, `_` and `-`. No part can hold a `.`,
        // a `/` or a `\`, the only separators are the one `.` and the at most
        // one `@` the split put there, and the whole thing is at most 194
        // bytes, so it cannot spell `.`, `..`, an absolute path or anything
        // that resolves outside `root`. 194 plus `.sqlite` is 201, and
        // SQLite's longest companion suffix adds a dozen more, so a binding
        // cannot push a name past the 255-byte limit either.
        let path = self.root.join(format!("{tenant}.sqlite"));
        tracing::info!(
            tenant = %tenant,
            path = %path.display(),
            "opening this tenant's database file"
        );
        // Guarded, not plain: everything that runs on these connections is
        // SQL an application sent. See the module docs.
        let pool = SqlitePool::new_guarded(
            path,
            format!("db:{tenant}"),
            PoolSize::new(self.connections_per_tenant),
            tenant_sql_authorizer,
            self.durability,
        );
        pools.insert(tenant.clone(), pool.clone());
        pool
    }

    /// Run one statement that does not return rows.
    pub async fn exec_statement(
        &self,
        tenant: &TenantId,
        sql: &str,
        params: &[SqlValue],
    ) -> StoreResult<ExecOutcome> {
        let sql = check_sql(sql).inspect_err(|e| note_refusal(tenant, e))?;
        let params = params.to_vec();
        self.pool(tenant)
            .with_conn_within(self.sql.statement_deadline, move |conn| {
                let rows_affected = conn.execute(&sql, rusqlite::params_from_iter(params))?;
                Ok(ExecOutcome {
                    rows_affected: rows_affected as i64,
                    last_insert_id: conn.last_insert_rowid(),
                })
            })
            .await
            .inspect_err(|e| note_cost(tenant, e))
    }

    /// Run one statement and collect its rows.
    pub async fn fetch_rows(
        &self,
        tenant: &TenantId,
        sql: &str,
        params: &[SqlValue],
    ) -> StoreResult<ResultSet> {
        let sql = check_sql(sql).inspect_err(|e| note_refusal(tenant, e))?;
        let params = params.to_vec();
        let limits = self.sql;
        self.pool(tenant)
            .with_conn_within(limits.statement_deadline, move |conn| {
                let mut stmt = conn.prepare(&sql)?;
                if !stmt.readonly() {
                    return Err(StoreError::Refused(
                        "Query accepts only read-only SQL; use Execute for a statement that changes data"
                            .to_string(),
                    ));
                }
                collect_rows(&mut stmt, params, limits)
            })
            .await
            .inspect_err(|e| note_cost(tenant, e))
    }

    /// Run several statements inside one transaction: either all of them
    /// land or none of them do.
    pub async fn exec_batch(
        &self,
        tenant: &TenantId,
        statements: Vec<(String, Vec<SqlValue>)>,
    ) -> StoreResult<Result<Vec<ExecOutcome>, BatchFailure>> {
        for (sql, _) in &statements {
            check_sql(sql).inspect_err(|e| note_refusal(tenant, e))?;
        }
        self.pool(tenant)
            .with_conn_within(self.sql.statement_deadline, move |conn| {
                // Deferred is enough here: `tx.execute` refuses a statement
                // that returns rows, so a batch never reads before it writes
                // and never has to upgrade a read snapshot to a write lock -
                // the upgrade being the case SQLite refuses outright instead
                // of waiting out with `busy_timeout`.
                let tx = conn.transaction()?;
                let mut done = Vec::with_capacity(statements.len());
                for (index, (sql, params)) in statements.into_iter().enumerate() {
                    match tx.execute(&sql, rusqlite::params_from_iter(params)) {
                        Ok(rows_affected) => done.push(ExecOutcome {
                            rows_affected: rows_affected as i64,
                            last_insert_id: tx.last_insert_rowid(),
                        }),
                        Err(e) if sqlite::was_interrupted(&e) => {
                            // The budget stopped the batch. This one does
                            // *not* become a named statement failure: a
                            // `BatchFailure` is an `Ok` at this level, which
                            // would slip past the wrapper that turns SQLite's
                            // bare "interrupted" into which ceiling was
                            // crossed, and the caller would be told its
                            // seventh statement failed for no stated reason.
                            return Err(StoreError::from(e));
                        }
                        Err(e) => {
                            // A batch names the statement that failed rather
                            // than failing outright, so the refusal is
                            // collected here instead of by the pool's
                            // `explaining_refusals`, which only rewrites an
                            // error that came all the way out. Size ceilings
                            // are explained the same way and for the same
                            // reason.
                            let message = sqlite::take_refusal()
                                .or_else(|| sqlite::size_limit_message(&e))
                                .unwrap_or_else(|| e.to_string());
                            // Dropping the transaction rolls it back.
                            return Ok(Err(BatchFailure {
                                index,
                                message,
                                partial: done,
                            }));
                        }
                    }
                }
                tx.commit()?;
                Ok(Ok(done))
            })
            .await
            .inspect_err(|e| note_cost(tenant, e))
    }

    /// Validate `sql` and remember it under a handle.
    pub async fn register_statement(&self, tenant: &TenantId, sql: &str) -> StoreResult<String> {
        let sql = check_sql(sql).inspect_err(|e| note_refusal(tenant, e))?;
        let owned = sql.clone();
        // Prepare it once for real so a syntax error is reported now, at the
        // call that can explain it, rather than at some later execution. A
        // statement the authorizer refuses is refused here too, for the same
        // reason and at the same call.
        self.pool(tenant)
            .with_conn_within(self.sql.statement_deadline, move |conn| {
                conn.prepare(&owned)?;
                Ok(())
            })
            .await
            .inspect_err(|e| note_cost(tenant, e))?;
        let id = self.statements.insert(tenant.clone(), sql);
        tracing::debug!(tenant = %tenant, statement = %id, "prepared statement registered");
        Ok(id)
    }

    /// The SQL behind a handle, if this tenant owns it.
    fn prepared_sql(&self, tenant: &TenantId, id: &str) -> StoreResult<String> {
        self.statements.resolve(tenant, id)
    }

    /// Open an interactive transaction on a connection pinned to it.
    ///
    /// `requested` is what the caller would like the deadline to be; it is
    /// clamped to the configured ceiling, and the handle says what was
    /// actually granted.
    pub async fn open_transaction(
        &self,
        tenant: &TenantId,
        requested: Option<Duration>,
    ) -> StoreResult<TransactionHandle> {
        let granted = self.limits.grant(requested)?;
        let (id, entry) = self.transactions.admit(tenant, granted, &self.limits)?;
        // Every way out from here - an error, or the caller disappearing
        // mid-call - gives the slot back, because the guard's drop does.
        let slot = TransactionSlot {
            registry: Arc::clone(&self.transactions),
            id,
            handed_over: false,
        };

        let pinned = self.pool(tenant).checkout().await?;
        let pinned = begin_immediate(pinned).await.inspect_err(|e| {
            tracing::debug!(
                tenant = %tenant,
                error = %e,
                "could not open a transaction; SQLite gives one file one writer, so this is \
                 usually another transaction on the same tenant still holding the write lock"
            );
        })?;

        *entry.conn.lock().unwrap_or_else(|e| e.into_inner()) = Some(pinned);
        // Nothing below awaits, so the slot cannot be dropped between being
        // handed over and the deadline task that takes over guarding it.
        let id = slot.keep();
        // The clock starts now, not before BEGIN: a transaction that spent
        // time waiting for the write lock has not spent any of the caller's.
        // Both readings of it start together - the task that reaps the
        // transaction, and the instant every statement on it is held to.
        entry.deadline_starts_now(granted);
        let task = self.spawn_deadline(id.clone(), granted);
        *entry
            .deadline_task
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(task);

        tracing::debug!(
            tenant = %tenant,
            transaction = %id,
            deadline_ms = granted.as_millis(),
            "opened an interactive transaction; it holds one of this tenant's connections until \
             it ends"
        );
        Ok(TransactionHandle {
            id,
            expires_in: granted,
        })
    }

    /// Roll the transaction back once its deadline passes.
    ///
    /// A transaction that ends on its own cancels this; one that does not -
    /// a handler awaiting something slow, a runtime that crashed, a client
    /// that dropped its connection - is ended here instead, which is the only
    /// thing standing between a forgotten handle and a tenant with no writer.
    fn spawn_deadline(&self, id: String, granted: Duration) -> JoinHandle<()> {
        let transactions = Arc::clone(&self.transactions);
        tokio::spawn(async move {
            tokio::time::sleep(granted).await;
            let Some(entry) = transactions.claim_expired(&id) else {
                return;
            };
            tracing::info!(
                tenant = %entry.tenant,
                transaction = %id,
                deadline_ms = granted.as_millis(),
                "rolling back a transaction that outlived its deadline; whatever it had written \
                 is discarded and the connection it pinned goes back to this tenant's pool"
            );
            if let Err(e) = end_transaction(entry, "ROLLBACK").await {
                tracing::error!(
                    transaction = %id,
                    error = %e,
                    "could not roll back a transaction that outlived its deadline; the connection \
                     is being closed rather than pooled, so the tenant is one connection down \
                     until the process restarts"
                );
            }
        })
    }

    /// Run a row-less statement inside an open transaction.
    pub async fn exec_in_transaction(
        &self,
        tenant: &TenantId,
        id: &str,
        sql: &str,
        params: &[SqlValue],
    ) -> StoreResult<ExecOutcome> {
        let sql = check_transaction_sql(sql).inspect_err(|e| note_refusal(tenant, e))?;
        let entry = self.transactions.resolve(tenant, id)?;
        let granted = self.statement_budget(tenant, id, &entry)?;
        let params = params.to_vec();
        let outcome = on_transaction(entry, id.to_string(), granted, move |conn| {
            let rows_affected = conn.execute(&sql, rusqlite::params_from_iter(params))?;
            Ok(ExecOutcome {
                rows_affected: rows_affected as i64,
                last_insert_id: conn.last_insert_rowid(),
            })
        })
        .await
        .inspect_err(|e| note_cost(tenant, e));
        self.end_after_a_stopped_statement(tenant, id, &outcome)
            .await;
        outcome
    }

    /// Run a query inside an open transaction. It sees that transaction's own
    /// uncommitted writes, which is the entire point of doing it here rather
    /// than with `Query`.
    pub async fn fetch_rows_in_transaction(
        &self,
        tenant: &TenantId,
        id: &str,
        sql: &str,
        params: &[SqlValue],
    ) -> StoreResult<ResultSet> {
        let sql = check_transaction_sql(sql).inspect_err(|e| note_refusal(tenant, e))?;
        let entry = self.transactions.resolve(tenant, id)?;
        let granted = self.statement_budget(tenant, id, &entry)?;
        let params = params.to_vec();
        let limits = self.sql;
        let outcome = on_transaction(entry, id.to_string(), granted, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            collect_rows(&mut stmt, params, limits)
        })
        .await
        .inspect_err(|e| note_cost(tenant, e));
        self.end_after_a_stopped_statement(tenant, id, &outcome)
            .await;
        outcome
    }

    /// How long a statement inside this transaction may run.
    ///
    /// The smaller of the request budget and what the transaction has left,
    /// which is the whole of what makes the transaction deadline enforceable:
    /// the deadline task cannot preempt a statement already running, so a
    /// statement given more time than the transaction has is a statement that
    /// outlives it.
    fn statement_budget(
        &self,
        tenant: &TenantId,
        id: &str,
        entry: &TransactionEntry,
    ) -> StoreResult<Duration> {
        entry.budget_for(self.sql.statement_deadline).ok_or_else(|| {
            tracing::debug!(
                tenant = %tenant,
                transaction = %id,
                "refused a statement on a transaction that is already past its deadline; the task \
                 that rolls it back has not run yet"
            );
            StoreError::Expired(format!(
                "transaction {id} is past its deadline and is being rolled back; nothing it wrote \
                 will be applied"
            ))
        })
    }

    /// End a transaction whose statement had to be stopped part-way.
    ///
    /// Not tidiness. SQLite rolls an explicit transaction back by itself when
    /// it stops a write inside one, which leaves the connection in autocommit
    /// while the handle still says a transaction is open - and the next
    /// statement sent on that handle would then run outside any transaction
    /// and be kept. Ending it here means the handle answers for itself
    /// instead, and the tenant gets its pinned connection back now rather
    /// than at the transaction's deadline.
    async fn end_after_a_stopped_statement<T>(
        &self,
        tenant: &TenantId,
        id: &str,
        outcome: &StoreResult<T>,
    ) {
        if !matches!(outcome, Err(StoreError::Expired(_))) {
            return;
        }
        tracing::warn!(
            tenant = %tenant,
            transaction = %id,
            "rolling this transaction back: a statement inside it was stopped part-way, so \
             SQLite may already have discarded its writes and the handle cannot be trusted to \
             still be inside a transaction"
        );
        // A losing race with the deadline task or with an explicit rollback is
        // ordinary: whoever got there first ended it, which is the outcome
        // wanted here.
        let _ = self
            .finish(tenant, id, Ended::StatementStopped, "ROLLBACK")
            .await;
    }

    /// Apply everything the transaction did and release its connection.
    pub async fn commit(&self, tenant: &TenantId, id: &str) -> StoreResult<()> {
        self.finish(tenant, id, Ended::Committed, "COMMIT").await
    }

    /// Discard everything the transaction did and release its connection.
    pub async fn roll_back(&self, tenant: &TenantId, id: &str) -> StoreResult<()> {
        self.finish(tenant, id, Ended::RolledBack, "ROLLBACK").await
    }

    async fn finish(
        &self,
        tenant: &TenantId,
        id: &str,
        how: Ended,
        sql: &'static str,
    ) -> StoreResult<()> {
        let entry = self.transactions.claim(tenant, id, how)?;
        // Claimed, so the deadline cannot also end it; stopping the task now
        // keeps a begin/commit loop from leaving a timer per transaction
        // pending for the rest of its deadline.
        entry.cancel_deadline();
        let result = end_transaction(entry, sql).await;
        match &result {
            Ok(()) => tracing::debug!(
                tenant = %tenant,
                transaction = %id,
                outcome = ?how,
                "transaction ended, its connection is back in the pool"
            ),
            Err(e) => tracing::warn!(
                tenant = %tenant,
                transaction = %id,
                outcome = ?how,
                error = %e,
                "transaction could not be ended cleanly; its connection was released either way, \
                 and anything the statement left open was rolled back rather than pooled"
            ),
        }
        result
    }

    /// Hand the deadline's task over, so a test can wait for exactly the work
    /// it does instead of watching the clock.
    #[cfg(test)]
    fn take_deadline_task(&self, id: &str) -> Option<JoinHandle<()>> {
        let state = self.transactions.lock();
        let entry = state.open.get(id)?;
        entry
            .deadline_task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

/// What a row-less statement did: how many rows it touched and, for an
/// insert, the id it handed out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutcome {
    pub rows_affected: i64,
    pub last_insert_id: i64,
}

/// The rows a query returned, plus the column names they are keyed by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSet {
    pub columns: Vec<String>,
    /// JSON array of objects, one per row, keyed by column name.
    pub rows: JsonValue,
}

/// Which statement of a batch failed, and what had run before it. The whole
/// transaction is rolled back regardless; `partial` is for diagnosis only.
#[derive(Debug, Clone)]
pub struct BatchFailure {
    pub index: usize,
    pub message: String,
    pub partial: Vec<ExecOutcome>,
}

/// Refuse SQL that is empty or nothing but whitespace before it reaches the
/// database, where it would come back as a confusing parse error - and refuse
/// `VACUUM`, which is the one statement whose refusal wants to be said here
/// rather than left to the authorizer.
///
/// The authorizer does refuse `VACUUM INTO`, as the `ATTACH` of the file it
/// writes, and that is the layer that holds; this one only improves what the
/// caller is told. It has to, for two reasons the authorizer cannot cover.
/// The `Attach` action for a vacuum arrives while the statement *runs*, not
/// while it is prepared, so `Prepare` would accept a `VACUUM INTO`, hand back
/// a handle, and let the refusal surface at whatever executed it later. And
/// the sentence it would produce says `ATTACH` about a statement that never
/// spelled the word.
///
/// Which is why [`leading_keyword`] looks past comments and empty statements
/// rather than reading the first characters it is given: this check is the
/// only thing standing between `Prepare` and a handle for a statement that
/// can never run, so a spelling that walks past it costs a caller a refusal
/// at the wrong call. It is still a keyword check and still not the boundary
/// - it reads one word, not a parse tree.
fn check_sql(sql: &str) -> StoreResult<String> {
    if sql.trim().is_empty() {
        return Err(StoreError::InvalidArgument("sql is empty".to_string()));
    }
    if leading_keyword(sql) == "VACUUM" {
        return Err(StoreError::Refused(
            "VACUUM: `VACUUM INTO` writes a copy of this tenant's database wherever the \
             statement points, and a plain VACUUM rewrites the whole file while holding one of \
             this tenant's few connections"
                .to_string(),
        ));
    }
    Ok(sql.to_string())
}

/// The first word of a statement, upper-cased, for the two keyword checks
/// above and below. Stops at whitespace, `;` or `(`, so `BEGIN;` and
/// `VACUUM(` are the same word as `BEGIN` and `VACUUM`.
///
/// What comes before that word is skipped rather than read: whitespace,
/// `--` line comments, `/* */` block comments and empty statements. SQLite
/// ignores all four, so a caller writing `/* x */ VACUUM INTO '<path>'` is
/// sending a vacuum however it looks to a naive reader, and the checks built
/// on this exist to name what was refused at the call that sent it. An
/// unterminated block comment is left where it is: SQLite will reject the
/// statement, and guessing what it meant to say would be worse than letting
/// it.
fn leading_keyword(sql: &str) -> String {
    let mut rest = sql;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ';');
        if let Some(after) = rest.strip_prefix("--") {
            // A line comment runs to the newline, or to the end if there is
            // none - in which case there is no keyword to find.
            rest = after.find('\n').map_or("", |end| &after[end + 1..]);
            continue;
        }
        if let Some(after) = rest.strip_prefix("/*") {
            match after.find("*/") {
                Some(end) => {
                    rest = &after[end + 2..];
                    continue;
                }
                None => return String::new(),
            }
        }
        break;
    }
    rest.split(|c: char| c.is_whitespace() || c == ';' || c == '(')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase()
}

/// The same check as [`check_sql`], plus a refusal of the statements that
/// would end the transaction behind the server's back.
///
/// A caller that sends `COMMIT` through `ExecuteInTransaction` would leave the
/// handle naming a transaction that is no longer open and the pinned
/// connection sitting in autocommit, so the next statement on that handle
/// would land outside any transaction and be kept. Transaction control is the
/// four RPCs, and only those.
fn check_transaction_sql(sql: &str) -> StoreResult<String> {
    let sql = check_sql(sql)?;
    let leading = leading_keyword(&sql);
    if matches!(
        leading.as_str(),
        "BEGIN" | "COMMIT" | "END" | "ROLLBACK" | "SAVEPOINT" | "RELEASE"
    ) {
        return Err(StoreError::InvalidArgument(format!(
            "{leading} cannot be run inside an interactive transaction; use CommitTransaction or \
             RollbackTransaction, which also release the connection this transaction is holding"
        )));
    }
    Ok(sql)
}

/// The largest integer magnitude an application can read out of JSON without
/// losing digits off it.
///
/// 2^53 - 1, which is JavaScript's `Number.MAX_SAFE_INTEGER`. Every JSON
/// number an application parses becomes an IEEE-754 double, and past this a
/// double can no longer tell one integer from the next: 9007199254740993
/// parses to 9007199254740992. So this is the line where [`sql_to_json`]
/// stops writing a number and writes a decimal string, and where
/// [`decode_params`] reads one back.
const MAX_SAFE_JSON_INTEGER: i64 = 9_007_199_254_740_991;

/// Whether an integer can travel as a JSON number without an application
/// silently reading a different one.
///
/// Written as a range rather than `abs()`: `i64::MIN.abs()` overflows, and
/// this is asked about every integer cell of every result set.
fn fits_json_number(value: i64) -> bool {
    (-MAX_SAFE_JSON_INTEGER..=MAX_SAFE_JSON_INTEGER).contains(&value)
}

/// Bind one JSON string, which is TEXT except for the single spelling this
/// service uses for an integer too large for a JSON number.
///
/// The exception is exactly as wide as [`sql_to_json`]'s: the string has to be
/// the digits `i64::to_string` would have produced, so a leading `+`, a
/// leading zero, or any integer a JSON number could have carried is the TEXT
/// it looks like. Without it a big id is a one-way value - it can be read and
/// never written back - and with anything wider than it a tenant storing the
/// digits of a large number as text would find them coming back as an integer.
fn bind_string(text: String) -> SqlValue {
    match text.parse::<i64>() {
        Ok(value) if !fits_json_number(value) && value.to_string() == text => {
            SqlValue::Integer(value)
        }
        _ => SqlValue::Text(text),
    }
}

/// Bind one JSON array as a BLOB, one element per byte.
///
/// This is the shape [`sql_to_json`] hands a BLOB back in, and the reason an
/// array is bindable at all: refusing arrays meant a row with a BLOB column
/// could be read but never written back, which is read-modify-write - the
/// ordinary shape of an application's database use - broken for the whole
/// table. There is no other spelling to offer, since a JSON string is TEXT and
/// decoding base64 out of one would turn a tenant's text into bytes.
///
/// Elements are required to be whole numbers in range rather than rounded or
/// truncated into one: a byte array is content, and quietly storing 2 where
/// the caller wrote 2.7 corrupts it in the one direction nothing downstream
/// can detect.
fn decode_blob(index: usize, items: Vec<JsonValue>) -> StoreResult<SqlValue> {
    let mut bytes = Vec::with_capacity(items.len());
    for (offset, item) in items.into_iter().enumerate() {
        match item.as_u64() {
            Some(byte) if byte <= u64::from(u8::MAX) => bytes.push(byte as u8),
            _ => {
                return Err(StoreError::InvalidArgument(format!(
                    "params[{index}][{offset}] is {item}; an array parameter is a BLOB written one \
                     byte at a time, so every element has to be an integer from 0 to 255"
                )));
            }
        }
    }
    Ok(SqlValue::Blob(bytes))
}

/// Decode the JSON parameter array that storage.proto carries as a string.
///
/// Every shape [`sql_to_json`] can produce is accepted here, because the two
/// are the two halves of one round trip: an application reads a row, changes
/// one field and writes the row back, and anything it cannot hand back is a
/// column it can never update. An object is the one exception, and it is not
/// one this service can produce: SQLite has no type to put it in, and
/// stringifying it would store something the tenant never asked for.
fn decode_params(params: Option<&str>) -> StoreResult<Vec<SqlValue>> {
    let Some(raw) = params else {
        return Ok(Vec::new());
    };
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let parsed: JsonValue = serde_json::from_str(raw)
        .map_err(|e| StoreError::InvalidArgument(format!("params is not valid JSON: {e}")))?;
    let JsonValue::Array(items) = parsed else {
        return Err(StoreError::InvalidArgument(
            "params must be a JSON array".to_string(),
        ));
    };
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| match item {
            JsonValue::Null => Ok(SqlValue::Null),
            JsonValue::Bool(b) => Ok(SqlValue::Integer(i64::from(b))),
            JsonValue::String(s) => Ok(bind_string(s)),
            JsonValue::Array(items) => decode_blob(index, items),
            JsonValue::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(SqlValue::Integer(i))
                } else if let Some(f) = n.as_f64() {
                    Ok(SqlValue::Real(f))
                } else {
                    Err(StoreError::InvalidArgument(format!(
                        "params[{index}] is a number SQLite cannot hold: {n}"
                    )))
                }
            }
            JsonValue::Object(_) => Err(StoreError::InvalidArgument(format!(
                "params[{index}] is an object, and SQLite has no type to hold one; bind the fields \
                 it is made of, or its text if the column is meant to keep JSON"
            ))),
        })
        .collect()
}

/// Map one SQLite cell to JSON, in a shape [`decode_params`] takes back.
///
/// The two spellings that are not the obvious one are the two an application
/// could otherwise never write back: a blob becomes an array of byte values,
/// and an integer past [`MAX_SAFE_JSON_INTEGER`] becomes a decimal string.
/// See the wire section of the module documentation for why each is the only
/// choice available.
fn sql_to_json(value: ValueRef<'_>) -> JsonValue {
    match value {
        ValueRef::Null => JsonValue::Null,
        ValueRef::Integer(i) if fits_json_number(i) => JsonValue::from(i),
        // Past the last integer a double can tell from its neighbour, a JSON
        // number would be read as a different number - with no error
        // anywhere - and written back as that different number. The digits
        // are exact, and decode_params reads this spelling back as INTEGER.
        ValueRef::Integer(i) => JsonValue::String(i.to_string()),
        ValueRef::Real(f) => serde_json::Number::from_f64(f)
            .map(JsonValue::Number)
            // NaN and the infinities have no JSON spelling. They cannot be
            // produced by a bound parameter, only by arithmetic inside the
            // statement, so null is the honest answer.
            .unwrap_or(JsonValue::Null),
        ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => JsonValue::String(text.to_string()),
            Err(_) => {
                tracing::warn!(
                    "a TEXT column holds bytes that are not UTF-8; returning it as a byte array so \
                     the row is still readable"
                );
                JsonValue::Array(bytes.iter().map(|b| JsonValue::from(*b)).collect())
            }
        },
        ValueRef::Blob(bytes) => {
            JsonValue::Array(bytes.iter().map(|b| JsonValue::from(*b)).collect())
        }
    }
}

/// storage.proto reports statement failures inside the response, not as a
/// gRPC status: a tenant's SQL typo is not a transport fault.
fn failed_execute(message: String) -> ExecuteResponse {
    ExecuteResponse {
        success: false,
        rows_affected: 0,
        last_insert_id: None,
        error: Some(message),
    }
}

fn failed_query(message: String) -> QueryResponse {
    QueryResponse {
        success: false,
        rows: "[]".to_string(),
        columns: Vec::new(),
        error: Some(message),
    }
}

fn failed_begin(message: String) -> BeginTransactionResponse {
    BeginTransactionResponse {
        success: false,
        transaction_id: String::new(),
        expires_in_ms: 0,
        error: Some(message),
    }
}

/// Errors a caller has to be able to read as text, because the answer is in
/// the message: which ceiling refused it, or what happened to the handle it
/// is still holding. Like a SQL typo these ride inside the response rather
/// than becoming a gRPC status - a tenant that used up its transaction slots
/// is not the store being down.
fn is_caller_visible(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::Sqlite(_)
            | StoreError::InvalidArgument(_)
            | StoreError::NotFound(_)
            | StoreError::Expired(_)
            | StoreError::Exhausted(_)
            // A result set the caller asked for and may not have. Same
            // reasoning as the ceilings above it: the answer is in the
            // message, and the store is not down.
            | StoreError::TooLarge(_)
            // The application is the only one who can act on this: it sent
            // SQL a tenant may not run, and the message names which part.
            | StoreError::Refused(_)
    )
}

/// The text a caller is given for such an error. A SQLite failure loses this
/// crate's `sqlite:` prefix, matching what the non-transactional handlers
/// have always reported.
fn caller_visible_text(error: &StoreError) -> String {
    match error {
        StoreError::Sqlite(e) => e.to_string(),
        other => other.to_string(),
    }
}

impl DatabaseService {
    async fn run_execute(
        &self,
        tenant: &TenantId,
        sql: &str,
        params: Option<&str>,
    ) -> Result<ExecuteResponse, Status> {
        let params = match decode_params(params) {
            Ok(p) => p,
            Err(e) => return Ok(failed_execute(e.to_string())),
        };
        match self.exec_statement(tenant, sql, &params).await {
            Ok(done) => {
                tracing::debug!(
                    tenant = %tenant,
                    rows_affected = done.rows_affected,
                    "sql execute finished"
                );
                Ok(ExecuteResponse {
                    success: true,
                    rows_affected: done.rows_affected,
                    last_insert_id: Some(done.last_insert_id),
                    error: None,
                })
            }
            Err(StoreError::Sqlite(e)) => {
                tracing::debug!(tenant = %tenant, error = %e, "sql execute rejected by sqlite");
                Ok(failed_execute(e.to_string()))
            }
            // Everything the caller is owed as text rides inside the
            // response, exactly as a SQL typo does: a ceiling it crossed and
            // what to change are the answer, and a gRPC status would leave
            // the sentence in a place a handler reads as "the store is down".
            Err(e) if is_caller_visible(&e) => Ok(failed_execute(caller_visible_text(&e))),
            Err(other) => {
                tracing::error!(
                    tenant = %tenant,
                    error = %other,
                    "sql execute failed below the statement level; the write did not happen"
                );
                Err(other.into())
            }
        }
    }

    async fn run_query(
        &self,
        tenant: &TenantId,
        sql: &str,
        params: Option<&str>,
    ) -> Result<QueryResponse, Status> {
        let params = match decode_params(params) {
            Ok(p) => p,
            Err(e) => return Ok(failed_query(e.to_string())),
        };
        match self.fetch_rows(tenant, sql, &params).await {
            Ok(result) => {
                let row_count = result.rows.as_array().map(Vec::len).unwrap_or(0);
                tracing::debug!(tenant = %tenant, rows = row_count, "sql query finished");
                Ok(QueryResponse {
                    success: true,
                    rows: result.rows.to_string(),
                    columns: result.columns,
                    error: None,
                })
            }
            Err(StoreError::Sqlite(e)) => {
                tracing::debug!(tenant = %tenant, error = %e, "sql query rejected by sqlite");
                Ok(failed_query(e.to_string()))
            }
            Err(e) if is_caller_visible(&e) => Ok(failed_query(caller_visible_text(&e))),
            Err(other) => {
                tracing::error!(
                    tenant = %tenant,
                    error = %other,
                    "sql query failed below the statement level; no rows were returned"
                );
                Err(other.into())
            }
        }
    }
}

#[tonic::async_trait]
impl Database for DatabaseService {
    async fn execute(
        &self,
        request: Request<ExecuteRequest>,
    ) -> Result<Response<ExecuteResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        tracing::trace!(tenant = %tenant, sql = %req.sql, "sql execute");
        Ok(Response::new(
            self.run_execute(&tenant, &req.sql, req.params.as_deref())
                .await?,
        ))
    }

    async fn query(
        &self,
        request: Request<QueryRequest>,
    ) -> Result<Response<QueryResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        tracing::trace!(tenant = %tenant, sql = %req.sql, "sql query");
        Ok(Response::new(
            self.run_query(&tenant, &req.sql, req.params.as_deref())
                .await?,
        ))
    }

    async fn batch(
        &self,
        request: Request<BatchRequest>,
    ) -> Result<Response<BatchResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        tracing::debug!(tenant = %tenant, statements = req.statements.len(), "sql batch");

        let mut decoded = Vec::with_capacity(req.statements.len());
        for (index, statement) in req.statements.iter().enumerate() {
            match decode_params(statement.params.as_deref()) {
                Ok(params) => decoded.push((statement.sql.clone(), params)),
                Err(e) => {
                    return Ok(Response::new(BatchResponse {
                        success: false,
                        results: Vec::new(),
                        error: Some(format!("statement {index}: {e}")),
                    }));
                }
            }
        }

        match self.exec_batch(&tenant, decoded).await {
            Ok(Ok(done)) => Ok(Response::new(BatchResponse {
                success: true,
                results: done
                    .into_iter()
                    .map(|d| ExecuteResponse {
                        success: true,
                        rows_affected: d.rows_affected,
                        last_insert_id: Some(d.last_insert_id),
                        error: None,
                    })
                    .collect(),
                error: None,
            })),
            Ok(Err(failure)) => {
                tracing::warn!(
                    tenant = %tenant,
                    statement = failure.index,
                    error = %failure.message,
                    "sql batch rolled back; none of its statements were applied"
                );
                Ok(Response::new(BatchResponse {
                    success: false,
                    results: failure
                        .partial
                        .into_iter()
                        .map(|d| ExecuteResponse {
                            success: true,
                            rows_affected: d.rows_affected,
                            last_insert_id: Some(d.last_insert_id),
                            error: None,
                        })
                        .collect(),
                    error: Some(format!(
                        "statement {} failed and the batch was rolled back: {}",
                        failure.index, failure.message
                    )),
                }))
            }
            // The two cost ceilings join `InvalidArgument` inside the
            // response: a batch stopped at its deadline is the tenant's own
            // statement to fix, not the store failing. A SQLite error at this
            // level is left where it was - it is the commit itself failing,
            // which is the store's problem and wants the `error` line below.
            Err(
                e @ (StoreError::InvalidArgument(_)
                | StoreError::Expired(_)
                | StoreError::TooLarge(_)),
            ) => Ok(Response::new(BatchResponse {
                success: false,
                results: Vec::new(),
                error: Some(e.to_string()),
            })),
            Err(other) => {
                tracing::error!(
                    tenant = %tenant,
                    error = %other,
                    "sql batch failed below the statement level; nothing was applied"
                );
                Err(other.into())
            }
        }
    }

    async fn prepare(
        &self,
        request: Request<PrepareRequest>,
    ) -> Result<Response<PrepareResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        match self.register_statement(&tenant, &req.sql).await {
            Ok(statement_id) => Ok(Response::new(PrepareResponse {
                success: true,
                statement_id,
                error: None,
            })),
            Err(StoreError::Sqlite(e)) => {
                tracing::debug!(tenant = %tenant, error = %e, "prepare rejected by sqlite");
                Ok(Response::new(PrepareResponse {
                    success: false,
                    statement_id: String::new(),
                    error: Some(e.to_string()),
                }))
            }
            Err(e) if is_caller_visible(&e) => Ok(Response::new(PrepareResponse {
                success: false,
                statement_id: String::new(),
                error: Some(caller_visible_text(&e)),
            })),
            Err(other) => {
                tracing::error!(
                    tenant = %tenant,
                    error = %other,
                    "prepare failed; no handle was issued"
                );
                Err(other.into())
            }
        }
    }

    async fn execute_prepared(
        &self,
        request: Request<ExecutePreparedRequest>,
    ) -> Result<Response<ExecuteResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let sql = match self.prepared_sql(&tenant, &req.statement_id) {
            Ok(sql) => sql,
            Err(e @ StoreError::NotFound(_)) => {
                return Ok(Response::new(failed_execute(e.to_string())));
            }
            Err(other) => return Err(other.into()),
        };
        Ok(Response::new(
            self.run_execute(&tenant, &sql, req.params.as_deref())
                .await?,
        ))
    }

    async fn query_prepared(
        &self,
        request: Request<QueryPreparedRequest>,
    ) -> Result<Response<QueryResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let sql = match self.prepared_sql(&tenant, &req.statement_id) {
            Ok(sql) => sql,
            Err(e @ StoreError::NotFound(_)) => {
                return Ok(Response::new(failed_query(e.to_string())));
            }
            Err(other) => return Err(other.into()),
        };
        Ok(Response::new(
            self.run_query(&tenant, &sql, req.params.as_deref()).await?,
        ))
    }

    async fn begin_transaction(
        &self,
        request: Request<BeginTransactionRequest>,
    ) -> Result<Response<BeginTransactionResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        let requested = match req.timeout_ms {
            None => None,
            Some(ms) if ms > 0 => Some(Duration::from_millis(ms as u64)),
            Some(ms) => {
                return Ok(Response::new(failed_begin(format!(
                    "timeout_ms must be positive, got {ms}"
                ))));
            }
        };
        match self.open_transaction(&tenant, requested).await {
            Ok(handle) => Ok(Response::new(BeginTransactionResponse {
                success: true,
                transaction_id: handle.id,
                expires_in_ms: handle.expires_in.as_millis() as i64,
                error: None,
            })),
            Err(e) if is_caller_visible(&e) => {
                Ok(Response::new(failed_begin(caller_visible_text(&e))))
            }
            Err(other) => {
                tracing::error!(
                    tenant = %tenant,
                    error = %other,
                    "could not open a transaction; no connection was pinned"
                );
                Err(other.into())
            }
        }
    }

    async fn execute_in_transaction(
        &self,
        request: Request<ExecuteInTransactionRequest>,
    ) -> Result<Response<ExecuteResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        tracing::trace!(
            tenant = %tenant,
            transaction = %req.transaction_id,
            sql = %req.sql,
            "sql execute in transaction"
        );
        let params = match decode_params(req.params.as_deref()) {
            Ok(params) => params,
            Err(e) => return Ok(Response::new(failed_execute(e.to_string()))),
        };
        match self
            .exec_in_transaction(&tenant, &req.transaction_id, &req.sql, &params)
            .await
        {
            Ok(done) => Ok(Response::new(ExecuteResponse {
                success: true,
                rows_affected: done.rows_affected,
                last_insert_id: Some(done.last_insert_id),
                error: None,
            })),
            Err(e) if is_caller_visible(&e) => {
                Ok(Response::new(failed_execute(caller_visible_text(&e))))
            }
            Err(other) => Err(other.into()),
        }
    }

    async fn query_in_transaction(
        &self,
        request: Request<QueryInTransactionRequest>,
    ) -> Result<Response<QueryResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        tracing::trace!(
            tenant = %tenant,
            transaction = %req.transaction_id,
            sql = %req.sql,
            "sql query in transaction"
        );
        let params = match decode_params(req.params.as_deref()) {
            Ok(params) => params,
            Err(e) => return Ok(Response::new(failed_query(e.to_string()))),
        };
        match self
            .fetch_rows_in_transaction(&tenant, &req.transaction_id, &req.sql, &params)
            .await
        {
            Ok(result) => Ok(Response::new(QueryResponse {
                success: true,
                rows: result.rows.to_string(),
                columns: result.columns,
                error: None,
            })),
            Err(e) if is_caller_visible(&e) => {
                Ok(Response::new(failed_query(caller_visible_text(&e))))
            }
            Err(other) => Err(other.into()),
        }
    }

    async fn commit_transaction(
        &self,
        request: Request<CommitTransactionRequest>,
    ) -> Result<Response<CommitTransactionResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        match self.commit(&tenant, &req.transaction_id).await {
            Ok(()) => Ok(Response::new(CommitTransactionResponse {
                success: true,
                error: None,
            })),
            Err(e) if is_caller_visible(&e) => Ok(Response::new(CommitTransactionResponse {
                success: false,
                error: Some(caller_visible_text(&e)),
            })),
            Err(other) => Err(other.into()),
        }
    }

    async fn rollback_transaction(
        &self,
        request: Request<RollbackTransactionRequest>,
    ) -> Result<Response<RollbackTransactionResponse>, Status> {
        let tenant = tenant_of(&request)?;
        let req = request.into_inner();
        match self.roll_back(&tenant, &req.transaction_id).await {
            Ok(()) => Ok(Response::new(RollbackTransactionResponse {
                success: true,
                error: None,
            })),
            Err(e) if is_caller_visible(&e) => Ok(Response::new(RollbackTransactionResponse {
                success: false,
                error: Some(caller_visible_text(&e)),
            })),
            Err(other) => Err(other.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn service() -> (TempDir, DatabaseService) {
        let dir = TempDir::new().unwrap();
        let svc = DatabaseService::open(dir.path(), 4).unwrap();
        (dir, svc)
    }

    fn service_with_sql(limits: SqlLimits) -> (TempDir, DatabaseService) {
        let dir = TempDir::new().unwrap();
        let svc = DatabaseService::open(dir.path(), 4)
            .unwrap()
            .with_sql_limits(limits);
        (dir, svc)
    }

    fn service_with(limits: TransactionLimits) -> (TempDir, DatabaseService) {
        let dir = TempDir::new().unwrap();
        let svc = DatabaseService::open(dir.path(), 4)
            .unwrap()
            .with_transaction_limits(limits);
        (dir, svc)
    }

    /// Fire a transaction's deadline without waiting for it, and wait for
    /// exactly the work the deadline does.
    ///
    /// The clock is frozen only for the jump: the task's own `sleep` is what
    /// is being driven, and leaving time paused afterwards would let the next
    /// transaction's deadline fire the moment the test awaited anything.
    async fn run_deadline(svc: &DatabaseService, handle: &TransactionHandle) {
        let task = svc
            .take_deadline_task(&handle.id)
            .expect("an open transaction has a deadline task");
        tokio::time::pause();
        tokio::time::advance(handle.expires_in + Duration::from_secs(1)).await;
        task.await.expect("the deadline task must not panic");
        tokio::time::resume();
    }

    fn tenant(name: &str) -> TenantId {
        TenantId::for_label(name)
    }

    async fn make_table(svc: &DatabaseService, tenant: &TenantId) {
        svc.exec_statement(
            tenant,
            "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT)",
            &[],
        )
        .await
        .unwrap();
    }

    /// Two of a tenant's batches overlapping is ordinary and must not make
    /// either of them fail. They contend for one file's write lock, and the
    /// loser is expected to wait its turn rather than come back "database is
    /// locked".
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_batches_wait_for_each_other() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let batches: Vec<_> = (0..16)
            .map(|n| {
                let (svc, a) = (svc.clone(), a.clone());
                tokio::spawn(async move {
                    svc.exec_batch(
                        &a,
                        vec![(
                            "INSERT INTO users(name) VALUES(?1)".to_string(),
                            vec![SqlValue::Text(format!("user-{n}"))],
                        )],
                    )
                    .await
                })
            })
            .collect();
        for (n, batch) in batches.into_iter().enumerate() {
            let outcome = batch
                .await
                .unwrap()
                .unwrap_or_else(|e| panic!("batch {n} failed: {e}"));
            assert!(outcome.is_ok(), "batch {n} rolled back: {outcome:?}");
        }

        let found = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(found.rows[0]["n"], serde_json::json!(16));
    }

    #[tokio::test]
    async fn executes_and_queries() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let done = svc
            .exec_statement(
                &a,
                "INSERT INTO users(name) VALUES(?1)",
                &[SqlValue::Text("Alice".into())],
            )
            .await
            .unwrap();
        assert_eq!(done.rows_affected, 1);
        assert_eq!(done.last_insert_id, 1);

        let result = svc
            .fetch_rows(&a, "SELECT id, name FROM users", &[])
            .await
            .unwrap();
        assert_eq!(result.columns, vec!["id".to_string(), "name".to_string()]);
        assert_eq!(
            result.rows,
            serde_json::json!([{ "id": 1, "name": "Alice" }])
        );
    }

    #[tokio::test]
    async fn each_app_gets_its_own_file() {
        let (dir, svc) = service();
        let one = tenant("app-one.production");
        let two = tenant("app-two.production");
        make_table(&svc, &one).await;
        svc.exec_statement(&one, "INSERT INTO users(name) VALUES('Alice')", &[])
            .await
            .unwrap();

        // The second tenant has no such table at all.
        let err = svc.fetch_rows(&two, "SELECT * FROM users", &[]).await;
        assert!(matches!(err, Err(StoreError::Sqlite(_))));

        assert!(
            dir.path()
                .join("db")
                .join(format!("{}.sqlite", tenant("app-one.production")))
                .exists()
        );
        assert!(
            dir.path()
                .join("db")
                .join(format!("{}.sqlite", tenant("app-two.production")))
                .exists()
        );
    }

    #[tokio::test]
    async fn a_batch_is_all_or_nothing() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let failure = svc
            .exec_batch(
                &a,
                vec![
                    (
                        "INSERT INTO users(name) VALUES('Alice')".to_string(),
                        vec![],
                    ),
                    ("INSERT INTO nope(name) VALUES('Bob')".to_string(), vec![]),
                ],
            )
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(failure.index, 1);
        assert_eq!(failure.partial.len(), 1);

        let result = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(result.rows, serde_json::json!([{ "n": 0 }]));

        let done = svc
            .exec_batch(
                &a,
                vec![
                    (
                        "INSERT INTO users(name) VALUES('Alice')".to_string(),
                        vec![],
                    ),
                    ("INSERT INTO users(name) VALUES('Bob')".to_string(), vec![]),
                ],
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.len(), 2);
        let result = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(result.rows, serde_json::json!([{ "n": 2 }]));
    }

    #[tokio::test]
    async fn prepared_statements_are_scoped_to_their_app() {
        let (_dir, svc) = service();
        let one = tenant("app-one.production");
        let two = tenant("app-two.production");
        make_table(&svc, &one).await;

        let id = svc
            .register_statement(&one, "INSERT INTO users(name) VALUES(?1)")
            .await
            .unwrap();
        assert!(matches!(
            svc.prepared_sql(&two, &id),
            Err(StoreError::Tenant(_))
        ));
        assert_eq!(
            svc.prepared_sql(&one, &id).unwrap(),
            "INSERT INTO users(name) VALUES(?1)"
        );
        assert!(matches!(
            svc.prepared_sql(&one, "no-such-handle"),
            Err(StoreError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn prepare_rejects_bad_sql_up_front() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        assert!(matches!(
            svc.register_statement(&a, "SELCT 1").await,
            Err(StoreError::Sqlite(_))
        ));
        assert!(matches!(
            svc.register_statement(&a, "   ").await,
            Err(StoreError::InvalidArgument(_))
        ));
    }

    #[test]
    fn statement_registry_evicts_the_oldest_when_full() {
        let registry = StatementRegistry::default();
        let a = tenant("app-one.production");
        let first = registry.insert(a.clone(), "SELECT 1".to_string());
        for i in 0..MAX_PREPARED_STATEMENTS {
            registry.insert(a.clone(), format!("SELECT {i}"));
        }
        assert_eq!(registry.len(), MAX_PREPARED_STATEMENTS);
        assert!(matches!(
            registry.resolve(&a, &first),
            Err(StoreError::NotFound(_))
        ));
    }

    /// The one row of a successful `Query`, parsed out of the JSON string the
    /// response carries. Going through the wire form is the point of these
    /// tests: what an application can write back is whatever this text says,
    /// not what the row looked like inside the store.
    fn row_of(response: &QueryResponse) -> JsonValue {
        assert!(response.success, "query failed: {:?}", response.error);
        let rows: JsonValue = serde_json::from_str(&response.rows).unwrap();
        rows.as_array().unwrap()[0].clone()
    }

    /// Read a row, change nothing, write it back. It is the ordinary shape of
    /// an application's database use, and it has to work for every value this
    /// service can hand out: a value that can only be read is a column an
    /// application can never update.
    ///
    /// Both halves go through the wire form deliberately. The JSON string the
    /// `Query` answers with is the JSON string the following `Execute` binds
    /// from, which is exactly what the runtime does with it.
    #[tokio::test]
    async fn every_value_a_query_hands_back_can_be_written_straight_back() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        svc.exec_statement(
            &a,
            "CREATE TABLE things(id INTEGER PRIMARY KEY, body BLOB, big INTEGER, note TEXT, \
             ratio REAL, absent TEXT)",
            &[],
        )
        .await
        .unwrap();
        svc.exec_statement(
            &a,
            "INSERT INTO things(id, body, big, note, ratio, absent) \
             VALUES(1, X'DEADBEEF00FF', 9007199254740993, 'hello', 2.5, NULL)",
            &[],
        )
        .await
        .unwrap();

        let columns = "body, big, note, ratio, absent";
        let read = svc
            .run_query(
                &a,
                &format!("SELECT {columns} FROM things WHERE id = 1"),
                None,
            )
            .await
            .unwrap();
        let row = row_of(&read);

        // The two spellings that are not the obvious one, and the reason
        // they are not: a blob has no JSON scalar to be, and 9007199254740993
        // read as a JSON number would reach the application as
        // 9007199254740992.
        assert_eq!(row["body"], serde_json::json!([222, 173, 190, 239, 0, 255]));
        assert_eq!(row["big"], serde_json::json!("9007199254740993"));

        let params = serde_json::json!([
            row["body"],
            row["big"],
            row["note"],
            row["ratio"],
            row["absent"]
        ])
        .to_string();
        let written = svc
            .run_execute(
                &a,
                &format!("INSERT INTO things(id, {columns}) VALUES(2, ?1, ?2, ?3, ?4, ?5)"),
                Some(&params),
            )
            .await
            .unwrap();
        assert!(written.success, "write back failed: {:?}", written.error);

        let again = svc
            .run_query(
                &a,
                &format!("SELECT {columns} FROM things WHERE id = 2"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(row_of(&again), row, "the row came back changed");

        // Same bytes and same digits is most of it; the storage class has to
        // survive too, or the next `typeof` or comparison in the tenant's own
        // SQL answers differently than it did for the row it was copied from.
        let types = svc
            .run_query(
                &a,
                "SELECT typeof(body) AS body, typeof(big) AS big, typeof(note) AS note, \
                 typeof(ratio) AS ratio, typeof(absent) AS absent FROM things WHERE id = 2",
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            row_of(&types),
            serde_json::json!({
                "body": "blob",
                "big": "integer",
                "note": "text",
                "ratio": "real",
                "absent": "null",
            })
        );
    }

    /// The integer spelling has to be the parameter's own storage class, not
    /// something a column's affinity rescues afterwards. Bound to a bare
    /// expression there is no column and no affinity, so this is the rule in
    /// [`bind_string`] and nothing else.
    #[tokio::test]
    async fn a_wide_integer_binds_as_an_integer_with_no_column_to_help_it() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        let out = svc
            .run_query(
                &a,
                "SELECT typeof(?1) AS class, ?1 + 0 AS value",
                Some(r#"["-9007199254740993"]"#),
            )
            .await
            .unwrap();
        assert_eq!(
            row_of(&out),
            serde_json::json!({ "class": "integer", "value": "-9007199254740993" })
        );
    }

    /// Every storage class a cell can hold, out and straight back in, checked
    /// as the pair of functions that carry it rather than through a database.
    #[test]
    fn the_wire_form_of_a_cell_is_a_form_that_binds_back() {
        let cases = [
            (SqlValue::Null, "null"),
            (SqlValue::Integer(0), "0"),
            (SqlValue::Integer(-1), "-1"),
            // The last integer a double still holds exactly, on both sides of
            // zero: a number here, a string one step further out.
            (SqlValue::Integer(MAX_SAFE_JSON_INTEGER), "9007199254740991"),
            (
                SqlValue::Integer(-MAX_SAFE_JSON_INTEGER),
                "-9007199254740991",
            ),
            (
                SqlValue::Integer(MAX_SAFE_JSON_INTEGER + 1),
                "\"9007199254740992\"",
            ),
            (SqlValue::Integer(i64::MAX), "\"9223372036854775807\""),
            (SqlValue::Integer(i64::MIN), "\"-9223372036854775808\""),
            (SqlValue::Real(2.5), "2.5"),
            (SqlValue::Text("hello".into()), "\"hello\""),
            (SqlValue::Blob(Vec::new()), "[]"),
            (SqlValue::Blob(vec![0, 127, 255]), "[0,127,255]"),
        ];
        for (value, expected) in cases {
            let wire = sql_to_json(ValueRef::from(&value));
            assert_eq!(wire.to_string(), expected, "wire form of {value:?}");
            let back = decode_params(Some(&JsonValue::Array(vec![wire]).to_string())).unwrap();
            assert_eq!(back, vec![value.clone()], "{value:?} did not survive");
        }
    }

    #[test]
    fn decodes_blobs_and_wide_integers_from_the_shapes_a_query_returns() {
        assert_eq!(
            decode_params(Some(r#"[[222, 173, 190, 239], [], "9007199254740992"]"#)).unwrap(),
            vec![
                SqlValue::Blob(vec![222, 173, 190, 239]),
                SqlValue::Blob(Vec::new()),
                SqlValue::Integer(9_007_199_254_740_992),
            ]
        );
        // A string is TEXT unless it is exactly how this service spells an
        // integer no JSON number can carry. Anything a number could have
        // carried, and any other spelling of one that could not, is the text
        // it looks like - otherwise a tenant keeping digits in a TEXT column
        // would find them coming back as an integer.
        for text in [
            "12",
            "9007199254740991",
            "+9007199254740992",
            "09007199254740992",
            " 9007199254740992",
            "9007199254740992.0",
            "99999999999999999999",
        ] {
            assert_eq!(
                decode_params(Some(&serde_json::json!([text]).to_string())).unwrap(),
                vec![SqlValue::Text(text.to_string())],
                "{text} should bind as text"
            );
        }
    }

    #[test]
    fn refuses_array_elements_that_are_not_bytes() {
        for bad in [
            "[[256]]",
            "[[-1]]",
            "[[2.7]]",
            "[[\"ff\"]]",
            "[[null]]",
            "[[[1]]]",
            "[[{}]]",
        ] {
            let message = match decode_params(Some(bad)) {
                Err(StoreError::InvalidArgument(m)) => m,
                other => panic!("{bad} should be refused, got {other:?}"),
            };
            // The offending element, not just the parameter: a byte array is
            // built in a loop and "one of these is wrong" does not locate it.
            assert!(
                message.contains("params[0][0]"),
                "{bad} was refused without naming the element: {message}"
            );
        }
        assert!(matches!(
            decode_params(Some(r#"[{"a": 1}]"#)),
            Err(StoreError::InvalidArgument(_))
        ));
    }

    #[test]
    fn decodes_scalar_params() {
        assert!(decode_params(None).unwrap().is_empty());
        assert!(decode_params(Some("")).unwrap().is_empty());
        assert_eq!(
            decode_params(Some(r#"[1, 2.5, "x", null, true]"#)).unwrap(),
            vec![
                SqlValue::Integer(1),
                SqlValue::Real(2.5),
                SqlValue::Text("x".into()),
                SqlValue::Null,
                SqlValue::Integer(1),
            ]
        );
        for bad in [r#"{"a":1}"#, r#"[{"a":1}]"#, "not json"] {
            assert!(
                matches!(
                    decode_params(Some(bad)),
                    Err(StoreError::InvalidArgument(_))
                ),
                "{bad} should be rejected"
            );
        }
    }

    /// The whole reason this exists: read a row, decide from what it says,
    /// write accordingly - all inside one transaction, which `Batch` cannot
    /// do because it needs every statement up front.
    #[tokio::test]
    async fn a_transaction_reads_then_decides_then_writes() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        svc.exec_statement(
            &a,
            "CREATE TABLE seats(id INTEGER PRIMARY KEY, taken INTEGER)",
            &[],
        )
        .await
        .unwrap();
        svc.exec_statement(&a, "INSERT INTO seats(id, taken) VALUES(1, 0)", &[])
            .await
            .unwrap();

        let tx = svc.open_transaction(&a, None).await.unwrap();
        let seen = svc
            .fetch_rows_in_transaction(&a, &tx.id, "SELECT taken FROM seats WHERE id = 1", &[])
            .await
            .unwrap();
        // The branch a batch cannot express: what is written depends on what
        // was read, and the row cannot change underneath in between.
        assert_eq!(seen.rows[0]["taken"], serde_json::json!(0));
        svc.exec_in_transaction(&a, &tx.id, "UPDATE seats SET taken = 1 WHERE id = 1", &[])
            .await
            .unwrap();
        svc.commit(&a, &tx.id).await.unwrap();

        let after = svc
            .fetch_rows(&a, "SELECT taken FROM seats WHERE id = 1", &[])
            .await
            .unwrap();
        assert_eq!(after.rows[0]["taken"], serde_json::json!(1));
    }

    #[tokio::test]
    async fn a_transaction_sees_its_own_writes_and_nobody_else_does_until_it_commits() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let tx = svc.open_transaction(&a, None).await.unwrap();
        svc.exec_in_transaction(&a, &tx.id, "INSERT INTO users(name) VALUES('Alice')", &[])
            .await
            .unwrap();

        let inside = svc
            .fetch_rows_in_transaction(&a, &tx.id, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(inside.rows, serde_json::json!([{ "n": 1 }]));
        let outside = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(
            outside.rows,
            serde_json::json!([{ "n": 0 }]),
            "an uncommitted write must not be visible outside its transaction"
        );

        svc.commit(&a, &tx.id).await.unwrap();
        let after = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(after.rows, serde_json::json!([{ "n": 1 }]));
    }

    #[tokio::test]
    async fn rolling_back_discards_everything_the_transaction_did() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let tx = svc.open_transaction(&a, None).await.unwrap();
        svc.exec_in_transaction(&a, &tx.id, "INSERT INTO users(name) VALUES('Alice')", &[])
            .await
            .unwrap();
        svc.roll_back(&a, &tx.id).await.unwrap();

        let after = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(after.rows, serde_json::json!([{ "n": 0 }]));

        // The spent handle says what happened to it rather than going blank.
        let err = svc
            .exec_in_transaction(&a, &tx.id, "INSERT INTO users(name) VALUES('Bob')", &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already rolled back"), "{err}");
    }

    #[tokio::test]
    async fn a_committed_handle_says_it_was_committed() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let tx = svc.open_transaction(&a, None).await.unwrap();
        svc.commit(&a, &tx.id).await.unwrap();

        let err = svc.commit(&a, &tx.id).await.unwrap_err();
        assert!(err.to_string().contains("already committed"), "{err}");
        let unknown = svc.commit(&a, "no-such-handle").await.unwrap_err();
        assert!(matches!(unknown, StoreError::NotFound(_)), "{unknown}");
    }

    #[tokio::test]
    async fn a_transaction_handle_belongs_to_the_tenant_that_opened_it() {
        let (_dir, svc) = service();
        let one = tenant("app-one.production");
        let two = tenant("app-two.production");
        make_table(&svc, &one).await;

        let tx = svc.open_transaction(&one, None).await.unwrap();
        for attempt in [
            svc.exec_in_transaction(&two, &tx.id, "INSERT INTO users(name) VALUES('x')", &[])
                .await
                .err(),
            svc.fetch_rows_in_transaction(&two, &tx.id, "SELECT 1", &[])
                .await
                .err(),
            svc.commit(&two, &tx.id).await.err(),
            svc.roll_back(&two, &tx.id).await.err(),
        ] {
            let error = attempt.expect("another tenant must not touch this handle");
            assert!(matches!(error, StoreError::Tenant(_)), "{error}");
        }

        // Still the owner's to use, and still holding its own work.
        svc.exec_in_transaction(&one, &tx.id, "INSERT INTO users(name) VALUES('Alice')", &[])
            .await
            .unwrap();
        svc.commit(&one, &tx.id).await.unwrap();

        // A handle that has ended is still not the other tenant's business.
        let error = svc.commit(&two, &tx.id).await.unwrap_err();
        assert!(matches!(error, StoreError::Tenant(_)), "{error}");
    }

    /// A handler that opens a transaction and then awaits something slow is
    /// sitting on its tenant's writer. The deadline is what takes it back.
    #[tokio::test]
    async fn the_deadline_rolls_a_forgotten_transaction_back_and_says_so() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let tx = svc.open_transaction(&a, None).await.unwrap();
        svc.exec_in_transaction(&a, &tx.id, "INSERT INTO users(name) VALUES('Alice')", &[])
            .await
            .unwrap();
        run_deadline(&svc, &tx).await;

        let err = svc
            .exec_in_transaction(&a, &tx.id, "INSERT INTO users(name) VALUES('Bob')", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Expired(_)), "{err}");
        let message = err.to_string();
        assert!(
            message.contains("rolled back") && message.contains("deadline"),
            "the caller has to be told what happened to its work, not that the handle is \
             unknown: {message}"
        );

        let after = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(after.rows, serde_json::json!([{ "n": 0 }]));
    }

    /// The dropped-client case: nothing ever commits or rolls back. The
    /// connection has to come back to the pool clean, without the write lock
    /// the transaction was holding.
    #[tokio::test]
    async fn a_handle_abandoned_without_commit_leaves_no_lock_behind() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let tx = svc.open_transaction(&a, None).await.unwrap();
        // A write, so the transaction really is holding the file's write lock
        // rather than merely occupying a connection.
        svc.exec_in_transaction(&a, &tx.id, "INSERT INTO users(name) VALUES('ghost')", &[])
            .await
            .unwrap();
        run_deadline(&svc, &tx).await;

        assert_eq!(
            svc.transactions.open_count(),
            0,
            "the abandoned transaction must not still be counted as open"
        );

        // More writes than the pool has connections, so every connection in
        // it is exercised. Any one still carrying the abandoned transaction
        // would wait out the busy timeout and then fail here.
        for n in 0..8 {
            svc.exec_statement(
                &a,
                "INSERT INTO users(name) VALUES(?1)",
                &[SqlValue::Text(format!("after-{n}"))],
            )
            .await
            .unwrap();
        }
        let after = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(
            after.rows,
            serde_json::json!([{ "n": 8 }]),
            "the abandoned insert must be gone and every later write must have landed"
        );

        // And the tenant can take the write lock again, which it could not if
        // the abandoned transaction still held it.
        let next = svc.open_transaction(&a, None).await.unwrap();
        svc.exec_in_transaction(&a, &next.id, "DELETE FROM users", &[])
            .await
            .unwrap();
        svc.commit(&a, &next.id).await.unwrap();
    }

    #[tokio::test]
    async fn a_tenant_cannot_open_more_transactions_than_its_ceiling() {
        let (_dir, svc) = service_with(TransactionLimits {
            max_open_per_tenant: 1,
            ..TransactionLimits::default()
        });
        let one = tenant("app-one.production");
        let two = tenant("app-two.production");

        let held = svc.open_transaction(&one, None).await.unwrap();
        let refused = svc.open_transaction(&one, None).await.unwrap_err();
        assert!(matches!(refused, StoreError::Exhausted(_)), "{refused}");
        let message = refused.to_string();
        assert!(
            message.contains("ceiling") && message.contains("tenant"),
            "a refusal has to say why: {message}"
        );

        // The ceiling is per tenant, so another tenant is unaffected.
        let other = svc.open_transaction(&two, None).await.unwrap();
        svc.commit(&two, &other.id).await.unwrap();

        // And the slot comes back when the transaction ends.
        svc.commit(&one, &held.id).await.unwrap();
        let again = svc.open_transaction(&one, None).await.unwrap();
        svc.roll_back(&one, &again.id).await.unwrap();
    }

    #[tokio::test]
    async fn the_server_wide_ceiling_refuses_across_tenants() {
        let (_dir, svc) = service_with(TransactionLimits {
            max_open_total: 1,
            ..TransactionLimits::default()
        });
        let one = tenant("app-one.production");
        let two = tenant("app-two.production");

        let held = svc.open_transaction(&one, None).await.unwrap();
        let refused = svc.open_transaction(&two, None).await.unwrap_err();
        assert!(matches!(refused, StoreError::Exhausted(_)), "{refused}");
        assert!(refused.to_string().contains("server"), "{refused}");

        svc.commit(&one, &held.id).await.unwrap();
        let now_free = svc.open_transaction(&two, None).await.unwrap();
        svc.commit(&two, &now_free.id).await.unwrap();
    }

    #[tokio::test]
    async fn transaction_control_sql_is_refused_inside_a_transaction() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let tx = svc.open_transaction(&a, None).await.unwrap();
        for sql in [
            "COMMIT",
            "commit;",
            "ROLLBACK",
            "BEGIN",
            "END",
            "SAVEPOINT s",
        ] {
            let err = svc
                .exec_in_transaction(&a, &tx.id, sql, &[])
                .await
                .unwrap_err();
            assert!(
                matches!(err, StoreError::InvalidArgument(_)),
                "{sql} must be refused: {err}"
            );
        }
        // The transaction is untouched by the refusals.
        svc.exec_in_transaction(&a, &tx.id, "INSERT INTO users(name) VALUES('Alice')", &[])
            .await
            .unwrap();
        svc.commit(&a, &tx.id).await.unwrap();
        let after = svc
            .fetch_rows(&a, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(after.rows, serde_json::json!([{ "n": 1 }]));
    }

    #[tokio::test]
    async fn a_caller_may_ask_for_less_time_but_never_for_more() {
        let (_dir, svc) = service_with(TransactionLimits {
            deadline: Duration::from_secs(2),
            ..TransactionLimits::default()
        });
        let a = tenant("app-one.production");

        let shorter = svc
            .open_transaction(&a, Some(Duration::from_millis(500)))
            .await
            .unwrap();
        assert_eq!(shorter.expires_in, Duration::from_millis(500));
        svc.commit(&a, &shorter.id).await.unwrap();

        let clamped = svc
            .open_transaction(&a, Some(Duration::from_secs(3600)))
            .await
            .unwrap();
        assert_eq!(
            clamped.expires_in,
            Duration::from_secs(2),
            "the configured ceiling is a ceiling, not a suggestion"
        );
        svc.commit(&a, &clamped.id).await.unwrap();

        let refused = svc
            .open_transaction(&a, Some(Duration::ZERO))
            .await
            .unwrap_err();
        assert!(
            matches!(refused, StoreError::InvalidArgument(_)),
            "{refused}"
        );
    }

    /// The client that disappears while `BeginTransaction` is still waiting.
    /// Nothing was opened, so no deadline task exists to clean up after it,
    /// and the slot it had taken has to come back on its own or the tenant is
    /// permanently one transaction poorer.
    #[tokio::test]
    async fn a_begin_that_never_finishes_gives_its_slot_back() {
        let dir = TempDir::new().unwrap();
        let svc = DatabaseService::open(dir.path(), 1).unwrap();
        let a = tenant("app-one.production");
        // Every connection the pool has, so a later caller parks on the
        // pool's semaphore for as long as the test likes. All of them, not
        // one: `open` raises the pool to `MIN_TENANT_CONNECTIONS`, and with a
        // connection still free the call below can get all the way through
        // before its deadline is looked at, which is a race, not a test.
        let mut hogs = Vec::new();
        for _ in 0..svc.connections_per_tenant {
            hogs.push(svc.pool(&a).checkout().await.unwrap());
        }

        // A zero timeout polls the call exactly once - long enough to take a
        // slot and park on the pool - and then drops it, which is what tonic
        // does to a handler whose client went away.
        let abandoned = tokio::time::timeout(Duration::ZERO, svc.open_transaction(&a, None)).await;
        assert!(
            abandoned.is_err(),
            "the pool has no connection to give, so the call cannot have finished"
        );
        assert_eq!(
            svc.transactions.open_count(),
            0,
            "the slot taken by a call that never finished has to be given back"
        );

        drop(hogs);
        let tx = svc.open_transaction(&a, None).await.unwrap();
        svc.commit(&a, &tx.id).await.unwrap();
    }

    /// `BEGIN IMMEDIATE` is the difference between contention that surfaces
    /// now, at begin, and contention that surfaces at commit as
    /// `SQLITE_BUSY_SNAPSHOT` - after the caller has been told every one of
    /// its statements succeeded. So the write lock has to be held from the
    /// moment the transaction opens, before it has written anything.
    ///
    /// The outsider here opens the file itself with no busy timeout, so it
    /// reports the lock instead of waiting for it. A deferred transaction
    /// would let it straight through.
    #[tokio::test]
    async fn a_transaction_holds_the_write_lock_from_the_moment_it_opens() {
        let (dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;
        let path = dir
            .path()
            .join("db")
            .join(format!("{}.sqlite", tenant("app-one.production")));

        let tx = svc.open_transaction(&a, None).await.unwrap();
        // Not one statement has run inside the transaction yet.
        let outsider = rusqlite::Connection::open(&path).unwrap();
        outsider.busy_timeout(Duration::ZERO).unwrap();
        let blocked = outsider.execute_batch("BEGIN IMMEDIATE");
        assert!(
            blocked.is_err(),
            "the write lock must already be held, or a read-then-write transaction would only \
             find out at commit"
        );

        svc.commit(&a, &tx.id).await.unwrap();
        outsider
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
            .expect("the lock must be released by the commit");
    }

    /// The handlers themselves, not the methods under them: this is what
    /// proves the five RPCs are wired to the service the server registers,
    /// that a dead handle is reported inside the response rather than as a
    /// transport failure, and that the field names the runtime will read are
    /// the ones the server fills in.
    #[tokio::test]
    async fn the_transaction_rpcs_are_wired_to_the_service() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        make_table(&svc, &a).await;

        let opened =
            Database::begin_transaction(&svc, tagged(&a, BeginTransactionRequest::default()))
                .await
                .unwrap()
                .into_inner();
        assert!(opened.success, "{:?}", opened.error);
        assert!(
            opened.expires_in_ms > 0,
            "the caller has to be told how long it has"
        );
        let id = opened.transaction_id.clone();

        let wrote = Database::execute_in_transaction(
            &svc,
            tagged(
                &a,
                ExecuteInTransactionRequest {
                    transaction_id: id.clone(),
                    sql: "INSERT INTO users(name) VALUES(?1)".to_string(),
                    params: Some(r#"["Alice"]"#.to_string()),
                },
            ),
        )
        .await
        .unwrap()
        .into_inner();
        assert!(wrote.success, "{:?}", wrote.error);
        assert_eq!(wrote.rows_affected, 1);

        let read = Database::query_in_transaction(
            &svc,
            tagged(
                &a,
                QueryInTransactionRequest {
                    transaction_id: id.clone(),
                    sql: "SELECT name FROM users".to_string(),
                    params: None,
                },
            ),
        )
        .await
        .unwrap()
        .into_inner();
        assert!(read.success, "{:?}", read.error);
        assert!(read.rows.contains("Alice"), "{}", read.rows);

        let committed = Database::commit_transaction(
            &svc,
            tagged(
                &a,
                CommitTransactionRequest {
                    transaction_id: id.clone(),
                },
            ),
        )
        .await
        .unwrap()
        .into_inner();
        assert!(committed.success, "{:?}", committed.error);

        // A spent handle is a response with an error in it, not a broken
        // transport: the store is fine, the caller's handle is not.
        let again = Database::rollback_transaction(
            &svc,
            tagged(&a, RollbackTransactionRequest { transaction_id: id }),
        )
        .await
        .unwrap()
        .into_inner();
        assert!(!again.success);
        assert!(
            again
                .error
                .unwrap_or_default()
                .contains("already committed"),
            "the response has to say what happened to the handle"
        );
    }

    /// A request carrying the tenant header the handlers read.
    fn tagged<T>(tenant: &TenantId, message: T) -> Request<T> {
        tenant.request(message)
    }

    #[test]
    fn the_per_tenant_ceiling_leaves_a_connection_for_ordinary_statements() {
        let asked = TransactionLimits {
            max_open_per_tenant: 100,
            ..TransactionLimits::default()
        };
        assert_eq!(clamp_to_pool(asked, 4).max_open_per_tenant, 3);
        assert_eq!(clamp_to_pool(asked, 2).max_open_per_tenant, 1);
        // A one-connection pool has nothing to hold back, so the honest
        // answer is zero: no transaction may pin the only connection its
        // tenant has. `DatabaseService::open` never builds such a pool.
        assert_eq!(clamp_to_pool(asked, 1).max_open_per_tenant, 0);
        assert_eq!(
            clamp_to_pool(TransactionLimits::default(), 4).max_open_per_tenant,
            DEFAULT_MAX_OPEN_TRANSACTIONS_PER_TENANT
        );
    }

    // -----------------------------------------------------------------
    // A tenant's SQL cannot leave its own file
    // -----------------------------------------------------------------

    /// What the message has to be for a refusal to be worth anything: the
    /// caller has to be able to read what it may not do.
    fn refusal(error: StoreError) -> String {
        match error {
            StoreError::Refused(what) => what,
            other => panic!("expected a refusal, got {other}"),
        }
    }

    /// The whole attack, end to end: learn your own path, edit one component,
    /// open the neighbour's file, read it, write it.
    #[tokio::test]
    async fn one_tenant_cannot_attach_another_tenants_file() {
        let (dir, svc) = service();
        let victim = tenant("victim.production");
        let attacker = tenant("attacker.production");

        svc.exec_statement(&victim, "CREATE TABLE secrets(v TEXT)", &[])
            .await
            .unwrap();
        svc.exec_statement(&victim, "INSERT INTO secrets VALUES('card-number')", &[])
            .await
            .unwrap();

        // Step one is already refused: the store does not tell a tenant where
        // its file is, so there is no path to edit.
        let disclosed = svc
            .fetch_rows(&attacker, "PRAGMA database_list", &[])
            .await
            .unwrap_err();
        assert!(
            refusal(disclosed).contains("database_list"),
            "the refusal has to name the pragma"
        );

        // Step two is refused even when the path is handed over anyway, which
        // is the case that matters: knowing the layout must not be enough.
        let path = dir.path().join("db").join("victim.production.sqlite");
        let attach = svc
            .exec_statement(
                &attacker,
                &format!("ATTACH DATABASE '{}' AS stolen", path.display()),
                &[],
            )
            .await
            .unwrap_err();
        assert!(refusal(attach).contains("ATTACH"));

        // Prepare refuses it as well. Unlike VACUUM, this one is the
        // authorizer speaking: it is consulted while the statement is being
        // prepared, so there is no handle to come back with later.
        let prepared = svc
            .register_statement(
                &attacker,
                &format!("ATTACH DATABASE '{}' AS stolen", path.display()),
            )
            .await
            .unwrap_err();
        assert!(refusal(prepared).contains("ATTACH"));

        // And the victim's rows are still only the victim's.
        let rows = svc
            .fetch_rows(&victim, "SELECT v FROM secrets", &[])
            .await
            .unwrap();
        assert_eq!(rows.rows, serde_json::json!([{ "v": "card-number" }]));
    }

    /// The eponymous virtual table spelling of a pragma reaches the same
    /// authorizer action, so one allowlist covers both. Case too: SQLite
    /// hands the name over as the caller wrote it.
    #[tokio::test]
    async fn the_other_spellings_of_a_refused_pragma_are_refused_too() {
        let (_dir, svc) = service();
        let app = tenant("app-one.production");
        for sql in [
            "SELECT * FROM pragma_database_list",
            "PRAGMA DATABASE_LIST",
            "  \n\t pragma database_list",
            "PRAGMA main.database_list",
            "PRAGMA compile_options",
        ] {
            let error = svc.fetch_rows(&app, sql, &[]).await.unwrap_err();
            assert!(
                matches!(error, StoreError::Refused(_)),
                "{sql} was not refused: {error}"
            );
        }
    }

    /// Writing a file the caller names, which is the other half of the hole:
    /// nothing is created outside the tenant's own file.
    #[tokio::test]
    async fn a_tenant_cannot_write_a_file_it_names() {
        let (_dir, svc) = service();
        let elsewhere = TempDir::new().unwrap();
        let app = tenant("app-one.production");
        svc.exec_statement(&app, "CREATE TABLE t(v TEXT)", &[])
            .await
            .unwrap();

        let vacuumed = elsewhere.path().join("vacuumed.db");
        let error = svc
            .exec_statement(&app, &format!("VACUUM INTO '{}'", vacuumed.display()), &[])
            .await
            .unwrap_err();
        assert!(refusal(error).contains("VACUUM"));

        // Prepare has to refuse it as well, or the handle it returns is a way
        // to run it later. This is the case the keyword check exists for: the
        // authorizer only sees the attach while the statement runs.
        let error = svc
            .register_statement(&app, &format!("VACUUM INTO '{}'", vacuumed.display()))
            .await
            .unwrap_err();
        assert!(refusal(error).contains("VACUUM"));

        // ATTACH creates the file it names, so it is a second way to write
        // one; it is refused whether or not the target exists.
        let created = elsewhere.path().join("created-by-attach.db");
        let error = svc
            .exec_statement(
                &app,
                &format!("ATTACH DATABASE '{}' AS made", created.display()),
                &[],
            )
            .await
            .unwrap_err();
        assert!(refusal(error).contains("ATTACH"));

        assert!(
            !vacuumed.exists(),
            "VACUUM INTO wrote {}",
            vacuumed.display()
        );
        assert!(!created.exists(), "ATTACH created {}", created.display());
        assert_eq!(
            std::fs::read_dir(elsewhere.path()).unwrap().count(),
            0,
            "nothing at all may appear outside the data directory"
        );
    }

    /// The floor under the authorizer. Even with the callback removed, a
    /// connection from a tenant's pool has no attach slot to spend.
    #[tokio::test]
    async fn a_tenant_pool_has_no_attach_slots_at_all() {
        let (_dir, svc) = service();
        let app = tenant("app-one.production");
        let attached: i32 = svc
            .pool(&app)
            .with_conn(|conn| Ok(conn.limit(rusqlite::limits::Limit::SQLITE_LIMIT_ATTACHED)?))
            .await
            .unwrap();
        assert_eq!(attached, 0);
    }

    /// Refused however they are dressed up, and nothing reaches the file
    /// system. Which layer catches which of these is deliberately not
    /// asserted here - the keyword check takes the ones it can name and the
    /// authorizer takes the rest, and the property that matters is that no
    /// spelling gets through either.
    #[tokio::test]
    async fn creative_spelling_does_not_get_past_the_authorizer() {
        let (_dir, svc) = service();
        let elsewhere = TempDir::new().unwrap();
        let app = tenant("app-one.production");
        svc.exec_statement(&app, "CREATE TABLE t(v TEXT)", &[])
            .await
            .unwrap();

        let copy = elsewhere.path().join("copy.db");
        let attached = elsewhere.path().join("attached.db");
        for sql in [
            format!(
                "/* not a vacuum, honest */ VACUUM INTO '{}'",
                copy.display()
            ),
            format!("-- a comment\nvAcUuM iNtO '{}'", copy.display()),
            format!("/* x */ attach database '{}' as made", attached.display()),
        ] {
            let error = svc.exec_statement(&app, &sql, &[]).await.unwrap_err();
            assert!(
                matches!(error, StoreError::Refused(_)),
                "{sql} was not refused: {error}"
            );
        }
        assert_eq!(
            std::fs::read_dir(elsewhere.path()).unwrap().count(),
            0,
            "a comment must not be enough to write a file"
        );
    }

    /// The module docs promise these six by name. They are only available if
    /// the PRAGMA allowlist lets them build themselves - R*Tree's `xCreate`
    /// reads `PRAGMA page_size` and fails outright without it - so the
    /// promise is checked by creating one table with each module and using
    /// it, not by trusting a compile flag. A future tightening of
    /// [`TENANT_PRAGMAS`] or [`TENANT_READ_ONLY_PRAGMAS`] breaks this test
    /// instead of somebody's application.
    #[tokio::test]
    async fn every_compiled_in_virtual_table_module_is_usable() {
        let (_dir, svc) = service();
        let app = tenant("app-one.production");
        for sql in [
            "CREATE VIRTUAL TABLE notes3 USING fts3(body)",
            "CREATE VIRTUAL TABLE notes4 USING fts4(body)",
            "CREATE VIRTUAL TABLE notes5 USING fts5(body)",
            "CREATE VIRTUAL TABLE notes5_terms USING fts5vocab(notes5, 'row')",
            "CREATE VIRTUAL TABLE pages USING dbstat",
            "CREATE VIRTUAL TABLE places USING rtree(id, min_x, max_x, min_y, max_y)",
        ] {
            svc.exec_statement(&app, sql, &[])
                .await
                .unwrap_or_else(|e| panic!("{sql} was refused: {e}"));
        }

        // Created is not the same as working: each one is written to and read
        // back through the index it maintains.
        for table in ["notes3", "notes4", "notes5"] {
            svc.exec_statement(
                &app,
                &format!("INSERT INTO {table}(body) VALUES('a haystack')"),
                &[],
            )
            .await
            .unwrap();
            let hits = svc
                .fetch_rows(
                    &app,
                    &format!("SELECT count(*) AS n FROM {table} WHERE body MATCH 'haystack'"),
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(hits.rows, serde_json::json!([{ "n": 1 }]), "{table}");
        }
        let terms = svc
            .fetch_rows(&app, "SELECT term FROM notes5_terms ORDER BY term", &[])
            .await
            .unwrap();
        assert_eq!(
            terms.rows,
            serde_json::json!([{ "term": "a" }, { "term": "haystack" }])
        );

        svc.exec_statement(
            &app,
            "INSERT INTO places(id, min_x, max_x, min_y, max_y) VALUES(1, 0.0, 1.0, 0.0, 1.0)",
            &[],
        )
        .await
        .unwrap();
        let inside = svc
            .fetch_rows(
                &app,
                "SELECT id FROM places WHERE min_x <= 0.5 AND max_x >= 0.5",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(inside.rows, serde_json::json!([{ "id": 1 }]));

        // dbstat reports on the tenant's own pages, which is also where the
        // page size it may read is visible anyway.
        let pages = svc
            .fetch_rows(&app, "SELECT count(*) AS n FROM pages", &[])
            .await
            .unwrap();
        assert!(
            pages.rows[0]["n"].as_i64().unwrap_or_default() > 0,
            "dbstat returned nothing: {:?}",
            pages.rows
        );
    }

    /// `page_size` is readable because R*Tree needs it and it discloses
    /// nothing - the caller's own page size, already in `dbstat.pgsize`.
    /// Setting it is a different statement and stays refused.
    #[tokio::test]
    async fn a_read_only_pragma_may_be_read_and_not_set() {
        let (_dir, svc) = service();
        let app = tenant("app-one.production");
        for name in TENANT_READ_ONLY_PRAGMAS {
            let read = svc
                .fetch_rows(&app, &format!("PRAGMA {name}"), &[])
                .await
                .unwrap_or_else(|e| panic!("PRAGMA {name} was refused: {e}"));
            let before = read.rows[0][*name].as_i64();
            assert!(before.is_some(), "PRAGMA {name} returned {:?}", read.rows);

            for sql in [
                format!("PRAGMA {name} = 8192"),
                format!("PRAGMA main.{name} = 8192"),
            ] {
                let what = refusal(svc.exec_statement(&app, &sql, &[]).await.unwrap_err());
                assert!(
                    what.starts_with(&format!("PRAGMA {name}")) && what.contains("may only read"),
                    "{sql} was not refused as a write: {what}"
                );
            }
            // And the write did not take: the file is what it was.
            let again = svc
                .fetch_rows(&app, &format!("PRAGMA {name}"), &[])
                .await
                .unwrap();
            assert_eq!(again.rows[0][*name].as_i64(), before, "PRAGMA {name}");
        }
    }

    /// SQLite only shows the authorizer an `ATTACH` filename when it is a
    /// string literal, so these two spellings arrive with the name missing.
    /// They have to be refused on the action alone: left to
    /// `SQLITE_LIMIT_ATTACHED` they come back as "too many attached
    /// databases", which tells the caller nothing and - because it is not a
    /// [`StoreError::Refused`], the one variant [`note_refusal`] logs - puts
    /// no line in front of an operator watching for a tenant probing at this.
    #[tokio::test]
    async fn an_attach_that_hides_its_filename_is_refused_like_any_other() {
        let (dir, svc) = service();
        let elsewhere = TempDir::new().unwrap();
        let app = tenant("app-one.production");
        let victim = dir.path().join("db").join("victim.production.sqlite");
        let created = elsewhere.path().join("made.db");

        for sql in [
            format!("ATTACH DATABASE ('{}' || '') AS stolen", victim.display()),
            format!("ATTACH DATABASE (SELECT '{}') AS stolen", victim.display()),
            format!("ATTACH DATABASE ('{}' || '') AS made", created.display()),
            "ATTACH DATABASE (SELECT char(47)) AS made".to_string(),
            "DETACH DATABASE ('stolen' || '')".to_string(),
        ] {
            let error = svc.exec_statement(&app, &sql, &[]).await.unwrap_err();
            let what = refusal(error);
            assert!(
                what.starts_with("ATTACH") || what.starts_with("DETACH"),
                "{sql} said: {what}"
            );
            // The message has to admit the filename is missing rather than
            // print an empty one as if SQLite had handed it over.
            assert!(
                !what.contains("named \"\""),
                "{sql} claimed a filename it never had: {what}"
            );
        }

        // Prepare refuses them too, because the authorizer runs while the
        // statement is prepared: no handle comes back to run later.
        let prepared = svc
            .register_statement(
                &app,
                &format!("ATTACH DATABASE (SELECT '{}') AS x", victim.display()),
            )
            .await
            .unwrap_err();
        assert!(refusal(prepared).starts_with("ATTACH"));

        assert_eq!(
            std::fs::read_dir(elsewhere.path()).unwrap().count(),
            0,
            "an expression filename must not be enough to create a file"
        );
    }

    /// `Prepare` is where a caller can still do something about a refusal,
    /// and it is the only call that ever sees a `VACUUM INTO` before it runs:
    /// the authorizer is shown the vacuum's `ATTACH` by *running* the vacuum.
    /// So the keyword check has to read the word SQLite would read, past
    /// whatever the caller put in front of it - and say `VACUUM`, not
    /// `ATTACH`, about a statement that never spelled `ATTACH`.
    #[tokio::test]
    async fn prepare_refuses_a_vacuum_however_it_is_spelled() {
        let (_dir, svc) = service();
        let elsewhere = TempDir::new().unwrap();
        let app = tenant("app-one.production");
        let copy = elsewhere.path().join("copy.db");

        for sql in [
            format!("VACUUM INTO '{}'", copy.display()),
            format!("/* x */ VACUUM INTO '{}'", copy.display()),
            format!("-- x\nvacuum into '{}'", copy.display()),
            format!("; VACUUM INTO '{}'", copy.display()),
            format!("/* a */ /* b */\n\t;-- c\nVACUUM INTO '{}'", copy.display()),
            "VACUUM".to_string(),
        ] {
            let error = svc.register_statement(&app, &sql).await.unwrap_err();
            let what = refusal(error);
            assert!(what.starts_with("VACUUM"), "Prepare of {sql} said: {what}");
        }

        assert_eq!(
            std::fs::read_dir(elsewhere.path()).unwrap().count(),
            0,
            "no vacuum may have run"
        );
        assert_eq!(
            svc.statements.len(),
            0,
            "no handle may have been handed out for a statement that can never run"
        );
    }

    /// The word the checks above are reading. An unterminated block comment
    /// is left alone on purpose: SQLite rejects the statement, and there is
    /// no keyword to guess at.
    #[test]
    fn the_leading_keyword_is_the_one_sqlite_would_read() {
        for (sql, want) in [
            ("VACUUM", "VACUUM"),
            ("  vacuum into 'x'", "VACUUM"),
            ("/* c */VACUUM", "VACUUM"),
            ("-- c\nVACUUM", "VACUUM"),
            ("--c", ""),
            (";;\n/* a */ ; -- b\n\tVACUUM", "VACUUM"),
            ("/* unterminated VACUUM", ""),
            ("SELECT 1; VACUUM", "SELECT"),
            ("BEGIN;", "BEGIN"),
            ("VACUUM(", "VACUUM"),
            ("", ""),
        ] {
            assert_eq!(leading_keyword(sql), want, "{sql:?}");
        }
    }

    /// The refusal an application actually sees, spelled out. A message that
    /// only said "not authorized" would send a developer to the wrong place.
    #[tokio::test]
    async fn a_refusal_says_what_was_refused() {
        let (_dir, svc) = service();
        let app = tenant("app-one.production");
        for (sql, wanted) in [
            (
                "ATTACH DATABASE '/tmp/other.sqlite' AS x",
                "refused: ATTACH, because a tenant's SQL reaches its own database file",
            ),
            (
                "PRAGMA database_list",
                "refused: PRAGMA database_list, because a tenant may run only",
            ),
            (
                "VACUUM INTO '/tmp/copy.db'",
                "refused: VACUUM: `VACUUM INTO` writes a copy of this tenant's database",
            ),
        ] {
            let text = svc
                .exec_statement(&app, sql, &[])
                .await
                .unwrap_err()
                .to_string();
            assert!(text.starts_with(wanted), "{sql} said: {text}");
        }
    }

    /// Still refused, and now with a message that says what it was.
    #[tokio::test]
    async fn load_extension_stays_refused() {
        let (_dir, svc) = service();
        let app = tenant("app-one.production");
        let error = svc
            .fetch_rows(&app, "SELECT load_extension('/tmp/anything.so')", &[])
            .await
            .unwrap_err();
        assert!(refusal(error).contains("load_extension"));
    }

    /// The allowlist has to leave an ordinary application working: its own
    /// schema, its own version counter, its own temporary tables.
    #[tokio::test]
    async fn the_ordinary_things_an_application_does_still_work() {
        let (_dir, svc) = service();
        let app = tenant("app-one.production");
        svc.exec_statement(
            &app,
            "CREATE TABLE people(id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
            &[],
        )
        .await
        .unwrap();
        svc.exec_statement(&app, "CREATE INDEX by_name ON people(name)", &[])
            .await
            .unwrap();
        svc.exec_statement(&app, "PRAGMA user_version = 7", &[])
            .await
            .unwrap();
        svc.exec_statement(&app, "CREATE TEMP TABLE scratch(v TEXT)", &[])
            .await
            .unwrap();

        let version = svc
            .fetch_rows(&app, "PRAGMA user_version", &[])
            .await
            .unwrap();
        assert_eq!(version.rows, serde_json::json!([{ "user_version": 7 }]));

        let columns = svc
            .fetch_rows(&app, "SELECT name FROM pragma_table_info('people')", &[])
            .await
            .unwrap();
        assert_eq!(
            columns.rows,
            serde_json::json!([{ "name": "id" }, { "name": "name" }])
        );

        let indexes = svc
            .fetch_rows(&app, "PRAGMA index_list('people')", &[])
            .await
            .unwrap();
        assert_eq!(indexes.rows.as_array().map(Vec::len), Some(1));
    }

    /// A refusal reaches the application through the response body, not as a
    /// transport fault: the store is not down, this application asked for
    /// something it may not have.
    #[tokio::test]
    async fn a_refusal_comes_back_in_the_response_the_application_reads() {
        let (_dir, svc) = service();
        let app = tenant("app-one.production");
        let response = svc
            .query(tagged(
                &app,
                QueryRequest {
                    sql: "PRAGMA database_list".to_string(),
                    params: None,
                },
            ))
            .await
            .expect("a refused statement is not a transport failure")
            .into_inner();
        assert!(!response.success);
        let message = response.error.expect("a failed query carries its reason");
        assert!(message.contains("database_list"), "{message}");
    }

    /// Inside a batch the refusal has to name the statement it belongs to,
    /// and nothing before it may survive.
    #[tokio::test]
    async fn a_refused_statement_inside_a_batch_names_itself_and_rolls_the_batch_back() {
        let (_dir, svc) = service();
        let app = tenant("app-one.production");
        svc.exec_statement(&app, "CREATE TABLE t(v TEXT)", &[])
            .await
            .unwrap();
        let failure = svc
            .exec_batch(
                &app,
                vec![
                    ("INSERT INTO t VALUES('kept?')".to_string(), Vec::new()),
                    (
                        "ATTACH DATABASE '/tmp/whatever.db' AS x".to_string(),
                        Vec::new(),
                    ),
                ],
            )
            .await
            .unwrap()
            .expect_err("the batch must fail");
        assert_eq!(failure.index, 1);
        assert!(failure.message.contains("ATTACH"), "{}", failure.message);

        let rows = svc.fetch_rows(&app, "SELECT v FROM t", &[]).await.unwrap();
        assert_eq!(rows.rows, serde_json::json!([]));
    }

    /// The same policy applies on a pinned connection, which does not go
    /// through the pool's ordinary path.
    #[tokio::test]
    async fn an_interactive_transaction_is_held_to_the_same_policy() {
        let (_dir, svc) = service();
        let app = tenant("app-one.production");
        let handle = svc.open_transaction(&app, None).await.unwrap();
        let error = svc
            .fetch_rows_in_transaction(&app, &handle.id, "PRAGMA database_list", &[])
            .await
            .unwrap_err();
        assert!(refusal(error).contains("database_list"));
        svc.roll_back(&app, &handle.id).await.unwrap();
    }

    /// The configured value is raised rather than believed, so the clamp
    /// above can actually leave a connection back.
    #[tokio::test]
    async fn a_pool_too_small_to_spare_a_connection_is_raised() {
        let dir = TempDir::new().unwrap();
        let svc = DatabaseService::open(dir.path(), 1).unwrap();
        assert_eq!(svc.connections_per_tenant, MIN_TENANT_CONNECTIONS);
        assert_eq!(svc.transaction_limits().max_open_per_tenant, 1);

        // And that spare connection is real: a transaction that is opened and
        // never ended does not stop the tenant's ordinary queries.
        let app = tenant("app-one.production");
        svc.exec_statement(&app, "CREATE TABLE t(v INTEGER)", &[])
            .await
            .unwrap();
        let abandoned = svc.open_transaction(&app, None).await.unwrap();
        let rows = svc
            .fetch_rows(&app, "SELECT count(*) AS n FROM t", &[])
            .await
            .unwrap();
        assert_eq!(rows.rows, serde_json::json!([{ "n": 0 }]));
        svc.roll_back(&app, &abandoned.id).await.unwrap();
    }

    // ------------------------------------------------------------------
    // What a statement may cost
    // ------------------------------------------------------------------
    //
    // The authorizer above decides what a tenant's SQL may touch. These
    // decide how much it may spend doing it, which is the other half and was
    // for a while missing entirely: one `WITH RECURSIVE` counting to a
    // hundred million pegged a core for 95.7 seconds, one
    // `SELECT randomblob(20000000)` took the process from 18 MB resident to
    // 1476 MB, and killing the client stopped neither.

    /// Ordinary, allowed SQL with no answer worth having, that counts far
    /// enough that only a ceiling ends it.
    const COUNTS_FOREVER: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c \
                                  WHERE x < 2000000000) SELECT count(*) FROM c";

    #[tokio::test]
    async fn a_query_that_runs_too_long_is_stopped_and_told_what_to_change() {
        let (_dir, svc) = service_with_sql(SqlLimits {
            statement_deadline: Duration::from_millis(300),
            ..SqlLimits::default()
        });
        let app = tenant("app-one.production");
        let started = std::time::Instant::now();
        let err = svc
            .fetch_rows(&app, COUNTS_FOREVER, &[])
            .await
            .expect_err("a query with no end must not run to one");
        assert!(matches!(err, StoreError::Expired(_)), "{err}");
        let message = err.to_string();
        assert!(
            message.contains("300 ms") && message.contains("LIMIT"),
            "the refusal has to name the ceiling and what to change: {message}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it ran {:?}, so it was not stopped anywhere near its deadline",
            started.elapsed()
        );
        // And the tenant's database is still perfectly usable afterwards.
        let one = svc.fetch_rows(&app, "SELECT 1 AS n", &[]).await.unwrap();
        assert_eq!(one.rows, serde_json::json!([{ "n": 1 }]));
    }

    /// `Execute` and `Batch` are held to the same ceiling as `Query`. A batch
    /// stopped part-way must not come back as "statement 0 failed:
    /// interrupted", which names the wrong thing and explains nothing.
    #[tokio::test]
    async fn execute_and_batch_are_held_to_the_same_ceiling_as_query() {
        let (_dir, svc) = service_with_sql(SqlLimits {
            statement_deadline: Duration::from_millis(300),
            ..SqlLimits::default()
        });
        let app = tenant("app-one.production");
        svc.exec_statement(&app, "CREATE TABLE t(a INTEGER)", &[])
            .await
            .unwrap();
        let counting_insert = format!("INSERT INTO t(a) {COUNTS_FOREVER}");

        let err = svc
            .exec_statement(&app, &counting_insert, &[])
            .await
            .expect_err("Execute must be bounded too");
        assert!(matches!(err, StoreError::Expired(_)), "{err}");

        let err = svc
            .exec_batch(&app, vec![(counting_insert, Vec::new())])
            .await
            .expect_err("a batch stopped at its ceiling is not a named statement failure");
        assert!(matches!(err, StoreError::Expired(_)), "{err}");
        assert!(err.to_string().contains("300 ms"), "{err}");

        let rows = svc
            .fetch_rows(&app, "SELECT count(*) AS n FROM t", &[])
            .await
            .unwrap();
        assert_eq!(
            rows.rows,
            serde_json::json!([{ "n": 0 }]),
            "a stopped statement must leave nothing behind"
        );
    }

    /// The row ceiling, which is the one that actually bounds memory: a
    /// hundred thousand single-column rows cost 67 MB resident while
    /// rendering to 1.5 MB of JSON, so counting the data alone would not see
    /// them.
    #[tokio::test]
    async fn a_result_set_with_too_many_rows_is_refused_before_they_are_all_read() {
        let (_dir, svc) = service_with_sql(SqlLimits {
            max_result_rows: 5,
            ..SqlLimits::default()
        });
        let app = tenant("app-one.production");
        let err = svc
            .fetch_rows(
                &app,
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 100) \
                 SELECT x FROM c",
                &[],
            )
            .await
            .expect_err("a hundred rows against a ceiling of five");
        assert!(matches!(err, StoreError::TooLarge(_)), "{err}");
        let message = err.to_string();
        assert!(
            message.contains('5') && message.contains("LIMIT"),
            "the refusal has to name the ceiling and what to change: {message}"
        );
        // Exactly the ceiling is fine; it is one past it that is refused.
        let full = svc
            .fetch_rows(
                &app,
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 5) \
                 SELECT x FROM c",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(full.rows.as_array().map(Vec::len), Some(5));
    }

    /// The byte ceiling, charged as the cells are read so the memory for a
    /// result set that cannot be delivered is never spent.
    #[tokio::test]
    async fn a_result_set_with_too_much_data_is_refused_before_the_memory_is_spent() {
        let (_dir, svc) = service_with_sql(SqlLimits {
            max_result_bytes: 1_000,
            ..SqlLimits::default()
        });
        let app = tenant("app-one.production");
        let err = svc
            .fetch_rows(&app, "SELECT randomblob(4000) AS b", &[])
            .await
            .expect_err("four kilobytes against a ceiling of one");
        assert!(matches!(err, StoreError::TooLarge(_)), "{err}");
        let message = err.to_string();
        assert!(
            message.contains("1000") && message.contains("blob service"),
            "the refusal has to name the ceiling and where large payloads belong: {message}"
        );
        let small = svc
            .fetch_rows(&app, "SELECT length(randomblob(100)) AS n", &[])
            .await
            .unwrap();
        assert_eq!(small.rows, serde_json::json!([{ "n": 100 }]));
    }

    /// The ceiling the progress callback cannot reach: `randomblob` is one
    /// virtual-machine instruction, so the memory is already gone by the time
    /// anything watching between instructions gets a turn. Only SQLite can
    /// refuse this one, and only because it is told to.
    #[tokio::test]
    async fn a_value_bigger_than_a_row_may_be_is_refused_inside_sqlite() {
        let (_dir, svc) = service();
        let app = tenant("app-one.production");
        let err = svc
            .fetch_rows(&app, "SELECT randomblob(20000000) AS b", &[])
            .await
            .expect_err("twenty megabytes in one cell took the store to 1476 MB resident");
        assert!(matches!(err, StoreError::TooLarge(_)), "{err}");
        let message = err.to_string();
        assert!(
            message.contains("2000000") && message.contains("blob service"),
            "the refusal has to name the ceiling and where such a payload belongs: {message}"
        );
    }

    /// The boundary of the shared single-value ceiling: exactly 2,000,000
    /// bytes fits, one byte more does not. The other target draws the same
    /// line, so an application at the boundary runs identically on both.
    #[tokio::test]
    async fn a_value_at_the_shared_ceiling_is_kept_and_one_byte_more_is_refused() {
        let (_dir, svc) = service();
        let app = tenant("app-one.production");
        svc.fetch_rows(&app, "SELECT length(randomblob(2000000)) AS n", &[])
            .await
            .expect("a value at the ceiling is a legal value");
        let err = svc
            .fetch_rows(&app, "SELECT randomblob(2000001) AS b", &[])
            .await
            .expect_err("one byte over the ceiling is over the ceiling");
        assert!(matches!(err, StoreError::TooLarge(_)), "{err}");
    }

    /// The same shape one layer over: `Prepare` remembers statement text
    /// under a handle, and until the statement ceiling existed the size of
    /// each was bounded only by the gRPC message that carried it - a table of
    /// 4096 handles could hold 64 GB.
    #[tokio::test]
    async fn a_statement_too_long_to_remember_is_refused_at_the_call_that_sent_it() {
        let (_dir, svc) = service();
        let app = tenant("app-one.production");
        // The padding is a comment, so this is one ordinary `SELECT 1` that
        // happens to weigh just over the shared 100 KB statement ceiling - the
        // ceiling is on the bytes, not on what they say.
        let padded = format!("/*{}*/ SELECT 1 AS n", "x".repeat(100_500));

        let err = svc
            .register_statement(&app, &padded)
            .await
            .expect_err("Prepare must not remember a statement this size");
        assert!(matches!(err, StoreError::TooLarge(_)), "{err}");
        assert!(err.to_string().contains("100000"), "{err}");
        assert_eq!(
            svc.statements.len(),
            0,
            "a refused Prepare must leave nothing in the table it would have grown"
        );

        // The same ceiling on the paths that do not remember anything.
        let err = svc.fetch_rows(&app, &padded, &[]).await.unwrap_err();
        assert!(matches!(err, StoreError::TooLarge(_)), "{err}");
    }

    /// The transaction deadline was unenforceable while a statement inside it
    /// could not be preempted: a 19.4 second statement ran inside a 5 second
    /// transaction. A statement now gets the smaller of its own budget and
    /// what the transaction has left.
    #[tokio::test]
    async fn a_statement_cannot_outlive_the_transaction_it_runs_in() {
        let (_dir, svc) = service_with(TransactionLimits {
            deadline: Duration::from_millis(400),
            ..TransactionLimits::default()
        });
        let app = tenant("app-one.production");
        let handle = svc.open_transaction(&app, None).await.unwrap();
        assert_eq!(handle.expires_in, Duration::from_millis(400));

        let started = std::time::Instant::now();
        let err = svc
            .fetch_rows_in_transaction(&app, &handle.id, COUNTS_FOREVER, &[])
            .await
            .expect_err("a statement must not outlive the transaction holding the write lock");
        assert!(matches!(err, StoreError::Expired(_)), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "it ran {:?} inside a 400 ms transaction",
            started.elapsed()
        );
        // The connection it pinned is back: the tenant can open another.
        let next = svc.open_transaction(&app, None).await.unwrap();
        svc.roll_back(&app, &next.id).await.unwrap();
    }

    /// SQLite rolls an explicit transaction back by itself when it stops a
    /// write inside one, which would otherwise leave the handle naming a
    /// transaction that no longer exists - and the next statement sent on it
    /// running outside any transaction and being kept.
    #[tokio::test]
    async fn a_transaction_whose_statement_was_stopped_is_closed_and_says_so() {
        let (_dir, svc) = service_with_sql(SqlLimits {
            statement_deadline: Duration::from_millis(300),
            ..SqlLimits::default()
        });
        let app = tenant("app-one.production");
        make_table(&svc, &app).await;
        // A transaction with far more time than the statement gets, so it is
        // the request budget that ends the statement and not the deadline.
        let handle = svc
            .open_transaction(&app, Some(Duration::from_secs(5)))
            .await
            .unwrap();
        let err = svc
            .exec_in_transaction(
                &app,
                &handle.id,
                &format!("INSERT INTO users(name) {COUNTS_FOREVER}"),
                &[],
            )
            .await
            .expect_err("a statement with no end must be stopped inside a transaction too");
        assert!(matches!(err, StoreError::Expired(_)), "{err}");

        // The handle answers for itself rather than looking unknown, and it
        // will not run anything else.
        let after = svc
            .fetch_rows_in_transaction(&app, &handle.id, "SELECT 1 AS n", &[])
            .await
            .expect_err("the handle must be closed, not reusable");
        let message = after.to_string();
        assert!(
            message.contains("stopped part-way") && message.contains("rolled back"),
            "the caller has to be told what happened to its transaction: {message}"
        );
        let rows = svc
            .fetch_rows(&app, "SELECT count(*) AS n FROM users", &[])
            .await
            .unwrap();
        assert_eq!(rows.rows, serde_json::json!([{ "n": 0 }]));
    }

    /// Every one of these is the answer to the caller's own request, so it
    /// belongs in the response body next to a SQL typo rather than arriving
    /// as a transport failure a handler reads as "the store is down".
    #[tokio::test]
    async fn a_crossed_ceiling_reaches_the_application_in_the_response() {
        let (_dir, svc) = service_with_sql(SqlLimits {
            statement_deadline: Duration::from_millis(300),
            max_result_rows: 5,
            ..SqlLimits::default()
        });
        let app = tenant("app-one.production");

        let stopped = svc
            .query(tagged(
                &app,
                QueryRequest {
                    sql: COUNTS_FOREVER.to_string(),
                    params: None,
                },
            ))
            .await
            .expect("a stopped query is not a transport failure")
            .into_inner();
        assert!(!stopped.success);
        let message = stopped.error.expect("a failed query carries its reason");
        assert!(message.contains("300 ms"), "{message}");

        let too_many = svc
            .query(tagged(
                &app,
                QueryRequest {
                    sql: "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c \
                          WHERE x < 100) SELECT x FROM c"
                        .to_string(),
                    params: None,
                },
            ))
            .await
            .expect("a result set over the ceiling is not a transport failure")
            .into_inner();
        assert!(!too_many.success);
        assert!(
            too_many
                .error
                .expect("a failed query carries its reason")
                .contains("LIMIT")
        );
    }

    #[tokio::test]
    async fn blob_columns_come_back_as_byte_arrays() {
        let (_dir, svc) = service();
        let a = tenant("app-one.production");
        svc.exec_statement(&a, "CREATE TABLE raw(b BLOB)", &[])
            .await
            .unwrap();
        svc.exec_statement(&a, "INSERT INTO raw(b) VALUES(x'00ff10')", &[])
            .await
            .unwrap();
        let result = svc.fetch_rows(&a, "SELECT b FROM raw", &[]).await.unwrap();
        assert_eq!(result.rows, serde_json::json!([{ "b": [0, 255, 16] }]));
    }
}
