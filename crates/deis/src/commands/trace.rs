//! `deis trace` - one line per invocation of an application.
//!
//! A log line says something happened; a trace record says one *invocation*
//! happened and how the platform decided it ended. That last part is the
//! reason this command exists next to `deis logs`: an application that fails
//! silently writes no log line at all, and "the handler threw" and "we cut it
//! off after 30 seconds" are the same 500 to everyone except the node that
//! made the decision. Both arrive here as different outcomes.
//!
//! # Why `--follow` is a flag here and two commands for logs
//!
//! `deis logs` and `deis tail` are separate because `TailRequest` carries
//! almost none of what `QueryRequest` does - no range, no message match, no
//! trace id - so a log tail really is a different, narrower question. Traces
//! are the other way round: `TailTracesRequest` carries the application, the
//! environment, the kinds and the outcomes, which is every filter that is not
//! about a window of past time. Following is therefore the same question
//! asked about the future, and the flags that cannot survive the move (a
//! range, a page size, a single trace id, a target or duration match) are
//! refused rather than accepted and ignored.
//!
//! What a record carries is fixed by `crates/deis-telemetry/proto/telemetry.
//! proto`; every column below is a field of `TraceRecord` and nothing here is
//! computed or guessed.

use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use colored::Colorize;

use crate::commands::resolve;
use crate::platform::telemetry::{
    self, Kind, OutcomeSelector, QueryTracesRequest, TailTracesRequest, TraceRecord, Traces,
    expand_outcomes,
};
use crate::render::{Output, parse_since, timestamp_micros};
use crate::settings::{Overrides, Settings};

/// Most pages `deis trace` will pull before it stops asking.
///
/// Only `--status` is filtered on this side, so a page can come back empty
/// after the service already narrowed by everything else. The cap keeps a
/// status that never occurs from turning into a walk of the whole retention
/// window; when it is reached the answer says so rather than looking
/// complete.
const MAX_PAGES: usize = 50;

/// Page size asked for when `--limit` is not given, and how many rows are
/// shown then.
const DEFAULT_LIMIT: usize = 50;

/// Range `--since` covers when it is not given.
const DEFAULT_SINCE: &str = "1h";

pub struct Args {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub all_environments: bool,
    /// Repeatable. Empty means every outcome.
    pub outcome: Vec<String>,
    /// Repeatable. Empty means every kind.
    pub kind: Vec<String>,
    /// An HTTP status (`500`) or a class (`5xx`).
    pub status: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub trace_id: Option<String>,
    /// Case-insensitive substring of the invocation's target.
    pub target: Option<String>,
    pub min_duration_ms: Option<u64>,
    pub limit: Option<usize>,
    pub follow: bool,
}

pub async fn execute(args: Args, output: Output) -> Result<()> {
    if args.follow {
        follow(args, output).await
    } else {
        list(args, output).await
    }
}

/// What both halves need: where to look and what to keep.
struct Filters {
    app: String,
    environment: Option<String>,
    outcomes: Vec<i32>,
    kinds: Vec<i32>,
    status: Option<StatusFilter>,
    endpoint: String,
}

fn filters(args: &Args, settings: &Settings) -> Result<Filters> {
    let outcomes = args
        .outcome
        .iter()
        .map(|text| text.parse::<OutcomeSelector>())
        .collect::<Result<Vec<_>>>()?;
    let kinds = args
        .kind
        .iter()
        .map(|text| Ok(text.parse::<Kind>()?.0 as i32))
        .collect::<Result<Vec<_>>>()?;
    let status = match &args.status {
        Some(text) => Some(text.parse::<StatusFilter>()?),
        None => None,
    };
    Ok(Filters {
        app: settings.checked_app()?.to_string(),
        environment: environment_filter(settings, args.all_environments)?,
        outcomes: expand_outcomes(&outcomes),
        kinds,
        status,
        endpoint: settings.telemetry_endpoint()?.to_string(),
    })
}

