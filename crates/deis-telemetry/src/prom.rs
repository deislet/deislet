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
    render_self_by_app(&mut out, stats);
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

/// Write this process's own numbers that are broken down by application.
///
/// Separate from [`render_self`] because these carry a label, and because the
/// set is not fixed: an application appears only once something of its was
/// actually thrown away. The header lines are written once per family even
/// though the series count varies, which is what the format requires.
fn render_self_by_app(out: &mut String, stats: &ServiceStats) {
    let metrics = stats.labelled_snapshot();
    let mut current: Option<&str> = None;
    for metric in &metrics {
        if current != Some(metric.name) {
            let _ = writeln!(out, "# HELP {SELF_PREFIX}_{} {}", metric.name, metric.help);
            let _ = writeln!(
                out,
                "# TYPE {SELF_PREFIX}_{} {}",
                metric.name,
                metric.kind.as_str()
            );
            current = Some(metric.name);
        }
        let pairs: Vec<(String, String)> = metric
            .labels
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect();
        let _ = writeln!(
            out,
            "{SELF_PREFIX}_{}{} {}",
            metric.name,
            render_pairs(&pairs),
            metric.value
        );
    }
}

/// A metric family as it will be written out: either a set of independent
/// gauge series, or one histogram assembled from the three suffixed series
/// Prometheus expects a histogram to be made of.
#[derive(Default)]
struct Family {
    /// Label signature -> (value, sample time).
    gauges: BTreeMap<String, (f64, i64)>,
    /// Label signature without `le` -> the histogram series under it.
    histograms: BTreeMap<String, HistogramSeries>,
}

/// One histogram series: every bucket, the sum and the observation count that
/// share one label set.
#[derive(Default)]
struct HistogramSeries {
    /// The label set, kept unrendered so `le` can be appended to it per line.
    pairs: Vec<(String, String)>,
    /// (upper bound, cumulative count, sample time).
    buckets: Vec<(f64, f64, i64)>,
    sum: Option<(f64, i64)>,
    count: Option<(f64, i64)>,
}

/// The three suffixes that make a series part of a histogram rather than a
/// gauge of its own.
const HISTOGRAM_SUFFIXES: [&str; 3] = ["_bucket", "_sum", "_count"];

/// Keep the newer of two samples of one series.
fn keep_newest(slot: &mut Option<(f64, i64)>, value: f64, timestamp_us: i64) {
    match slot {
        Some((_, seen)) if *seen >= timestamp_us => {}
        _ => *slot = Some((value, timestamp_us)),
    }
}

/// Read the `le` label off a bucket sample.
///
/// `+Inf` is recognised as itself rather than left to `parse`, because that
/// is the token the exposition format uses and the one a producer sends.
fn bucket_bound(sample: &MetricRecord) -> Option<f64> {
    let raw = sample.labels.get("le")?;
    if raw == "+Inf" || raw.eq_ignore_ascii_case("inf") {
        return Some(f64::INFINITY);
    }
    raw.parse::<f64>().ok().filter(|v| !v.is_nan())
}

