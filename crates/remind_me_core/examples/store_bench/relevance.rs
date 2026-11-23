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
//!   D1's vectors, both read from outside the repo (D39). `keyword` and
//!   `vector` under every strategy are gated. Relevant ids the D1 copy
//!   lacks are left out of every candidate's denominator alike; a query
//!   with none left is dropped. Only counts and ordinals are reported,
//!   never query text.

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
    Ok(json!({
        "cells": cells(&engine.scores, &cand.scores),
        "reported": cells(&engine.measured, &cand.measured),
        "misses": misses,
        "engine_baseline_drift": drift,
        "valid": drift.is_empty(),
        "passed": drift.is_empty() && misses.is_empty(),
    }))
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
    pub backup: String,
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
    /// vector, or vectors from another model, role or dimension than
    /// `identity` (the D1 store's embedding meta).
    pub fn read(queries: &Path, vectors: &Path, identity: &[(String, String)]) -> Result<Self> {
        let set: Vec<RealQuery> = jsonl(queries)?;
        let mut lines = std::io::BufReader::new(std::fs::File::open(vectors)?).lines();
        let header: VectorsHeader =
            serde_json::from_str(&lines.next().ok_or("empty vectors file")??)?;
        check_header(&header, identity)?;
        let mut by_n = HashMap::new();
        for line in lines {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let v: QueryVector = serde_json::from_str(&line)?;
            if v.vector.len() != header.dim {
                return Err(format!(
                    "query {} has a {}-dim vector, not {}",
                    v.n,
                    v.vector.len(),
                    header.dim
                )
                .into());
            }
            by_n.insert(v.n, v);
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

/// Refuse vectors not made by the store's model, for queries, at its
/// dimension.
fn check_header(header: &VectorsHeader, identity: &[(String, String)]) -> Result<()> {
    if header.role != "query" {
        return Err(format!("the vectors were embedded as {:?}, not query", header.role).into());
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
            return Err(format!(
                "the vectors' {key} is {want:?}; the store's is {:?}",
                meta(key)
            )
            .into());
        }
    }
    Ok(())
}

fn jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    let text = std::fs::read_to_string(path)?;
    let mut rows = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        rows.push(
            serde_json::from_str(line)
                .map_err(|e| format!("{} line {}: {e}", path.display(), i + 1))?,
        );
    }
    Ok(rows)
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
    for (mode, embedder) in [
        ("keyword", None),
        ("vector", Some(&set.vectors as &dyn Embedder)),
    ] {
        for (name, strategy) in STRATEGIES {
            let key = format!("{mode}/{name}");
            e.insert(key.clone(), score(engine, &kept, strategy, embedder));
            c.insert(key, score(cand, &kept, strategy, embedder));
        }
    }
    let misses = cell_misses(&e, &c);
    Ok(json!({
        "vectors_for_backup": set.for_backup,
        "queries": set.queries.len(),
        "scored": kept.len(),
        "dropped": dropped,
        "relevant_ids_missing": missing,
        "cells": cells(&e, &c),
        "misses": misses,
        "passed": misses.is_empty(),
    }))
}

/// Mean recall@5, recall@10 and MRR of `golden` on `store`.
fn score(
    store: &Store<'_>,
    golden: &[eval::GoldenQuery],
    strategy: remind_me_core::RetrievalStrategy,
    embedder: Option<&dyn Embedder>,
) -> Scores {
    let rows: Vec<(f64, f64, f64)> = golden
        .iter()
        .map(|g| {
            let ranking = eval::ranked(store, g, strategy, embedder, false);
            (
                eval::recall_at(&ranking, &g.relevant, 5),
                eval::recall_at(&ranking, &g.relevant, eval::DEPTH),
                eval::reciprocal_rank(&ranking, &g.relevant),
            )
        })
        .collect();
    Scores::mean(&rows)
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
        }
    }

    #[test]
    fn vectors_from_another_model_role_or_dimension_are_refused() {
        let meta = vec![
            ("backend".to_string(), "openai".to_string()),
            ("model".to_string(), "m".to_string()),
            ("dim".to_string(), "4".to_string()),
        ];
        check_header(&header("m", 4, "query"), &meta).unwrap();
        assert!(check_header(&header("other", 4, "query"), &meta).is_err());
        assert!(check_header(&header("m", 8, "query"), &meta).is_err());
        assert!(check_header(&header("m", 4, "document"), &meta).is_err());
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
        let head = "{\"backend\":\"openai\",\"model\":\"m\",\"dim\":2,\"role\":\"query\",\"backup\":\"b\"}";
        let v = root.join("v.jsonl");
        std::fs::write(&v, format!("{head}\n{{\"n\":1,\"query\":\"a\",\"vector\":[1,0]}}\n{{\"n\":2,\"query\":\"b\",\"vector\":[0,1]}}\n")).unwrap();
        RealSet::read(&q, &v, &meta).unwrap();
        std::fs::write(
            &v,
            format!("{head}\n{{\"n\":1,\"query\":\"a\",\"vector\":[1,0]}}\n"),
        )
        .unwrap();
        assert!(RealSet::read(&q, &v, &meta).is_err(), "missing vector");
        std::fs::write(&v, format!("{head}\n{{\"n\":1,\"query\":\"a\",\"vector\":[1,0]}}\n{{\"n\":2,\"query\":\"z\",\"vector\":[0,1]}}\n")).unwrap();
        assert!(RealSet::read(&q, &v, &meta).is_err(), "reordered set");
        std::fs::write(
            &v,
            format!("{head}\n{{\"n\":1,\"query\":\"a\",\"vector\":[1,0,0]}}\n"),
        )
        .unwrap();
        assert!(RealSet::read(&q, &v, &meta).is_err(), "wrong dimension");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
