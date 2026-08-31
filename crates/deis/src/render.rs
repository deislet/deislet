//! Turning results into either something a person reads or something a script
//! parses.
//!
//! Human output is the default because the CLI is what a person uses; `--json`
//! is what a script gets, and it prints nothing else on stdout so the output
//! stays parseable.

use chrono::{DateTime, Local, TimeZone, Utc};
use colored::Colorize;

/// Which shape this run's output takes. Threaded through every command rather
/// than read from a global, so a command cannot forget which mode it is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Output {
    pub json: bool,
}

impl Output {
    pub fn human(&self) -> bool {
        !self.json
    }

    /// Print a value as the whole of this command's stdout.
    pub fn emit(&self, value: &serde_json::Value) -> anyhow::Result<()> {
        println!("{}", serde_json::to_string_pretty(value)?);
        Ok(())
    }

    /// Print a line only when a person is reading.
    pub fn line(&self, text: impl AsRef<str>) {
        if self.human() {
            println!("{}", text.as_ref());
        }
    }
}

/// Refuse to run a destructive call until the operator running it types
/// `tenant` back, or passed `skip_confirmation` for a script.
///
/// Typing the tenant rather than answering `y` is deliberate: `y` is muscle
/// memory after the tenth time, identical whichever tenant is on screen, so
/// it protects against nothing. Re-reading `demo.production` and typing it
/// back is the one thing a person cannot do on autopilot, which is what
/// "hard to run by accident" has to mean for a command whose entire job is
/// an action that cannot be undone.
///
/// A non-interactive stdin (a script, a CI job, an SSH command with no
/// pseudo-terminal) is refused outright rather than prompted: reading a
/// confirmation line from a pipe that is actually the next command in a
/// script would make the prompt worse than useless, since a script producing
/// exactly the wrong next line would make the call happen. `skip_confirmation`
/// is `-y`/`--yes`, which is what a script should pass instead once it has
/// decided on its own that the call is correct.
pub fn confirm_destructive(
    tenant: &str,
    action: &str,
    skip_confirmation: bool,
) -> anyhow::Result<()> {
    use std::io::Write;

    if skip_confirmation {
        return Ok(());
    }

    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        anyhow::bail!(
            "{action} on {tenant} is destructive and this is not a terminal to confirm it on. \
             Pass -y/--yes once you are sure - that is what a script or a CI job should do."
        );
    }

    eprintln!();
    eprintln!("{}", format!("About to {action} on {tenant}.").red().bold());
    eprintln!("{}", "This cannot be undone.".red());
    eprint!("Type {tenant} to continue: ");
    std::io::stderr().flush().ok();

    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| anyhow::anyhow!("reading the confirmation: {e}"))?;

    if answer.trim() != tenant {
        anyhow::bail!("confirmation did not match {tenant}; nothing was done");
    }
    Ok(())
}

/// How a KV value or a blob object should be shown when a person did not ask
/// for it to be written to a file.
///
/// A value from `deis kv`/`deis blob` is whatever bytes an application
/// wrote, with no reason to expect UTF-8. Printing them straight into a
/// terminal either works by luck or corrupts the scrollback with control
/// characters and stray escape sequences, so this is decided once, here,
/// rather than by each command finding out the hard way.
pub enum BytesForDisplay<'a> {
    /// Valid UTF-8: safe, and almost always what the operator wanted to see.
    Text(&'a str),
    /// Not valid UTF-8. `preview_hex` is the first bytes, so the operator
    /// still sees *something* concrete without the whole value hitting the
    /// terminal.
    Binary { len: usize, preview_hex: String },
}

/// How many leading bytes of a binary value to show as a hex preview.
/// Enough to recognise a magic number (PNG, gzip, a protobuf tag byte)
/// without printing enough to be mistaken for the whole value.
const BINARY_PREVIEW_BYTES: usize = 16;

/// Base64 for a value that has to travel through JSON: `--json` output for a
/// KV value or a blob object carries this rather than the raw bytes, which
/// JSON cannot represent, and rather than an array of byte numbers, which
/// would cost roughly three times the space for the same content.
pub fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

pub fn bytes_for_display(data: &[u8]) -> BytesForDisplay<'_> {
    match std::str::from_utf8(data) {
        Ok(text) => BytesForDisplay::Text(text),
        Err(_) => {
            let preview_len = data.len().min(BINARY_PREVIEW_BYTES);
            let preview_hex = data[..preview_len]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            BytesForDisplay::Binary {
                len: data.len(),
                preview_hex,
            }
        }
    }
}

