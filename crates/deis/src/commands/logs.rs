//! `deis logs` and `deis tail` - reading an application's log lines back out
//! of deis-telemetry.
//!
//! One service, two calls with different shapes: `Query` answers a page of
//! what is already stored, `Tail` streams what arrives from now on. They are
//! separate commands rather than a `--follow` flag because they answer
//! different questions and neither is a special case of the other - a tail
//! shows nothing at all about the minute that just went wrong.
//!
//! The environment is a filter this CLI applies, not one the service can be
//! asked for: deis-telemetry stores an application's lines under one
//! `app_id` with the environment as a structured field, on purpose, so that
//! "show me this application's logs" answers with all of it. See
//! [`crate::platform::telemetry`].

use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use colored::Colorize;

use crate::commands::{escape_control, escaped, resolve};
use crate::platform::telemetry::{
    self, LogEntry, LogLevel, QueryRequest, TailRequest, Telemetry, TelemetryCredential,
};
use crate::render::{Output, parse_since, timestamp_micros};
use crate::settings::{Overrides, Settings};

/// Most pages `deis logs` will pull before it stops asking.
///
/// The environment filter runs on this side, so a request for 200 lines of
/// one preview environment can need many pages of a busy application. The
/// cap keeps a filter that matches nothing from turning into an unbounded
/// walk of the whole retention window.
const MAX_PAGES: usize = 50;

pub struct QueryArgs {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub level: Option<String>,
    pub since: String,
    pub until: Option<String>,
    pub contains: Option<String>,
    pub trace_id: Option<String>,
    pub limit: usize,
    pub all_environments: bool,
}

pub struct TailArgs {
    pub app: Option<String>,
    pub environment: Option<String>,
    pub level: Option<String>,
    pub all_environments: bool,
}

pub async fn query(args: QueryArgs, output: Output) -> Result<()> {
    let settings = resolve(&Overrides {
        app: args.app,
        environment: args.environment,
        binding: None,
    })?;
    let app = settings.checked_app()?.to_string();
    let environment = environment_filter(&settings, args.all_environments)?;
    let level = parse_level(args.level.as_deref())?;

    let since = parse_since(&args.since)?;
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

    // `--limit 0` would otherwise stop after the first entry, which reads as
    // a broken filter rather than as a request for nothing.
    let limit = args.limit.max(1);

    let endpoint = settings.telemetry_endpoint()?.to_string();
    let credential = TelemetryCredential::obtain(&settings, &app, environment.as_deref()).await?;
    let mut telemetry = Telemetry::connect(&endpoint, credential).await?;

    let mut entries: Vec<LogEntry> = Vec::new();
    let mut cursor = String::new();
    let mut truncated = false;
    for page in 0..MAX_PAGES {
        let response = telemetry
            .query(QueryRequest {
                app_id: app.clone(),
                start_us: since.timestamp_micros(),
                end_us: until.map(|t| t.timestamp_micros()).unwrap_or(0),
                min_level: level as i32,
                trace_id: args.trace_id.clone().unwrap_or_default(),
                contains: args.contains.clone().unwrap_or_default(),
                limit: limit.min(i32::MAX as usize) as i32,
                cursor: cursor.clone(),
            })
            .await?;

        for entry in response.entries {
            if let Some(environment) = &environment
                && !telemetry::entry_matches_environment(&entry.fields, environment)
            {
                continue;
            }
            entries.push(entry);
            if entries.len() >= limit {
                break;
            }
        }

        if entries.len() >= limit || !response.has_more || response.next_cursor.is_empty() {
            break;
        }
        cursor = response.next_cursor;
        if page + 1 == MAX_PAGES {
            truncated = true;
        }
    }

    // The service answers newest first; a person reads a log downwards, and
    // so does every other log tool, so the page is turned around here.
    entries.reverse();

    if output.json {
        return output.emit(&serde_json::json!({
            "app": app,
            "environment": environment,
            "telemetry_endpoint": telemetry.endpoint(),
            "since": since.to_rfc3339(),
            "until": until.map(|t| t.to_rfc3339()),
            "count": entries.len(),
            "truncated": truncated,
            "entries": entries.iter().map(entry_json).collect::<Vec<_>>(),
        }));
    }

    print_header(
        &app,
        environment.as_deref(),
        telemetry.endpoint(),
        Some((since, until)),
    );

    if entries.is_empty() {
        println!(
            "\n  {}",
            format!(
                "nothing matched. Widen the range with --since 24h, drop --level, or pass \
                 --all-environments; `deis status -a {app}` says whether anything is running at \
                 all."
            )
            .yellow()
        );
        return Ok(());
    }

    println!();
    for entry in &entries {
        println!("{}", format_entry(entry));
    }

    if truncated {
        println!(
            "\n  {}",
            format!(
                "stopped after {MAX_PAGES} pages without filling --limit {limit}; narrow the range \
                 or the filters to see the rest."
            )
            .yellow()
        );
    }
    Ok(())
}

