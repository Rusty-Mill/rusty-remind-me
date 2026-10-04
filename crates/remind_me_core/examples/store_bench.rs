//! A node workload on the engine (ADR-0023).
//!
//! Run in release, in its own process, so peak memory is its own:
//!
//! ```text
//! cargo run --release -p remind_me_core --example store_bench -- engine 15000 /tmp/bench
//! cargo run --release -p remind_me_core --example store_bench -- open   15000 /tmp/bench
//! ```
//!
//! `engine` fills a fresh store with `n` synthetic memories and times each
//! operation a node serves. `open` fills a store and times reopening it,
//! which rebuilds the derived indexes. Each run prints one JSON line. The
//! SQLite workload and the copy timing went with the SQLite store
//! (ADR-0025); `tests/fixtures/legacy_store` keeps a copy source.
//!
//! The data is synthetic and deterministic: a Zipf-skewed vocabulary, so
//! full-text posting lists look like prose, and a mix of categories, tags
//! and content lengths shaped like a real node. No embedder is configured,
//! so search is keyword-only and nothing leaves the machine.

use remind_me_core::db::queries;
use remind_me_core::models::{
    MemoryAddInput, MemoryListInput, MemorySearchInput, MemoryUpdateInput,
};
use remind_me_core::Database;
use serde_json::{json, Map, Value};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const VOCABULARY: usize = 6_000;
const SAMPLES: usize = 500;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [mode, n, dir] = args.as_slice() else {
        return Err("usage: store_bench <engine|open> <n> <dir>".into());
    };
    let n: usize = n.parse()?;
    let dir = PathBuf::from(dir).join(format!("{mode}-{n}"));
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    let file = dir.join("memory.db");

    let mut report = Map::new();
    report.insert("backend".into(), json!(mode));
    report.insert("n".into(), json!(n));
    match mode.as_str() {
        "engine" => workload(&file, n, &mut report)?,
        "open" => reopen(&file, n, &mut report)?,
        other => return Err(format!("unknown mode {other:?}").into()),
    }
    report.insert("disk_mb".into(), json!(megabytes(dir_size(&dir)?)));
    report.insert("peak_rss_mb".into(), json!(peak_rss_mb()));
    println!("{}", Value::Object(report));
    Ok(())
}

fn open(file: &Path) -> Result<Database> {
    Ok(Database::open(file)?)
}

/// Fill a store with `n` memories, reopen it, then time each operation.
fn workload(file: &Path, n: usize, report: &mut Map<String, Value>) -> Result<()> {
    let corpus = Corpus::new();
    let mut rng = Rng(0x5eed);
    let mut ids = Vec::with_capacity(n);
    {
        let db = open(file)?;
        let store = db.store();
        let mut adds = Vec::with_capacity(n);
        let started = Instant::now();
        for _ in 0..n {
            let input = corpus.memory(&mut rng);
            let t = Instant::now();
            ids.push(queries::add_memory(&store, input)?.id);
            adds.push(t.elapsed());
        }
        report.insert("fill_s".into(), json!(started.elapsed().as_secs_f64()));
        report.insert("add".into(), summary(adds));
    }

    let started = Instant::now();
    let db = open(file)?;
    report.insert("reopen_ms".into(), json!(millis(started.elapsed())));
    let store = db.store();

    let mut times = Vec::new();
    for _ in 0..SAMPLES {
        let id = &ids[rng.below(ids.len())];
        let t = Instant::now();
        queries::get_memory_by_id(&store, id)?;
        times.push(t.elapsed());
    }
    report.insert("get".into(), summary(std::mem::take(&mut times)));

    for _ in 0..SAMPLES {
        let input = MemoryListInput {
            limit: 20,
            offset: rng.below(n.min(2_000)),
            ..MemoryListInput::default()
        };
        let t = Instant::now();
        queries::list_memories(&store, &input)?;
        times.push(t.elapsed());
    }
    report.insert("list".into(), summary(std::mem::take(&mut times)));

    for _ in 0..SAMPLES {
        let input = MemoryListInput {
            limit: 20,
            category: Some("fact".into()),
            ..MemoryListInput::default()
        };
        let t = Instant::now();
        queries::list_memories(&store, &input)?;
        times.push(t.elapsed());
    }
    report.insert(
        "list_by_category".into(),
        summary(std::mem::take(&mut times)),
    );

    // Common words (long posting lists) and rarer ones, one to three terms.
    for i in 0..SAMPLES {
        let terms = 1 + i % 3;
        let query = (0..terms)
            .map(|_| corpus.search_word(&mut rng, i % 2 == 0))
            .collect::<Vec<_>>()
            .join(" ");
        let input: MemorySearchInput = serde_json::from_value(json!({ "query": query }))?;
        let t = Instant::now();
        queries::search_memories_with_embedder(&store, &input, None)?;
        times.push(t.elapsed());
    }
    report.insert("search".into(), summary(std::mem::take(&mut times)));

    for _ in 0..SAMPLES {
        let id = &ids[rng.below(ids.len())];
        let input: MemoryUpdateInput = serde_json::from_value(json!({
            "memory_id": id,
            "content": corpus.content(&mut rng),
        }))?;
        let t = Instant::now();
        queries::update_memory(&store, &input)?;
        times.push(t.elapsed());
    }
    report.insert("update".into(), summary(std::mem::take(&mut times)));

    let t = Instant::now();
    remind_me_core::stats::collect(&store)?;
    report.insert("stats_ms".into(), json!(millis(t.elapsed())));

    for id in ids.iter().rev().take(SAMPLES) {
        let t = Instant::now();
        queries::delete_memory(&store, id)?;
        times.push(t.elapsed());
    }
    report.insert("delete".into(), summary(times));
    Ok(())
}