/// A Unix timestamp in seconds, as a local wall-clock time.
pub fn timestamp(seconds: i64) -> String {
    match Local.timestamp_opt(seconds, 0).single() {
        Some(time) => time.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => format!("{seconds} (not a valid time)"),
    }
}

/// A Unix timestamp in microseconds, as a local wall-clock time with
/// milliseconds - log lines land many to the second.
pub fn timestamp_micros(micros: i64) -> String {
    let seconds = micros.div_euclid(1_000_000);
    let nanos = (micros.rem_euclid(1_000_000) * 1_000) as u32;
    match Local.timestamp_opt(seconds, nanos).single() {
        Some(time) => time.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        None => format!("{micros} (not a valid time)"),
    }
}

/// How long ago something happened, in the roughest unit that still says
/// something: "3m ago", "2d ago".
pub fn age(seconds: i64) -> String {
    let now = Utc::now().timestamp();
    let delta = now - seconds;
    if delta < 0 {
        return "in the future".to_string();
    }
    let (value, unit) = match delta {
        d if d < 60 => (d, "s"),
        d if d < 3600 => (d / 60, "m"),
        d if d < 86_400 => (d / 3600, "h"),
        d => (d / 86_400, "d"),
    };
    format!("{value}{unit} ago")
}

/// The first 12 characters of a digest, which is what fits in a table and is
/// still enough to tell two versions apart by eye.
/// How a version's compatibility date reads in a table cell.
///
/// Three answers, and the middle one is the reason this exists: a version with
/// no date is not missing information, it is pinned to the oldest behaviour
/// this platform has, for the rest of its life. It is printed as a word rather
/// than a dash so that "which of my applications have no date" is a question a
/// person can answer by looking at one column.
pub fn compatibility_cell(profile: Option<&deis_bundle::CompatibilityProfile>) -> String {
    match profile {
        Some(profile) => match profile.date() {
            Some(date) => date.to_string(),
            None => "none (oldest)".to_string(),
        },
        None => "unknown".to_string(),
    }
}

/// The same answer as a sentence, for a command that has room for one.
pub fn compatibility_line(profile: Option<&deis_bundle::CompatibilityProfile>) -> String {
    match profile {
        Some(profile) => match profile.date() {
            Some(date) => {
                let flags = profile.enabled_flag_names();
                if flags.is_empty() {
                    format!("compatibility date {date}")
                } else {
                    format!("compatibility date {date} ({})", flags.join(", "))
                }
            }
            None => format!(
                "no compatibility date: this application is held to the oldest behaviour this \
                 platform has and no later fix that changes an observable behaviour will reach \
                 it. Add \"compatibilityDate\": \"{}\" to .config.json and deploy again to opt in",
                deis_bundle::PLATFORM_KNOWN_THROUGH
            ),
        },
        None => "compatibility date unknown: this control plane did not say".to_string(),
    }
}

pub fn short_digest(digest: &str) -> String {
    digest.chars().take(12).collect()
}

/// Parse a point in time written either as a duration back from now ("15m",
/// "2h", "3d") or as an RFC 3339 instant.
pub fn parse_since(input: &str) -> anyhow::Result<DateTime<Utc>> {
    let text = input.trim();
    if let Some(duration) = parse_duration(text) {
        return Ok(Utc::now() - duration);
    }
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| {
            anyhow::anyhow!(
                "{input:?} is neither a duration nor a time: {e}\nWrite a duration back from now \
                 (30s, 15m, 2h, 3d) or an RFC 3339 instant (2026-08-29T10:00:00Z)."
            )
        })
}

fn parse_duration(text: &str) -> Option<chrono::Duration> {
    let (digits, unit) = text.split_at(text.len().checked_sub(1)?);
    let amount: i64 = digits.parse().ok()?;
    if amount < 0 {
        return None;
    }
    match unit {
        "s" => Some(chrono::Duration::seconds(amount)),
        "m" => Some(chrono::Duration::minutes(amount)),
        "h" => Some(chrono::Duration::hours(amount)),
        "d" => Some(chrono::Duration::days(amount)),
        _ => None,
    }
}

