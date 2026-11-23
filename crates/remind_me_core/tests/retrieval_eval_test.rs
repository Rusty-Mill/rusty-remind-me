//! Retrieval quality as a number CI checks (#59).
//!
//! A checked-in fixture store (`tests/fixtures/retrieval_eval/memories.jsonl`:
//! synthetic, no personal data) and a golden set of queries
//! (`queries.jsonl`), each with the memories it should find and, optionally,
//! the ones it must never surface. Every query runs through the real ranker
//! for each embedder mode and [`RetrievalStrategy`], scored as recall@5,
//! recall@10 and MRR, and compared with `baseline.json`.
//!
//! - A score more than [`TOLERANCE`] below its baseline fails the test.
//! - A `must_not` id (superseded, sensitive, other-project) anywhere in the top
//!   ten fails it outright: that is a correctness check, not a quality score.
//! - `REMIND_ME_EVAL_BLESS=1` rewrites `baseline.json` from this run, so a PR
//!   that moves a score shows the move in its diff.
//! - `REMIND_ME_EVAL_LIVE=1` also scores the configured embedder (and any
//!   configured rerank or HyDE), with and without date inference, and prints
//!   the date-phrase lift for it. It is printed, never gated: CI has no model.
//! - The gated runs switch HyDE and rerank off, whatever the environment says,
//!   so no model is called and the baseline compares like with like.
//! - A query with an `entity` filter that finds none of its relevant ids
//!   fails outright, so a broken filter path cannot hide inside the averages.
//!
//! CI has no embedding server, so the gated modes are keyword-only and
//! [`HashedEmbedder`], a deterministic stand-in that exercises the vector
//! stage and fusion but knows nothing about meaning.
//!
//! Two more modes, `keyword+dates` and `hashed+dates`, run every query through
//! the date-phrase inference step the search tool uses when
//! `REMIND_ME_DATE_PHRASES=on` (card #63), and print each kind's lift over the
//! plain mode. Their `must_not` and entity checks are gated like every other
//! run, but their scores are printed, not compared with the baseline: which
//! age-based rows fall inside "this week" changes with the weekday, so those
//! scores move by a query or two through the week however the code behaves.
//!
//! Ages in the fixture are relative to the run, so recency sees the same ages
//! every day and the fixture never expires with the calendar. A row that must
//! sit inside a calendar window names it instead (`during: "last week"`) and is
//! placed mid-way through that window at load time, so a calendar phrase finds
//! the same rows whatever weekday the test runs on. A row is last
//! read at load time unless it sets `accessed_days`; the few that do are
//! dormant, and the `dormant` queries check that no stage surfaces them. The env-reading
//! parts keep this file its own test binary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use remind_me_core::date_phrase;
use remind_me_core::embedder::{available_embedder, Embedder};
use remind_me_core::query_expansion::EXPANSION_MODE_ENV;
use remind_me_core::reranker::BACKEND_ENV as RERANK_ENV;
use remind_me_core::Database;

#[path = "../src/test_env.rs"]
mod test_env;

#[path = "retrieval_eval/shared.rs"]
mod shared;
use shared::*;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/retrieval_eval")
}

/// One mode on an in-memory engine store, its lines printed.
fn evaluate(mode: &str, embedder: Option<&dyn Embedder>, report: &mut Report) {
    let mut open = || Database::open_in_memory().unwrap();
    evaluate_in(mode, embedder, &mut open, &mut std::io::stdout(), report);
}

/// Held by every test here that reads or writes the model-stage variables.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Sets env vars until dropped (`None` removes one), then restores what the
/// environment held before, panics included.
struct EnvOverride(Vec<(&'static str, Option<String>)>);

impl EnvOverride {
    fn new(vars: &[(&'static str, Option<&str>)]) -> Self {
        let saved = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var(k).ok()))
            .collect();
        for (key, value) in vars {
            match value {
                Some(v) => test_env::set_var(key, v),
                None => test_env::remove_var(key),
            }
        }
        Self(saved)
    }
}

impl Drop for EnvOverride {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(v) => test_env::set_var(key, v),
                None => test_env::remove_var(key),
            }
        }
    }
}

/// Turns the model-backed stages (HyDE, rerank) off until dropped, then
/// restores what the environment held, so the live run still sees them.
/// HyDE runs only for "hyde"; rerank is off for an empty value.
fn model_stages_off() -> EnvOverride {
    EnvOverride::new(&[(EXPANSION_MODE_ENV, Some("off")), (RERANK_ENV, Some(""))])
}

fn print_date_lift(report: &Report) {
    println!("date-phrase lift (auto, +dates minus plain):");
    for (key, r5, mrr) in date_lift(report) {
        println!("  {key:40} r@5 {r5:+.3}  mrr {mrr:+.3}");
    }
}

