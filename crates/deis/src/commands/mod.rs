pub mod account;
pub mod audit;
pub mod blob;
pub mod build;
pub mod config;
pub mod db;
pub mod deploy;
pub mod deployments;
pub mod dev;
pub mod domains;
pub mod init;
pub mod kv;
pub mod login;
pub mod logs;
pub mod provider_deployments;
pub mod queue;
pub mod rollback;
pub mod routes;
pub mod secret;
pub mod status;
pub mod targets;
pub mod token;
pub mod trace;
pub mod validate;
pub mod versions;

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::platform::Control;
use crate::platform::queue::QueueSession;
use crate::platform::storage::StorageSession;
use crate::settings::{Overrides, Settings};

/// What a string that arrived over the wire may still contain by the time it
/// is bytes on the operator's terminal: no control character, each one spelled
/// as its `escape_debug` form instead.
///
/// # Why this is at the reader and not only at the writer
///
/// `crates/deis-runtime/src/runtime/ops.rs::op_log` applies the same rule to
/// an application's `console.*` text before it becomes a `tracing` event, and
/// that is the fix for the node's own log file. It is not enough here. Every
/// string this CLI prints came back from deis-control, deis-telemetry,
/// deis-store or deis-queue, and those services log and store text from
/// producers this CLI has no say over: a queue message body, an audit entry's
/// peer address, a node's `last_error`, a blob key, a log entry written by a
/// node running an older build than the one that escapes. Escaping only at
/// the writer means every one of those is a hole the day a new writer appears.
///
/// # What a terminal does with what is not escaped
///
/// `\x1b[2J\x1b[H` clears the screen and homes the cursor, so an application
/// can make `deis logs` print a report that is entirely its own; `\x1b]0;...\x07`
/// retitles the operator's window; a bare `\n` splits one entry into what
/// reads as two; `\r` overwrites the line just printed. None of this needs a
/// TTY on the far end - it is bytes in a pipe until something renders them,
/// which is why "stdout is redirected" is not a defence.
///
/// # One rule, in two crates, kept one rule
///
/// `escape_control` in `crates/deis-runtime/src/runtime/ops.rs` is the same
/// function. They cannot be one function - this crate does not depend on
/// deis-runtime, and neither belongs in `deis-wire`, which is the wire format
/// rather than a rendering rule - so what stops them becoming two rules that
/// disagree is `escaping_test::the_two_crates_tables_are_the_same_table`
/// below, which reads both source files and refuses two tables that carry
/// different cases. Each crate then asserts its own `escape_control` against
/// its own copy, which closes the loop: same cases, and each function held
/// to them.
///
/// `char::is_control` is Unicode `Cc`: C0, DEL, and the C1 block, which
/// includes U+009B - CSI as a single character, a terminal escape containing
/// no ESC to grep for. Bidirectional overrides (U+202E) are `Cf`, not `Cc`,
/// and are deliberately left: escaping everything `escape_debug` escapes would
/// also escape combining accents and mangle ordinary text in most of the
/// languages that are not English.
///
/// Borrows when there is nothing to escape, which is nearly every string.
pub fn escape_control(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains(char::is_control) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for character in text.chars() {
        push_escaped(&mut out, character);
    }
    std::borrow::Cow::Owned(out)
}

/// The whole of the decision [`escape_control`] is made of, for one character,
/// and the number of characters it appended.
///
/// Split out so that a renderer with a length ceiling to keep -
/// `queue::body_preview` is the one - can hold the ceiling against what the
/// terminal will actually see without owning a second opinion about what a
/// character escapes to. One character escaping to six is exactly why that
/// renderer needs the count, and a second copy of the escaping to get it is
/// exactly how two renderers end up disagreeing about what is safe.
pub fn push_escaped(out: &mut String, character: char) -> usize {
    if !character.is_control() {
        out.push(character);
        return 1;
    }
    let mut appended = 0usize;
    for escaped in character.escape_debug() {
        out.push(escaped);
        appended += 1;
    }
    appended
}

/// [`escape_control`] for a value that is about to be handed to something
/// wanting an owned `String` - a table cell, mostly.
pub fn escaped(text: &str) -> String {
    escape_control(text).into_owned()
}

/// [`escaped`] for a cell the control plane may have nothing to put in: the
/// text, or a dash.
///
/// The dash is this CLI's single spelling for "there is no value here", and
/// it is a decision the renderer makes rather than anything that arrived over
/// the wire - which is why it lives beside `escaped` instead of being written
/// out again at each table that needs it. Absent and empty are deliberately
/// *not* the same: an empty note is printed as the empty string it is, and
/// only a missing one becomes a dash.
pub fn escaped_or_dash(value: Option<&str>) -> String {
    match value {
        Some(text) => escaped(text),
        None => "-".to_string(),
    }
}

/// The project directory a command works in: wherever it was run.
///
/// Deliberately not a search up the tree. `deis build` has always used the
/// working directory, and a deploy that silently picked a different project
/// than the build in the same shell would be worse than an error saying to
/// change directory.
pub fn project_dir() -> Result<PathBuf> {
    std::env::current_dir().context("reading the working directory")
}

