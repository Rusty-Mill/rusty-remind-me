//! Prometheus text-exposition metrics.
//!
//! # Off by default
//!
//! Gated on `REMIND_ME_METRICS_ENABLED`. This is instrumentation surface, not
//! a core feature, and while disabled every `record_*` call is a no-op and
//! `GET /metrics` is a plain 404 — "off means genuinely absent" rather than a
//! 403 or an empty-but-200 body that would imply the feature is running.
//!
//! Because the recorders decide for themselves, call sites never need their
//! own `if enabled` guard. The env var is read on each call rather than cached
//! at startup, so a test can flip it.
//!
//! # No Prometheus client crate
//!
//! The text exposition format is a few `# HELP`/`# TYPE` lines and
//! `name{labels} value` samples — string formatting, not a protocol problem.
//! The one thing a client library would buy is safe concurrent counters, and
//! a `Mutex` around plain maps covers that. This workspace has made the same
//! call for the webhook POST, the HTTP client and the ICS document.
//!
//! # Counter state vs. computed fresh
//!
//! Only things that are genuinely *events over time* live here: tool calls,
//! their durations, search-tier results, rate-limit rejections. No query can
//! reconstruct "how many times was search called since start" after the fact.
//!
//! Anything already answerable by a cheap point-in-time query — total
//! memories, sync outbox depth — is deliberately **not** shadowed as a
//! counter. The `/metrics` handler computes those per scrape and passes them
//! in as [`GaugeSpec`]s, so they cannot drift from the tables they describe.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

pub const METRICS_ENABLED_ENV: &str = "REMIND_ME_METRICS_ENABLED";

/// The three ranking tiers, emitted zero-valued rather than omitted so a
/// dashboard's query does not break on a server that has served no searches.
pub const SEARCH_TIERS: [&str; 3] = ["keyword", "semantic", "hybrid"];

/// Whether `/metrics` is served and recorders accumulate.
///
/// An empty string disables it explicitly, matching the reference's
/// empty-string opt-out convention.
pub fn metrics_enabled() -> bool {
    match std::env::var(METRICS_ENABLED_ENV) {
        Ok(raw) => {
            let raw = raw.trim().to_ascii_lowercase();
            !raw.is_empty() && raw != "0" && raw != "false" && raw != "no"
        }
        Err(_) => false,
    }
}

#[derive(Default)]
struct Counters {
    tool_calls: BTreeMap<String, u64>,
    tool_seconds: BTreeMap<String, f64>,
    search_tiers: BTreeMap<String, u64>,
    rate_limit_rejections: u64,
}

fn counters() -> &'static Mutex<Counters> {
    static COUNTERS: OnceLock<Mutex<Counters>> = OnceLock::new();
    COUNTERS.get_or_init(|| Mutex::new(Counters::default()))
}

/// Record one MCP tool call and how long it took.
pub fn record_tool_call(tool: &str, duration_seconds: f64) {
    if !metrics_enabled() {
        return;
    }
    let mut counters = counters().lock().unwrap_or_else(|e| e.into_inner());
    *counters.tool_calls.entry(tool.to_string()).or_insert(0) += 1;
    *counters.tool_seconds.entry(tool.to_string()).or_insert(0.0) += duration_seconds;
}

/// Record how many results each ranking tier contributed to one search.
pub fn record_search_tier(tier: &str, results: u64) {
    if !metrics_enabled() {
        return;
    }
    let mut counters = counters().lock().unwrap_or_else(|e| e.into_inner());
    *counters.search_tiers.entry(tier.to_string()).or_insert(0) += results;
}

/// Record one request turned away by the rate limiter.
pub fn record_rate_limit_rejection() {
    if !metrics_enabled() {
        return;
    }
    counters()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rate_limit_rejections += 1;
}

/// Drop all counter state. For tests — a process-wide counter otherwise
/// carries one test's calls into the next one's assertions.
pub fn reset() {
    *counters().lock().unwrap_or_else(|e| e.into_inner()) = Counters::default();
}

/// A value computed fresh for one scrape rather than tracked as a counter.
pub struct GaugeSpec {
    pub name: String,
    pub help: String,
    pub value: f64,
    pub labels: Vec<(String, String)>,
}

