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
//!   configured rerank or HyDE). It is printed, never gated: CI has no model.
//! - The gated runs switch HyDE and rerank off, whatever the environment says,
//!   so no model is called and the baseline compares like with like.
//! - A query with an `entity` filter that finds none of its relevant ids
//!   fails outright, so a broken filter path cannot hide inside the averages.
//!
//! CI has no embedding server, so the gated modes are keyword-only and
//! [`HashedEmbedder`], a deterministic stand-in that exercises the vector
//! stage and fusion but knows nothing about meaning.
//!
//! Ages in the fixture are relative to the run, so recency sees the same ages
//! every day and the fixture never expires with the calendar. Every row was
//! last read at load time, so none is dormant. The env-reading
//! parts keep this file its own test binary.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{Duration, Utc};
use remind_me_core::context::ScopeFilter;
use remind_me_core::db::memories::{Memories, NewMemory};
use remind_me_core::db::{queries, Store};
use remind_me_core::embedder::{
    available_embedder, EmbedError, EmbedRole, Embedder, EmbeddingIdentity,
};
use remind_me_core::entity::{link_memory_entity, upsert_entity};
use remind_me_core::query_expansion::EXPANSION_MODE_ENV;
use remind_me_core::reranker::BACKEND_ENV as RERANK_ENV;
use remind_me_core::retrieval::Deadline;
use remind_me_core::vectors::embed_and_store;
use remind_me_core::{Database, EntityInput, MemorySearchInput, RetrievalStrategy, SearchFilters};
use serde::{Deserialize, Serialize};

#[path = "../src/test_env.rs"]
mod test_env;

/// How far a score may fall below its baseline before the test fails. Two
/// points is about one or two queries moving on a set this size.
const TOLERANCE: f64 = 0.02;
/// Results per query: recall@10 and the `must_not` check read this deep.
const DEPTH: usize = 10;
const STRATEGIES: [(&str, RetrievalStrategy); 4] = [
    ("auto", RetrievalStrategy::Auto),
    ("balanced", RetrievalStrategy::Balanced),
    ("keyword_favored", RetrievalStrategy::KeywordFavored),
    ("semantic_favored", RetrievalStrategy::SemanticFavored),
];

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// One stored memory. `age_days` is resolved against the clock at load time.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureMemory {
    id: String,
    content: String,
    category: String,
    memory_type: String,
    #[serde(default)]
    tags: Vec<String>,
    project: Option<String>,
    age_days: i64,
    superseded_by: Option<String>,
    #[serde(default)]
    sensitive: bool,
    #[serde(default)]
    entities: Vec<String>,
}

/// One golden query: what it should find, and what it must never surface.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoldenQuery {
    query: String,
    relevant: Vec<String>,
    kind: String,
    #[serde(default)]
    must_not: Vec<String>,
    project: Option<String>,
    entity: Option<String>,
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/retrieval_eval")
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(name: &str) -> Vec<T> {
    let path = fixture_dir().join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(n, line)| {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{name}:{}: {e}", n + 1))
        })
        .collect()
}

/// Refuse a golden set that cannot mean what it says: an empty or unknown
/// `relevant`, an unknown `must_not`, or an id that is both.
fn validate(memories: &[FixtureMemory], golden: &[GoldenQuery]) -> Result<(), String> {
    let known: HashSet<&str> = memories.iter().map(|m| m.id.as_str()).collect();
    if known.len() != memories.len() {
        return Err("duplicate memory id in memories.jsonl".into());
    }
    for m in memories {
        if let Some(next) = &m.superseded_by {
            if !known.contains(next.as_str()) {
                return Err(format!(
                    "{}: superseded_by {next} is not in the fixture",
                    m.id
                ));
            }
        }
    }
    for g in golden {
        if g.relevant.is_empty() {
            return Err(format!("{:?}: no relevant ids", g.query));
        }
        for id in g.relevant.iter().chain(&g.must_not) {
            if !known.contains(id.as_str()) {
                return Err(format!("{:?}: unknown id {id}", g.query));
            }
        }
        if let Some(both) = g.must_not.iter().find(|id| g.relevant.contains(id)) {
            return Err(format!(
                "{:?}: {both} is both relevant and must_not",
                g.query
            ));
        }
    }
    Ok(())
}

