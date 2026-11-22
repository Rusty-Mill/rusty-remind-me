//! A node workload on any store backend (ADR-0023, ADR-0035; R3, #1618).
//!
//! Run in release, one candidate per process, so peak memory is its own:
//!
//! ```text
//! cargo run --release -p remind_me_core --example store_bench -- bench engine 15000 /tmp/bench
//! cargo run --release -p remind_me_core --example store_bench -- bench sqlite 15000 /tmp/bench
//! cargo run --release -p remind_me_core --example store_bench -- open  sqlite 15000 /tmp/bench
//! ```
//!
//! `bench` fills a fresh store with `n` synthetic memories, each with a
//! vector, plus an entity graph, and times what R3 measures: writes,
//! keyword, semantic and hybrid search, the vitality pass, one- and two-hop
//! entity walks, search with four readers beside one writer, and a backup
//! restored and compared row by row. `open` fills a store and times
//! reopening it, which rebuilds whatever the backend derives at open. Each
//! run gets a new directory of its own under `<dir>` (never deleted, never
//! reused) and prints one JSON line.
//!
//! Bodies follow R3's layout (D50, D51, `bodies.rs`): every dialog and
//! wiki page body goes whole to a blob store under the run, and the
//! candidate's tables keep a 512-byte head and a link, identically for
//! every candidate. The backup is a self-contained package restored in a
//! child process with the source run moved away (D52, `package.rs`), and
//! every row and body is compared by content hash; a difference fails the
//! run. Disk is reported for the store, the candidate's temporary files
//! (`<store>.temp`, Attest's engine tables) and the blob root (D53). The
//! beside-writer search records how far the writer overlapped each reader
//! and refuses a run with too little overlap (D54). Each run also checks
//! the two G1 behaviours the port suites leave out (`g1.rs`): a dialog row
//! refuses an in-place rewrite, and readers see whole transactions while
//! the writer commits.
//!
//! The reopen is still warm, so it is not G7 evidence, and there is no D3
//! ingest workload yet. These are not scoring runs.
//!
//! The run is offline. It refuses to start when the environment configures
//! an embedder, query expansion, a reranking model or a cloud backup
//! upload, rather than letting them clear the bench vectors, call a model
//! inside a timed search or upload the bench backup. Searches
//! run without a deadline, and one that skipped a stage, or a hybrid search
//! whose semantic stage did not run, fails the run.
//!
//! The data is synthetic and deterministic: a Zipf-skewed vocabulary, so
//! full-text posting lists look like prose, a mix of categories, tags and
//! content lengths shaped like a real node, and seeded random unit vectors
//! from a stand-in embedder (`BenchEmbedder`), so no model is touched and
//! nothing leaves the machine.

use remind_me_core::db::backend::Backend;
use remind_me_core::db::{queries, Store};
use remind_me_core::embedder::{EmbedError, EmbedRole, Embedder, EmbeddingIdentity};
use remind_me_core::models::{
    EntityInput, MemoryAddInput, MemoryListInput, MemorySearchInput, MemoryUpdateInput,
};
use remind_me_core::retrieval::Deadline;
use remind_me_core::wiki;
use remind_me_core::{entity, testing, vectors, Database};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Barrier;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod bodies;
mod g1;
mod package;

use bodies::Bodies;
use package::Manifest;

/// Any error, boxed.
pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

const VOCABULARY: usize = 6_000;
const SAMPLES: usize = 500;
const DIM: usize = 1024;
const ENTITIES: usize = 500;
const READERS: usize = 4;
/// The category whose bodies are verbatim records (#1616).
const DIALOG: &str = "dialog";
/// Pages per this many memories (D3's raw pages, scaled to the run).
const MEMORIES_PER_PAGE: usize = 10;
/// The least share of each reader's window the writer must cover (D54).
const MIN_OVERLAP: f64 = 0.95;

/// Opens a backend's files in a directory.
pub type Opener = fn(&Path) -> Result<Box<dyn Backend>>;

/// Writes a candidate's own backup of the open database, whose store is in
/// the first directory, into the second, a new directory.
pub type Packager = fn(&Database, &Path, &Path) -> Result<()>;

/// A [`Packager`] for a backend whose `backup_to` is the whole store (the
/// engine, SQLite): its backup, moved into `dest`.
pub fn backup_package(db: &Database, _store: &Path, dest: &Path) -> Result<()> {
    let backup = remind_me_core::backup::create_backup(&db.store(), "bench")?;
    std::fs::rename(&backup.path, dest)?;
    Ok(())
}

