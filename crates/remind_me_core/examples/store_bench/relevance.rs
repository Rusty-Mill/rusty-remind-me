//! G2 relevance (spec § Thresholds; D58, D59): recall@10 and MRR on two
//! query sets, each candidate against the engine in the same run, failing
//! a cell more than two points below the engine's.
//!
//! - **Golden set** (D58): the #59 fixture and its 82 queries, through the
//!   very code `retrieval_eval_test.rs` runs (`tests/retrieval_eval/
//!   shared.rs`), loaded into a fresh candidate store per mode. `keyword`
//!   and `hashed` under every strategy are gated; the `+dates` modes are
//!   reported only, as in CI. A `must_not` hit, or an entity or topic
//!   filter that finds nothing relevant, is a miss outright. The engine's
//!   own cells must sit within two points of `baseline.json`, or the run is
//!   invalid (not a miss).
//! - **#1563 set** (D59), on D1 runs only: real queries and their vectors,
//!   embedded once beforehand by `embed_queries` with the model that made
//!   D1's vectors, both read from outside the repo (D39). The vectors'
//!   header names the backup's fingerprint ([`super::fingerprint`]), which
//!   must be the loaded backup's, and its model, role and dimension, which
//!   must be the loaded store's. Errors reading either file name a line or
//!   a query's ordinal and a fixed class, never a path, a parser's message
//!   or anything the files hold. `keyword` and `vector` under every
//!   strategy are gated. Every query must have exactly one vector, keyed
//!   by its ordinal, and no vector may name an ordinal the set lacks. A
//!   run counts for scoring only with all [`SOURCE_QUERIES`] source
//!   queries (D55), reported as `scoring_input`; a shorter set, such as a
//!   synthetic fixture, is scored but never passes. Relevant ids the D1
//!   copy lacks are left out of every candidate's denominator alike, after
//!   that count; a query with none left is dropped. Only counts and
//!   ordinals are reported, never query text.

use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;
use std::path::Path;

use super::{Precomputed, Result};
use remind_me_core::db::vectors::Vectors;
use remind_me_core::db::{queries, Store};
use remind_me_core::embedder::Embedder;
use remind_me_core::Database;

#[path = "../../tests/retrieval_eval/shared.rs"]
mod eval;

use eval::{Report, Scores, STRATEGIES, TOLERANCE};

/// The golden modes CI gates, then the ones it only reports.
const GATED: [&str; 2] = ["keyword", "hashed"];
const REPORTED: [&str; 2] = ["keyword+dates", "hashed+dates"];

/// The golden set on `candidate` (each mode a fresh store from `open`)
/// against the engine; its report.
pub fn golden(open: &mut dyn FnMut(&str) -> Result<Database>) -> Result<Value> {
    let (mut cand, mut engine) = (Report::default(), Report::default());
    for mode in GATED.iter().chain(&REPORTED) {
        let hashed = mode.starts_with("hashed");
        let embedder: Option<&dyn Embedder> = hashed.then_some(&eval::HashedEmbedder as _);
        let db = open(mode)?;
        eval::evaluate_in(mode, embedder, &db, &mut std::io::sink(), &mut cand);
        let reference = Database::open_in_memory()?;
        eval::evaluate_in(
            mode,
            embedder,
            &reference,
            &mut std::io::sink(),
            &mut engine,
        );
    }
    let baseline: BTreeMap<String, Scores> = serde_json::from_str(eval::BASELINE)?;
    let drift = drift_from(&baseline, &engine.scores);
    let mut misses = cand.violations.clone();
    misses.extend(cell_misses(&engine.scores, &cand.scores));
    let degraded = degraded_cells(&engine.degraded, &cand.degraded);
    let valid = drift.is_empty() && engine.degraded.is_empty() && cand.degraded.is_empty();
    Ok(json!({
        "cells": cells(&engine.scores, &cand.scores),
        "reported": cells(&engine.measured, &cand.measured),
        "misses": misses,
        "engine_baseline_drift": drift,
        "degraded_searches": degraded,
        "valid": valid,
        "passed": valid && misses.is_empty(),
    }))
}