/// Insert the fixture into `store`, link its entities and, given an
/// embedder, write its vectors.
fn load(store: &Store<'_>, memories: &[FixtureMemory], embedder: Option<&dyn Embedder>) {
    let now = Utc::now();
    let repo = Memories::new(store);
    for m in memories {
        let at = (now - Duration::days(m.age_days)).to_rfc3339();
        repo.insert(&NewMemory {
            category: m.category.clone(),
            memory_type: m.memory_type.clone(),
            tags: m.tags.clone(),
            project: m.project.clone(),
            superseded_by: m.superseded_by.clone(),
            sensitive: m.sensitive,
            // Read today, so `age_days` moves recency and nothing goes
            // dormant: dormancy is not what this set measures.
            accessed_at: Some(now.to_rfc3339()),
            ..NewMemory::new(&m.id, &m.content, &at)
        })
        .unwrap();
        for name in &m.entities {
            let entity = upsert_entity(
                store,
                &EntityInput {
                    name: name.clone(),
                    kind: None,
                    aliases: Vec::new(),
                },
            )
            .unwrap();
            link_memory_entity(store, &m.id, &entity.id).unwrap();
        }
    }
    let Some(embedder) = embedder else { return };
    for m in memories {
        embed_and_store(store, embedder, &m.id, &m.content).unwrap();
    }
}

fn ranked(
    store: &Store<'_>,
    g: &GoldenQuery,
    strategy: RetrievalStrategy,
    embedder: Option<&dyn Embedder>,
) -> Vec<String> {
    let input = MemorySearchInput {
        query: g.query.clone(),
        limit: DEPTH,
        token_budget: 0,
        strategy,
        scope: ScopeFilter {
            project: g.project.clone(),
            ..Default::default()
        },
        filters: SearchFilters {
            entity: g.entity.clone(),
            ..Default::default()
        },
        ..Default::default()
    };
    queries::search_memories_deadlined(store, &input, embedder, Deadline::unbounded())
        .unwrap()
        .results
        .into_iter()
        .map(|r| r.memory.id)
        .collect()
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Share of `relevant` found in the first `k` of `ranking`.
fn recall_at(ranking: &[String], relevant: &[String], k: usize) -> f64 {
    let top: HashSet<&String> = ranking.iter().take(k).collect();
    let found = relevant.iter().filter(|id| top.contains(id)).count();
    found as f64 / relevant.len() as f64
}

/// 1 / rank of the first relevant hit within [`DEPTH`], or 0.
fn reciprocal_rank(ranking: &[String], relevant: &[String]) -> f64 {
    ranking
        .iter()
        .take(DEPTH)
        .position(|id| relevant.contains(id))
        .map_or(0.0, |i| 1.0 / (i + 1) as f64)
}

/// Mean scores over a set of queries, rounded so the baseline diffs cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scores {
    recall_at_5: f64,
    recall_at_10: f64,
    mrr: f64,
}

impl Scores {
    fn mean(rows: &[(f64, f64, f64)]) -> Self {
        let n = rows.len().max(1) as f64;
        let sum = |f: fn(&(f64, f64, f64)) -> f64| round4(rows.iter().map(f).sum::<f64>() / n);
        Self {
            recall_at_5: sum(|r| r.0),
            recall_at_10: sum(|r| r.1),
            mrr: sum(|r| r.2),
        }
    }

    fn named(&self) -> [(&'static str, f64); 3] {
        [
            ("recall@5", self.recall_at_5),
            ("recall@10", self.recall_at_10),
            ("mrr", self.mrr),
        ]
    }
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

/// Every metric of `now` more than `tolerance` below `base`, by name.
fn regressions(base: &Scores, now: &Scores, tolerance: f64) -> Vec<String> {
    base.named()
        .iter()
        .zip(now.named())
        .filter(|((_, b), (_, n))| *n < b - tolerance - 1e-9)
        .map(|((name, b), (_, n))| format!("{name} {b:.4} -> {n:.4}"))
        .collect()
}

// ---------------------------------------------------------------------------
// Embedders
// ---------------------------------------------------------------------------

/// Feature hashing of lowercased words and character trigrams into 256
/// dimensions. Deterministic and offline: it tests that the vector stage and
/// fusion work, not that anything understands meaning.
struct HashedEmbedder;

const HASH_DIM: usize = 256;

impl HashedEmbedder {
    fn vector(text: &str) -> Vec<f32> {
        let lower = text.to_lowercase();
        let mut v = vec![0f32; HASH_DIM];
        for word in lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            v[fnv1a(word.as_bytes()) % HASH_DIM] += 1.0;
            let padded: Vec<char> = format!(" {word} ").chars().collect();
            for gram in padded.windows(3) {
                let gram: String = gram.iter().collect();
                v[fnv1a(gram.as_bytes()) % HASH_DIM] += 0.5;
            }
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        }
        v
    }
}

fn fnv1a(bytes: &[u8]) -> usize {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x0100_0000_01b3)
    }) as usize
}

impl Embedder for HashedEmbedder {
    fn embed(&self, texts: &[String], _role: EmbedRole) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|t| Self::vector(t)).collect())
    }
    fn dim(&self) -> usize {
        HASH_DIM
    }
    fn identity(&self) -> EmbeddingIdentity {
        EmbeddingIdentity {
            backend: "eval-hashed".into(),
            model: "fnv1a-words-trigrams".into(),
            dim: HASH_DIM,
        }
    }
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// Scores per `mode/strategy`, plus every hard failure seen: a `must_not`
/// hit, or an entity-filtered query that found nothing it should.
#[derive(Default)]
struct Report {
    scores: BTreeMap<String, Scores>,
    violations: Vec<String>,
}