pub async fn tail(args: TailArgs, output: Output) -> Result<()> {
    let settings = resolve(&Overrides {
        app: args.app,
        environment: args.environment,
        binding: None,
    })?;
    let app = settings.checked_app()?.to_string();
    let environment = environment_filter(&settings, args.all_environments)?;
    let level = parse_level(args.level.as_deref())?;

    let endpoint = settings.telemetry_endpoint()?.to_string();
    let credential = TelemetryCredential::obtain(&settings, &app, environment.as_deref()).await?;
    let mut telemetry = Telemetry::connect(&endpoint, credential).await?;
    let mut stream = telemetry
        .tail(TailRequest {
            app_id: app.clone(),
            min_level: level as i32,
        })
        .await?;

    if output.human() {
        print_header(&app, environment.as_deref(), telemetry.endpoint(), None);
        println!(
            "\n  {}",
            "following new lines only; Ctrl-C to stop. `deis logs` reads what is already stored."
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
                    println!("\n  {}", format!("stopped after {shown} line(s)").dimmed());
                }
                return Ok(());
            }
            next = stream.message() => next,
        };

        let entry = match next {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                // The server closed the stream. Said plainly, because the
                // alternative reading - "this application went quiet" - is
                // wrong and would send someone looking in the wrong place.
                if output.human() {
                    println!(
                        "\n  {}",
                        format!(
                            "deis-telemetry at {} closed the stream after {shown} line(s); run \
                             `deis tail` again to reconnect.",
                            telemetry.endpoint()
                        )
                        .yellow()
                    );
                }
                return Ok(());
            }
            Err(e) => {
                anyhow::bail!(
                    "the log stream from deis-telemetry at {} ended with an error after {shown} \
                     line(s): {}\nRun `deis tail` again to reconnect.",
                    telemetry.endpoint(),
                    e.message()
                );
            }
        };

        if let Some(environment) = &environment
            && !telemetry::entry_matches_environment(&entry.fields, environment)
        {
            continue;
        }

        shown += 1;
        if output.json {
            // One object per line: the stream has no end, so a single
            // document would never be complete enough to parse.
            println!("{}", serde_json::to_string(&entry_json(&entry))?);
        } else {
            println!("{}", format_entry(&entry));
        }
    }
}

/// Which environment to filter by, or `None` for every environment.
fn environment_filter(settings: &Settings, all: bool) -> Result<Option<String>> {
    if all {
        return Ok(None);
    }
    Ok(Some(settings.checked_environment()?.to_string()))
}

fn parse_level(level: Option<&str>) -> Result<LogLevel> {
    match level {
        Some(text) => Ok(text.parse::<telemetry::Level>()?.0),
        None => Ok(LogLevel::Unspecified),
    }
}

