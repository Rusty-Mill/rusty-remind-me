//! Card #64's measurement: does the entity-overlap signal
//! (`REMIND_ME_RRF_W_ENTITY`) help ranking enough to keep?
//!
//! Runs the #59 harness's gated modes (keyword, hashed) under every strategy
//! at entity weights 0, 0.5 and 1.0, and prints, per query kind, recall@5,
//! recall@10 and MRR, the per-query wins and losses against weight 0, the
//! search latency each weight adds, and the D92 keep rule's verdict.
//!
//! Two stores, so the gated baseline is never touched:
//!
//! - **base**: the #59 fixture alone, scored with its own golden set. At
//!   weight 0 it reproduces `baseline.json`.
//! - **synthetic**: the #59 fixture plus `entity_spike_memories.jsonl`, a
//!   second synthetic world whose rows are entity-linked (aliases from
//!   `entity_spike_aliases.jsonl`), scored with `entity_spike_queries.jsonl`
//!   only. Its `entity-adversarial` queries name an entity whose linked rows
//!   are *not* the answer, so the signal can be seen to hurt as well as help.
//!
//! The measurement is `#[ignore]`d: it prints numbers, gates nothing, and
//! wants `--release` for its latency figures:
//!
//! ```text
//! cargo test -p remind_me_core --release --test entity_signal_spike_test \
//!     -- --ignored --nocapture
//! ```
//!
//! The clock: rows are aged relative to the run, and only the clock-free
//! gated modes run (no `+dates`), so every run ranks the same.

use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

use remind_me_core::db::Store;
use remind_me_core::embedder::Embedder;
use remind_me_core::entity::upsert_entity;
use remind_me_core::query_expansion::EXPANSION_MODE_ENV;
use remind_me_core::reranker::BACKEND_ENV as RERANK_ENV;
use remind_me_core::{Database, EntityInput};
use serde::Deserialize;

#[path = "../src/test_env.rs"]
mod test_env;

#[path = "retrieval_eval/shared.rs"]
mod shared;
use shared::*;

const SPIKE_MEMORIES: &str = include_str!("fixtures/retrieval_eval/entity_spike_memories.jsonl");
const SPIKE_QUERIES: &str = include_str!("fixtures/retrieval_eval/entity_spike_queries.jsonl");
const SPIKE_ALIASES: &str = include_str!("fixtures/retrieval_eval/entity_spike_aliases.jsonl");

const ENTITY_WEIGHT_ENV: &str = "REMIND_ME_RRF_W_ENTITY";
const WEIGHTS: [&str; 3] = ["0", "0.5", "1.0"];
/// Searches per query per strategy, for the latency percentiles.
const REPEATS: usize = 5;
/// D92: the lift that keeps the signal, and the drop that sinks it.
const KEEP_LIFT: f64 = 0.02;
const MAX_DROP: f64 = 0.02;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpikeAlias {
    name: String,
    aliases: Vec<String>,
}

fn parse<T: for<'de> Deserialize<'de>>(text: &str) -> Vec<T> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The #59 fixture followed by the synthetic rows.
fn synthetic_memories() -> Vec<FixtureMemory> {
    let mut all: Vec<FixtureMemory> = read_jsonl("memories.jsonl");
    all.extend(parse::<FixtureMemory>(SPIKE_MEMORIES));
    all
}

fn add_aliases(store: &Store<'_>) {
    for alias in parse::<SpikeAlias>(SPIKE_ALIASES) {
        upsert_entity(
            store,
            &EntityInput {
                name: alias.name,
                kind: None,
                aliases: alias.aliases,
            },
        )
        .unwrap();
    }
}

/// One query's scores at one weight.
#[derive(Debug, Clone, Copy)]
struct Row {
    r5: f64,
    r10: f64,
    rr: f64,
}

/// `(mode, weight, strategy, set, kind)` -> one row per query, in order.
type Rows = BTreeMap<(String, String, String, String, String), Vec<Row>>;

/// Sets one variable until dropped, then restores it.
struct EnvGuard(&'static str, Option<String>);

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let saved = std::env::var(key).ok();
        test_env::set_var(key, value);
        Self(key, saved)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.1 {
            Some(v) => test_env::set_var(self.0, v),
            None => test_env::remove_var(self.0),
        }
    }
}