/// Held by every test here that reads or writes the model-stage variables.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Turns the model-backed stages (HyDE, rerank) off until dropped, then
/// restores what the environment held, so the live run still sees them.
struct ModelStagesOff(Vec<(&'static str, Option<String>)>);

impl ModelStagesOff {
    fn new() -> Self {
        // HyDE runs only for "hyde"; rerank is off for an empty value.
        let off = [(EXPANSION_MODE_ENV, "off"), (RERANK_ENV, "")];
        let saved = off
            .iter()
            .map(|(k, _)| (*k, std::env::var(k).ok()))
            .collect();
        for (key, value) in off {
            test_env::set_var(key, value);
        }
        Self(saved)
    }
}

impl Drop for ModelStagesOff {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(v) => test_env::set_var(key, v),
                None => test_env::remove_var(key),
            }
        }
    }
}

fn evaluate(mode: &str, embedder: Option<&dyn Embedder>, report: &mut Report) {
    let memories: Vec<FixtureMemory> = read_jsonl("memories.jsonl");
    let golden: Vec<GoldenQuery> = read_jsonl("queries.jsonl");
    validate(&memories, &golden).unwrap();

    // Search here does not write (no access or co-retrieval bookkeeping), so
    // one store serves every strategy and query order cannot matter.
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    load(&store, &memories, embedder);

    for (name, strategy) in STRATEGIES {
        let mut all = Vec::new();
        let mut by_kind: BTreeMap<&str, Vec<(f64, f64, f64)>> = BTreeMap::new();
        for g in &golden {
            let ranking = ranked(&store, g, strategy, embedder);
            for leak in g.must_not.iter().filter(|id| ranking.contains(id)) {
                report
                    .violations
                    .push(format!("{mode}/{name}: {:?} surfaced {leak}", g.query));
            }
            let row = (
                recall_at(&ranking, &g.relevant, 5),
                recall_at(&ranking, &g.relevant, DEPTH),
                reciprocal_rank(&ranking, &g.relevant),
            );
            if g.entity.is_some() && row.1 == 0.0 {
                report.violations.push(format!(
                    "{mode}/{name}: {:?} with its entity filter found nothing relevant",
                    g.query
                ));
            }
            // Misses under the default router, so a drop names its queries.
            if strategy == RetrievalStrategy::Auto && row.1 < 1.0 {
                println!(
                    "  miss [{}] {:?}: got {:?}",
                    g.kind,
                    g.query,
                    &ranking[..ranking.len().min(5)]
                );
            }
            all.push(row);
            by_kind.entry(g.kind.as_str()).or_default().push(row);
        }
        let key = format!("{mode}/{name}");
        let total = Scores::mean(&all);
        println!("{key:32} {}", fmt(&total));
        for (kind, rows) in &by_kind {
            println!(
                "  {kind:30} {} (n={})",
                fmt(&Scores::mean(rows)),
                rows.len()
            );
        }
        report.scores.insert(key, total);
    }
}

fn fmt(s: &Scores) -> String {
    format!(
        "r@5 {:.3}  r@10 {:.3}  mrr {:.3}",
        s.recall_at_5, s.recall_at_10, s.mrr
    )
}

fn env_on(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1")
}

#[test]
fn retrieval_quality_holds_against_the_baseline() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut report = Report::default();
    {
        let _offline = ModelStagesOff::new();
        evaluate("keyword", None, &mut report);
        evaluate("hashed", Some(&HashedEmbedder), &mut report);
    }

    if env_on("REMIND_ME_EVAL_LIVE") {
        let live = available_embedder();
        let mut live_report = Report::default();
        match live.as_deref() {
            Some(e) => evaluate("live", Some(e), &mut live_report),
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
    };
    assert!(validate(&memories, &[golden(&["a"], &[])]).is_ok());
    assert!(validate(&memories, &[golden(&[], &[])]).is_err());
    assert!(validate(&memories, &[golden(&["missing"], &[])]).is_err());
    assert!(validate(&memories, &[golden(&["a"], &["missing"])]).is_err());
    assert!(validate(&memories, &[golden(&["a"], &["a"])]).is_err());
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
    test_env::set_var(EXPANSION_MODE_ENV, "hyde");
    test_env::remove_var(RERANK_ENV);
    assert!(remind_me_core::query_expansion::enabled());
    assert!(remind_me_core::reranker::enabled());
    {
        let _offline = ModelStagesOff::new();
        assert!(!remind_me_core::query_expansion::enabled());
        assert!(!remind_me_core::reranker::enabled());
    }
    assert_eq!(std::env::var(EXPANSION_MODE_ENV).as_deref(), Ok("hyde"));
    assert!(std::env::var(RERANK_ENV).is_err());
    test_env::remove_var(EXPANSION_MODE_ENV);
}
