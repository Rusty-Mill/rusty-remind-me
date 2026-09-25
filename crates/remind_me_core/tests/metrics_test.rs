//! Coverage for `remind_me_core::metrics`'s public surface (#284).
//!
//! `crates/remind_me_api/tests/metrics_test.rs` already covers `GET
//! /metrics` and `GET /manifest.json` end to end over HTTP. This file
//! exercises the same recording/rendering/env-parsing logic directly against
//! `remind_me_core::metrics`, so `cargo test -p remind_me_core` gives signal
//! on it without needing the API crate at all.
//!
//! `escape_label_value`/`sample` themselves are private, so their coverage
//! lives as an inline `#[cfg(test)]` module in `src/metrics.rs`; this file
//! sticks to the `pub` surface.

#[path = "../src/test_env.rs"]
mod test_env;

use remind_me_core::metrics::{self, GaugeSpec, METRICS_ENABLED_ENV, SEARCH_TIERS};
use std::sync::{Mutex, OnceLock};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Runs `body` with `REMIND_ME_METRICS_ENABLED` set (or unset) and freshly
/// reset counter state, serialized against every other test in this file --
/// both the env var and the counters are process-global.
fn with_metrics<T>(raw: Option<&str>, body: impl FnOnce() -> T) -> T {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    match raw {
        Some(v) => crate::test_env::set_var(METRICS_ENABLED_ENV, v),
        None => crate::test_env::remove_var(METRICS_ENABLED_ENV),
    }
    metrics::reset();
    let out = body();
    crate::test_env::remove_var(METRICS_ENABLED_ENV);
    out
}

// ---------------------------------------------------------------------------
// metrics_enabled() env parsing
// ---------------------------------------------------------------------------

#[test]
fn unset_is_disabled() {
    with_metrics(None, || assert!(!metrics::metrics_enabled()));
}

#[test]
fn empty_string_is_disabled_matching_the_reference_opt_out() {
    with_metrics(Some(""), || assert!(!metrics::metrics_enabled()));
}

#[test]
fn zero_false_and_no_disable_it_case_insensitively_and_trimmed() {
    for v in ["0", "false", "FALSE", "  false  ", "no", "No", "  0  "] {
        with_metrics(Some(v), || {
            assert!(!metrics::metrics_enabled(), "{v:?} should disable metrics");
        });
    }
}

#[test]
fn anything_else_enables_it() {
    for v in ["1", "true", "yes", "on", "enabled", "  1  "] {
        with_metrics(Some(v), || {
            assert!(metrics::metrics_enabled(), "{v:?} should enable metrics");
        });
    }
}

// ---------------------------------------------------------------------------
// Recorders
// ---------------------------------------------------------------------------

#[test]
fn recorders_are_no_ops_while_disabled() {
    with_metrics(None, || {
        metrics::record_tool_call("search", 1.0);
        metrics::record_search_tier("keyword", 5);
        metrics::record_rate_limit_rejection();

        let text = metrics::render_prometheus_text(&[]);
        assert!(!text.contains("tool=\"search\""));
        assert!(text.contains("remind_me_search_tier_results_total{tier=\"keyword\"} 0"));
        assert!(text.contains("remind_me_rate_limit_rejections_total 0"));
    });
}

#[test]
fn recorders_accumulate_across_calls_while_enabled() {
    with_metrics(Some("1"), || {
        metrics::record_tool_call("search", 0.25);
        metrics::record_tool_call("search", 0.75);
        metrics::record_search_tier("semantic", 3);
        metrics::record_rate_limit_rejection();
        metrics::record_rate_limit_rejection();

        let text = metrics::render_prometheus_text(&[]);
        assert!(text.contains("remind_me_tool_calls_total{tool=\"search\"} 2"));
        assert!(text.contains("remind_me_tool_call_duration_seconds_sum{tool=\"search\"} 1"));
        assert!(text.contains("remind_me_tool_call_duration_seconds_count{tool=\"search\"} 2"));
        assert!(text.contains("remind_me_search_tier_results_total{tier=\"semantic\"} 3"));
        assert!(text.contains("remind_me_rate_limit_rejections_total 2"));
    });
}

#[test]
fn reset_drops_all_counter_state() {
    with_metrics(Some("1"), || {
        metrics::record_tool_call("search", 1.0);
        metrics::record_rate_limit_rejection();

        metrics::reset();

        let text = metrics::render_prometheus_text(&[]);
        assert!(!text.contains("tool=\"search\""));
        assert!(text.contains("remind_me_rate_limit_rejections_total 0"));
    });
}

// ---------------------------------------------------------------------------
// Exposition shape
// ---------------------------------------------------------------------------

#[test]
fn build_info_carries_the_crate_version() {
    with_metrics(Some("1"), || {
        let text = metrics::render_prometheus_text(&[]);
        assert!(text.contains(&format!(
            "remind_me_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        )));
    });
}

#[test]
fn every_search_tier_is_zero_filled_even_with_no_searches() {
    with_metrics(Some("1"), || {
        let text = metrics::render_prometheus_text(&[]);
        for tier in SEARCH_TIERS {
            assert!(
                text.contains(&format!(
                    "remind_me_search_tier_results_total{{tier=\"{tier}\"}} 0"
                )),
                "tier {tier} not zero-filled:\n{text}"
            );
        }
    });
}

#[test]
fn a_quoted_label_value_survives_the_round_trip_escaped() {
    with_metrics(Some("1"), || {
        metrics::record_tool_call("odd\"name", 0.5);
        let text = metrics::render_prometheus_text(&[]);
        assert!(
            text.contains("tool=\"odd\\\"name\""),
            "not escaped:\n{text}"
        );
    });
}

#[test]
fn passed_gauges_are_appended_with_their_help_and_type() {
    with_metrics(Some("1"), || {
        let gauge = GaugeSpec::new("remind_me_memories_total", "Total live memories.", 7.0);
        let text = metrics::render_prometheus_text(&[gauge]);
        assert!(text.contains("# HELP remind_me_memories_total Total live memories."));
        assert!(text.contains("# TYPE remind_me_memories_total gauge"));
        assert!(text.contains("remind_me_memories_total 7"));
    });
}

// ---------------------------------------------------------------------------
// The PWA manifest
// ---------------------------------------------------------------------------

#[test]
fn manifest_json_has_the_expected_shape_and_no_icons_key() {
    let manifest = metrics::manifest_json();
    assert_eq!(manifest["name"], "Remind Me — Memory Dashboard");
    assert_eq!(manifest["short_name"], "Remind Me");
    assert_eq!(manifest["start_url"], "/");
    assert_eq!(manifest["display"], "standalone");
    // No icon asset ships in the repository -- pointing at one that does not
    // exist would be worse than omitting the key, and a manifest without
    // `icons` is still valid.
    assert!(manifest.get("icons").is_none());
}