/// How many searches degraded in each cell, by side: a degraded search
/// ranks a fallback's results (a failed semantic stage leaves keyword
/// only), so a cell holding one is not a comparison of full searches and
/// the run is invalid.
fn degraded_cells(engine: &[String], cand: &[String]) -> Value {
    let mut cells: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for (side, list) in [("engine", engine), ("candidate", cand)] {
        for key in list {
            let n = cells
                .entry(key.clone())
                .or_default()
                .entry(side)
                .or_insert(json!(0));
            *n = json!(n.as_u64().unwrap_or(0) + 1);
        }
    }
    json!(cells)
}

/// Each cell of `cand` whose recall@10 or MRR is more than [`TOLERANCE`]
/// below the engine's same cell (G2), by name.
fn cell_misses(engine: &BTreeMap<String, Scores>, cand: &BTreeMap<String, Scores>) -> Vec<String> {
    let mut misses = Vec::new();
    for (key, base) in engine {
        let Some(now) = cand.get(key) else {
            misses.push(format!("{key}: no candidate score"));
            continue;
        };
        for (name, b, n) in [
            ("recall@10", base.recall_at_10, now.recall_at_10),
            ("mrr", base.mrr, now.mrr),
        ] {
            if n < b - TOLERANCE - 1e-9 {
                misses.push(format!("{key}: {name} {n:.4}, engine {b:.4}"));
            }
        }
    }
    misses
}

/// Every engine cell more than [`TOLERANCE`] from `baseline.json`, either
/// way: the reference itself moved, so the run cannot judge anyone.
fn drift_from(
    baseline: &BTreeMap<String, Scores>,
    engine: &BTreeMap<String, Scores>,
) -> Vec<String> {
    let mut drift = Vec::new();
    for (key, now) in engine {
        let Some(base) = baseline.get(key) else {
            drift.push(format!("{key}: not in baseline.json"));
            continue;
        };
        for ((name, b), (_, n)) in base.named().iter().zip(now.named()) {
            if (n - b).abs() > TOLERANCE + 1e-9 {
                drift.push(format!("{key}: {name} {n:.4}, baseline {b:.4}"));
            }
        }
    }
    drift
}

fn cells(engine: &BTreeMap<String, Scores>, cand: &BTreeMap<String, Scores>) -> Value {
    let rows: Map<String, Value> = engine
        .iter()
        .map(|(key, e)| {
            (
                key.clone(),
                json!({ "engine": e, "candidate": cand.get(key) }),
            )
        })
        .collect();
    Value::Object(rows)
}

/// How many source queries the #1563 set holds (D55). A scoring run
/// needs every one of them, before D59's missing-id exclusions.
pub const SOURCE_QUERIES: usize = 50;

/// One #1563 query, in the #59 harness line format.
#[derive(Deserialize)]
struct RealQuery {
    query: String,
    relevant: Vec<String>,
}

/// The vectors file's first line: what made the vectors, and for which
/// backup.
#[derive(Deserialize)]
pub struct VectorsHeader {
    pub backend: String,
    pub model: String,
    pub dim: usize,
    pub role: String,
    /// The backup directory's name, for the report.
    pub backup: String,
    /// The backup's fingerprint ([`super::fingerprint::backup_fingerprint`]).
    pub backup_sha256: String,
}

/// One query vector, keyed by the query's line number in the set (from
/// 1), with its text so a reordered set is caught.
#[derive(Deserialize)]
struct QueryVector {
    n: usize,
    query: String,
    vector: Vec<f32>,
}

/// The #1563 set and its vectors, checked against each other and against
/// the store's embedding identity.
pub struct RealSet {
    queries: Vec<RealQuery>,
    vectors: Precomputed,
    /// The backup the vectors were made for, as their header names it.
    for_backup: String,
}