/// Fill a store, then time reopening it: the cost of rebuilding the
/// derived indexes at open.
fn reopen(file: &Path, n: usize, report: &mut Map<String, Value>) -> Result<()> {
    let corpus = Corpus::new();
    let mut rng = Rng(0x5eed);
    {
        let db = open(file)?;
        let store = db.store();
        for _ in 0..n {
            queries::add_memory(&store, corpus.memory(&mut rng))?;
        }
    }
    let started = Instant::now();
    let _db = open(file)?;
    report.insert("reopen_ms".into(), json!(millis(started.elapsed())));
    Ok(())
}

/// Synthetic memories shaped like a node's.
struct Corpus {
    words: Vec<String>,
}

impl Corpus {
    fn new() -> Self {
        const SYLLABLES: [&str; 24] = [
            "ka", "ro", "mi", "te", "lu", "sa", "no", "vi", "da", "pe", "zo", "ri", "ba", "gu",
            "fe", "ho", "ne", "ta", "yo", "chi", "ul", "ar", "en", "os",
        ];
        let mut rng = Rng(0xc0ffee);
        let words = (0..VOCABULARY)
            .map(|_| {
                let len = 2 + rng.below(3);
                (0..len)
                    .map(|_| SYLLABLES[rng.below(SYLLABLES.len())])
                    .collect()
            })
            .collect();
        Self { words }
    }

    /// A word, skewed so that low indexes are common, as in prose.
    fn word(&self, rng: &mut Rng) -> &str {
        let u = rng.unit();
        &self.words[((u * u * u) * VOCABULARY as f64) as usize]
    }

    /// A common word (from the head of the vocabulary) or a rarer one.
    fn search_word(&self, rng: &mut Rng, common: bool) -> String {
        let index = if common {
            rng.below(50)
        } else {
            500 + rng.below(VOCABULARY - 500)
        };
        self.words[index].clone()
    }

    /// 200 to 4000 characters, most of them short.
    fn content(&self, rng: &mut Rng) -> String {
        let u = rng.unit();
        let target = 200 + (u * u * 3_800.0) as usize;
        let mut text = String::with_capacity(target + 16);
        while text.len() < target {
            text.push_str(self.word(rng));
            text.push(if rng.below(12) == 0 { '.' } else { ' ' });
        }
        text
    }

    fn memory(&self, rng: &mut Rng) -> MemoryAddInput {
        extract: true,
        attachments: vec![],
        let category = match rng.below(100) {
            0..=76 => "dialog",
            77..=84 => "fact",
            85..=90 => "chat_import",
            91..=95 => "normalized",
            96..=98 => "conversation",
            _ => "decision",
        };
        let tags = (0..rng.below(4))
            .map(|_| format!("tag{}", rng.below(40)))
            .collect();
        MemoryAddInput {
            extract: true,
            attachments: vec![],
            content: self.content(rng),
            category: category.to_string(),
            tags,
            source: "chat_import".to_string(),
            metadata: json!({}),
            subject: None,
            predicate: None,
            object: None,
            entities: vec![],
            sensitive: false,
        }
    }
}

/// xorshift64: deterministic, and no dependency for a benchmark.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Mean and percentiles of `times`, in milliseconds.
fn summary(mut times: Vec<Duration>) -> Value {
    times.sort();
    let at = |q: f64| millis(times[((times.len() - 1) as f64 * q) as usize]);
    let mean = times.iter().sum::<Duration>() / times.len() as u32;
    json!({ "mean": millis(mean), "p50": at(0.5), "p99": at(0.99), "max": at(1.0) })
}

fn millis(d: Duration) -> f64 {
    (d.as_secs_f64() * 1e5).round() / 100.0
}

fn megabytes(bytes: u64) -> f64 {
    (bytes as f64 / 1e4).round() / 100.0
}

fn dir_size(dir: &Path) -> Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        total += if path.is_dir() {
            dir_size(&path)?
        } else {
            std::fs::metadata(&path)?.len()
        };
    }
    Ok(total)
}

/// The process's peak resident memory, from Linux's `VmHWM`; null elsewhere.
fn peak_rss_mb() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kb: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some((kb / 10.24).round() / 100.0)
}