/// The opt-in live run: `embedder` with and without date inference, so the
/// lift is measured on a real model too. Printed by the caller, never gated.
fn evaluate_live(embedder: &dyn Embedder, report: &mut Report) {
    evaluate("live", Some(embedder), report);
    evaluate("live+dates", Some(embedder), report);
}

fn env_on(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1")
}

#[test]
fn retrieval_quality_holds_against_the_baseline() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut report = Report::default();
    {
        let _offline = model_stages_off();
        evaluate("keyword", None, &mut report);
        evaluate("hashed", Some(&HashedEmbedder), &mut report);
        evaluate("keyword+dates", None, &mut report);
        evaluate("hashed+dates", Some(&HashedEmbedder), &mut report);
    }
    print_date_lift(&report);

    if env_on("REMIND_ME_EVAL_LIVE") {
        let live = available_embedder();
        let mut live_report = Report::default();
        match live.as_deref() {
            Some(e) => {
                evaluate_live(e, &mut live_report);
                print_date_lift(&live_report);
            }
            None => println!("live: no embedder configured or reachable; skipped"),
        }
        for violation in &live_report.violations {
            println!("live (not gated): {violation}");
        }
    }

    assert!(
        report.violations.is_empty(),
        "hard failures:\n{}",
        report.violations.join("\n")
    );

    let path = fixture_dir().join("baseline.json");
    if env_on("REMIND_ME_EVAL_BLESS") {
        let text = serde_json::to_string_pretty(&report.scores).unwrap() + "\n";
        std::fs::write(&path, text).unwrap();
        println!("baseline written to {}", path.display());
        return;
    }

    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}; run with REMIND_ME_EVAL_BLESS=1 to create it",
            path.display()
        )
    });
    let baseline: BTreeMap<String, Scores> = serde_json::from_str(&text).unwrap();
    assert_eq!(
        baseline.keys().collect::<Vec<_>>(),
        report.scores.keys().collect::<Vec<_>>(),
        "baseline.json covers different runs than this test; re-bless it"
    );
    let failed: Vec<String> = baseline
        .iter()
        .flat_map(|(key, base)| {
            regressions(base, &report.scores[key], TOLERANCE)
                .into_iter()
                .map(move |r| format!("{key}: {r}"))
        })
        .collect();
    assert!(
        failed.is_empty(),
        "retrieval quality fell more than {TOLERANCE} below baseline.json:\n{}",
        failed.join("\n")
    );
}

// ---------------------------------------------------------------------------
// The harness's own checks
// ---------------------------------------------------------------------------