/// Score `golden` on `store` under every strategy, into `rows` and
/// `latency_us` (per weight).
#[allow(clippy::too_many_arguments)]
fn run_set(
    store: &Store<'_>,
    golden: &[GoldenQuery],
    embedder: Option<&dyn Embedder>,
    mode: &str,
    weight: &str,
    set: &str,
    rows: &mut Rows,
    latency_us: &mut BTreeMap<String, Vec<u128>>,
) {
    for (strategy_name, strategy) in STRATEGIES {
        for g in golden {
            let mut ranking = Vec::new();
            for _ in 0..REPEATS {
                let started = Instant::now();
                ranking = ranked(store, g, strategy, embedder, false);
                latency_us
                    .entry(weight.to_string())
                    .or_default()
                    .push(started.elapsed().as_micros());
            }
            let row = Row {
                r5: recall_at(&ranking, &g.relevant, 5),
                r10: recall_at(&ranking, &g.relevant, DEPTH),
                rr: reciprocal_rank(&ranking, &g.relevant),
            };
            let key = (
                mode.to_string(),
                weight.to_string(),
                strategy_name.to_string(),
                set.to_string(),
                g.kind.clone(),
            );
            rows.entry(key).or_default().push(row);
        }
    }
}

fn mean(rows: &[Row]) -> Scores {
    let triples: Vec<(f64, f64, f64)> = rows.iter().map(|r| (r.r5, r.r10, r.rr)).collect();
    Scores::mean(&triples)
}

/// Queries whose (recall@10, RR) went up / down against `base`.
fn wins_losses(base: &[Row], now: &[Row]) -> (usize, usize) {
    let mut wins = 0;
    let mut losses = 0;
    for (b, n) in base.iter().zip(now) {
        let delta = (n.r10 - b.r10) + (n.rr - b.rr);
        if delta > 1e-9 {
            wins += 1;
        } else if delta < -1e-9 {
            losses += 1;
        }
    }
    (wins, losses)
}

fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

fn is_entity_kind(set: &str, kind: &str) -> bool {
    set == "synthetic" || kind == "entity"
}

#[test]
fn the_spike_fixture_is_valid_and_apart_from_the_base_one() {
    let base: Vec<FixtureMemory> = read_jsonl("memories.jsonl");
    let base_ids: HashSet<String> = base.iter().map(|m| m.id.clone()).collect();
    let spike: Vec<FixtureMemory> = parse(SPIKE_MEMORIES);
    assert!(spike.iter().all(|m| !base_ids.contains(&m.id)));
    let golden: Vec<GoldenQuery> = parse(SPIKE_QUERIES);
    validate(&synthetic_memories(), &golden).unwrap();
    let named: HashSet<String> = spike.iter().flat_map(|m| m.entities.clone()).collect();
    for alias in parse::<SpikeAlias>(SPIKE_ALIASES) {
        assert!(named.contains(&alias.name), "{}", alias.name);
    }
}

#[test]
#[ignore = "measurement for card #64: prints, gates nothing"]
fn entity_signal_spike() {
    let _offline_hyde = EnvGuard::set(EXPANSION_MODE_ENV, "off");
    let _offline_rerank = EnvGuard::set(RERANK_ENV, "");
    let base_golden: Vec<GoldenQuery> = read_jsonl("queries.jsonl");
    let spike_golden: Vec<GoldenQuery> = parse(SPIKE_QUERIES);
    let modes: [(&str, Option<&dyn Embedder>); 2] =
        [("keyword", None), ("hashed", Some(&HashedEmbedder))];

    let mut rows = Rows::new();
    let mut latency_us: BTreeMap<String, Vec<u128>> = BTreeMap::new();
    for (mode, embedder) in modes {
        let base_db = Database::open_in_memory().unwrap();
        let base_store = base_db.store();
        load(&base_store, &read_jsonl("memories.jsonl"), embedder);
        let synth_db = Database::open_in_memory().unwrap();
        let synth_store = synth_db.store();
        load(&synth_store, &synthetic_memories(), embedder);
        add_aliases(&synth_store);
        for weight in WEIGHTS {
            let _weight = EnvGuard::set(ENTITY_WEIGHT_ENV, weight);
            let mut sink = |store: &Store<'_>, golden: &[GoldenQuery], set: &str| {
                run_set(
                    store,
                    golden,
                    embedder,
                    mode,
                    weight,
                    set,
                    &mut rows,
                    &mut latency_us,
                )
            };
            sink(&base_store, &base_golden, "base");
            sink(&synth_store, &spike_golden, "synthetic");
        }
    }

    report(&rows, &latency_us);
}