fn render_samples(out: &mut String, samples: &[MetricRecord]) {
    // A histogram arrives as three ordinary series - `_bucket`, `_sum` and
    // `_count` - and is only a histogram if they are exported together under
    // one `# TYPE ... histogram` header. The `_bucket` series is what proves
    // the family is one, so it is looked for first: without this pass a
    // producer pushing buckets would get a heap of unrelated gauges and
    // `histogram_quantile` would have nothing to work with, which is the
    // whole point of pushing buckets.
    let mut histogram_families: BTreeSet<String> = BTreeSet::new();
    for sample in samples {
        let Some(name) = sanitize_name(&sample.name) else {
            continue;
        };
        if let Some(base) = name.strip_suffix("_bucket")
            && !base.is_empty()
            && bucket_bound(sample).is_some()
        {
            histogram_families.insert(base.to_string());
        }
    }

    // Group by exported family so `# HELP` and `# TYPE` appear exactly once
    // per family, then by label signature so one series appears exactly once.
    let mut grouped: BTreeMap<String, Family> = BTreeMap::new();
    for sample in samples {
        let Some(name) = sanitize_name(&sample.name) else {
            warn!(
                metric = %sample.name,
                "metric name has no character usable in Prometheus format, \
                 the series is left out of /metrics but stays queryable"
            );
            continue;
        };

        let part = HISTOGRAM_SUFFIXES.iter().find_map(|suffix| {
            let base = name.strip_suffix(suffix)?;
            histogram_families
                .contains(base)
                .then(|| (base.to_string(), *suffix))
        });

        let Some((base, suffix)) = part else {
            let labels = render_pairs(&label_pairs(sample, ""));
            let gauges = &mut grouped.entry(name).or_default().gauges;
            match gauges.get(&labels) {
                Some((_, seen)) if *seen >= sample.timestamp_us => {}
                _ => {
                    gauges.insert(labels, (sample.value, sample.timestamp_us));
                }
            }
            continue;
        };

        // `le` is not part of a series' identity: every bucket of one series
        // carries the same other labels and differs only in its bound, so it
        // is left out of the key here and put back, last, on the line.
        let pairs = label_pairs(sample, "le");
        let labels = render_pairs(&pairs);
        let series = grouped
            .entry(base)
            .or_default()
            .histograms
            .entry(labels)
            .or_default();
        series.pairs = pairs;
        match suffix {
            "_bucket" => {
                let Some(bound) = bucket_bound(sample) else {
                    warn!(
                        metric = %sample.name,
                        "a bucket sample carries no usable `le` label, so it names no upper \
                         bound; it is left out of /metrics and the histogram it belongs to \
                         will under-report at that bound"
                    );
                    continue;
                };
                match series
                    .buckets
                    .iter_mut()
                    .find(|(existing, _, _)| *existing == bound)
                {
                    Some(slot) if slot.2 < sample.timestamp_us => {
                        *slot = (bound, sample.value, sample.timestamp_us)
                    }
                    Some(_) => {}
                    None => series
                        .buckets
                        .push((bound, sample.value, sample.timestamp_us)),
                }
            }
            "_sum" => keep_newest(&mut series.sum, sample.value, sample.timestamp_us),
            _ => keep_newest(&mut series.count, sample.value, sample.timestamp_us),
        }
    }

    for (name, family) in grouped {
        if !family.histograms.is_empty() {
            render_histogram(out, &name, family.histograms);
            continue;
        }
        let _ = writeln!(out, "# HELP {name} Latest sample pushed to deis-telemetry");
        // Every other pushed series is exported as a gauge: this service is
        // handed values, it does not know whether the producer resets them,
        // so it cannot honestly claim a counter's monotonicity. A histogram
        // is the exception only because its three parts say so themselves.
        let _ = writeln!(out, "# TYPE {name} gauge");
        for (labels, (value, _)) in family.gauges {
            // No timestamp is emitted: the scrape time is used instead, which
            // keeps samples inside Prometheus's staleness window. The export
            // window already keeps series that stopped reporting out.
            let _ = writeln!(out, "{name}{labels} {}", format_value(value));
        }
    }
}

/// Write one histogram family.
///
/// The bucket lines ascend by upper bound and `+Inf` comes last, because that
/// is the order `histogram_quantile` reads them in.
fn render_histogram(out: &mut String, name: &str, series: BTreeMap<String, HistogramSeries>) {
    let _ = writeln!(
        out,
        "# HELP {name} Latest histogram pushed to deis-telemetry"
    );
    let _ = writeln!(out, "# TYPE {name} histogram");
    for (labels, mut series) in series {
        // A family with no `+Inf` bucket is one a scraper cannot compute a
        // quantile from. The observation count is exactly what that bucket
        // holds, so it stands in when the producer left it out; when there is
        // no count either, nothing honest can be written and the gap is said
        // out loud instead of being invented.
        let has_infinity = series
            .buckets
            .iter()
            .any(|(bound, _, _)| bound.is_infinite());
        if !has_infinity {
            match series.count {
                Some((count, timestamp_us)) => {
                    series.buckets.push((f64::INFINITY, count, timestamp_us))
                }
                None => warn!(
                    metric = %name,
                    labels = %labels,
                    "a histogram series arrived with no +Inf bucket and no count, so it has \
                     no total; histogram_quantile will refuse the family until the producer \
                     sends a complete histogram"
                ),
            }
        }
        series
            .buckets
            .sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        for (bound, value, _) in &series.buckets {
            let mut pairs = series.pairs.clone();
            pairs.push(("le".to_string(), format_value(*bound)));
            let _ = writeln!(
                out,
                "{name}_bucket{} {}",
                render_pairs(&pairs),
                format_value(*value)
            );
        }
        if let Some((sum, _)) = series.sum {
            let _ = writeln!(out, "{name}_sum{labels} {}", format_value(sum));
        }
        if let Some((count, _)) = series.count {
            let _ = writeln!(out, "{name}_count{labels} {}", format_value(count));
        }
    }
}