fn ids(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

#[test]
fn recall_counts_relevant_ids_within_k() {
    let ranking = ids(&["a", "b", "c", "d", "e", "f"]);
    assert_eq!(recall_at(&ranking, &ids(&["a", "b"]), 5), 1.0);
    assert_eq!(recall_at(&ranking, &ids(&["a", "f"]), 5), 0.5);
    assert_eq!(recall_at(&ranking, &ids(&["a", "f"]), 10), 1.0);
    assert_eq!(recall_at(&[], &ids(&["a"]), 5), 0.0);
}

#[test]
fn reciprocal_rank_is_the_first_relevant_position() {
    let ranking = ids(&["x", "y", "a", "b"]);
    assert_eq!(reciprocal_rank(&ranking, &ids(&["a", "b"])), 1.0 / 3.0);
    assert_eq!(reciprocal_rank(&ranking, &ids(&["x"])), 1.0);
    assert_eq!(reciprocal_rank(&ranking, &ids(&["z"])), 0.0);
    let deep: Vec<String> = (0..DEPTH + 1).map(|i| format!("n{i}")).collect();
    assert_eq!(reciprocal_rank(&deep, &ids(&[&format!("n{DEPTH}")])), 0.0);
}

#[test]
fn a_drop_beyond_tolerance_is_reported_and_a_small_one_is_not() {
    let base = Scores {
        recall_at_5: 0.8,
        recall_at_10: 0.9,
        mrr: 0.7,
    };
    let small = Scores {
        recall_at_5: 0.79,
        ..base
    };
    assert!(regressions(&base, &small, TOLERANCE).is_empty());
    let large = Scores { mrr: 0.6, ..base };
    assert_eq!(
        regressions(&base, &large, TOLERANCE),
        ["mrr 0.7000 -> 0.6000"]
    );
    // A reversed ranking scores worse than the right one, so the gate can fire.
    let relevant = ids(&["a"]);
    let good = ids(&["a", "b", "c", "d", "e", "f"]);
    let reversed: Vec<String> = good.iter().rev().cloned().collect();
    let score = |r: &[String]| {
        Scores::mean(&[(
            recall_at(r, &relevant, 5),
            recall_at(r, &relevant, DEPTH),
            reciprocal_rank(r, &relevant),
        )])
    };
    assert!(!regressions(&score(&good), &score(&reversed), TOLERANCE).is_empty());
}

#[test]
fn validate_refuses_a_golden_set_that_cannot_hold() {
    let memories: Vec<FixtureMemory> = vec![serde_json::from_value(serde_json::json!({
        "id": "a", "content": "x", "category": "general", "memory_type": "fact",
        "project": null, "age_days": 1
    }))
    .unwrap()];
    let golden = |relevant: &[&str], must_not: &[&str]| GoldenQuery {
        query: "q".into(),
        relevant: ids(relevant),
        kind: "exact".into(),
        must_not: ids(must_not),
        project: None,
        entity: None,
        topic: None,
    };
    assert!(validate(&memories, &[golden(&["a"], &[])]).is_ok());
    assert!(validate(&memories, &[golden(&[], &[])]).is_err());
    assert!(validate(&memories, &[golden(&["missing"], &[])]).is_err());
    assert!(validate(&memories, &[golden(&["a"], &["missing"])]).is_err());
    assert!(validate(&memories, &[golden(&["a"], &["a"])]).is_err());
    let both: Vec<FixtureMemory> = vec![serde_json::from_value(serde_json::json!({
        "id": "a", "content": "x", "category": "general", "memory_type": "fact",
        "project": null, "age_days": 1, "during": "last week"
    }))
    .unwrap()];
    assert!(validate(&both, &[golden(&["a"], &[])]).is_err());
}

#[test]
fn a_during_row_lands_inside_its_window_on_every_weekday() {
    let memories: Vec<FixtureMemory> = read_jsonl("memories.jsonl");
    let start = DateTime::parse_from_rfc3339("2026-10-26T00:00:30Z")
        .unwrap()
        .with_timezone(&Utc);
    for hours in (0..24 * 9).step_by(5) {
        let now = start + Duration::hours(hours);
        for m in memories.iter().filter(|m| m.during.is_some()) {
            let phrase = m.during.as_deref().unwrap();
            let span = date_phrase::parse(phrase, now.with_timezone(&Tz::UTC)).unwrap();
            let at = created_at(m, now);
            assert!(at >= span.start && at <= now, "{} at {now}", m.id);
            assert!(span.end.is_none_or(|e| at < e), "{} at {now}", m.id);
        }
    }
}

/// The live arms cannot run in CI, so wire them to the hashed embedder and
/// check both land and pair up into lift lines.
#[test]
fn the_live_run_measures_both_date_arms() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _offline = model_stages_off();
    let mut report = Report::default();
    evaluate_live(&HashedEmbedder, &mut report);
    assert!(report.scores.contains_key("live/auto"));
    assert!(report.measured.contains_key("live+dates/auto"));
    let lift = date_lift(&report);
    assert!(
        lift.iter().any(|(key, _, _)| key == "live/auto/dated"),
        "{lift:?}"
    );
}

#[test]
fn the_checked_in_golden_set_is_valid() {
    let memories: Vec<FixtureMemory> = read_jsonl("memories.jsonl");
    let golden: Vec<GoldenQuery> = read_jsonl("queries.jsonl");
    validate(&memories, &golden).unwrap();
}

#[test]
fn hashed_embedder_is_deterministic_and_normalised() {
    let a = HashedEmbedder::vector("Dock heartbeat timeout");
    assert_eq!(a, HashedEmbedder::vector("dock heartbeat timeout"));
    let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-5);
    assert!(HashedEmbedder::vector("").iter().all(|x| *x == 0.0));
}

#[test]
fn gated_runs_switch_inherited_model_stages_off_and_restore_them() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let inherited = [EXPANSION_MODE_ENV, RERANK_ENV].map(|k| std::env::var(k).ok());
    {
        // Model stages on, as a developer environment might have them; the
        // outer guard puts back whatever this process inherited.
        let _on = EnvOverride::new(&[(EXPANSION_MODE_ENV, Some("hyde")), (RERANK_ENV, None)]);
        assert!(remind_me_core::query_expansion::enabled());
        assert!(remind_me_core::reranker::enabled());
        {
            let _offline = model_stages_off();
            assert!(!remind_me_core::query_expansion::enabled());
            assert!(!remind_me_core::reranker::enabled());
        }
        assert_eq!(std::env::var(EXPANSION_MODE_ENV).as_deref(), Ok("hyde"));
        assert!(std::env::var(RERANK_ENV).is_err());
    }
    assert_eq!(
        [EXPANSION_MODE_ENV, RERANK_ENV].map(|k| std::env::var(k).ok()),
        inherited
    );
}
