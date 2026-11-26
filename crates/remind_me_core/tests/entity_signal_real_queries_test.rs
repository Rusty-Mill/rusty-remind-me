//! Card #64, real-query half: run a labelled query set against a copy of a
//! live store through the `remind_me_search` path (`search_with_expansions`),
//! at whatever `REMIND_ME_RRF_W_ENTITY` the process was started with.
//!
//! Ignored, and reads everything from the environment, so nothing private
//! enters the repo:
//!
//! - `REMIND_ME_DB_PATH`: the store copy (never the live store).
//! - `RRM64_QUERIES`: a JSONL file of `{query, relevant, kind}`.
//! - `RRM64_OUT`: where per-query results are written (JSONL), outside the
//!   repo. Ids and resolved-entity counts go there, never to stdout.
//!
//! ```text
//! REMIND_ME_DAEMON=0 REMIND_ME_DB_PATH=... RRM64_QUERIES=... RRM64_OUT=... \
//! REMIND_ME_RRF_W_ENTITY=0.5 cargo test -p remind_me_core --release \
//!     --test entity_signal_real_queries_test -- --ignored --nocapture
//! ```
//!
//! The search path writes to the store, so give each run a fresh copy.
//!
//! HyDE and rerank must be off in the caller's environment
//! (`REMIND_ME_QUERY_EXPANSION=off`, `REMIND_ME_RERANK=`); the test checks.

use std::io::Write;
use std::time::Instant;

use remind_me_core::db::queries;
use remind_me_core::entity::resolve_query_entities;
use remind_me_core::{Database, MemorySearchInput, SearchDetail};
use serde::Deserialize;

const DEPTH: usize = 10;
/// Searches per query, for the latency percentiles.
const REPEATS: usize = 5;

#[derive(Debug, Deserialize)]
struct Labelled {
    query: String,
    relevant: Vec<String>,
    kind: String,
}

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("{key} is not set"))
}

#[test]
#[ignore = "measurement for card #64 on a private store copy: prints aggregates only"]
fn entity_signal_real_queries() {
    assert_eq!(std::env::var("REMIND_ME_DAEMON").as_deref(), Ok("0"));
    assert_eq!(
        std::env::var("REMIND_ME_QUERY_EXPANSION").as_deref(),
        Ok("off")
    );
    assert!(std::env::var("REMIND_ME_RERANK")
        .unwrap_or_default()
        .is_empty());
    assert!(
        remind_me_core::embedder::available_embedder().is_some(),
        "no embedder configured: semantic search would silently drop out"
    );
    let weight = env("REMIND_ME_RRF_W_ENTITY");
    let labelled: Vec<Labelled> = std::fs::read_to_string(env("RRM64_QUERIES"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let db = Database::open(env("REMIND_ME_DB_PATH")).unwrap();
    let store = db.store();
    let mut out = std::fs::File::create(env("RRM64_OUT")).unwrap();
    let mut latencies_us = Vec::new();

    for (index, q) in labelled.iter().enumerate() {
        let entities = resolve_query_entities(&store, &q.query).unwrap().len();
        let input = MemorySearchInput {
            query: q.query.clone(),
            limit: DEPTH,
            token_budget: 0,
            detail: SearchDetail::Compact,
            ..Default::default()
        };
        // The search path writes (access counts, co-retrieval pairs), and
        // those writes move later rankings. Score the first search only; the
        // repeats are for latency. Start each weight from a fresh copy.
        let mut ids: Vec<String> = Vec::new();
        for repeat in 0..REPEATS {
            let started = Instant::now();
            let response = queries::search_with_expansions(&store, &input).unwrap();
            latencies_us.push(started.elapsed().as_micros() as u64);
            if repeat == 0 {
                ids = response.memories.into_iter().map(|r| r.memory.id).collect();
            }
        }
        let found = q
            .relevant
            .iter()
            .filter(|id| ids.iter().take(DEPTH).any(|h| h == *id));
        let recall = found.count() as f64 / q.relevant.len() as f64;
        let rr = ids
            .iter()
            .take(DEPTH)
            .position(|id| q.relevant.contains(id))
            .map_or(0.0, |i| 1.0 / (i + 1) as f64);
        let line = serde_json::json!({
            "index": index,
            "weight": weight,
            "kind": q.kind,
            "entities_resolved": entities,
            "recall10": recall,
            "rr": rr,
            "ids": ids,
        });
        writeln!(out, "{line}").unwrap();
    }
    latencies_us.sort_unstable();
    let pick = |p: f64| latencies_us[((latencies_us.len() - 1) as f64 * p).round() as usize];
    println!(
        "weight {weight}: {} queries, {} searches, p50 {} us, p95 {} us",
        labelled.len(),
        latencies_us.len(),
        pick(0.5),
        pick(0.95)
    );
}
