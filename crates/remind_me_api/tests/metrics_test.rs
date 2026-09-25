//! Coverage for `GET /metrics` and `GET /manifest.json` (gap A4, issue #119).
//!
//! The exposition is asserted by *parsing* it rather than by substring match:
//! a scrape that Prometheus rejects and one it accepts differ in structure —
//! a `# TYPE` without its family, a sample whose name does not match its
//! header — and substring assertions pass on both.

#[path = "../../remind_me_core/src/test_env.rs"]
mod test_env;

mod common;
use common::{get, server};
use remind_me_core::metrics::METRICS_ENABLED_ENV;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A parsed exposition: family name to its declared type, and every sample.
struct Exposition {
    types: HashMap<String, String>,
    helps: HashMap<String, String>,
    samples: Vec<(String, String)>,
}

/// Parse the text exposition format, rejecting anything malformed.
///
/// Deliberately strict. The point of the test is that a real scraper would
/// accept this, and a lenient parser here would defeat that.
fn parse(text: &str) -> Exposition {
    let mut types = HashMap::new();
    let mut helps = HashMap::new();
    let mut samples = Vec::new();

    for line in text.lines() {
        assert!(!line.is_empty(), "a blank line is not valid exposition");
        if let Some(rest) = line.strip_prefix("# HELP ") {
            let (name, help) = rest.split_once(' ').expect("# HELP needs a name and text");
            assert!(!help.trim().is_empty(), "{name} has an empty HELP");
            helps.insert(name.to_string(), help.to_string());
        } else if let Some(rest) = line.strip_prefix("# TYPE ") {
            let (name, kind) = rest
                .split_once(' ')
                .expect("# TYPE needs a name and a type");
            assert!(
                ["counter", "gauge", "histogram", "summary", "untyped"].contains(&kind),
                "{name} declares unknown type {kind:?}"
            );
            types.insert(name.to_string(), kind.to_string());
        } else {
            assert!(!line.starts_with('#'), "unknown comment line: {line:?}");
            let (left, value) = line.rsplit_once(' ').expect("a sample is `name value`");
            value
                .parse::<f64>()
                .unwrap_or_else(|_| panic!("sample value {value:?} is not a number: {line:?}"));
            let name = left.split('{').next().unwrap().to_string();
            samples.push((name, left.to_string()));
        }
    }

    Exposition {
        types,
        helps,
        samples,
    }
}

fn with_metrics<T>(enabled: bool, body: impl FnOnce() -> T) -> T {
    let _guard = env_lock().lock().unwrap();
    if enabled {
        crate::test_env::set_var(METRICS_ENABLED_ENV, "1");
    } else {
        crate::test_env::remove_var(METRICS_ENABLED_ENV);
    }
    remind_me_core::metrics::reset();
    let out = body();
    crate::test_env::remove_var(METRICS_ENABLED_ENV);
    out
}

#[test]
fn metrics_is_a_plain_404_while_disabled() {
    with_metrics(false, || {
        let (srv, root) = server("metrics-off");

        let response = get(&srv, "/metrics");

        // Not a 403 and not an empty 200: "off" has to be indistinguishable
        // from "this build does not have it", so a scrape pointed at a
        // metrics-disabled server fails loudly rather than silently recording
        // nothing forever.
        assert_eq!(response.status, 404);
        assert!(!response.body.contains("remind_me_build_info"));
        std::fs::remove_dir_all(&root).unwrap();
    });
}

#[test]
fn the_exposition_parses_and_every_family_is_well_formed() {
    with_metrics(true, || {
        let (srv, root) = server("metrics-parse");

        let response = get(&srv, "/metrics");

        assert_eq!(response.status, 200);
        assert!(
            response.content_type.starts_with("text/plain"),
            "Prometheus dispatches on this, got {:?}",
            response.content_type
        );

        let parsed = parse(&response.body);

        // Every sample must belong to a declared family, and every declared
        // family must carry both a HELP and a TYPE. A stray sample is the
        // shape a copy-pasted metric name takes.
        for (name, line) in &parsed.samples {
            assert!(
                parsed.types.contains_key(name),
                "sample {line:?} has no # TYPE"
            );
            assert!(
                parsed.helps.contains_key(name),
                "sample {line:?} has no # HELP"
            );
        }
        std::fs::remove_dir_all(&root).unwrap();
    });
}

#[test]
fn build_info_is_emitted_even_on_a_server_that_has_done_nothing() {
    with_metrics(true, || {
        let (srv, root) = server("metrics-build");

        let parsed = parse(&get(&srv, "/metrics").body);

        // An absent series reads as "scrape target down", not "idle" — which
        // is exactly backwards for a freshly started server.
        let build = parsed
            .samples
            .iter()
            .find(|(name, _)| name == "remind_me_build_info")
            .expect("build_info is unconditional");
        assert!(build
            .1
            .contains(&format!("version=\"{}\"", env!("CARGO_PKG_VERSION"))));
        assert_eq!(parsed.types["remind_me_build_info"], "gauge");
        std::fs::remove_dir_all(&root).unwrap();
    });
}

#[test]
fn the_reference_metric_families_are_all_present() {
    with_metrics(true, || {
        let (srv, root) = server("metrics-families");

        let parsed = parse(&get(&srv, "/metrics").body);

        for family in [
            "remind_me_build_info",
            "remind_me_tool_calls_total",
            "remind_me_tool_call_duration_seconds_sum",
            "remind_me_tool_call_duration_seconds_count",
            "remind_me_search_tier_results_total",
            "remind_me_rate_limit_rejections_total",
            "remind_me_memories_total",
        ] {
            assert!(
                parsed.types.contains_key(family),
                "{family} missing — a dashboard built against the reference would break"
            );
        }
        std::fs::remove_dir_all(&root).unwrap();
    });
}