impl RealSet {
    /// Read `queries` and `vectors`, refusing a missing or mismatched
    /// vector, vectors made for another backup than the one fingerprinted
    /// `backup_sha256`, or from another model, role or dimension than
    /// `identity` (the D1 store's embedding meta).
    pub fn read(
        queries: &Path,
        vectors: &Path,
        identity: &[(String, String)],
        backup_sha256: &str,
    ) -> Result<Self> {
        let set: Vec<RealQuery> = jsonl(queries, QUERIES)?;
        let file = std::fs::File::open(vectors).map_err(unreadable(VECTORS))?;
        let mut lines = std::io::BufReader::new(file).lines();
        let first = lines
            .next()
            .ok_or("the vectors file is empty")?
            .map_err(unreadable(VECTORS))?;
        let header: VectorsHeader =
            serde_json::from_str(&first).map_err(|_| malformed(VECTORS, 1))?;
        check_header(&header, identity, backup_sha256)?;
        let mut by_n = HashMap::new();
        for (i, line) in lines.enumerate() {
            let line = line.map_err(unreadable(VECTORS))?;
            if line.trim().is_empty() {
                continue;
            }
            let v: QueryVector =
                serde_json::from_str(&line).map_err(|_| malformed(VECTORS, i + 2))?;
            if v.vector.len() != header.dim {
                return Err(format!(
                    "query {} has a {}-dim vector, not {}",
                    v.n,
                    v.vector.len(),
                    header.dim
                )
                .into());
            }
            if v.n == 0 || v.n > set.len() {
                return Err(
                    format!("{VECTORS}, line {}: no query {} in the set", i + 2, v.n).into(),
                );
            }
            let n = v.n;
            if by_n.insert(n, v).is_some() {
                return Err(format!("query {n} has two vectors").into());
            }
        }
        let mut map = HashMap::new();
        for (i, q) in set.iter().enumerate() {
            let v = by_n
                .get(&(i + 1))
                .ok_or_else(|| format!("query {} has no vector", i + 1))?;
            if v.query != q.query {
                return Err(
                    format!("query {}: the vectors file names another query", i + 1).into(),
                );
            }
            if map
                .insert(q.query.clone(), v.vector.clone())
                .is_some_and(|old| old != v.vector)
            {
                return Err(format!("query {}: one text, two vectors", i + 1).into());
            }
        }
        Ok(Self {
            queries: set,
            vectors: Precomputed(map),
            for_backup: header.backup,
        })
    }
}

/// Refuse vectors not made for the loaded backup, by the store's model,
/// for queries, at its dimension. The errors name the field, never a
/// value from either side.
fn check_header(
    header: &VectorsHeader,
    identity: &[(String, String)],
    backup_sha256: &str,
) -> Result<()> {
    if header.backup_sha256 != backup_sha256 {
        return Err("the vectors were made for another backup than the one loaded".into());
    }
    if header.role != "query" {
        return Err("the vectors were not embedded as queries".into());
    }
    let meta = |key: &str| {
        identity
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    let dim = header.dim.to_string();
    for (key, want) in [
        ("backend", header.backend.as_str()),
        ("model", header.model.as_str()),
        ("dim", dim.as_str()),
    ] {
        if meta(key) != Some(want) {
            return Err(format!("the vectors' {key} is not the loaded store's").into());
        }
    }
    Ok(())
}

/// The input files, as errors name them.
const QUERIES: &str = "the queries file";
const VECTORS: &str = "the vectors file";

/// An I/O error reading `file`, as its kind only.
fn unreadable(file: &'static str) -> impl Fn(std::io::Error) -> Box<dyn std::error::Error> {
    move |e| format!("cannot read {file} ({:?})", e.kind()).into()
}

/// A line of `file` that does not parse, by its number from 1.
fn malformed(file: &str, line: usize) -> String {
    format!("{file}, line {line}: not a valid line")
}

fn jsonl<T: for<'de> Deserialize<'de>>(path: &Path, file: &'static str) -> Result<Vec<T>> {
    let text = std::fs::read_to_string(path).map_err(unreadable(file))?;
    let mut rows = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        rows.push(serde_json::from_str(line).map_err(|_| malformed(file, i + 1))?);
    }
    Ok(rows)
}