impl GaugeSpec {
    pub fn new(name: &str, help: &str, value: f64) -> Self {
        Self {
            name: name.to_string(),
            help: help.to_string(),
            value,
            labels: Vec::new(),
        }
    }
}

/// Escape a label value per the exposition format's quoting rules.
fn escape_label_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn format_labels(labels: &[(String, String)]) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let rendered: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{}=\"{}\"", k, escape_label_value(v)))
        .collect();
    format!("{{{}}}", rendered.join(","))
}

/// Render one sample line. Floats print without a trailing `.0` when whole,
/// because a counter reading `3` is what every other exporter emits.
fn sample(name: &str, value: f64, labels: &[(String, String)]) -> String {
    let rendered = if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{}", value)
    };
    format!("{}{} {}", name, format_labels(labels), rendered)
}

fn label(key: &str, value: &str) -> Vec<(String, String)> {
    vec![(key.to_string(), value.to_string())]
}

/// Render current counter state, plus any freshly-computed gauges.
///
/// Always valid for empty state: every family emits its `# HELP`/`# TYPE`
/// header, and the families whose label set is known ahead of time (the three
/// search tiers, the single rejection counter) emit zero-valued samples. Tool
/// families have no samples until a tool has been called, which is correct —
/// there is no known set of tool names to zero-fill.
///
/// Sorted within each family, so the output is diff-friendly and testable.
pub fn render_prometheus_text(gauges: &[GaugeSpec]) -> String {
    let counters = counters().lock().unwrap_or_else(|e| e.into_inner());
    // Build info first. The Prometheus idiom for metadata is a constant-1
    // gauge carrying it in labels rather than a metric per fact: a panel then
    // joins on it, which is what makes "latency changed" and "we upgraded"
    // the same graph instead of two unrelated observations. Emitted even on a
    // server that has served nothing, because an absent series reads as
    // "scrape target down", not "idle".
    let mut lines: Vec<String> = vec![
        "# HELP remind_me_build_info Build metadata; the value is always 1, the labels carry the information."
            .to_string(),
        "# TYPE remind_me_build_info gauge".to_string(),
        sample(
            "remind_me_build_info",
            1.0,
            &label("version", env!("CARGO_PKG_VERSION")),
        ),
    ];

    lines.push("# HELP remind_me_tool_calls_total Total MCP tool calls, by tool name.".to_string());
    lines.push("# TYPE remind_me_tool_calls_total counter".to_string());
    for (tool, count) in &counters.tool_calls {
        lines.push(sample(
            "remind_me_tool_calls_total",
            *count as f64,
            &label("tool", tool),
        ));
    }

    lines.push(
        "# HELP remind_me_tool_call_duration_seconds_sum Sum of MCP tool call durations in seconds, by tool name."
            .to_string(),
    );
    lines.push("# TYPE remind_me_tool_call_duration_seconds_sum counter".to_string());
    for (tool, seconds) in &counters.tool_seconds {
        lines.push(sample(
            "remind_me_tool_call_duration_seconds_sum",
            *seconds,
            &label("tool", tool),
        ));
    }

    lines.push(
        "# HELP remind_me_tool_call_duration_seconds_count Count of MCP tool calls timed, by tool name (divide the _sum by this for the average)."
            .to_string(),
    );
    lines.push("# TYPE remind_me_tool_call_duration_seconds_count counter".to_string());
    for (tool, count) in &counters.tool_calls {
        lines.push(sample(
            "remind_me_tool_call_duration_seconds_count",
            *count as f64,
            &label("tool", tool),
        ));
    }

    lines.push(
        "# HELP remind_me_search_tier_results_total Cumulative remind_me_search result count, by ranking tier (keyword/semantic/hybrid)."
            .to_string(),
    );
    lines.push("# TYPE remind_me_search_tier_results_total counter".to_string());
    for tier in SEARCH_TIERS {
        lines.push(sample(
            "remind_me_search_tier_results_total",
            counters.search_tiers.get(tier).copied().unwrap_or(0) as f64,
            &label("tier", tier),
        ));
    }

    lines.push(
        "# HELP remind_me_rate_limit_rejections_total Total requests rejected by the rate limiter."
            .to_string(),
    );
    lines.push("# TYPE remind_me_rate_limit_rejections_total counter".to_string());
    lines.push(sample(
        "remind_me_rate_limit_rejections_total",
        counters.rate_limit_rejections as f64,
        &[],
    ));

    for gauge in gauges {
        lines.push(format!("# HELP {} {}", gauge.name, gauge.help));
        lines.push(format!("# TYPE {} gauge", gauge.name));
        lines.push(sample(&gauge.name, gauge.value, &gauge.labels));
    }

    lines.join("\n") + "\n"
}