#[test]
fn the_search_tiers_are_zero_filled_rather_than_omitted() {
    with_metrics(true, || {
        let (srv, root) = server("metrics-tiers");

        let body = get(&srv, "/metrics").body;

        // A server that has served no searches must still emit all three
        // tiers at zero. Omitted, a dashboard query returns no data and
        // renders as a gap rather than a flat line at zero.
        for tier in ["keyword", "semantic", "hybrid"] {
            assert!(
                body.contains(&format!(
                    "remind_me_search_tier_results_total{{tier=\"{tier}\"}} 0"
                )),
                "tier {tier} not zero-filled:\n{body}"
            );
        }
        std::fs::remove_dir_all(&root).unwrap();
    });
}

#[test]
fn the_memory_gauge_counts_live_memories_only() {
    with_metrics(true, || {
        let (srv, root) = common::seeded_server("metrics-gauge", |conn| {
            for content in ["one", "two", "three"] {
                remind_me_core::db::queries::add_memory(
                    conn,
                    remind_me_core::MemoryAddInput {
                        content: content.to_string(),
                        category: "general".into(),
                        tags: vec![],
                        source: "manual".into(),
                        metadata: serde_json::json!({}),
                        subject: None,
                        predicate: None,
                        object: None,
                        entities: vec![],
                        sensitive: false,
                    },
                )
                .unwrap();
            }
            conn.execute(
                "UPDATE memories SET deleted_at = '2020-01-01T00:00:00+00:00' WHERE content = 'three'",
                [],
            )
            .unwrap();
        });

        let body = get(&srv, "/metrics").body;

        // Computed fresh per scrape rather than tracked as a counter, so it
        // cannot drift from the table — and tombstones are not memories.
        assert!(
            body.contains("remind_me_memories_total 2"),
            "expected 2 live memories:\n{body}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    });
}

#[test]
fn recorded_tool_calls_show_up_as_samples() {
    with_metrics(true, || {
        let (srv, root) = server("metrics-tools");
        remind_me_core::metrics::record_tool_call("remind_me_search", 0.25);
        remind_me_core::metrics::record_tool_call("remind_me_search", 0.75);

        let body = get(&srv, "/metrics").body;

        assert!(body.contains("remind_me_tool_calls_total{tool=\"remind_me_search\"} 2"));
        assert!(
            body.contains("remind_me_tool_call_duration_seconds_sum{tool=\"remind_me_search\"} 1")
        );
        assert!(body
            .contains("remind_me_tool_call_duration_seconds_count{tool=\"remind_me_search\"} 2"));
        std::fs::remove_dir_all(&root).unwrap();
    });
}

#[test]
fn recorders_do_nothing_while_metrics_are_disabled() {
    with_metrics(false, || {
        remind_me_core::metrics::record_tool_call("remind_me_search", 1.0);
        remind_me_core::metrics::record_rate_limit_rejection();
    });

    with_metrics(true, || {
        let (srv, root) = server("metrics-noop");

        let body = get(&srv, "/metrics").body;

        // No counter state accumulates while off, so enabling metrics does
        // not suddenly publish a backlog gathered before the operator opted
        // in.
        assert!(!body.contains("tool=\"remind_me_search\""));
        assert!(body.contains("remind_me_rate_limit_rejections_total 0"));
        std::fs::remove_dir_all(&root).unwrap();
    });
}

#[test]
fn a_label_value_with_a_quote_is_escaped() {
    with_metrics(true, || {
        remind_me_core::metrics::record_tool_call("odd\"name", 0.5);

        let text = remind_me_core::metrics::render_prometheus_text(&[]);

        // An unescaped quote closes the label early and makes the rest of the
        // line garbage, which takes the whole scrape down rather than one
        // sample.
        assert!(
            text.contains("tool=\"odd\\\"name\""),
            "not escaped:\n{text}"
        );
        parse(&text);
    });
}

// ---------------------------------------------------------------------------
// The manifest
// ---------------------------------------------------------------------------

#[test]
fn the_manifest_is_served_unauthenticated_with_the_right_content_type() {
    let (srv, root) = common::authed_server("manifest");

    // No Authorization header: a browser fetches `<link rel="manifest">`
    // without one, so requiring a key would mean the manifest never loads.
    // It carries no user data.
    let response = get(&srv, "/manifest.json");

    assert_eq!(response.status, 200);
    assert!(
        response
            .content_type
            .starts_with("application/manifest+json"),
        "browsers check this, got {:?}",
        response.content_type
    );

    let body: serde_json::Value = serde_json::from_str(&response.body).expect("valid JSON");
    assert_eq!(body["name"], "Remind Me — Memory Dashboard");
    assert_eq!(body["short_name"], "Remind Me");
    assert_eq!(body["start_url"], "/");
    assert_eq!(body["display"], "standalone");

    // No `icons` key, deliberately: there is no icon asset in the repository,
    // and pointing at one that does not exist is worse than omitting it — a
    // manifest without icons is still valid and the OS falls back to a glyph.
    assert!(body.get("icons").is_none());
    std::fs::remove_dir_all(&root).unwrap();
}