async fn list(args: Args, output: Output) -> Result<()> {
    let settings = resolve(&Overrides {
        app: args.app.clone(),
        environment: args.environment.clone(),
    })?;
    let filters = filters(&args, &settings)?;

    let since = parse_since(args.since.as_deref().unwrap_or(DEFAULT_SINCE))?;
    let until = match &args.until {
        Some(text) => Some(parse_since(text)?),
        None => None,
    };
    if let Some(until) = until
        && until <= since
    {
        anyhow::bail!(
            "--until {} is not after --since {}, so the range is empty. Both accept a duration \
             back from now (15m, 2h) or an RFC 3339 instant, and --since is the older end.",
            until.to_rfc3339(),
            since.to_rfc3339()
        );
    }
    // `--limit 0` would otherwise stop after the first record, which reads as
    // a broken filter rather than as a request for nothing.
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).max(1);
    let min_duration_us = args
        .min_duration_ms
        .map(|ms| (ms as i64).saturating_mul(1_000))
        .unwrap_or(0);

    let mut traces = Traces::connect(&filters.endpoint).await?;

    let mut records: Vec<TraceRecord> = Vec::new();
    let mut cursor = String::new();
    let mut truncated = false;
    for page in 0..MAX_PAGES {
        let response = traces
            .query(QueryTracesRequest {
                app_id: filters.app.clone(),
                environment: filters.environment.clone().unwrap_or_default(),
                start_us: since.timestamp_micros(),
                end_us: until.map(|t| t.timestamp_micros()).unwrap_or(0),
                kinds: filters.kinds.clone(),
                outcomes: filters.outcomes.clone(),
                trace_id: args.trace_id.clone().unwrap_or_default(),
                min_duration_us,
                target_contains: args.target.clone().unwrap_or_default(),
                limit: limit.min(i32::MAX as usize) as i32,
                cursor: cursor.clone(),
            })
            .await?;

        for record in response.records {
            if let Some(status) = &filters.status
                && !status.matches(record.status)
            {
                continue;
            }
            records.push(record);
            if records.len() >= limit {
                break;
            }
        }

        if records.len() >= limit || !response.has_more || response.next_cursor.is_empty() {
            break;
        }
        cursor = response.next_cursor;
        if page + 1 == MAX_PAGES {
            truncated = true;
        }
    }

    // The service answers newest first; the newest belongs next to the
    // prompt, the same way `deis logs` puts it there.
    records.reverse();

    if output.json {
        return output.emit(&serde_json::json!({
            "app": filters.app,
            "environment": filters.environment,
            "telemetry_endpoint": traces.endpoint(),
            "since": since.to_rfc3339(),
            "until": until.map(|t| t.to_rfc3339()),
            "count": records.len(),
            "truncated": truncated,
            "outcomes": outcome_counts(&records)
                .into_iter()
                .map(|(name, count)| (name.to_string(), count))
                .collect::<std::collections::BTreeMap<_, _>>(),
            "invocations": records.iter().map(record_json).collect::<Vec<_>>(),
        }));
    }

    print_header(&filters, traces.endpoint(), Some((since, until)));

    if records.is_empty() {
        println!(
            "\n  {}",
            format!(
                "no invocation matched. Widen the range with --since 24h, drop --outcome, or \
                 pass --all-environments. A node ships traces only while telemetry shipping is \
                 on, so if nothing at all appears, check that the node's logging.endpoint names \
                 this deis-telemetry; `deis logs -a {}` shows whether it is reporting anything.",
                filters.app
            )
            .yellow()
        );
        return Ok(());
    }

    println!();
    for record in &records {
        println!("{}", format_record(record));
    }
    println!("\n  {}", summary(&records).dimmed());

    // Printed only while every row is blank in both columns, so it erases
    // itself the moment a node starts reporting either. Without it a page of
    // rows with no target and no status reads as this command being broken,
    // when what it is showing is exactly what the nodes sent.
    if records.iter().all(|r| r.target.is_empty() && r.status == 0) {
        println!(
            "  {}",
            "no row carries a target or a status: the nodes that served these invocations \
             reported neither, so --status matches nothing and there is no URL to read. That is \
             a gap on the node, not an empty result."
                .yellow()
        );
    }

    if truncated {
        println!(
            "\n  {}",
            format!(
                "stopped after {MAX_PAGES} pages without filling --limit {limit}; --status is the \
                 one filter applied here rather than by deis-telemetry, so narrow the range or \
                 drop it to see the rest."
            )
            .yellow()
        );
    }
    Ok(())
}