impl RealSet {
    /// Whether this set can score G2: all [`SOURCE_QUERIES`] source
    /// queries, each with its one vector (`read` refuses any gap), counted
    /// before D59 drops any (D55). A shorter set, such as a synthetic
    /// fixture, still runs but its report never passes.
    pub fn scoring_input(&self) -> bool {
        self.queries.len() == SOURCE_QUERIES
    }
}

/// The #1563 set on the candidate's D1 store against the engine's; its
/// report (counts and ordinals only).
pub fn real(set: &RealSet, cand: &Store<'_>, engine: &Store<'_>) -> Result<Value> {
    // The same queries and relevant ids for every candidate: those the D1
    // copy holds.
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    let mut missing = 0;
    for (i, q) in set.queries.iter().enumerate() {
        let mut present = Vec::new();
        for id in &q.relevant {
            if queries::get_memory_by_id(engine, id)?.is_some() {
                present.push(id.clone());
            } else {
                missing += 1;
            }
        }
        if present.is_empty() {
            dropped.push(i + 1);
        } else {
            kept.push(eval::GoldenQuery {
                query: q.query.clone(),
                relevant: present,
                kind: "real".into(),
                must_not: Vec::new(),
                project: None,
                entity: None,
                topic: None,
            });
        }
    }
    if kept.is_empty() {
        return Err("no #1563 query has a relevant id in this D1 copy".into());
    }
    let (mut e, mut c) = (BTreeMap::new(), BTreeMap::new());
    let (mut e_degraded, mut c_degraded) = (Vec::new(), Vec::new());
    for (mode, embedder) in [
        ("keyword", None),
        ("vector", Some(&set.vectors as &dyn Embedder)),
    ] {
        for (name, strategy) in STRATEGIES {
            let key = format!("{mode}/{name}");
            let (scores, degraded) = score(engine, &kept, strategy, embedder);
            e_degraded.extend(std::iter::repeat_n(key.clone(), degraded));
            e.insert(key.clone(), scores);
            let (scores, degraded) = score(cand, &kept, strategy, embedder);
            c_degraded.extend(std::iter::repeat_n(key.clone(), degraded));
            c.insert(key, scores);
        }
    }
    let misses = cell_misses(&e, &c);
    let degraded = degraded_cells(&e_degraded, &c_degraded);
    let scoring_input = set.scoring_input();
    let valid = scoring_input && e_degraded.is_empty() && c_degraded.is_empty();
    Ok(json!({
        "vectors_for_backup": set.for_backup,
        "queries": set.queries.len(),
        "source_queries_required": SOURCE_QUERIES,
        "scoring_input": scoring_input,
        "scored": kept.len(),
        "dropped": dropped,
        "relevant_ids_missing": missing,
        "cells": cells(&e, &c),
        "misses": misses,
        "degraded_searches": degraded,
        "valid": valid,
        "passed": valid && misses.is_empty(),
    }))
}

/// Mean recall@5, recall@10 and MRR of `golden` on `store`, and how many
/// of its searches degraded.
fn score(
    store: &Store<'_>,
    golden: &[eval::GoldenQuery],
    strategy: remind_me_core::RetrievalStrategy,
    embedder: Option<&dyn Embedder>,
) -> (Scores, usize) {
    let mut degraded = 0;
    let rows: Vec<(f64, f64, f64)> = golden
        .iter()
        .map(|g| {
            let (ranking, failed) = eval::ranked_outcome(store, g, strategy, embedder, false);
            degraded += usize::from(failed);
            (
                eval::recall_at(&ranking, &g.relevant, 5),
                eval::recall_at(&ranking, &g.relevant, eval::DEPTH),
                eval::reciprocal_rank(&ranking, &g.relevant),
            )
        })
        .collect();
    (Scores::mean(&rows), degraded)
}