// ---------------------------------------------------------------------------
// The PWA manifest
// ---------------------------------------------------------------------------

/// The dashboard's Web App Manifest, so it can be installed to a home screen.
///
/// Deliberately minimal, matching the reference: no service worker, no offline
/// support, and **no `icons`** — there is no icon asset in the repository to
/// point at, and a manifest without one is still valid (the OS falls back to a
/// generic glyph). Pointing at an icon that does not exist would be worse than
/// omitting the key.
pub fn manifest_json() -> serde_json::Value {
    serde_json::json!({
        "name": "Remind Me — Memory Dashboard",
        "short_name": "Remind Me",
        "start_url": "/",
        "display": "standalone",
        "background_color": "#0a0a0f",
        "theme_color": "#0a0a0f",
    })
}

// ---------------------------------------------------------------------------
// `escape_label_value`/`sample`/`label` are private, so their coverage lives
// here rather than in `tests/metrics_test.rs` (#284) -- an external
// integration test has no way to call them directly. The public surface
// (`metrics_enabled`, the recorders, `render_prometheus_text`,
// `manifest_json`) is covered there instead.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_label_value_leaves_plain_text_untouched() {
        assert_eq!(escape_label_value("remind_me_search"), "remind_me_search");
    }

    #[test]
    fn escape_label_value_escapes_backslashes_quotes_and_newlines() {
        // Order matters: backslashes must be escaped first, or a value like
        // `\"` would have its escaping backslash re-escaped and the quote
        // would still close the label early.
        assert_eq!(escape_label_value("back\\slash"), "back\\\\slash");
        assert_eq!(escape_label_value("has \"quotes\""), "has \\\"quotes\\\"");
        assert_eq!(escape_label_value("line\nbreak"), "line\\nbreak");
        assert_eq!(
            escape_label_value("\\\"\n"),
            "\\\\\\\"\\n",
            "a value combining all three must escape every occurrence"
        );
    }

    #[test]
    fn format_labels_is_empty_for_no_labels() {
        assert_eq!(format_labels(&[]), "");
    }

    #[test]
    fn format_labels_renders_prometheus_curly_brace_syntax() {
        assert_eq!(format_labels(&label("tool", "search")), "{tool=\"search\"}");
    }

    #[test]
    fn format_labels_joins_multiple_labels_with_commas() {
        let labels = vec![
            ("tool".to_string(), "search".to_string()),
            ("tier".to_string(), "hybrid".to_string()),
        ];
        assert_eq!(format_labels(&labels), "{tool=\"search\",tier=\"hybrid\"}");
    }

    #[test]
    fn sample_formats_whole_floats_without_a_trailing_decimal() {
        assert_eq!(sample("m", 3.0, &[]), "m 3");
        assert_eq!(sample("m", 0.0, &[]), "m 0");
        assert_eq!(sample("m", -2.0, &[]), "m -2");
    }

    #[test]
    fn sample_keeps_the_fraction_for_non_whole_values() {
        assert_eq!(sample("m", 1.5, &[]), "m 1.5");
        assert_eq!(sample("m", 0.25, &[]), "m 0.25");
    }

    #[test]
    fn sample_includes_labels_before_the_value() {
        assert_eq!(
            sample("remind_me_tool_calls_total", 2.0, &label("tool", "search")),
            "remind_me_tool_calls_total{tool=\"search\"} 2"
        );
    }
}