async fn follow(args: Args, output: Output) -> Result<()> {
    let settings = resolve(&Overrides {
        app: args.app.clone(),
        environment: args.environment.clone(),
    })?;
    let filters = filters(&args, &settings)?;

    let mut traces = Traces::connect(&filters.endpoint).await?;
    let mut stream = traces
        .tail(TailTracesRequest {
            app_id: filters.app.clone(),
            environment: filters.environment.clone().unwrap_or_default(),
            kinds: filters.kinds.clone(),
            outcomes: filters.outcomes.clone(),
        })
        .await?;

    if output.human() {
        print_header(&filters, traces.endpoint(), None);
        println!(
            "\n  {}",
            "following invocations from now on; Ctrl-C to stop. Without --follow this command \
             reads what is already stored."
                .dimmed()
        );
        println!(
            "  {}",
            "a node buffers its traces and pushes them on a timer, so a record appears a few \
             seconds after the invocation it describes."
                .dimmed()
        );
        println!();
    }

    let mut shown = 0usize;
    loop {
        // Ctrl-C is raced against the stream rather than left to the default
        // handler, so the process unwinds normally, the channel is closed
        // and a `--json` consumer sees a complete last line.
        let next = tokio::select! {
            biased;
            signal = tokio::signal::ctrl_c() => {
                signal.context("waiting for Ctrl-C")?;
                if output.human() {
                    println!("\n  {}", format!("stopped after {shown} invocation(s)").dimmed());
                }
                return Ok(());
            }
            next = stream.message() => next,
        };

        let record = match next {
            Ok(Some(record)) => record,
            Ok(None) => {
                // Said plainly, because the other reading - "this application
                // stopped being called" - is wrong and would send someone
                // looking in the wrong place.
                if output.human() {
                    println!(
                        "\n  {}",
                        format!(
                            "deis-telemetry at {} closed the stream after {shown} invocation(s); \
                             run `deis trace --follow` again to reconnect.",
                            traces.endpoint()
                        )
                        .yellow()
                    );
                }
                return Ok(());
            }
            Err(e) => {
                anyhow::bail!(
                    "the trace stream from deis-telemetry at {} ended with an error after {shown} \
                     invocation(s): {}\nRun `deis trace --follow` again to reconnect.",
                    traces.endpoint(),
                    e.message()
                );
            }
        };

        if let Some(status) = &filters.status
            && !status.matches(record.status)
        {
            continue;
        }

        shown += 1;
        if output.json {
            // One object per line: the stream has no end, so a single
            // document would never be complete enough to parse.
            println!("{}", serde_json::to_string(&record_json(&record))?);
        } else {
            println!("{}", format_record(&record));
        }
    }
}

/// Which environment to narrow to, or `None` for every environment.
///
/// Unlike `deis logs`, this goes into the request: a trace record carries the
/// environment as its own field, so deis-telemetry does the narrowing.
fn environment_filter(settings: &Settings, all: bool) -> Result<Option<String>> {
    if all {
        return Ok(None);
    }
    Ok(Some(settings.checked_environment()?.to_string()))
}

