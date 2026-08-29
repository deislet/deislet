//! Rendering of the Prometheus text exposition format.
//!
//! What is exported is exactly what was pushed in: the latest sample of every
//! series inside the export window, plus this process's own counters. Nothing
//! is derived, judged or alerted on here - a scraper (or Grafana on top of it)
//! decides what any of it means.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use tracing::warn;

use crate::stats::ServiceStats;
use crate::store::MetricRecord;

/// Prefix of this service's own counters.
const SELF_PREFIX: &str = "deis_telemetry";

/// Render the export body.
///
/// `samples` is expected to be the latest sample per series; duplicates of the
/// same series are collapsed keeping the newest, because Prometheus rejects a
/// scrape that repeats a series.
pub fn render(samples: &[MetricRecord], stats: &ServiceStats) -> String {
    let mut out = String::with_capacity(1024 + samples.len() * 96);
    render_self(&mut out, stats);
    render_samples(&mut out, samples);
    out
}

fn render_self(out: &mut String, stats: &ServiceStats) {
    // Whatever the process keeps, the export shows: the set is walked from
    // its single declaration in `stats`, never copied here. A counter that
    // exists but is not exported would leave an operator with no evidence
    // for something the service really did.
    for metric in stats.snapshot() {
        let name = metric.name;
        let _ = writeln!(out, "# HELP {SELF_PREFIX}_{name} {}", metric.help);
        let _ = writeln!(out, "# TYPE {SELF_PREFIX}_{name} {}", metric.kind.as_str());
        let _ = writeln!(out, "{SELF_PREFIX}_{name} {}", metric.value);
    }
}

fn render_samples(out: &mut String, samples: &[MetricRecord]) {
    // Group by exported name so `# HELP` and `# TYPE` appear exactly once per
    // name, then by label signature so one series appears exactly once.
    let mut grouped: BTreeMap<String, BTreeMap<String, (f64, i64)>> = BTreeMap::new();
    for sample in samples {
        let Some(name) = sanitize_name(&sample.name) else {
            warn!(
                metric = %sample.name,
                "metric name has no character usable in Prometheus format, \
                 the series is left out of /metrics but stays queryable"
            );
            continue;
        };
        let labels = label_signature(sample);
        grouped
            .entry(name)
            .or_default()
            .entry(labels)
            .and_modify(|slot| {
                if sample.timestamp_us > slot.1 {
                    *slot = (sample.value, sample.timestamp_us);
                }
            })
            .or_insert((sample.value, sample.timestamp_us));
    }

    for (name, series) in grouped {
        let _ = writeln!(out, "# HELP {name} Latest sample pushed to deis-telemetry");
        // Every pushed series is exported as a gauge: this service is handed
        // values, it does not know whether the producer resets them, so it
        // cannot honestly claim a counter's monotonicity.
        let _ = writeln!(out, "# TYPE {name} gauge");
        for (labels, (value, _)) in series {
            // No timestamp is emitted: the scrape time is used instead, which
            // keeps samples inside Prometheus's staleness window. The export
            // window already keeps series that stopped reporting out.
            let _ = writeln!(out, "{name}{labels} {}", format_value(value));
        }
    }
}

/// Build the `{k="v",...}` part, with `app_id` first when present.
///
/// One label name may appear at most once on a line. Prometheus does not skip
/// a line that repeats one, it fails the entire scrape, so a single bad pair
/// of names pushed by a single application would blank out every series on
/// this endpoint - including this service's own counters. Names are therefore
/// deduplicated after sanitising, which is where the collisions come from:
/// `http.status` and `http-status` are two different names to a producer and
/// the same name here. The first one wins, which also settles `app_id`: the
/// dedicated field is placed first, so a label of that name loses to it.
fn label_signature(sample: &MetricRecord) -> String {
    let mut pairs: Vec<(String, &str)> = Vec::new();
    let mut taken: BTreeSet<String> = BTreeSet::new();
    if !sample.app_id.is_empty() {
        taken.insert("app_id".to_string());
        pairs.push(("app_id".to_string(), sample.app_id.as_str()));
    }
    for (key, value) in &sample.labels {
        let Some(sanitized) = sanitize_name(key) else {
            warn!(
                label = %key,
                metric = %sample.name,
                "label name has no character usable in Prometheus format, \
                 the label is left out of /metrics"
            );
            continue;
        };
        if !taken.insert(sanitized.clone()) {
            warn!(
                label = %key,
                exported_as = %sanitized,
                metric = %sample.name,
                "this label name is already taken on the series - by the dedicated app_id \
                 field, or by an earlier label that sanitises to the same name - so it is \
                 left out of /metrics; emitting it twice would repeat a label name and make \
                 the scraper reject the entire scrape, not just this series"
            );
            continue;
        }
        pairs.push((sanitized, value.as_str()));
    }
    if pairs.is_empty() {
        return String::new();
    }
    let mut out = String::from("{");
    for (i, (key, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{key}=\"{}\"", escape_label_value(value));
    }
    out.push('}');
    out
}

/// Map an arbitrary string onto `[a-zA-Z_:][a-zA-Z0-9_:]*`, which is what the
/// exposition format accepts. Returns `None` when nothing usable is left.
fn sanitize_name(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(raw.len());
    for (i, ch) in raw.chars().enumerate() {
        let ok =
            ch.is_ascii_alphabetic() || ch == '_' || ch == ':' || (i > 0 && ch.is_ascii_digit());
        out.push(if ok { ch } else { '_' });
    }
    if out.chars().all(|c| c == '_') {
        return None;
    }
    Some(out)
}

fn escape_label_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out
}