/// A fixed-width table that keeps its columns lined up whatever is in them.
pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(headers: &[&str]) -> Self {
        Self {
            headers: headers.iter().map(|h| h.to_string()).collect(),
            rows: Vec::new(),
        }
    }

    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    pub fn print(&self) {
        let mut widths: Vec<usize> = self.headers.iter().map(|h| h.chars().count()).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < widths.len() {
                    widths[i] = widths[i].max(cell.chars().count());
                }
            }
        }

        let header: Vec<String> = self
            .headers
            .iter()
            .enumerate()
            .map(|(i, h)| pad(h, widths[i]))
            .collect();
        println!("{}", header.join("  ").trim_end().bold());

        for row in &self.rows {
            let line: Vec<String> = row
                .iter()
                .enumerate()
                .map(|(i, c)| pad(c, *widths.get(i).unwrap_or(&0)))
                .collect();
            println!("{}", line.join("  ").trim_end());
        }
    }
}

fn pad(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skipping_confirmation_never_touches_stdin() {
        // `cargo test` gives every test a non-terminal stdin, which is
        // exactly the situation `-y`/`--yes` exists to get past without a
        // prompt this call would otherwise refuse to make.
        confirm_destructive("demo.production", "delete kv key \"x\"", true)
            .expect("skip_confirmation must short-circuit before stdin is examined");
    }

    #[test]
    fn a_non_terminal_stdin_is_refused_rather_than_read_from() {
        // The test harness's stdin is never a terminal, so this is the path
        // every un-skipped call takes here - and it is the path that matters
        // most: a script whose stdin is a pipe must be refused, not have a
        // line of its own output silently consumed as a confirmation.
        let error = confirm_destructive("demo.production", "delete kv key \"x\"", false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("-y/--yes"), "{error}");
        assert!(error.contains("demo.production"), "{error}");
    }

    #[test]
    fn base64_matches_the_published_vectors() {
        // Same RFC 4648 vectors deis_wire::conventions checks its own
        // encoder against, so a KV value round-trips identically to a
        // storage capability token wherever both cross this CLI.
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(&[0xffu8, 0x00, 0x01]), "/wAB");
    }

    #[test]
    fn valid_utf8_is_shown_as_text() {
        assert!(matches!(
            bytes_for_display(b"hello"),
            BytesForDisplay::Text("hello")
        ));
    }

    #[test]
    fn invalid_utf8_is_described_not_dumped() {
        // 0xff can never start a valid UTF-8 sequence, so this is binary by
        // construction, not by chance.
        let data = [0xffu8, 0x00, 0x01, 0x02];
        match bytes_for_display(&data) {
            BytesForDisplay::Binary { len, preview_hex } => {
                assert_eq!(len, 4);
                assert_eq!(preview_hex, "ff 00 01 02");
            }
            BytesForDisplay::Text(_) => panic!("0xff is never valid UTF-8"),
        }
    }

    #[test]
    fn the_binary_preview_is_capped_even_for_a_large_value() {
        let data = vec![0xffu8; 1024];
        match bytes_for_display(&data) {
            BytesForDisplay::Binary { len, preview_hex } => {
                assert_eq!(len, 1024);
                assert_eq!(preview_hex.split(' ').count(), BINARY_PREVIEW_BYTES);
            }
            BytesForDisplay::Text(_) => panic!("0xff repeated is never valid UTF-8"),
        }
    }

    #[test]
    fn durations_and_instants_both_parse() {
        let now = Utc::now();
        let fifteen_minutes = parse_since("15m").unwrap();
        let delta = (now - fifteen_minutes).num_seconds();
        assert!(
            (890..=910).contains(&delta),
            "15m should be about 900 seconds back, got {delta}"
        );

        let instant = parse_since("2026-08-29T10:00:00Z").unwrap();
        assert_eq!(instant.timestamp(), 1_787_997_600);
    }

    #[test]
    fn an_unparseable_time_says_both_forms_that_work() {
        let error = parse_since("yesterday").unwrap_err().to_string();
        assert!(error.contains("15m"), "{error}");
        assert!(error.contains("RFC 3339"), "{error}");
    }

    #[test]
    fn ages_use_the_roughest_unit_that_still_says_something() {
        let now = Utc::now().timestamp();
        assert_eq!(age(now - 30), "30s ago");
        assert_eq!(age(now - 600), "10m ago");
        assert_eq!(age(now - 7200), "2h ago");
        assert_eq!(age(now - 172_800), "2d ago");
    }

    #[test]
    fn microsecond_timestamps_keep_their_milliseconds() {
        // 2026-08-29T10:00:00.123Z, printed in whatever zone the test runs in.
        let rendered = timestamp_micros(1_787_998_800_123_000);
        assert!(rendered.ends_with(".123"), "{rendered}");
    }
}