fn print_header(
    filters: &Filters,
    endpoint: &str,
    range: Option<(DateTime<Utc>, Option<DateTime<Utc>>)>,
) {
    println!(
        "\n{} {} {} {}",
        "Invocations of".bold(),
        filters.app.cyan(),
        "from".bold(),
        endpoint.dimmed()
    );
    match &filters.environment {
        Some(environment) => println!(
            "  environment  {} {}",
            environment.cyan(),
            "(--all-environments shows every one)".dimmed()
        ),
        None => println!("  environment  {}", "all".cyan()),
    }
    if !filters.outcomes.is_empty() {
        println!(
            "  outcome      {}",
            filters
                .outcomes
                .iter()
                .map(|o| telemetry::outcome_name(*o))
                .collect::<Vec<_>>()
                .join(", ")
                .cyan()
        );
    }
    if !filters.kinds.is_empty() {
        println!(
            "  kind         {}",
            filters
                .kinds
                .iter()
                .map(|k| telemetry::kind_name(*k))
                .collect::<Vec<_>>()
                .join(", ")
                .cyan()
        );
    }
    if let Some(status) = &filters.status {
        println!("  status       {}", status.to_string().cyan());
    }
    if let Some((since, until)) = range {
        println!(
            "  range        {} {}",
            since
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S"),
            match until {
                Some(until) => format!(
                    "to {}",
                    until
                        .with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M:%S")
                ),
                None => "to now".to_string(),
            }
        );
    }
}

/// One line: when, how it ended, what kind it was, what it answered, how long
/// it took, and what it was aimed at. The error, when there is one, goes on a
/// second line rather than being cut to fit the first.
fn format_record(record: &TraceRecord) -> String {
    let outcome = telemetry::outcome_name(record.outcome);
    let painted = match () {
        _ if outcome == "ok" => format!("{outcome:<14}").green().to_string(),
        // The application's own failure and the platform's are both red: an
        // operator wants both to stand out, and the word says which is which.
        _ if telemetry::is_failure(record.outcome) => {
            format!("{outcome:<14}").red().bold().to_string()
        }
        _ => format!("{outcome:<14}").dimmed().to_string(),
    };

    let kind = format!("{:<9}", telemetry::kind_name(record.kind));
    let status = status_cell(record.status);
    let duration = duration_cell(record.duration_us);
    let mut line = format!(
        "{} {} {} {status:>3} {duration:>9}  {}",
        timestamp_micros(record.started_us).dimmed(),
        painted,
        kind.cyan(),
        target_cell(record)
    );

    let mut trailer = format!("node={}", record.node);
    if !record.trace_id.is_empty() {
        trailer.push_str(&format!(" trace={}", record.trace_id));
    }
    line.push_str("  ");
    line.push_str(&trailer.dimmed().to_string());

    if !record.error.is_empty() {
        let (first, more) = first_line(&record.error);
        let mut error = format!("    {first}");
        if more {
            error.push_str(" [...] (--json has the whole message)");
        }
        line.push('\n');
        line.push_str(&error.red().to_string());
    }
    line
}

/// The status column. A queue batch or a cron tick has no HTTP status, and
/// the wire says so with 0; printing "0" would read as a status.
fn status_cell(status: i32) -> String {
    if status == 0 {
        "-".to_string()
    } else {
        status.to_string()
    }
}

/// Duration in the unit that keeps three significant figures at the scale
/// invocations actually run at: milliseconds up to a second, then seconds.
fn duration_cell(duration_us: i64) -> String {
    let ms = duration_us as f64 / 1_000.0;
    if ms >= 1_000.0 {
        format!("{:.2}s", ms / 1_000.0)
    } else {
        format!("{ms:.1}ms")
    }
}

/// What the invocation was aimed at, with the HTTP method in front of it when
/// there is one. Empty for neither is possible - a record with no target is
/// still worth a row - so this can be blank.
fn target_cell(record: &TraceRecord) -> String {
    match (record.method.is_empty(), record.target.is_empty()) {
        (true, true) => String::new(),
        (true, false) => record.target.clone(),
        (false, true) => record.method.clone(),
        (false, false) => format!("{} {}", record.method, record.target),
    }
}