/// Format a value the way the exposition format expects, including the three
/// special tokens it uses for values `f64` can hold but decimal text cannot.
fn format_value(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_string()
    } else if value.is_infinite() {
        if value.is_sign_positive() {
            "+Inf".to_string()
        } else {
            "-Inf".to_string()
        }
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(
        name: &str,
        app_id: &str,
        labels: &[(&str, &str)],
        value: f64,
        ts: i64,
    ) -> MetricRecord {
        MetricRecord {
            name: name.to_string(),
            app_id: app_id.to_string(),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            value,
            timestamp_us: ts,
        }
    }

    #[test]
    fn self_counters_are_always_present() {
        let text = render(&[], &ServiceStats::new());
        assert!(text.contains("# TYPE deis_telemetry_logs_accepted_total counter"));
        assert!(text.contains("deis_telemetry_tail_streams_open 0"));
    }

    #[test]
    fn series_carry_app_id_and_labels() {
        let text = render(
            &[sample(
                "requests_total",
                "shop",
                &[("route", "/api"), ("status", "200")],
                7.0,
                10,
            )],
            &ServiceStats::new(),
        );
        assert!(
            text.contains("requests_total{app_id=\"shop\",route=\"/api\",status=\"200\"} 7"),
            "unexpected export:\n{text}"
        );
        assert_eq!(text.matches("# TYPE requests_total").count(), 1);
    }

    #[test]
    fn one_type_line_per_name_across_apps() {
        let text = render(
            &[
                sample("requests_total", "shop", &[], 1.0, 10),
                sample("requests_total", "blog", &[], 2.0, 10),
            ],
            &ServiceStats::new(),
        );
        assert_eq!(text.matches("# HELP requests_total").count(), 1);
        assert!(text.contains("requests_total{app_id=\"shop\"} 1"));
        assert!(text.contains("requests_total{app_id=\"blog\"} 2"));
    }

    #[test]
    fn duplicate_series_keeps_the_newest_sample() {
        let text = render(
            &[
                sample("latency_ms", "shop", &[], 1.0, 10),
                sample("latency_ms", "shop", &[], 9.0, 20),
            ],
            &ServiceStats::new(),
        );
        assert!(text.contains("latency_ms{app_id=\"shop\"} 9"));
        assert!(!text.contains("latency_ms{app_id=\"shop\"} 1"));
    }

    #[test]
    fn illegal_characters_are_replaced() {
        assert_eq!(
            sanitize_name("http.requests-total").as_deref(),
            Some("http_requests_total")
        );
        assert_eq!(sanitize_name("9lives").as_deref(), Some("_lives"));
        assert_eq!(sanitize_name(""), None);
        assert_eq!(sanitize_name("---"), None);
    }

    #[test]
    fn colliding_label_names_are_not_emitted_twice() {
        // Two distinct label names that sanitise to the same identifier. A
        // line carrying both is rejected by Prometheus as a duplicate label
        // name, and the rejection throws away the whole scrape, not just the
        // offending series.
        let text = render(
            &[sample(
                "requests",
                "",
                &[("http.status", "200"), ("http-status", "500")],
                1.0,
                1,
            )],
            &ServiceStats::new(),
        );
        let line = text
            .lines()
            .find(|l| l.starts_with("requests{"))
            .unwrap_or_else(|| panic!("the series must be exported:\n{text}"));
        assert_eq!(
            line.matches("http_status=").count(),
            1,
            "a label name must appear once per line, got: {line}"
        );
    }

    #[test]
    fn a_label_named_app_id_never_shadows_the_field() {
        let text = render(
            &[sample("requests", "shop", &[("app_id", "other")], 1.0, 1)],
            &ServiceStats::new(),
        );
        let line = text
            .lines()
            .find(|l| l.starts_with("requests{"))
            .unwrap_or_else(|| panic!("the series must be exported:\n{text}"));
        assert_eq!(line.matches("app_id=").count(), 1, "got: {line}");
        assert!(line.contains("app_id=\"shop\""), "got: {line}");
    }

    #[test]
    fn every_counter_the_process_keeps_is_exported() {
        // The counters are declared in exactly one place; this proves the
        // export walks that declaration rather than a hand-kept copy of it.
        let stats = ServiceStats::new();
        let text = render(&[], &stats);
        for metric in stats.snapshot() {
            assert!(
                text.contains(&format!("{SELF_PREFIX}_{} ", metric.name)),
                "counter {} is kept but never exported:\n{text}",
                metric.name
            );
        }
    }

    #[test]
    fn retention_of_metric_samples_is_visible_on_metrics() {
        let stats = ServiceStats::new();
        ServiceStats::add(&stats.retention_metric_samples_dropped, 5);
        let text = render(&[], &stats);
        assert!(
            text.contains("deis_telemetry_retention_metric_samples_dropped_total 5"),
            "retention deletes data permanently and must be observable:\n{text}"
        );
    }

    #[test]
    fn label_values_are_escaped() {
        let text = render(
            &[sample("weird", "", &[("path", "a\"b\\c\nd")], 1.0, 1)],
            &ServiceStats::new(),
        );
        assert!(
            text.contains(r#"weird{path="a\"b\\c\nd"} 1"#),
            "unexpected export:\n{text}"
        );
    }

    #[test]
    fn special_float_values_use_prometheus_tokens() {
        assert_eq!(format_value(f64::NAN), "NaN");
        assert_eq!(format_value(f64::INFINITY), "+Inf");
        assert_eq!(format_value(f64::NEG_INFINITY), "-Inf");
        assert_eq!(format_value(1.5), "1.5");
    }
}
