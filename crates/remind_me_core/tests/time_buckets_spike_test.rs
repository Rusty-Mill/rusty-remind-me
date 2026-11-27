//! Card #568's measurement, fixture half: does recency bucketing
//! (`REMIND_ME_TIME_BUCKETS=recency`) help? Keep rule pre-registered by Nano
//! (pack D136): recall@10 or MRR on temporal queries rises 2+ points and no
//! other kind or strategy drops more than 0.02, plus a status-question set
//! (10+ "latest on X", clock relative to the run) where a newest-week
//! relevant hit must reach the top 10.
//!
//! The spike measured a `window` mode too; it lowered temporal MRR and was
//! dropped (`docs/research/568-time-buckets-spike.md`). Recency failed the
//! lift half but fixed the status set, so it ships opt-in (Nano, 2026-11-27).
//! This runner stays so the default can be revisited on new numbers.
//!
//! Recency only acts on a search without a window, so the `+dates` modes
//! (date inference makes the window) move only where no date is inferred;
//! those move with the weekday, as in the #59 test.
//!
//! Two stores, so the gated baseline is never touched:
//!
//! - **base**: the #59 fixture and its golden set.
//! - **synthetic**: the #59 fixture plus a generated `status` world: 12
//!   subjects, ten heavily read rows 8-17 days old that match the query
//!   well, one fresh row that is the answer.
//!
//! Ignored: it prints numbers and gates nothing. For latency use --release:
//!
//! ```text
//! cargo test -p remind_me_core --release --test time_buckets_spike_test \
//!     -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;
use std::time::Instant;

use chrono::Utc;
use remind_me_core::db::memories::Memories;
use remind_me_core::db::Store;
use remind_me_core::embedder::Embedder;
use remind_me_core::time_buckets::Mode;
use remind_me_core::Database;

#[path = "retrieval_eval/shared.rs"]
mod shared;
use shared::*;

const FLAGS: [(&str, Mode); 2] = [("off", Mode::Off), ("recency", Mode::Recency)];
const TEMPORAL_KINDS: [&str; 3] = ["temporal", "dated", "occurred"];
/// D136 (as D92): the lift that keeps a mode, and the drop that sinks it.
const KEEP_LIFT: f64 = 0.02;
const MAX_DROP: f64 = 0.02;
/// Searches per query, for the latency percentiles.
const REPEATS: usize = 3;

const STATUS_SUBJECTS: [&str; 12] = [
    "Quillmark",
    "Brindlewood",
    "Tessarine",
    "Ombrelux",
    "Vantorre",
    "Kestrova",
    "Halcyra",
    "Morrowfen",
    "Pellucid",
    "Zephyrine",
    "Corvane",
    "Lirabel",
];
fn row(id: String, content: String, age_days: i64) -> FixtureMemory {
    serde_json::from_value(serde_json::json!({
        "id": id, "content": content, "category": "general",
        "memory_type": "fact", "age_days": age_days,
    }))
    .unwrap()
}

fn golden(query: String, relevant: Vec<String>, kind: &str) -> GoldenQuery {
    serde_json::from_value(serde_json::json!({
        "query": query, "relevant": relevant, "kind": kind,
    }))
    .unwrap()
}

/// The status world: rows, the ids to mark heavily read, and its queries.
fn status_world() -> (Vec<FixtureMemory>, Vec<String>, Vec<GoldenQuery>) {
    let (mut rows, mut hot, mut queries) = (Vec::new(), Vec::new(), Vec::new());
    for (s, subject) in STATUS_SUBJECTS.iter().enumerate() {
        for k in 0..10 {
            let id = format!("s{s}-old{k}");
            rows.push(row(
                id.clone(),
                format!("Latest on {subject}: status review {k}, the {subject} plan is still pending sign-off."),
                8 + k,
            ));
            hot.push(id);
        }
        let fresh = format!("s{s}-fresh");
        rows.push(row(
            fresh.clone(),
            format!("{subject} front end merged and deployed today."),
            1,
        ));
        queries.push(golden(
            format!("latest on {subject}"),
            vec![fresh],
            "status",
        ));
    }
    (rows, hot, queries)
}

/// Mark `ids` as read thirty times, today: what crowded the newest week out
/// on 2026-11-18 was access boosting.
fn heat(store: &Store<'_>, ids: &[String]) {
    let now = Utc::now().to_rfc3339();
    let repo = Memories::new(store);
    for id in ids {
        repo.record_access(id, &now, 30, 3.0, "active").unwrap();
    }
}

/// One query's scores.
#[derive(Debug, Clone, Copy)]
struct Row {
    r10: f64,
    rr: f64,
    /// A relevant hit in the top 10 (the status check).
    hit10: bool,
}

/// `(mode, flag, strategy, kind)` -> one row per query.
type Rows = BTreeMap<(String, String, String, String), Vec<Row>>;

fn mean(rows: &[Row]) -> (f64, f64, f64) {
    let n = rows.len() as f64;
    (
        rows.iter().map(|r| r.r10).sum::<f64>() / n,
        rows.iter().map(|r| r.rr).sum::<f64>() / n,
        rows.iter().filter(|r| r.hit10).count() as f64 / n,
    )
}