/// The first line of a multi-line message, and whether there was more.
fn first_line(text: &str) -> (&str, bool) {
    match text.split_once('\n') {
        Some((first, rest)) => (first.trim_end(), !rest.trim().is_empty()),
        None => (text, false),
    }
}

/// How many of each outcome are in what was shown. The whole point of the
/// command is the split between kinds of ending, so it is stated rather than
/// left to be counted by eye.
///
/// Ordered by the outcome's own number in `telemetry.proto`, which reads as
/// ok first and then the ways an invocation can fail. Counting into a map
/// keyed by name would have sorted it alphabetically, putting "app-error"
/// ahead of "ok" in every summary.
fn outcome_counts(records: &[TraceRecord]) -> Vec<(&'static str, usize)> {
    let mut counts: std::collections::BTreeMap<i32, usize> = std::collections::BTreeMap::new();
    for record in records {
        *counts.entry(record.outcome).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(outcome, count)| (telemetry::outcome_name(outcome), count))
        .collect()
}

fn summary(records: &[TraceRecord]) -> String {
    let breakdown = outcome_counts(records)
        .iter()
        .map(|(name, count)| format!("{count} {name}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{} invocation(s): {breakdown}", records.len())
}

fn record_json(record: &TraceRecord) -> serde_json::Value {
    serde_json::json!({
        "trace_id": record.trace_id,
        "time": timestamp_micros(record.started_us),
        "started_us": record.started_us,
        "duration_us": record.duration_us,
        "app_id": record.app_id,
        "environment": record.environment,
        "node": record.node,
        "kind": telemetry::kind_name(record.kind),
        "outcome": telemetry::outcome_name(record.outcome),
        "failure": telemetry::is_failure(record.outcome),
        "target": record.target,
        "method": record.method,
        // Null rather than 0: an invocation with no HTTP status has none,
        // and 0 in a numeric field would be read as one.
        "status": (record.status != 0).then_some(record.status),
        "error": record.error,
        "attributes": record.attributes,
    })
}

/// `--status`: one HTTP status, or a class of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    Exact(i32),
    /// A class written `5xx`, held as its leading digit.
    Class(i32),
}

impl StatusFilter {
    fn matches(&self, status: i32) -> bool {
        match self {
            // An invocation with no HTTP status matches no status filter,
            // rather than matching every one: `--status 5xx` asking about
            // cron ticks would answer with all of them.
            StatusFilter::Exact(_) | StatusFilter::Class(_) if status == 0 => false,
            StatusFilter::Exact(wanted) => status == *wanted,
            StatusFilter::Class(digit) => status / 100 == *digit,
        }
    }
}

impl std::fmt::Display for StatusFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StatusFilter::Exact(status) => write!(f, "{status}"),
            StatusFilter::Class(digit) => write!(f, "{digit}xx"),
        }
    }
}