/// Resolve every setting for a command running in the working directory.
///
/// This is all a command that talks only to deis-telemetry needs. `deis
/// logs`, `deis tail` and `deis trace` deliberately stop here: reading an
/// application's logs or its invocation history is not a control-plane
/// operation, and a checkout that has never had a control plane configured
/// must still be able to look at what its application is doing.
pub fn resolve(overrides: &Overrides) -> Result<Settings> {
    Settings::load(&project_dir()?, overrides)
}

/// Everything a command that talks to the control plane needs: the resolved
/// settings and a client pointed at the control plane they name.
pub struct Session {
    pub settings: Settings,
    pub control: Control,
    /// Which namespace inside the tenant a storage command acts in, or `None`
    /// for the tenant's unnamed one.
    ///
    /// Kept on the session rather than in [`Settings`] because it is not a
    /// setting: it is not read from the project file or the environment, it
    /// has no default worth remembering, and it means nothing to any command
    /// that is not `deis kv`, `deis db` or `deis blob`. See
    /// [`crate::settings::Overrides::binding`].
    binding: Option<String>,
}

impl Session {
    /// Resolve settings and open a client. Fails with an actionable message
    /// when the control plane is not configured.
    pub fn open(overrides: &Overrides) -> Result<Self> {
        let settings = resolve(overrides)?;
        let control = Control::new(settings.control_endpoint()?, settings.token())?;
        Ok(Self {
            settings,
            control,
            binding: overrides
                .binding
                .as_ref()
                .map(|binding| binding.trim().to_string())
                .filter(|binding| !binding.is_empty()),
        })
    }

    /// The namespace this command acts in, as a person should read it:
    /// `demo.production`, or `demo.production@HOT` when a binding was named.
    ///
    /// Every line `deis kv`, `deis db` and `deis blob` print uses this rather
    /// than the bare tenant. A `deis kv get` that printed only the tenant
    /// would tell an operator which application and environment they had
    /// looked at and not which of its namespaces, which is exactly the thing
    /// named bindings make ambiguous.
    ///
    /// It is the same spelling deis-store keys the data by
    /// (`deis_wire::conventions::namespace_id`), so what is printed here is
    /// findable on disk.
    pub fn namespace(&self) -> Result<String> {
        Ok(deis_wire::conventions::namespace_id(
            &self.tenant()?,
            self.binding.as_deref(),
        ))
    }

    /// The application this command is about, checked against the naming rule
    /// the storage services enforce.
    pub fn app(&self) -> Result<&str> {
        self.settings.checked_app()
    }

    /// The environment this command is about, checked the same way.
    pub fn environment(&self) -> Result<&str> {
        self.settings.checked_environment()
    }

    /// The storage tenant this command is about: application and environment
    /// joined the one way the platform ever joins them.
    ///
    /// Never build this pair by hand elsewhere - `deis_wire::conventions` is
    /// what deis-control and deis-store also build it with, and a second
    /// copy of the join logic is exactly how the three would drift apart.
    pub fn tenant(&self) -> Result<String> {
        Ok(deis_wire::conventions::tenant_id(
            self.app()?,
            self.environment()?,
        ))
    }

    /// Open a connection to this tenant's KV store, database and blob
    /// storage.
    ///
    /// The address and the capability token both come from the control
    /// plane, fetched fresh on every call: this is what lets an operator who
    /// can already run `deis deploy` also run `deis kv`/`deis db`/`deis
    /// blob` with no separate credential to manage, and what keeps the raw
    /// storage secret off every machine this CLI runs on. See
    /// [`crate::platform::Control::storage_credentials`] for the contract.
    pub async fn storage(&self) -> Result<StorageSession> {
        let app = self.app()?;
        let environment = self.environment()?;
        let tenant = self.tenant()?;

        let credentials = self.control.storage_credentials(app, environment).await?;
        let Some(endpoint) = credentials.store_endpoint else {
            anyhow::bail!(
                "the control plane has no deis-store address configured, so {tenant} has \
                 nowhere to keep kv, database or blob data. This is a platform-level setting an \
                 operator sets on deis-control itself; `deis config` has no lever for it."
            );
        };
        // The capability token is for the tenant, and the binding selects a
        // namespace inside it. That ordering is the whole authorisation story:
        // the credential this CLI already holds says which tenant's data may
        // be touched, and naming a binding cannot reach past it.
        StorageSession::connect(
            &endpoint,
            &tenant,
            credentials.token.as_deref(),
            self.binding.as_deref(),
        )
    }