fn print_header(
    app: &str,
    environment: Option<&str>,
    endpoint: &str,
    range: Option<(DateTime<Utc>, Option<DateTime<Utc>>)>,
) {
    println!(
        "\n{} {} {} {}",
        "Logs of".bold(),
        app.cyan(),
        "from".bold(),
        endpoint.dimmed()
    );
    match environment {
        Some(environment) => println!(
            "  environment  {} {}",
            environment.cyan(),
            "(--all-environments shows every one)".dimmed()
        ),
        None => println!("  environment  {}", "all".cyan()),
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

/// One line: when, how bad, who said it, and what it said.
fn format_entry(entry: &LogEntry) -> String {
    let level = telemetry::level_name(entry.level);
    let painted = match LogLevel::try_from(entry.level).unwrap_or(LogLevel::Unspecified) {
        LogLevel::Error | LogLevel::Fatal => format!("{level:<5}").red().bold().to_string(),
        LogLevel::Warn => format!("{level:<5}").yellow().to_string(),
        LogLevel::Info => format!("{level:<5}").green().to_string(),
        _ => format!("{level:<5}").dimmed().to_string(),
    };

    // Every one of these came off the wire, and the message came from an
    // application verbatim: `console.log("\u{1b}[2J\u{1b}[31mCOMPROMISED")` is
    // a repainted terminal for whoever reads that application's logs, and a
    // `\n` in it is one entry printing as two. The component and the fields
    // are escaped for the same reason and not because they are as reachable:
    // a rule applied to the field this quarter's attacker used is how the next
    // field becomes next quarter's finding.
    let mut source = escaped(&entry.component);
    if let Some(environment) = entry.fields.get(telemetry::ENVIRONMENT_FIELD) {
        source.push('/');
        source.push_str(&escape_control(environment));
    }

    let mut line = format!(
        "{} {} {} {}",
        timestamp_micros(entry.timestamp_us).dimmed(),
        painted,
        source.cyan(),
        escape_control(&entry.message)
    );

    let extra = extra_fields(&entry.fields);
    if !extra.is_empty() {
        line.push(' ');
        line.push_str(&extra.dimmed().to_string());
    }
    if !entry.trace_id.is_empty() {
        line.push(' ');
        line.push_str(
            &format!("trace={}", escape_control(&entry.trace_id))
                .dimmed()
                .to_string(),
        );
    }
    line
}

/// Structured fields other than the environment, which is already in the
/// source column and would otherwise be printed twice on every line.
fn extra_fields(fields: &HashMap<String, String>) -> String {
    let mut pairs: Vec<String> = fields
        .iter()
        .filter(|(key, _)| key.as_str() != telemetry::ENVIRONMENT_FIELD)
        // The key as well as the value: a node is free to record a field
        // under any name, and an application that reaches a code path which
        // records one of its own strings as a field name gets the same
        // terminal for free if only values are escaped.
        .map(|(key, value)| format!("{}={}", escape_control(key), escape_control(value)))
        .collect();
    pairs.sort();
    pairs.join(" ")
}

fn entry_json(entry: &LogEntry) -> serde_json::Value {
    serde_json::json!({
        "timestamp_us": entry.timestamp_us,
        "time": timestamp_micros(entry.timestamp_us),
        "level": telemetry::level_name(entry.level),
        "component": entry.component,
        "app_id": entry.app_id,
        "environment": entry.fields.get(telemetry::ENVIRONMENT_FIELD),
        "message": entry.message,
        "fields": entry.fields,
        "trace_id": entry.trace_id,
        "span_id": entry.span_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(level: LogLevel, environment: Option<&str>) -> LogEntry {
        let mut fields = HashMap::new();
        if let Some(environment) = environment {
            fields.insert(
                telemetry::ENVIRONMENT_FIELD.to_string(),
                environment.to_string(),
            );
        }
        fields.insert("request_id".to_string(), "r-1".to_string());
        LogEntry {
            timestamp_us: 1_787_998_800_000_000,
            level: level as i32,
            component: "deis-runtime".to_string(),
            app_id: "demo".to_string(),
            message: "served".to_string(),
            fields,
            trace_id: "t-1".to_string(),
            span_id: String::new(),
            tenant: String::new(),
        }
    }

    #[test]
    fn a_line_names_the_component_and_the_environment_it_came_from() {
        let rendered = format_entry(&entry(LogLevel::Info, Some("preview")));
        assert!(rendered.contains("deis-runtime/preview"), "{rendered}");
        assert!(rendered.contains("served"), "{rendered}");
        assert!(rendered.contains("request_id=r-1"), "{rendered}");
        assert!(rendered.contains("trace=t-1"), "{rendered}");
    }

    /// The environment is already the second half of the source column;
    /// repeating it in the field list would push the message off the screen
    /// on every single line.
    #[test]
    fn the_environment_is_not_printed_twice() {
        let entry = entry(LogLevel::Warn, Some("production"));
        assert_eq!(extra_fields(&entry.fields), "request_id=r-1");
    }

    #[test]
    fn an_unnamed_level_means_no_floor() {
        assert_eq!(parse_level(None).unwrap(), LogLevel::Unspecified);
        assert_eq!(parse_level(Some("error")).unwrap(), LogLevel::Error);
        assert!(parse_level(Some("loud")).is_err());
    }

    /// `--json` on a tail is newline-delimited, so each line has to be a
    /// complete document on its own.
    #[test]
    fn one_entry_serialises_to_one_line() {
        let json = serde_json::to_string(&entry_json(&entry(LogLevel::Error, Some("production"))))
            .unwrap();
        assert!(!json.contains('\n'), "{json}");
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["level"], "ERROR");
        assert_eq!(parsed["environment"], "production");
    }
}

/// What an application can put on the operator's terminal through `deis logs`.
///
/// Separate from `mod tests` above because these need colour forced off:
/// `format_entry` paints the level and the source, and a test asserting "no
/// escape sequence reached the line" would otherwise be asserting about
/// `colored`'s own escapes rather than about the tenant's.
#[cfg(test)]
mod terminal_safety_test {
    use super::*;

    fn plain(entry: &LogEntry) -> String {
        colored::control::set_override(false);
        let rendered = format_entry(entry);
        colored::control::unset_override();
        rendered
    }

    fn entry_saying(message: &str) -> LogEntry {
        LogEntry {
            timestamp_us: 1_787_998_800_000_000,
            level: LogLevel::Info as i32,
            component: "deis-runtime".to_string(),
            app_id: "demo".to_string(),
            message: message.to_string(),
            fields: HashMap::new(),
            trace_id: String::new(),
            span_id: String::new(),
            tenant: String::new(),
        }
    }

    #[test]
    fn an_application_cannot_repaint_the_screen_through_its_own_log_line() {
        // Reproduced against a real deis-telemetry before this was closed:
        // `console.log("\u{1b}[2J\u{1b}[H\u{1b}[31mCOMPROMISED")` reached
        // stdout unchanged, with stdout not a TTY. The message travels to the
        // CLI through the shipper, which never passes through the `fmt` layer
        // that would have escaped the ESC, so nothing between the isolate and
        // this line escapes anything.
        let rendered = plain(&entry_saying("\u{1b}[2J\u{1b}[H\u{1b}[31mCOMPROMISED"));
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert!(rendered.contains("\\u{1b}[2J"), "{rendered:?}");
    }

    #[test]
    fn an_application_cannot_retitle_the_operators_window() {
        let rendered = plain(&entry_saying("\u{1b}]0;pwned\u{7}"));
        assert!(!rendered.contains('\u{7}'), "{rendered:?}");
        assert!(rendered.contains("\\u{1b}]0;pwned\\u{7}"), "{rendered:?}");
    }

    #[test]
    fn one_entry_cannot_print_as_two() {
        // A newline in the message would make one entry look like two, the
        // second of which carries no timestamp, no level and no source - and
        // therefore reads as a continuation of whatever the operator was
        // looking at rather than as something an application said.
        let rendered = plain(&entry_saying("first\nsecond"));
        assert_eq!(rendered.lines().count(), 1, "{rendered:?}");
        assert!(rendered.contains("first\\nsecond"), "{rendered:?}");
    }

    #[test]
    fn the_source_column_is_escaped_too() {
        // `component` and the environment are the source column. Neither is
        // checked against a shape on the way in, and both are joined into one
        // painted cell.
        let mut entry = entry_saying("ordinary");
        entry.component = "deis-runtime\u{1b}[31m".to_string();
        entry.fields.insert(
            telemetry::ENVIRONMENT_FIELD.to_string(),
            "production\nWARN forged".to_string(),
        );
        let rendered = plain(&entry);
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert_eq!(rendered.lines().count(), 1, "{rendered:?}");
    }

    #[test]
    fn a_structured_fields_name_is_escaped_as_well_as_its_value() {
        // Both halves: a node records fields under names it chooses, and the
        // renderer prints `name=value` with neither half constrained.
        let mut entry = entry_saying("ordinary");
        entry
            .fields
            .insert("key\u{1b}[31m".to_string(), "value\nforged".to_string());
        let rendered = plain(&entry);
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert_eq!(rendered.lines().count(), 1, "{rendered:?}");
        assert!(
            rendered.contains("key\\u{1b}[31m=value\\nforged"),
            "{rendered:?}"
        );
    }

    #[test]
    fn the_invocation_id_is_escaped() {
        let mut entry = entry_saying("ordinary");
        entry.trace_id = "abc\u{1b}[2J".to_string();
        let rendered = plain(&entry);
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
    }

    #[test]
    fn an_ordinary_line_is_left_exactly_as_it_was_written() {
        // The other half of the rule: escaping that mangles legitimate text is
        // escaping an operator turns off.
        let rendered = plain(&entry_saying(
            "served 200 in 4ms - \u{65e5}\u{672c}\u{8a9e} - caf\u{e9}",
        ));
        assert!(
            rendered.contains("served 200 in 4ms - \u{65e5}\u{672c}\u{8a9e} - caf\u{e9}"),
            "{rendered:?}"
        );
    }
}