impl FromStr for StatusFilter {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let text = s.trim().to_ascii_lowercase();
        if let Some(digit) = text.strip_suffix("xx")
            && let Ok(digit) = digit.parse::<i32>()
            && (1..=5).contains(&digit)
        {
            return Ok(Self::Class(digit));
        }
        if let Ok(status) = text.parse::<i32>()
            && (100..=599).contains(&status)
        {
            return Ok(Self::Exact(status));
        }
        anyhow::bail!(
            "{s:?} is not an HTTP status; write one status (500) or a class (5xx). Invocations \
             with no status of their own - a queue batch, a cron tick - match neither."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::telemetry::{InvocationKind, InvocationOutcome};

    fn record(outcome: InvocationOutcome, status: i32) -> TraceRecord {
        TraceRecord {
            trace_id: "t-1".to_string(),
            started_us: 1_787_998_800_000_000,
            duration_us: 12_400,
            app_id: "demo".to_string(),
            environment: "production".to_string(),
            node: "deis-runtime-demo-01".to_string(),
            kind: InvocationKind::HttpRequest as i32,
            outcome: outcome as i32,
            target: "/api/hello".to_string(),
            method: "GET".to_string(),
            status,
            error: String::new(),
            attributes: Default::default(),
        }
    }

    #[test]
    fn a_row_names_the_outcome_the_target_and_the_trace_to_follow_it_with() {
        let rendered = format_record(&record(InvocationOutcome::Ok, 200));
        assert!(rendered.contains("ok"), "{rendered}");
        assert!(rendered.contains("http"), "{rendered}");
        assert!(rendered.contains("GET /api/hello"), "{rendered}");
        assert!(rendered.contains("12.4ms"), "{rendered}");
        assert!(rendered.contains("trace=t-1"), "{rendered}");
        assert!(rendered.contains("node=deis-runtime-demo-01"), "{rendered}");
    }

    /// The error is why anyone is reading this at all, so it survives on a
    /// line of its own instead of being cut to fit a column.
    #[test]
    fn a_cut_off_invocation_shows_why() {
        let mut record = record(InvocationOutcome::ExecutionTimeout, 0);
        record.error = "execution budget of 30s expired\n  at handler (worker.js:3)".to_string();
        let rendered = format_record(&record);
        assert!(rendered.contains("timeout"), "{rendered}");
        assert!(
            rendered.contains("execution budget of 30s expired"),
            "{rendered}"
        );
        assert!(
            rendered.contains("--json has the whole message"),
            "{rendered}"
        );
        // No status: a cut-off invocation never answered, and "0" in that
        // column would read as one.
        assert!(rendered.contains(" - "), "{rendered}");
    }

    #[test]
    fn durations_change_unit_where_a_person_would() {
        assert_eq!(duration_cell(400), "0.4ms");
        assert_eq!(duration_cell(12_400), "12.4ms");
        assert_eq!(duration_cell(999_000), "999.0ms");
        assert_eq!(duration_cell(1_500_000), "1.50s");
        assert_eq!(duration_cell(30_000_000), "30.00s");
    }

    #[test]
    fn a_status_filter_takes_one_status_or_a_class() {
        assert_eq!(
            "500".parse::<StatusFilter>().unwrap(),
            StatusFilter::Exact(500)
        );
        assert_eq!(
            "5xx".parse::<StatusFilter>().unwrap(),
            StatusFilter::Class(5)
        );
        assert_eq!(
            "4XX".parse::<StatusFilter>().unwrap(),
            StatusFilter::Class(4)
        );
        assert!("nope".parse::<StatusFilter>().is_err());
        assert!("42".parse::<StatusFilter>().is_err());
        assert!("9xx".parse::<StatusFilter>().is_err());
    }

    /// A cron tick has no HTTP status. `--status 5xx` must not answer with
    /// every one of them just because 0 is not in the 500s.
    #[test]
    fn an_invocation_with_no_status_matches_no_status_filter() {
        let class = "5xx".parse::<StatusFilter>().unwrap();
        assert!(class.matches(503));
        assert!(!class.matches(404));
        assert!(!class.matches(0));

        let exact = "200".parse::<StatusFilter>().unwrap();
        assert!(exact.matches(200));
        assert!(!exact.matches(0));
    }

    /// `--json` on a follow is newline-delimited, so each line has to be a
    /// complete document on its own, and a record with no HTTP status must
    /// not claim status 0.
    #[test]
    fn one_record_serialises_to_one_line() {
        let json = serde_json::to_string(&record_json(&record(InvocationOutcome::RateLimited, 0)))
            .unwrap();
        assert!(!json.contains('\n'), "{json}");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["outcome"], "rate-limited");
        assert_eq!(parsed["failure"], true);
        assert_eq!(parsed["status"], serde_json::Value::Null);
        assert_eq!(parsed["environment"], "production");
    }

    #[test]
    fn the_summary_counts_each_ending_separately() {
        let records = vec![
            record(InvocationOutcome::Ok, 200),
            record(InvocationOutcome::Ok, 200),
            record(InvocationOutcome::ApplicationError, 500),
        ];
        assert_eq!(summary(&records), "3 invocation(s): 2 ok, 1 app-error");
    }
}