/// Build the label pairs of one sample, `app_id` first when present, leaving
/// out `skip` (a label the caller renders itself, such as a bucket's `le`).
///
/// One label name may appear at most once on a line. Prometheus does not skip
/// a line that repeats one, it fails the entire scrape, so a single bad pair
/// of names pushed by a single application would blank out every series on
/// this endpoint - including this service's own counters. Names are therefore
/// deduplicated after sanitising, which is where the collisions come from:
/// `http.status` and `http-status` are two different names to a producer and
/// the same name here. The first one wins, which also settles `app_id`: the
/// dedicated field is placed first, so a label of that name loses to it.
fn label_pairs(sample: &MetricRecord, skip: &str) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut taken: BTreeSet<String> = BTreeSet::new();
    if !sample.app_id.is_empty() {
        taken.insert("app_id".to_string());
        pairs.push(("app_id".to_string(), sample.app_id.clone()));
    }
    for (key, value) in &sample.labels {
        if !skip.is_empty() && key == skip {
            continue;
        }
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
        pairs.push((sanitized, value.clone()));
    }
    pairs
}

/// Render label pairs as the `{k="v",...}` part of an exported line.
fn render_pairs(pairs: &[(String, String)]) -> String {
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
    fn a_live_buffer_loss_is_exported_against_the_application_that_caused_it() {
        // The number an operator goes looking for after a follow stream
        // skipped the lines they were watching for. The plain total says
        // something went; only this says whose.
        let stats = ServiceStats::new();
        stats.tail_entries_dropped_by_app.add("noisy", 996);
        stats.tail_entries_dropped_by_app.add("noisy", 4);
        stats.trace_tail_entries_dropped_by_app.add("noisy", 7);
        let text = render(&[], &stats);
        assert!(
            text.contains("# TYPE deis_telemetry_tail_entries_dropped_by_app_total counter"),
            "{text}"
        );
        assert!(
            text.contains(
                "deis_telemetry_tail_entries_dropped_by_app_total{app_id=\"noisy\"} 1000"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "deis_telemetry_trace_tail_entries_dropped_by_app_total{app_id=\"noisy\"} 7"
            ),
            "{text}"
        );
    }

    #[test]
    fn an_application_that_lost_nothing_is_not_exported_at_all() {
        // One series per application that has never lost a line would be
        // cardinality bought for nothing, and a flat zero next to a real
        // loss reads as if the loss had been measured for both.
        let text = render(&[], &ServiceStats::new());
        assert!(
            !text.contains("tail_entries_dropped_by_app_total{"),
            "{text}"
        );
    }

    #[test]
    fn an_application_id_with_a_quote_in_it_cannot_break_the_page() {
        // The id is whatever a producer put in the record, so it reaches the
        // export unvalidated. An unescaped quote would end the label early
        // and make the scraper reject every series on the page, not just
        // this one.
        let stats = ServiceStats::new();
        stats.tail_entries_dropped_by_app.add("we\"ird\\", 3);
        let text = render(&[], &stats);
        assert!(
            text.contains(
                "deis_telemetry_tail_entries_dropped_by_app_total{app_id=\"we\\\"ird\\\\\"} 3"
            ),
            "{text}"
        );
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
    fn buckets_sum_and_count_are_exported_as_one_histogram() {
        // The three series a producer pushes have to come back out as one
        // family with a histogram TYPE line, or histogram_quantile has
        // nothing to compute a p99 from - which is the whole reason the
        // runtime pushes buckets instead of a high-water gauge.
        let text = render(
            &[
                sample("latency_ms_bucket", "shop", &[("le", "10")], 3.0, 10),
                sample("latency_ms_bucket", "shop", &[("le", "100")], 7.0, 10),
                sample("latency_ms_bucket", "shop", &[("le", "+Inf")], 9.0, 10),
                sample("latency_ms_sum", "shop", &[], 420.0, 10),
                sample("latency_ms_count", "shop", &[], 9.0, 10),
            ],
            &ServiceStats::new(),
        );
        assert!(
            text.contains("# TYPE latency_ms histogram"),
            "unexpected export:\n{text}"
        );
        assert_eq!(text.matches("# TYPE latency_ms").count(), 1);
        assert!(text.contains("latency_ms_bucket{app_id=\"shop\",le=\"10\"} 3"));
        assert!(text.contains("latency_ms_bucket{app_id=\"shop\",le=\"+Inf\"} 9"));
        assert!(text.contains("latency_ms_sum{app_id=\"shop\"} 420"));
        assert!(text.contains("latency_ms_count{app_id=\"shop\"} 9"));
        assert!(
            !text.contains("# TYPE latency_ms_bucket"),
            "the parts must not also be exported as families of their own:\n{text}"
        );
    }

    #[test]
    fn bucket_lines_ascend_by_bound_with_infinity_last() {
        // Pushed in the wrong order on purpose: a scraper reads the bounds in
        // the order they appear, and out-of-order buckets interpolate
        // nonsense.
        let text = render(
            &[
                sample("latency_ms_bucket", "shop", &[("le", "+Inf")], 9.0, 10),
                sample("latency_ms_bucket", "shop", &[("le", "100")], 7.0, 10),
                sample("latency_ms_bucket", "shop", &[("le", "2")], 1.0, 10),
                sample("latency_ms_bucket", "shop", &[("le", "10")], 3.0, 10),
            ],
            &ServiceStats::new(),
        );
        let bounds: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("latency_ms_bucket"))
            .map(|line| {
                let start = line.find("le=\"").expect("a bucket line carries le") + 4;
                let rest = &line[start..];
                &rest[..rest.find('"').expect("le is quoted")]
            })
            .collect();
        assert_eq!(bounds, vec!["2", "10", "100", "+Inf"], "got:\n{text}");
    }

    #[test]
    fn two_label_sets_of_one_histogram_stay_separate_series() {
        let text = render(
            &[
                sample("latency_ms_bucket", "shop", &[("le", "10")], 1.0, 10),
                sample("latency_ms_bucket", "blog", &[("le", "10")], 5.0, 10),
                sample("latency_ms_count", "shop", &[], 1.0, 10),
                sample("latency_ms_count", "blog", &[], 5.0, 10),
            ],
            &ServiceStats::new(),
        );
        assert_eq!(text.matches("# TYPE latency_ms histogram").count(), 1);
        assert!(text.contains("latency_ms_bucket{app_id=\"blog\",le=\"10\"} 5"));
        assert!(text.contains("latency_ms_bucket{app_id=\"shop\",le=\"10\"} 1"));
    }

    #[test]
    fn a_histogram_missing_its_infinity_bucket_gets_one_from_the_count() {
        // A family with no +Inf bucket is one Prometheus refuses to compute a
        // quantile from, and the observation count is exactly what that
        // bucket holds.
        let text = render(
            &[
                sample("latency_ms_bucket", "shop", &[("le", "10")], 3.0, 10),
                sample("latency_ms_count", "shop", &[], 8.0, 10),
            ],
            &ServiceStats::new(),
        );
        assert!(
            text.contains("latency_ms_bucket{app_id=\"shop\",le=\"+Inf\"} 8"),
            "unexpected export:\n{text}"
        );
    }

    #[test]
    fn a_sum_without_buckets_is_still_a_plain_gauge() {
        // The existing counters end in `_sum` and `_count` and nothing else;
        // making them part of a histogram nobody pushed would break every
        // dashboard reading them.
        let text = render(
            &[
                sample(
                    "deis_runtime_isolate_execution_ms_sum",
                    "shop",
                    &[],
                    5.0,
                    10,
                ),
                sample(
                    "deis_runtime_isolate_execution_ms_count",
                    "shop",
                    &[],
                    2.0,
                    10,
                ),
            ],
            &ServiceStats::new(),
        );
        assert!(text.contains("# TYPE deis_runtime_isolate_execution_ms_sum gauge"));
        assert!(text.contains("# TYPE deis_runtime_isolate_execution_ms_count gauge"));
        assert!(!text.contains("histogram"));
    }

    #[test]
    fn a_bucket_without_a_usable_bound_is_left_out_rather_than_exported_wrong() {
        let text = render(
            &[
                sample("latency_ms_bucket", "shop", &[("le", "wat")], 3.0, 10),
                sample("latency_ms_bucket", "shop", &[("le", "10")], 4.0, 10),
                sample("latency_ms_count", "shop", &[], 4.0, 10),
            ],
            &ServiceStats::new(),
        );
        assert!(text.contains("# TYPE latency_ms histogram"));
        assert!(!text.contains("le=\"wat\""), "got:\n{text}");
        assert!(text.contains("latency_ms_bucket{app_id=\"shop\",le=\"10\"} 4"));
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
