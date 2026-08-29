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
