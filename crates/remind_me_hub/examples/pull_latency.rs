//! Pull latency under push load on the engine store: the measurement
//! ADR-0021 records for each change to the push path.
//!
//! ```sh
//! cargo run --release -p remind_me_hub \
//!     --example pull_latency -- [PRELOAD] [SECONDS] [PUSHERS] [PULLERS] [BATCH]
//! ```
//!
//! A fresh on-disk hub is preloaded with `PRELOAD` memories
//! (default 20000). Then `PUSHERS` threads (default 4) apply `BATCH`
//! memories at a time (default 1), as one `/sync/push` of that many
//! records does, while `PULLERS` threads (default 2)
//! page `since_seq` pulls of 500 from random cursors, for `SECONDS`
//! (default 10). It reports push throughput and pull latency percentiles.
//!
//! Numbers depend on the disk's `fsync` more than anything: every chunk of
//! a push waits for one.

use remind_me_hub::record::{self, Record};
use remind_me_hub::store::multimodal::MultimodalHubStore;
use remind_me_hub::store::{HubStore, PullCursor, PullQuery, MAX_PULL_LIMIT};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Settings {
    preload: u64,
    duration: Duration,
    pushers: u64,
    pullers: u64,
    batch: u64,
}

fn settings() -> Settings {
    let arg = |i: usize, default: u64| {
        std::env::args()
            .nth(i)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    Settings {
        preload: arg(1, 20_000),
        duration: Duration::from_secs(arg(2, 10)),
        pushers: arg(3, 4),
        pullers: arg(4, 2),
        batch: arg(5, 1).max(1),
    }
}

fn memory(id: &str, n: u64) -> Record {
    record::parse(&json!({
        "id": id,
        "content": format!("memory {n}: {}", "lorem ipsum ".repeat(20)),
        "tags": ["bench", format!("t{}", n % 17)],
        "category": format!("c{}", n % 7),
        "created_at": "2026-08-01T00:00:00Z",
        "updated_at": format!("2026-08-01T{:02}:{:02}:{:02}Z", (n / 3600) % 24, (n / 60) % 60, n % 60),
    }))
    .expect("a valid memory")
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[rank]
}

fn run(name: &str, store: Arc<dyn HubStore>, s: &Settings) {
    let started = Instant::now();
    for n in 0..s.preload {
        store
            .apply_record(&memory(&format!("pre-{n}"), n), Some("preload"))
            .expect("preload");
    }
    let preload_time = started.elapsed();

    let stop = Arc::new(AtomicBool::new(false));
    let pushed = Arc::new(AtomicU64::new(0));
    let mut pushers = Vec::new();
    for t in 0..s.pushers {
        let (store, stop, pushed) = (Arc::clone(&store), Arc::clone(&stop), Arc::clone(&pushed));
        let batch = s.batch;
        pushers.push(std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let records: Vec<Record> = (n..n + batch)
                    .map(|k| memory(&format!("push-{t}-{k}"), k))
                    .collect();
                for result in store.apply_records(&records, Some("pusher")) {
                    result.expect("push");
                }
                pushed.fetch_add(batch, Ordering::Relaxed);
                n += batch;
            }
        }));
    }
    let mut pullers = Vec::new();
    for p in 0..s.pullers {
        let (store, stop) = (Arc::clone(&store), Arc::clone(&stop));
        let preload = s.preload.max(1);
        pullers.push(std::thread::spawn(move || {
            let mut latencies = Vec::new();
            // A cheap deterministic spread of cursors over the preload.
            let mut cursor = p * 7919;
            while !stop.load(Ordering::Relaxed) {
                cursor = (cursor * 48_271 + 11) % preload;
                let started = Instant::now();
                let page = store
                    .pull_memories(&PullQuery {
                        cursor: PullCursor::Seq(cursor as i64),
                        exclude_node: None,
                        full: false,
                        limit: MAX_PULL_LIMIT,
                    })
                    .expect("pull");
                latencies.push(started.elapsed());
                assert!(!page.is_empty());
            }
            latencies
        }));
    }
    std::thread::sleep(s.duration);
    stop.store(true, Ordering::Relaxed);
    for pusher in pushers {
        pusher.join().expect("a pusher panicked");
    }
    let mut latencies: Vec<Duration> = pullers
        .into_iter()
        .flat_map(|p| p.join().expect("a puller panicked"))
        .collect();
    latencies.sort();

    let pushed = pushed.load(Ordering::Relaxed);
    let secs = s.duration.as_secs_f64();
    println!(
        "{name:>10}: preload {:>6.1}s | push {:>7.0}/s | pulls {:>6} | pull p50 {:>8.2?} p95 {:>8.2?} p99 {:>8.2?} max {:>8.2?}",
        preload_time.as_secs_f64(),
        pushed as f64 / secs,
        latencies.len(),
        percentile(&latencies, 0.50),
        percentile(&latencies, 0.95),
        percentile(&latencies, 0.99),
        latencies.last().copied().unwrap_or_default(),
    );
}

fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("remind_me_hub_bench_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create a scratch directory");
    dir
}

fn remove(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

fn main() {
    let s = settings();
    println!(
        "preload {} memories; {} pushers of {}-record batches and {} pullers (since_seq, limit {MAX_PULL_LIMIT}) for {:?}",
        s.preload, s.pushers, s.batch, s.pullers, s.duration
    );

    let dir = scratch("engine");
    let engine = MultimodalHubStore::open(&dir.join("data")).expect("open the engine store");
    run("engine", Arc::new(engine), &s);
    remove(&dir);
}