/// The D1 store's embedding identity, as `RealSet::read` checks it.
pub fn identity(store: &Store<'_>) -> Result<Vec<(String, String)>> {
    Ok(Vectors::new(store).meta()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(r10: f64, mrr: f64) -> Scores {
        Scores {
            recall_at_5: 0.0,
            recall_at_10: r10,
            mrr,
        }
    }

    #[test]
    fn a_cell_more_than_two_points_below_the_engine_is_a_miss() {
        let engine = BTreeMap::from([("keyword/auto".to_string(), s(0.90, 0.80))]);
        let close = BTreeMap::from([("keyword/auto".to_string(), s(0.885, 0.79))]);
        assert!(cell_misses(&engine, &close).is_empty());
        let far = BTreeMap::from([("keyword/auto".to_string(), s(0.87, 0.80))]);
        assert_eq!(cell_misses(&engine, &far).len(), 1);
        let worse_mrr = BTreeMap::from([("keyword/auto".to_string(), s(0.90, 0.77))]);
        assert!(cell_misses(&engine, &worse_mrr)[0].contains("mrr"));
        assert!(cell_misses(&engine, &BTreeMap::new())[0].contains("no candidate"));
    }

    /// An embedder that always fails, as a precomputed set missing a
    /// query would.
    struct Failing;

    impl Embedder for Failing {
        fn embed(
            &self,
            _texts: &[String],
            _role: remind_me_core::embedder::EmbedRole,
        ) -> std::result::Result<Vec<Vec<f32>>, remind_me_core::embedder::EmbedError> {
            Err(remind_me_core::embedder::EmbedError("down".into()))
        }

        fn dim(&self) -> usize {
            eval::HashedEmbedder.dim()
        }

        fn identity(&self) -> remind_me_core::embedder::EmbeddingIdentity {
            eval::HashedEmbedder.identity()
        }
    }

    /// A vector search whose semantic stage fails falls back to keyword
    /// results; the cell counts it as degraded instead of scoring it as a
    /// vector result.
    #[test]
    fn a_degraded_search_is_counted_not_scored_as_a_full_one() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let input = remind_me_core::models::MemoryAddInput {
            content: "wombat burrows".into(),
            extract: false,
            ..Default::default()
        };
        let id = queries::add_memory(&store, input).unwrap().id;
        let golden = vec![eval::GoldenQuery {
            query: "wombat".into(),
            relevant: vec![id],
            kind: "real".into(),
            must_not: Vec::new(),
            project: None,
            entity: None,
            topic: None,
        }];
        let strategy = remind_me_core::RetrievalStrategy::Balanced;
        let (_, degraded) = score(&store, &golden, strategy, Some(&Failing));
        assert_eq!(degraded, 1);
        let (_, clean) = score(&store, &golden, strategy, None);
        assert_eq!(clean, 0);
        let cells = degraded_cells(&[], &["vector/balanced".into()]);
        assert_eq!(cells["vector/balanced"]["candidate"], 1);
    }

    #[test]
    fn engine_drift_either_way_invalidates_the_run() {
        let base = BTreeMap::from([("hashed/auto".to_string(), s(0.80, 0.70))]);
        let up = BTreeMap::from([("hashed/auto".to_string(), s(0.83, 0.70))]);
        assert_eq!(drift_from(&base, &up).len(), 1);
        let same = BTreeMap::from([("hashed/auto".to_string(), s(0.81, 0.69))]);
        assert!(drift_from(&base, &same).is_empty());
    }

    fn header(model: &str, dim: usize, role: &str) -> VectorsHeader {
        VectorsHeader {
            backend: "openai".into(),
            model: model.into(),
            dim,
            role: role.into(),
            backup: "manual-x.engine".into(),
            backup_sha256: "fp".into(),
        }
    }

    #[test]
    fn vectors_from_another_model_role_or_dimension_are_refused() {
        let meta = vec![
            ("backend".to_string(), "openai".to_string()),
            ("model".to_string(), "m".to_string()),
            ("dim".to_string(), "4".to_string()),
        ];
        check_header(&header("m", 4, "query"), &meta, "fp").unwrap();
        assert!(check_header(&header("other", 4, "query"), &meta, "fp").is_err());
        assert!(check_header(&header("m", 8, "query"), &meta, "fp").is_err());
        assert!(check_header(&header("m", 4, "document"), &meta, "fp").is_err());
        let error = check_header(&header("m", 4, "query"), &meta, "another").unwrap_err();
        assert!(error.to_string().contains("another backup"), "{error}");
        let error = check_header(&header("secret-model", 4, "query"), &meta, "fp").unwrap_err();
        assert!(!error.to_string().contains("secret"), "{error}");
    }

    #[test]
    fn a_query_without_its_vector_or_with_another_text_is_refused() {
        let root = std::env::temp_dir().join(format!("store_bench_rel_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let meta = vec![
            ("backend".to_string(), "openai".to_string()),
            ("model".to_string(), "m".to_string()),
            ("dim".to_string(), "2".to_string()),
        ];
        let q = root.join("q.jsonl");
        std::fs::write(&q, "{\"query\":\"a\",\"relevant\":[\"x\"],\"kind\":\"k\"}\n{\"query\":\"b\",\"relevant\":[\"y\"],\"kind\":\"k\"}\n").unwrap();
        let head = "{\"backend\":\"openai\",\"model\":\"m\",\"dim\":2,\"role\":\"query\",\"backup\":\"b\",\"backup_sha256\":\"fp\"}";
        let v = root.join("v.jsonl");
        std::fs::write(&v, format!("{head}\n{{\"n\":1,\"query\":\"a\",\"vector\":[1,0]}}\n{{\"n\":2,\"query\":\"b\",\"vector\":[0,1]}}\n")).unwrap();
        RealSet::read(&q, &v, &meta, "fp").unwrap();
        std::fs::write(
            &v,
            format!("{head}\n{{\"n\":1,\"query\":\"a\",\"vector\":[1,0]}}\n"),
        )
        .unwrap();
        assert!(
            RealSet::read(&q, &v, &meta, "fp").is_err(),
            "missing vector"
        );
        std::fs::write(&v, format!("{head}\n{{\"n\":1,\"query\":\"a\",\"vector\":[1,0]}}\n{{\"n\":2,\"query\":\"z\",\"vector\":[0,1]}}\n")).unwrap();
        assert!(RealSet::read(&q, &v, &meta, "fp").is_err(), "reordered set");
        std::fs::write(
            &v,
            format!("{head}\n{{\"n\":1,\"query\":\"a\",\"vector\":[1,0,0]}}\n"),
        )
        .unwrap();
        assert!(
            RealSet::read(&q, &v, &meta, "fp").is_err(),
            "wrong dimension"
        );
        std::fs::write(&v, format!("{head}\n{{\"n\":1,\"query\":\"a\",\"vector\":[1,0]}}\n{{\"n\":1,\"query\":\"a\",\"vector\":[1,0]}}\n{{\"n\":2,\"query\":\"b\",\"vector\":[0,1]}}\n")).unwrap();
        let error = refused(RealSet::read(&q, &v, &meta, "fp"));
        assert_eq!(error, "query 1 has two vectors");
        std::fs::write(&v, format!("{head}\n{{\"n\":1,\"query\":\"a\",\"vector\":[1,0]}}\n{{\"n\":2,\"query\":\"b\",\"vector\":[0,1]}}\n{{\"n\":3,\"query\":\"c\",\"vector\":[1,1]}}\n")).unwrap();
        let error = refused(RealSet::read(&q, &v, &meta, "fp"));
        assert_eq!(error, "the vectors file, line 4: no query 3 in the set");
        std::fs::write(
            &v,
            format!("{head}\n{{\"n\":0,\"query\":\"a\",\"vector\":[1,0]}}\n"),
        )
        .unwrap();
        assert!(RealSet::read(&q, &v, &meta, "fp").is_err(), "ordinal 0");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A 49-query set reads but is not a scoring input; the full 50 is.
    /// A gap in the vectors' ordinals, or one past 50, is refused.
    #[test]
    fn only_the_full_fifty_query_set_is_a_scoring_input() {
        let root = std::env::temp_dir().join(format!("store_bench_rel50_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let meta = vec![
            ("backend".to_string(), "openai".to_string()),
            ("model".to_string(), "m".to_string()),
            ("dim".to_string(), "2".to_string()),
        ];
        let head = "{\"backend\":\"openai\",\"model\":\"m\",\"dim\":2,\"role\":\"query\",\"backup\":\"b\",\"backup_sha256\":\"fp\"}";
        let (q, v) = (root.join("q.jsonl"), root.join("v.jsonl"));
        let write = |queries: usize, vectors: &[usize]| {
            let set: String = (1..=queries)
                .map(|n| format!("{{\"query\":\"q{n}\",\"relevant\":[\"x\"]}}\n"))
                .collect();
            std::fs::write(&q, set).unwrap();
            let lines: String = vectors
                .iter()
                .map(|n| format!("{{\"n\":{n},\"query\":\"q{n}\",\"vector\":[1,0]}}\n"))
                .collect();
            std::fs::write(&v, format!("{head}\n{lines}")).unwrap();
        };
        let all: Vec<usize> = (1..=SOURCE_QUERIES).collect();
        write(SOURCE_QUERIES - 1, &all[..SOURCE_QUERIES - 1]);
        assert!(
            !RealSet::read(&q, &v, &meta, "fp").unwrap().scoring_input(),
            "49 queries"
        );
        write(SOURCE_QUERIES, &all);
        assert!(
            RealSet::read(&q, &v, &meta, "fp").unwrap().scoring_input(),
            "the full set"
        );
        let gap: Vec<usize> = all.iter().copied().filter(|&n| n != 25).collect();
        write(SOURCE_QUERIES, &gap);
        assert_eq!(
            refused(RealSet::read(&q, &v, &meta, "fp")),
            "query 25 has no vector"
        );
        write(SOURCE_QUERIES, &[all.as_slice(), &[51]].concat());
        assert_eq!(
            refused(RealSet::read(&q, &v, &meta, "fp")),
            "the vectors file, line 52: no query 51 in the set"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Why `read` refused: `RealSet` has no `Debug`, as it holds the
    /// private queries.
    fn refused(read: Result<RealSet>) -> String {
        match read {
            Ok(_) => panic!("the set was read"),
            Err(e) => e.to_string(),
        }
    }

    /// A line that does not parse is named by its file and number only:
    /// no path, no parser message, nothing the line holds.
    #[test]
    fn a_malformed_private_line_is_named_by_its_number_only() {
        let root =
            std::env::temp_dir().join(format!("store_bench_rel_secret_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let meta = vec![
            ("backend".to_string(), "openai".to_string()),
            ("model".to_string(), "m".to_string()),
            ("dim".to_string(), "2".to_string()),
        ];
        let q = root.join("q.jsonl");
        let v = root.join("v.jsonl");
        std::fs::write(
            &q,
            "{\"query\":\"a\",\"relevant\":[\"x\"]}\n{\"query\": secret text\n",
        )
        .unwrap();
        std::fs::write(&v, "{\"backend\":\"openai\"}\n").unwrap();
        let error = refused(RealSet::read(&q, &v, &meta, "fp"));
        assert_eq!(error, "the queries file, line 2: not a valid line");
        std::fs::write(&q, "{\"query\":\"a\",\"relevant\":[\"x\"]}\n").unwrap();
        let error = refused(RealSet::read(&q, &v, &meta, "fp"));
        assert_eq!(error, "the vectors file, line 1: not a valid line");
        let error = refused(RealSet::read(&root.join("missing"), &v, &meta, "fp"));
        assert!(
            !error.contains("secret") && !error.contains("missing"),
            "{error}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