#[allow(clippy::too_many_arguments)]
fn run(
    store: &Store<'_>,
    golden: &[GoldenQuery],
    embedder: Option<&dyn Embedder>,
    mode: &str,
    dates: bool,
    rows: &mut Rows,
    latency_us: &mut BTreeMap<String, Vec<u128>>,
) {
    for (flag, flag_mode) in FLAGS {
        for (strategy_name, strategy) in STRATEGIES {
            for g in golden {
                let mut ids = Vec::new();
                for _ in 0..REPEATS {
                    let started = Instant::now();
                    ids = ranked_outcome_with(store, g, strategy, embedder, dates, flag_mode).0;
                    latency_us
                        .entry(flag.to_string())
                        .or_default()
                        .push(started.elapsed().as_micros());
                }
                let row = Row {
                    r10: recall_at(&ids, &g.relevant, DEPTH),
                    rr: reciprocal_rank(&ids, &g.relevant),
                    hit10: recall_at(&ids, &g.relevant, DEPTH) > 0.0,
                };
                let kind = if TEMPORAL_KINDS.contains(&g.kind.as_str()) {
                    "temporal-pool"
                } else {
                    g.kind.as_str()
                };
                let key = (
                    mode.to_string(),
                    flag.to_string(),
                    strategy_name.to_string(),
                    kind.to_string(),
                );
                rows.entry(key).or_default().push(row);
            }
        }
    }
}

#[test]
#[ignore = "measurement for card #568: prints numbers, gates nothing"]
fn time_buckets_spike() {
    let (status_rows, hot, status_queries) = status_world();
    let base_golden: Vec<GoldenQuery> = read_jsonl("queries.jsonl");
    let mut rows = Rows::new();
    let mut latency_us = BTreeMap::new();
    let embedders: [(&str, Option<&dyn Embedder>); 2] =
        [("keyword", None), ("hashed", Some(&HashedEmbedder))];
    for (name, embedder) in embedders {
        let base_db = Database::open_in_memory().unwrap();
        let base = base_db.store();
        load(&base, &read_jsonl("memories.jsonl"), embedder);
        let synth_db = Database::open_in_memory().unwrap();
        let synth = synth_db.store();
        let mut all: Vec<FixtureMemory> = read_jsonl("memories.jsonl");
        all.extend(status_rows.iter().map(clone_row));
        load(&synth, &all, embedder);
        heat(&synth, &hot);
        for dates in [false, true] {
            let mode = if dates {
                format!("{name}+dates")
            } else {
                name.to_string()
            };
            run(
                &base,
                &base_golden,
                embedder,
                &mode,
                dates,
                &mut rows,
                &mut latency_us,
            );
            run(
                &synth,
                &status_queries,
                embedder,
                &mode,
                dates,
                &mut rows,
                &mut latency_us,
            );
        }
    }
    report(&rows, &latency_us);
}

fn clone_row(m: &FixtureMemory) -> FixtureMemory {
    row(m.id.clone(), m.content.clone(), m.age_days)
}

fn report(rows: &Rows, latency_us: &BTreeMap<String, Vec<u128>>) {
    println!("\n== per cell: mode / flag / strategy / kind: recall@10  MRR  hit@10 (n)");
    for ((mode, flag, strategy, kind), r) in rows {
        let (r10, mrr, hit) = mean(r);
        println!(
            "{mode:15} {flag:8} {strategy:17} {kind:14} {r10:.4} {mrr:.4} {hit:.4} ({})",
            r.len()
        );
    }

    println!("\n== deltas against off (flag - off), worst over strategies");
    let mut verdict_drop: BTreeMap<String, f64> = BTreeMap::new();
    let mut verdict_lift: BTreeMap<String, f64> = BTreeMap::new();
    for ((mode, flag, strategy, kind), r) in rows {
        if flag == "off" {
            continue;
        }
        let off_key = (
            mode.clone(),
            "off".to_string(),
            strategy.clone(),
            kind.clone(),
        );
        let (r10, mrr, _) = mean(r);
        let (o10, omrr, _) = mean(&rows[&off_key]);
        let (d10, dmrr) = (r10 - o10, mrr - omrr);
        let cell = format!("{flag}@{mode}");
        if kind == "temporal-pool" {
            let lift = verdict_lift.entry(cell.clone()).or_insert(f64::INFINITY);
            *lift = lift.min(d10.max(dmrr));
        }
        if kind != "status" {
            // Any gated kind, the temporal pool included, counts as a drop.
            let drop = verdict_drop.entry(cell.clone()).or_insert(0.0);
            *drop = drop.min(d10.min(dmrr));
        }
        if d10.abs() > 1e-9 || dmrr.abs() > 1e-9 {
            println!("{cell:24} {strategy:17} {kind:14} recall@10 {d10:+.4}  MRR {dmrr:+.4}");
        }
    }
    println!("\n== D136 fixture half, per flag and mode");
    for (cell, lift) in &verdict_lift {
        let drop = verdict_drop.get(cell).copied().unwrap_or(0.0);
        let keep = *lift >= KEEP_LIFT && drop >= -MAX_DROP;
        println!(
            "{cell:24} temporal lift (min over strategies of the better metric) {lift:+.4}, worst drop on any gated kind {drop:+.4}: {}",
            if keep { "passes" } else { "fails" }
        );
    }
    println!("\n== status check (recency): hit@10 per mode, pooled over strategies");
    for flag in ["off", "recency"] {
        for mode in ["keyword", "hashed"] {
            let all: Vec<Row> = rows
                .iter()
                .filter(|((m, f, _, k), _)| m == mode && f == flag && k == "status")
                .flat_map(|(_, r)| r.iter().copied())
                .collect();
            let (_, mrr, hit) = mean(&all);
            println!(
                "{flag:8} {mode:8} hit@10 {hit:.4} MRR {mrr:.4} ({} searches)",
                all.len()
            );
        }
    }
    println!("\n== latency per search, us (p50 / p95)");
    for (flag, samples) in latency_us {
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        let pick = |p: f64| sorted[((sorted.len() - 1) as f64 * p).round() as usize];
        println!(
            "{flag:8} {} / {} ({} searches)",
            pick(0.5),
            pick(0.95),
            sorted.len()
        );
    }
}