/// A store backend under test.
#[derive(Clone, Copy)]
pub struct Candidate {
    /// The name `run` takes on the command line.
    pub name: &'static str,
    /// Opens the candidate's store in a directory.
    pub tables: Opener,
    /// Opens a restored package's `store/`, as a fresh store.
    pub restore: Opener,
    /// Writes the backup that goes in the package's `store/`.
    pub package: Packager,
}

impl Candidate {
    /// The database whose store lives in `dir/store`, standing beside
    /// `dir/memory.db` (where backups go, under `dir/backups`).
    fn open(self, dir: &Path) -> Result<Database> {
        let tables = (self.tables)(&dir.join("store"))?;
        Ok(Database::open_backend(tables, Some(dir.join("memory.db")))?)
    }
}

/// Where the G8 restore runs (D52).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isolation {
    /// A child process: this executable, re-run as `restore`. Scoring runs
    /// use this.
    Child,
    /// This process, for the example's own tests, where the executable is
    /// the test runner (so no binary constructs it).
    #[cfg_attr(not(test), allow(dead_code))]
    InProcess,
}

/// What a run measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Fill a store and time each workload.
    Bench,
    /// Fill a store and time reopening it.
    Open,
}

impl Mode {
    fn parse(name: &str) -> Result<Self> {
        match name {
            "bench" => Ok(Self::Bench),
            "open" => Ok(Self::Open),
            other => Err(format!("unknown mode {other:?}; one of bench, open").into()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Bench => "bench",
            Self::Open => "open",
        }
    }
}

/// Run the benchmark the command line asks for, on one of `candidates`:
/// `<bench|open> <backend> <n> <dir>`. Prints one JSON line.
pub fn run(candidates: &[Candidate]) -> Result<()> {
    let names: Vec<&str> = candidates.iter().map(|c| c.name).collect();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [mode, backend, n, dir] = args.as_slice() else {
        return Err(format!(
            "usage: store_bench <bench|open> <{}> <n> <dir>",
            names.join("|")
        )
        .into());
    };
    let Some(&candidate) = candidates.iter().find(|c| c.name == backend) else {
        return Err(format!("unknown backend {backend:?}; one of {names:?}").into());
    };
    // The G8 child (D52): `restore <backend> <package> <fresh dir>`.
    if mode == "restore" {
        let restored = restore(candidate, Path::new(n), Path::new(dir))?;
        println!("{}", serde_json::to_string(&restored)?);
        return Ok(());
    }
    let mode = Mode::parse(mode)?;
    let report = measure(
        candidate,
        mode,
        n.parse()?,
        Path::new(dir),
        Isolation::Child,
    )?;
    println!("{}", Value::Object(report));
    Ok(())
}

/// One run of `candidate` in a new directory under `root`; its report.
/// `n` and the environment are checked before the filesystem is touched.
pub fn measure(
    candidate: Candidate,
    mode: Mode,
    n: usize,
    root: &Path,
    isolation: Isolation,
) -> Result<Map<String, Value>> {
    if n == 0 {
        return Err("n must be at least 1".into());
    }
    refuse_model_config()?;
    let dir = fresh_run_dir(root, &format!("{}-{}-{n}", mode.name(), candidate.name))?;

    let mut report = Map::new();
    report.insert("mode".into(), json!(mode.name()));
    report.insert("backend".into(), json!(candidate.name));
    report.insert("n".into(), json!(n));
    report.insert("run_dir".into(), json!(dir));
    report.insert(
        "note".into(),
        json!("not a scoring run: the reopen is warm (not G7) and there is no D3 ingest yet"),
    );
    match mode {
        Mode::Bench => workload(candidate, &dir, n, &mut report, isolation)?,
        Mode::Open => reopen(candidate, &dir, n, &mut report)?,
    }
    report.insert("disk_mb".into(), disk(&dir)?);
    report.insert("peak_rss_mb".into(), json!(peak_rss_mb()));
    Ok(report)
}

/// The run's disk use, in MB (D53): the store, the candidate's temporary
/// files beside it (`store.temp`), the blob root, and their total.
fn disk(dir: &Path) -> Result<Value> {
    let size = |name: &str| -> Result<u64> {
        let path = dir.join(name);
        if path.exists() {
            dir_size(&path)
        } else {
            Ok(0)
        }
    };
    let (store, temp, blobs) = (size("store")?, size("store.temp")?, size("blobs")?);
    Ok(json!({
        "store": megabytes(store),
        "temp": megabytes(temp),
        "blobs": megabytes(blobs),
        "total": megabytes(store + temp + blobs),
    }))
}

/// A new directory under `root` that this run alone owns: `create_dir`
/// fails if the name is taken, and nothing is ever deleted.
fn fresh_run_dir(root: &Path, stem: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let dir = root.join(format!("{stem}-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Refuse to run when the environment configures a model: an embedder
/// would make `open_backend` reconcile and clear the bench vectors, and
/// query expansion or reranking would call a model inside timed searches.
/// The benchmark is offline and uses only [`BenchEmbedder`]; it never
/// falls back to keyword-only search silently.
fn refuse_model_config() -> Result<()> {
    use remind_me_core::{embedder, query_expansion, reranker};
    let set = |name: &str| std::env::var(name).is_ok_and(|v| !v.trim().is_empty());
    if set(embedder::EMBEDDING_BACKEND_ENV) {
        return Err(format!(
            "{} is set; unset it, the benchmark embeds with its own seeded vectors",
            embedder::EMBEDDING_BACKEND_ENV
        )
        .into());
    }
    if query_expansion::enabled() {
        return Err(format!(
            "{} asks for query expansion; unset it",
            query_expansion::EXPANSION_MODE_ENV
        )
        .into());
    }
    if remind_me_core::cloud_backup::enabled() {
        return Err(format!(
            "{} is set; unset it, the benchmark's backups must not be uploaded",
            remind_me_core::cloud_backup::BUCKET_ENV
        )
        .into());
    }
    // Reranking is on by default but runs only with a model configured.
    if matches!(reranker::status(), reranker::Status::Ready) {
        return Err(format!(
            "a reranking model is configured; set {}= (empty) to turn reranking off",
            reranker::BACKEND_ENV
        )
        .into());
    }

    Ok(())
}

/// Add `n` memories, each with its vector; their ids, and each add's time.
/// A dialog's body goes to `bodies` and its row keeps the head and link
/// (D50); the time includes that blob write.
fn fill(
    store: &Store<'_>,
    bodies: &Bodies,
    n: usize,
    corpus: &Corpus,
    rng: &mut Rng,
) -> Result<(Vec<String>, Vec<Duration>)> {
    let mut ids = Vec::with_capacity(n);
    let mut adds = Vec::with_capacity(n);
    for _ in 0..n {
        let mut input = corpus.memory(rng);
        let t = Instant::now();
        if input.category == DIALOG {
            let stashed = bodies.stash(&input.content)?;
            input.metadata = stashed.metadata();
            input.content = stashed.head;
        }
        let content = input.content.clone();
        let id = queries::add_memory(store, input)?.id;
        vectors::embed_and_store(store, &BenchEmbedder, &id, &content)?;
        adds.push(t.elapsed());
        ids.push(id);
    }
    Ok((ids, adds))
}

/// `count` wiki pages of 10 to 30 KB, each body in `bodies` and its row a
/// head and a link (D50); the total time.
fn pages(
    store: &Store<'_>,
    bodies: &Bodies,
    count: usize,
    corpus: &Corpus,
    rng: &mut Rng,
) -> Result<Duration> {
    let t = Instant::now();
    for i in 0..count {
        let body = corpus.page(rng);
        let stashed = bodies.stash(&body)?;
        let slug = format!("page-{i}");
        wiki::write_wiki_page(store, &slug, &slug, &stashed.head, &stashed.page_summary())?;
    }
    Ok(t.elapsed())
}

/// `ENTITIES` entities, each related to three others and linked to a few
/// memories; their ids.
fn graph(db: &Database, ids: &[String], rng: &mut Rng) -> Result<Vec<String>> {
    let store = db.store();
    let mut entities = Vec::with_capacity(ENTITIES);
    for i in 0..ENTITIES {
        let input = EntityInput {
            name: format!("entity {i}"),
            kind: Some("concept".into()),
            aliases: vec![],
        };
        let id = entity::upsert_entity(&store, &input)?.id;
        for _ in 0..4 {
            entity::link_memory_entity(&store, &ids[rng.below(ids.len())], &id)?;
        }
        entities.push(id);
    }
    for subject in &entities {
        for _ in 0..3 {
            let object = &entities[rng.below(entities.len())];
            entity::upsert_entity_relation(&store, subject, "relates to", object)?;
        }
    }
    Ok(entities)
}

/// Time `SAMPLES` runs of `f`, the `i`th given `i`.
fn timed(mut f: impl FnMut(usize) -> Result<()>) -> Result<Value> {
    let mut times = Vec::with_capacity(SAMPLES);
    for i in 0..SAMPLES {
        let t = Instant::now();
        f(i)?;
        times.push(t.elapsed());
    }
    Ok(summary(times))
}

/// A search query: common words (long posting lists) or rarer ones, one to
/// three terms.
fn query(corpus: &Corpus, rng: &mut Rng, i: usize) -> String {
    (0..1 + i % 3)
        .map(|_| corpus.search_word(rng, i.is_multiple_of(2)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Fill a store, reopen it, then time each operation.
fn workload(
    candidate: Candidate,
    dir: &Path,
    n: usize,
    report: &mut Map<String, Value>,
    isolation: Isolation,
) -> Result<()> {
    let corpus = Corpus::new();
    let mut rng = Rng(0x5eed);
    let bodies = Bodies::new(dir.join("blobs"));
    let (ids, entities) = {
        let db = candidate.open(dir)?;
        let started = Instant::now();
        let (ids, adds) = fill(&db.store(), &bodies, n, &corpus, &mut rng)?;
        report.insert("fill_s".into(), json!(started.elapsed().as_secs_f64()));
        report.insert("add".into(), summary(adds));
        let count = n.div_ceil(MEMORIES_PER_PAGE);
        let took = pages(&db.store(), &bodies, count, &corpus, &mut rng)?;
        report.insert(
            "pages".into(),
            json!({ "count": count, "ms": millis(took) }),
        );
        let entities = graph(&db, &ids, &mut rng)?;
        (ids, entities)
    };
    report.insert("disk_mb_after_fill".into(), disk(dir)?);

    let started = Instant::now();
    let db = candidate.open(dir)?;
    report.insert("reopen_warm_ms".into(), json!(millis(started.elapsed())));
    let store = db.store();

    // Dialogs are the verbatim record and refuse edits (#1616), so updates
    // and deletes take the other memories.
    let (mut editable, mut dialog) = (Vec::new(), None);
    for id in &ids {
        if testing::memory_text(&store, id, "category")?.as_deref() != Some(DIALOG) {
            editable.push(id.clone());
        } else if dialog.is_none() {
            dialog = Some(id.clone());
        }
    }
    let pick = |rng: &mut Rng| ids[rng.below(ids.len())].clone();
    let pick_editable = |rng: &mut Rng| editable[rng.below(editable.len())].clone();
    report.insert(
        "get".into(),
        timed(|_| {
            queries::get_memory_by_id(&store, &pick(&mut rng))?;
            Ok(())
        })?,
    );
    report.insert(
        "list".into(),
        timed(|_| {
            let input = MemoryListInput {
                limit: 20,
                offset: rng.below(n.min(2_000)),
                ..MemoryListInput::default()
            };
            queries::list_memories(&store, &input)?;
            Ok(())
        })?,
    );
    report.insert(
        "list_by_category".into(),
        timed(|_| {
            let input = MemoryListInput {
                limit: 20,
                category: Some("fact".into()),
                ..MemoryListInput::default()
            };
            queries::list_memories(&store, &input)?;
            Ok(())
        })?,
    );

    let queries_: Vec<String> = (0..SAMPLES).map(|i| query(&corpus, &mut rng, i)).collect();
    // Query vectors are made before any timed span, so the searches time
    // retrieval, not embedding.
    let vectors_ = Precomputed::new(&queries_);
    let search = |i: usize, vectors: Option<&Precomputed>| -> Result<()> {
        let input: MemorySearchInput = serde_json::from_value(json!({ "query": queries_[i] }))?;
        searched(&store, &input, vectors)
    };
    report.insert("search_keyword".into(), timed(|i| search(i, None))?);
    report.insert(
        "search_semantic".into(),
        timed(|i| {
            vectors::semantic_search(&store, &vectors_, &queries_[i], 10, None)?;
            Ok(())
        })?,
    );
    report.insert(
        "search_hybrid".into(),
        timed(|i| search(i, Some(&vectors_)))?,
    );

    let t = Instant::now();
    remind_me_core::vitality::build_vitality_report(&store)?;
    report.insert("vitality_pass_ms".into(), json!(millis(t.elapsed())));

    for hops in [1, 2] {
        let walk = timed(|_| {
            let seed = entities[rng.below(entities.len())].clone();
            entity::traverse_entities(&store, &[seed], hops, None, 1_000)?;
            Ok(())
        })?;
        report.insert(format!("entity_{hops}_hop"), walk);
    }

    // A small store can have no editable rows (n = 1 draws a dialog):
    // updates and deletes then report zero samples rather than failing.
    let update = if editable.is_empty() {
        skipped("no editable (non-dialog) rows")
    } else {
        timed(|_| {
            let input: MemoryUpdateInput = serde_json::from_value(json!({
                "memory_id": pick_editable(&mut rng),
                "content": corpus.content(&mut rng),
            }))?;
            queries::update_memory(&store, &input)?;
            Ok(())
        })?
    };
    report.insert("update".into(), update);

    // The same readers, inputs and searches with and without the writer,
    // so the two differ only by the writer (D54).
    let add_one = |s: &Store<'_>, rng: &mut Rng| -> Result<()> {
        fill(s, &bodies, 1, &corpus, rng)?;
        Ok(())
    };
    report.insert(
        "search_hybrid_readers_idle".into(),
        readers(&db, &store, &queries_, &vectors_, None)?,
    );
    report.insert(
        "search_hybrid_beside_writer".into(),
        readers(&db, &store, &queries_, &vectors_, Some(&add_one))?,
    );

    let t = Instant::now();
    remind_me_core::stats::collect(&store)?;
    report.insert("stats_ms".into(), json!(millis(t.elapsed())));

    // Distinct live rows, each deleted once; a delete that finds nothing is
    // an error, not a fast sample.
    let mut times = Vec::new();
    for id in editable.iter().rev().take(SAMPLES) {
        let t = Instant::now();
        let deleted = queries::delete_memory(&store, id)?;
        times.push(t.elapsed());
        if !deleted {
            return Err(format!("delete of live memory {id} did nothing").into());
        }
    }
    let delete = if times.is_empty() {
        skipped("no editable (non-dialog) rows")
    } else {
        summary(times)
    };
    report.insert("delete".into(), delete);

    // G1's checks beyond the port suites, on this candidate.
    let rewrite = match &dialog {
        Some(id) => g1::dialog_rewrite_refused(&store, id)?,
        None => skipped("no dialog rows"),
    };
    report.insert(
        "g1".into(),
        json!({
            "dialog_rewrite": rewrite,
            "whole_transactions": g1::readers_see_whole_transactions(&db, &store)?,
        }),
    );

    drop(store);
    backup_and_restore(candidate, db, &bodies, dir, report, isolation)
}

/// One search, keyword-only (`vectors` is `None`) or hybrid, that fails
/// rather than letting a degraded search into the summary. It ignores any
/// inherited `REMIND_ME_SEARCH_DEADLINE_MS` (the deadline is unbounded),
/// refuses a search that skipped or failed a stage (a failed vector scan
/// shows in `timing.failed`, D49), and for hybrid requires that the
/// semantic stage ran and got every query vector it asked for. A scan that
/// completed with no hits is fine.
fn searched(
    store: &Store<'_>,
    input: &MemorySearchInput,
    vectors: Option<&Precomputed>,
) -> Result<()> {
    let probe = vectors.map(Probe::new);
    let embedder = probe.as_ref().map(|p| p as &dyn Embedder);
    let outcome =
        queries::search_memories_deadlined(store, input, embedder, Deadline::unbounded())?;
    if outcome.timing.degraded() {
        return Err(format!(
            "the search degraded: skipped {:?}, failed {:?}",
            outcome.timing.skipped, outcome.timing.failed
        )
        .into());
    }
    let Some(probe) = probe else {
        return Ok(());
    };
    let (calls, misses) = (
        probe.calls.load(Ordering::SeqCst),
        probe.misses.load(Ordering::SeqCst),
    );
    if calls == 0 || misses > 0 {
        return Err(format!(
            "the semantic stage did not run cleanly ({calls} embeds, {misses} missing vectors)"
        )
        .into());
    }
    Ok(())
}

/// One write the loaded run's writer repeats.
type Write<'a> = &'a (dyn Fn(&Store<'_>, &mut Rng) -> Result<()> + Sync);

/// Hybrid search on `READERS` threads, all started together at a
/// barrier; with a `writer`, beside one thread that keeps repeating it.
/// The readers' times. Each thread takes a store from the database's
/// secondary source, as the node's background threads do: a store from
/// [`Database::store`] holds the process lock, so the caller's `store`
/// (which holds it) only counts rows before and after.
///
/// The writer writes once before the barrier and stops only after the
/// last reader has finished, so it covers every reader's window by
/// construction. If that first write fails, the writer still reaches the
/// barrier, flagged, and the readers return at once: the run fails with
/// the writer's error instead of hanging. Each reader reads the actual
/// memory row count at both ends of its window; the report records each
/// window, the writer's span, their overlap and those counts, and the
/// run fails as invalid when any reader saw less than [`MIN_OVERLAP`] of
/// its window covered or no row added inside it (D54).
fn readers(
    db: &Database,
    store: &Store<'_>,
    queries_: &[String],
    vectors_: &Precomputed,
    writer: Option<Write<'_>>,
) -> Result<Value> {
    let source = &db.secondary_source().ok_or("an in-memory database")?;
    let start = &Barrier::new(READERS + usize::from(writer.is_some()));
    let readers_done = &AtomicBool::new(false);
    let writer_failed = &AtomicBool::new(false);
    let written = &AtomicUsize::new(0);
    let epoch = Instant::now();
    let rows_before = testing::count(store, testing::Table::Memories)?;
    let mut windows: Vec<Window> = Vec::new();
    let mut span = None;
    std::thread::scope(|scope| -> Result<()> {
        let writing = writer.map(|write| {
            scope.spawn(move || -> std::result::Result<(f64, f64), String> {
                let store = source.store();
                let mut rng = Rng(0xbeef);
                let mut add = || -> std::result::Result<(), String> {
                    write(&store, &mut rng).map_err(|e| e.to_string())?;
                    written.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                };
                let first = epoch.elapsed().as_secs_f64();
                let started = add();
                writer_failed.store(started.is_err(), Ordering::SeqCst);
                start.wait();
                started?;
                while !readers_done.load(Ordering::SeqCst) {
                    add()?;
                }
                Ok((first, epoch.elapsed().as_secs_f64()))
            })
        });
        let threads: Vec<_> = (0..READERS)
            .map(|r| {
                scope.spawn(move || -> std::result::Result<Option<Window>, String> {
                    let store = source.store();
                    let rows = || {
                        testing::count(&store, testing::Table::Memories).map_err(|e| e.to_string())
                    };
                    let mut times = Vec::with_capacity(SAMPLES / READERS);
                    start.wait();
                    if writer_failed.load(Ordering::SeqCst) {
                        return Ok(None);
                    }
                    let rows_at_begin = rows()?;
                    let begin = epoch.elapsed().as_secs_f64();
                    for i in (r..SAMPLES).step_by(READERS) {
                        let input: MemorySearchInput =
                            serde_json::from_value(json!({ "query": queries_[i] }))
                                .map_err(|e| e.to_string())?;
                        let t = Instant::now();
                        searched(&store, &input, Some(vectors_)).map_err(|e| e.to_string())?;
                        times.push(t.elapsed());
                    }
                    let end = epoch.elapsed().as_secs_f64();
                    Ok(Some(Window {
                        begin,
                        end,
                        rows_at_begin,
                        rows_at_end: rows()?,
                        times,
                    }))
                })
            })
            .collect();
        let mut joined = Ok(());
        for thread in threads {
            match thread.join().map_err(|_| "a reader panicked".to_string()) {
                Ok(Ok(w)) => windows.extend(w),
                Ok(Err(e)) | Err(e) => joined = Err(e),
            }
        }
        readers_done.store(true, Ordering::SeqCst);
        if let Some(writing) = writing {
            span = Some(writing.join().map_err(|_| "the writer panicked")??);
        }
        Ok(joined?)
    })?;
    let rows_after = testing::count(store, testing::Table::Memories)?;
    let times = windows
        .iter()
        .flat_map(|w| w.times.iter().copied())
        .collect();
    let mut value = summary(times);
    value["memory_rows"] = json!({ "before": rows_before, "after": rows_after });
    let Some((first, last)) = span else {
        return Ok(value);
    };
    let overlaps: Vec<f64> = windows.iter().map(|w| w.overlap(first, last)).collect();
    value["writer_adds"] = json!(written.load(Ordering::SeqCst));
    value["writer_s"] = json!({ "first": first, "last": last });
    value["readers"] = windows
        .iter()
        .zip(&overlaps)
        .map(|(w, o)| {
            json!({
                "begin_s": w.begin,
                "end_s": w.end,
                "overlap": o,
                "rows_at_begin": w.rows_at_begin,
                "rows_at_end": w.rows_at_end,
            })
        })
        .collect();
    let starved = windows.iter().any(|w| w.rows_at_end <= w.rows_at_begin);
    if overlaps.iter().any(|o| *o < MIN_OVERLAP) || starved {
        return Err(format!(
            "invalid run, re-run it: reader overlaps {overlaps:?} (need {MIN_OVERLAP}), \
             a reader saw no row added: {starved}"
        )
        .into());
    }
    Ok(value)
}

/// One reader's span in seconds since the run's epoch, the memory row
/// count it read at each end, and its samples.
struct Window {
    begin: f64,
    end: f64,
    rows_at_begin: i64,
    rows_at_end: i64,
    times: Vec<Duration>,
}

impl Window {
    /// The share of this window the writer's span `[first, last]` covers.
    fn overlap(&self, first: f64, last: f64) -> f64 {
        let length = self.end - self.begin;
        if length <= 0.0 {
            return 1.0;
        }
        ((self.end.min(last) - self.begin.max(first)).max(0.0) / length).min(1.0)
    }
}

/// Write the G8 package (D52), move the run away, restore the package in
/// isolation into a fresh directory, and compare what it holds with the
/// source's manifest, row by row and body by body.
fn backup_and_restore(
    candidate: Candidate,
    db: Database,
    bodies: &Bodies,
    dir: &Path,
    report: &mut Map<String, Value>,
    isolation: Isolation,
) -> Result<()> {
    let source = Manifest::read(&db.store(), bodies.blobs())?;
    let manifest = serde_json::to_vec(&source)?;
    let package = dir.with_extension("package");
    std::fs::create_dir(&package)
        .map_err(|e| format!("cannot create {}: {e}", package.display()))?;
    let t = Instant::now();
    (candidate.package)(&db, &dir.join("store"), &package.join("store"))?;
    package::copy_dir(bodies.blobs().root(), &package.join("blobs"), &[])?;
    std::fs::write(package.join("manifest.json"), &manifest)?;
    report.insert("backup_ms".into(), json!(millis(t.elapsed())));
    std::fs::write(dir.join("manifest.json"), &manifest)?;
    drop(db);

    let fresh = dir.with_extension("restored");
    let away = dir.with_extension("away");
    std::fs::rename(dir, &away)?;
    let restored = match isolation {
        Isolation::Child => restore_in_child(candidate, &package, &fresh),
        Isolation::InProcess => restore(candidate, &package, &fresh),
    };
    std::fs::rename(&away, dir)?;
    let restored = restored?;

    let differences = source.differences(&restored.manifest);
    report.insert(
        "restore".into(),
        json!({
            "items": source.size(),
            "matched": differences.is_empty(),
            "open_ms": restored.open_ms,
            "isolation": format!("{isolation:?}"),
        }),
    );
    if !differences.is_empty() {
        return Err(format!("the restored package differs: {differences:?}").into());
    }
    Ok(())
}

/// What a restore read back, as the child prints it.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Restored {
    manifest: Manifest,
    open_ms: f64,
}

/// Restore `package` into `fresh`, a new directory: copy its store and
/// blobs there, open the store (Attest rebuilds from its log here, and the
/// time counts), and read its manifest back.
pub fn restore(candidate: Candidate, package: &Path, fresh: &Path) -> Result<Restored> {
    refuse_model_config()?;
    std::fs::create_dir(fresh).map_err(|e| format!("cannot create {}: {e}", fresh.display()))?;
    package::copy_dir(&package.join("store"), &fresh.join("store"), &[])?;
    package::copy_dir(&package.join("blobs"), &fresh.join("blobs"), &[])?;
    let t = Instant::now();
    let db = Database::open_backend((candidate.restore)(&fresh.join("store"))?, None)?;
    let open_ms = millis(t.elapsed());
    let blobs = remind_me_core::blobs::BlobStore::new(fresh.join("blobs"));
    let manifest = Manifest::read(&db.store(), &blobs)?;
    Ok(Restored { manifest, open_ms })
}

/// [`restore`] in a child process: this executable, run as
/// `restore <backend> <package> <fresh>`. Its last stdout line is the
/// result.
fn restore_in_child(candidate: Candidate, package: &Path, fresh: &Path) -> Result<Restored> {
    let output = std::process::Command::new(std::env::current_exe()?)
        .arg("restore")
        .arg(candidate.name)
        .arg(package)
        .arg(fresh)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "the restore child failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    let stdout = String::from_utf8(output.stdout)?;
    let line = stdout
        .lines()
        .last()
        .ok_or("the restore child printed nothing")?;
    Ok(serde_json::from_str(line)?)
}

/// Fill a store, then time reopening it: the cost of rebuilding what the
/// backend derives at open.
fn reopen(
    candidate: Candidate,
    dir: &Path,
    n: usize,
    report: &mut Map<String, Value>,
) -> Result<()> {
    let corpus = Corpus::new();
    let mut rng = Rng(0x5eed);
    let bodies = Bodies::new(dir.join("blobs"));
    fill(&candidate.open(dir)?.store(), &bodies, n, &corpus, &mut rng)?;
    let started = Instant::now();
    let _db = candidate.open(dir)?;
    report.insert("reopen_warm_ms".into(), json!(millis(started.elapsed())));
    Ok(())
}

/// Seeded random unit vectors, one per text: a stand-in embedder, so
/// semantic search runs without a model (the spec's D2 vectors).
struct BenchEmbedder;

impl Embedder for BenchEmbedder {
    fn embed(
        &self,
        texts: &[String],
        _role: EmbedRole,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|t| unit_vector(t)).collect())
    }

    fn dim(&self) -> usize {
        DIM
    }

    fn identity(&self) -> EmbeddingIdentity {
        EmbeddingIdentity {
            backend: "bench".into(),
            model: "seeded-random".into(),
            dim: DIM,
        }
    }
}

/// A unit vector seeded by `text` (FNV-1a), so the same text always gets
/// the same vector.
fn unit_vector(text: &str) -> Vec<f32> {
    let seed = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    });
    let mut rng = Rng(seed | 1);
    let v: Vec<f32> = (0..DIM).map(|_| rng.unit() as f32 - 0.5).collect();
    let norm = v
        .iter()
        .map(|x| x * x)
        .sum::<f32>()
        .sqrt()
        .max(f32::EPSILON);
    v.into_iter().map(|x| x / norm).collect()
}

/// The query vectors, made up front: an embedder that only looks them up,
/// so timed searches do no embedding work. A text it was not given is an
/// error, never embedded on the spot.
struct Precomputed(HashMap<String, Vec<f32>>);

impl Precomputed {
    fn new(texts: &[String]) -> Self {
        Self(texts.iter().map(|t| (t.clone(), unit_vector(t))).collect())
    }
}

impl Embedder for Precomputed {
    fn embed(
        &self,
        texts: &[String],
        _role: EmbedRole,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        texts
            .iter()
            .map(|t| {
                self.0
                    .get(t)
                    .cloned()
                    .ok_or_else(|| EmbedError(format!("no precomputed vector for {t:?}")))
            })
            .collect()
    }

    fn dim(&self) -> usize {
        DIM
    }

    fn identity(&self) -> EmbeddingIdentity {
        BenchEmbedder.identity()
    }
}

/// One search's view of [`Precomputed`]: counts the lookups it served and
/// the ones it could not, so the caller can tell the semantic stage ran.
struct Probe<'a> {
    vectors: &'a Precomputed,
    calls: AtomicUsize,
    misses: AtomicUsize,
}

impl<'a> Probe<'a> {
    fn new(vectors: &'a Precomputed) -> Self {
        Self {
            vectors,
            calls: AtomicUsize::new(0),
            misses: AtomicUsize::new(0),
        }
    }
}

impl Embedder for Probe<'_> {
    fn embed(
        &self,
        texts: &[String],
        role: EmbedRole,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        let embedded = self.vectors.embed(texts, role);
        let counter = if embedded.is_ok() {
            &self.calls
        } else {
            &self.misses
        };
        counter.fetch_add(1, Ordering::SeqCst);
        embedded
    }

    fn dim(&self) -> usize {
        self.vectors.dim()
    }

    fn identity(&self) -> EmbeddingIdentity {
        self.vectors.identity()
    }
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

    /// A page of 10 to 30 KB, D3's raw page sizes.
    fn page(&self, rng: &mut Rng) -> String {
        let target = 10_000 + rng.below(20_001);
        let mut text = String::with_capacity(target + 16);
        while text.len() < target {
            text.push_str(self.word(rng));
            text.push(if rng.below(12) == 0 { '\n' } else { ' ' });
        }
        text
    }

    fn memory(&self, rng: &mut Rng) -> MemoryAddInput {
        let category = match rng.below(100) {
            0..=76 => DIALOG,
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
            ..Default::default()
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

/// A metric with no samples, and why.
fn skipped(why: &str) -> Value {
    json!({ "count": 0, "skipped": why })
}

/// Mean and percentiles of `times`, in milliseconds. `times` must not be
/// empty; a metric with nothing to time reports [`skipped`] instead.
fn summary(mut times: Vec<Duration>) -> Value {
    times.sort();
    let at = |q: f64| millis(times[((times.len() - 1) as f64 * q) as usize]);
    let mean = times.iter().sum::<Duration>() / times.len() as u32;
    json!({ "count": times.len(), "mean": millis(mean), "p50": at(0.5), "p95": at(0.95), "p99": at(0.99), "max": at(1.0) })
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

#[cfg(test)]
mod tests {
    use super::*;
    use remind_me_core::db::sqlite::SqliteTables;
    use std::sync::mpsc;

    /// A writer whose first write fails ends the run with its error, in
    /// bounded time, instead of leaving the readers at the barrier.
    #[test]
    fn a_failed_first_write_fails_the_run_instead_of_hanging() {
        let dir = std::env::temp_dir().join(format!("store_bench_hang_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (sent, received) = mpsc::channel();
        let run_dir = dir.clone();
        std::thread::spawn(move || {
            let tables = SqliteTables::open(&run_dir.join("store")).unwrap();
            let db =
                Database::open_backend(Box::new(tables), Some(run_dir.join("memory.db"))).unwrap();
            let store = db.store();
            let fail = |_: &Store<'_>, _: &mut Rng| -> Result<()> { Err("refused".into()) };
            let outcome = readers(&db, &store, &[], &Precomputed::new(&[]), Some(&fail));
            let _ = sent.send(outcome.map_err(|e| e.to_string()));
        });
        let outcome = received
            .recv_timeout(Duration::from_secs(60))
            .expect("the run returned instead of hanging");
        assert!(outcome.unwrap_err().contains("refused"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