    /// Open a connection to this tenant's queues.
    ///
    /// The credential is the same one [`Self::storage`] uses and is fetched
    /// the same way: deis-queue checks the capability token against the same
    /// shared secret deis-store does, so an operator who can already run
    /// `deis kv` needs nothing further to read the dead letters.
    ///
    /// The *address*, though, comes from the project settings rather than
    /// from the control plane, which is the one difference worth knowing:
    /// the storage-token route answers with deis-store's address and says
    /// nothing about deis-queue.
    pub async fn queue(&self) -> Result<QueueSession> {
        let app = self.app()?;
        let environment = self.environment()?;
        let tenant = self.tenant()?;
        let endpoint = self.settings.queue_endpoint()?.to_string();

        let credentials = self.control.storage_credentials(app, environment).await?;
        QueueSession::connect(&endpoint, &tenant, credentials.token.as_deref())
    }
}

/// `--json` output is deliberately *not* escaped by these: `serde_json`
/// already escapes every control character on its way into a JSON string, and
/// escaping first would show an operator `\\n` where their data has a
/// newline. The rule is for the human renderer only.
#[cfg(test)]
pub(crate) const ESCAPING_CASES: &[(&str, &str)] = &[
    ("ordinary text", "ordinary text"),
    ("", ""),
    ("start\nWARN forged", "start\\nWARN forged"),
    ("carriage\rreturn", "carriage\\rreturn"),
    ("tab\there", "tab\\there"),
    (
        "\u{1b}[2J\u{1b}[31mCOMPROMISED",
        "\\u{1b}[2J\\u{1b}[31mCOMPROMISED",
    ),
    ("\u{1b}]0;pwned\u{7}", "\\u{1b}]0;pwned\\u{7}"),
    ("nul\0byte", "nul\\0byte"),
    ("\u{9b}2J", "\\u{9b}2J"),
    ("del\u{7f}", "del\\u{7f}"),
    ("nel\u{85}here", "nel\\u{85}here"),
    ("\u{65e5}\u{672c}\u{8a9e}", "\u{65e5}\u{672c}\u{8a9e}"),
    ("caf\u{e9}", "caf\u{e9}"),
];

#[cfg(test)]
mod escaping_test {
    use super::{ESCAPING_CASES, escape_control, escaped, escaped_or_dash};

    /// The module header's claim - one rule, in two crates, kept one rule -
    /// enforced rather than remembered. The two copies of `ESCAPING_CASES`
    /// cannot reference each other (the crates do not depend on each other),
    /// so this reads both source files and compares the tables' cases with
    /// each file's own comments and whitespace stripped. Change one table
    /// alone and this is the test that says so.
    #[test]
    fn the_two_crates_tables_are_the_same_table() {
        fn cases_of(source: &str, file: &str) -> String {
            let marker = "ESCAPING_CASES: &[(&str, &str)] = &[";
            let start = source
                .find(marker)
                .unwrap_or_else(|| panic!("{file} no longer declares ESCAPING_CASES"));
            let rest = &source[start + marker.len()..];
            let end = rest
                .find("];")
                .unwrap_or_else(|| panic!("{file}: the table never closes"));
            rest[..end]
                .lines()
                .map(|line| line.split("//").next().unwrap_or("").trim())
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .concat()
        }
        let own = cases_of(include_str!("mod.rs"), "deis/src/commands/mod.rs");
        let sibling_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../deis-runtime/src/runtime/ops.rs"
        );
        let sibling = std::fs::read_to_string(sibling_path)
            .expect("the sibling table's file is in this workspace");
        assert_eq!(
            own,
            cases_of(&sibling, "deis-runtime/src/runtime/ops.rs"),
            "the two ESCAPING_CASES tables have drifted apart; whichever case changed, \
             change it in both files or the two renderers disagree about what an operator sees"
        );
    }

    #[test]
    fn the_shared_table_is_what_this_crate_escapes_to() {
        // The runtime asserts its own copy of the table against its own
        // `escape_control`; `the_two_crates_tables_are_the_same_table` above
        // is what holds the two copies to the same cases.
        for (input, expected) in ESCAPING_CASES {
            assert_eq!(
                escape_control(input),
                *expected,
                "input {input:?} must escape to {expected:?}"
            );
            assert_eq!(escaped(input), *expected);
        }
    }

    #[test]
    fn no_control_character_at_all_survives() {
        let every_control: String = (0u32..=0x9f)
            .filter_map(char::from_u32)
            .filter(|c| c.is_control())
            .collect();
        assert!(every_control.chars().count() >= 65);
        let rendered = escape_control(&every_control);
        assert!(!rendered.chars().any(|c| c.is_control()), "{rendered:?}");
    }

    #[test]
    fn an_absent_cell_is_a_dash_and_a_present_one_is_still_escaped() {
        assert_eq!(escaped_or_dash(None), "-");
        // An empty string is a value the control plane holds, not a missing
        // one, so it must not be dressed up as absent.
        assert_eq!(escaped_or_dash(Some("")), "");
        assert_eq!(
            escaped_or_dash(Some("start\nWARN forged")),
            "start\\nWARN forged"
        );
    }

    #[test]
    fn clean_text_is_not_copied() {
        assert!(matches!(
            escape_control("nothing to do"),
            std::borrow::Cow::Borrowed(_)
        ));
    }
}