fn report(rows: &Rows, latency_us: &BTreeMap<String, Vec<u128>>) {
    println!("mode/strategy/set/kind  n  | w=0 r@5 r@10 mrr | w=0.5 delta | w=1.0 delta | W/L 0.5 | W/L 1.0");
    // Per non-zero weight: the worst drop outside the entity kinds.
    let mut worst_drop: BTreeMap<String, (f64, String)> = BTreeMap::new();
    let mut entity_pool: BTreeMap<(String, String), Vec<Row>> = BTreeMap::new();
    let mut strategy_totals: BTreeMap<(String, String, String), Vec<Row>> = BTreeMap::new();
    for ((mode, weight, strategy, set, kind), now) in rows {
        let group = (mode.clone(), weight.clone());
        if is_entity_kind(set, kind) {
            entity_pool
                .entry(group)
                .or_default()
                .extend(now.iter().copied());
        }
        if set == "base" {
            strategy_totals
                .entry((mode.clone(), strategy.clone(), weight.clone()))
                .or_default()
                .extend(now.iter().copied());
        }
        if weight != WEIGHTS[0] {
            continue;
        }
        let base = mean(now);
        let mut line = format!(
            "{mode}/{strategy}/{set}/{kind} n={} | {:.3} {:.3} {:.3}",
            now.len(),
            base.recall_at_5,
            base.recall_at_10,
            base.mrr
        );
        let mut wl = String::new();
        for w in &WEIGHTS[1..] {
            let key = (
                mode.clone(),
                w.to_string(),
                strategy.clone(),
                set.clone(),
                kind.clone(),
            );
            let other = &rows[&key];
            let s = mean(other);
            let deltas = [
                s.recall_at_5 - base.recall_at_5,
                s.recall_at_10 - base.recall_at_10,
                s.mrr - base.mrr,
            ];
            line += &format!(" | {:+.3} {:+.3} {:+.3}", deltas[0], deltas[1], deltas[2]);
            let (wins, losses) = wins_losses(now, other);
            wl += &format!(" | {wins}/{losses}");
            if !is_entity_kind(set, kind) {
                for d in deltas {
                    note_drop(
                        &mut worst_drop,
                        w,
                        d,
                        format!("{mode}/{strategy}/{set}/{kind}"),
                    );
                }
            }
        }
        println!("{line}{wl}");
    }

    println!("\nper-strategy totals over the base golden set (all kinds):");
    for ((mode, strategy, weight), now) in &strategy_totals {
        let s = mean(now);
        let base =
            mean(&strategy_totals[&(mode.clone(), strategy.clone(), WEIGHTS[0].to_string())]);
        println!("  {mode}/{strategy} w={weight}: {}", fmt(&s));
        if weight == WEIGHTS[0] {
            continue;
        }
        for d in [
            s.recall_at_5 - base.recall_at_5,
            s.recall_at_10 - base.recall_at_10,
            s.mrr - base.mrr,
        ] {
            note_drop(
                &mut worst_drop,
                weight,
                d,
                format!("{mode}/{strategy}/total"),
            );
        }
    }

    println!("\nentity queries pooled (base entity + all synthetic), all strategies:");
    let mut by_weight: BTreeMap<String, Vec<Row>> = BTreeMap::new();
    for ((mode, weight), now) in &entity_pool {
        let s = mean(now);
        let base = mean(&entity_pool[&(mode.clone(), WEIGHTS[0].to_string())]);
        println!(
            "  {mode} w={weight} n={}: {}  (r@10 {:+.3}, mrr {:+.3})",
            now.len(),
            fmt(&s),
            s.recall_at_10 - base.recall_at_10,
            s.mrr - base.mrr
        );
        by_weight
            .entry(weight.clone())
            .or_default()
            .extend(now.iter().copied());
    }

    println!("\nlatency per search (us), all modes and sets:");
    let mut zero = Vec::new();
    for (weight, samples) in latency_us {
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        let (p50, p95) = (percentile(&sorted, 0.5), percentile(&sorted, 0.95));
        if weight == WEIGHTS[0] {
            zero = vec![p50, p95];
        }
        println!("  w={weight}: p50 {p50}  p95 {p95}  (n={})", sorted.len());
    }
    println!(
        "  (added = each line minus w=0: p50 {}, p95 {})",
        zero[0], zero[1]
    );

    println!("\nD91 on the fixture, per weight (entity queries pooled over both modes):");
    let zero_pool = mean(&by_weight[WEIGHTS[0]]);
    let mut keep = false;
    for w in &WEIGHTS[1..] {
        let s = mean(&by_weight[*w]);
        let lift = (s.recall_at_10 - zero_pool.recall_at_10).max(s.mrr - zero_pool.mrr);
        let (drop, at) = worst_drop.get(*w).cloned().unwrap_or((0.0, "none".into()));
        let ok = lift >= KEEP_LIFT - 1e-9 && drop >= -MAX_DROP - 1e-9;
        keep |= ok;
        println!("  w={w}: entity lift {lift:+.4}, worst other drop {drop:+.4} ({at}) -> {ok}");
    }
    println!("verdict: {}", if keep { "KEEP" } else { "DROP" });
}

/// Record `delta` at `weight` if it is the worst drop seen there.
fn note_drop(worst: &mut BTreeMap<String, (f64, String)>, weight: &str, delta: f64, at: String) {
    let entry = worst
        .entry(weight.to_string())
        .or_insert((0.0, "none".into()));
    if delta < entry.0 {
        *entry = (delta, at);
    }
}
