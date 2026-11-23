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
//! The dataset is `n` synthetic memories, `d1:<backup>` (the real store,
//! from a copy of a `manual-*.engine` backup, loaded through the ports;
//! `d1.rs`) or `d2:<backup>` (D1 grown to 75,000 with synthetic memories).
//!
//! ```text
//! cargo run --release -p remind_me_core --example store_bench -- bench sqlite d1:/data/manual-x.engine /tmp/bench
//! ```
//!
//! `bench` fills a fresh store with the dataset (synthetic data also gets
//! an entity graph and pages) and times what R3 measures: writes,
//! keyword, semantic and hybrid search, the vitality pass, one- and two-hop
//! entity walks, search with four readers beside one writer, and a backup
//! restored and compared row by row, then the cold open (G7): a child
//! process opens the store and answers one query, timed from its spawn,
//! with the page cache dropped first when `R3_DROP_CACHES=1`, and reports
//! its resident memory once idle (the G5 baseline). `open` fills a
//! synthetic store and times
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
//! beside-writer search times each reader's span and each write's
//! completion on one clock, and refuses a run where the writer covered
//! too little of a span or completed no write inside one (D54). Each run also checks
//! the two G1 behaviours the port suites leave out (`g1.rs`): a dialog row
//! refuses an in-place rewrite, and readers see whole transactions while
//! the writer commits.
//!
//! There is no D3 ingest, crash or relevance workload yet (D55), so these
//! are not scoring runs.
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
use remind_me_core::db::vectors::Vectors;
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
mod cold;
mod d1;
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

/// The data a run loads (spec § Datasets).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dataset {
    /// `n` synthetic memories with an entity graph and pages: `<n>`.
    Synthetic(usize),
    /// D1, the real store, from a copy of a backup: `d1:<backup dir>`.
    D1(PathBuf),
    /// D2, D1 grown with synthetic memories to `total`: `d2:<backup dir>`
    /// (to [`d1::D2_TOTAL`]).
    D2 { backup: PathBuf, total: usize },
}

impl Dataset {
    fn parse(arg: &str) -> Result<Self> {
        if let Some(path) = arg.strip_prefix("d1:") {
            return Ok(Self::D1(PathBuf::from(path)));
        }
        if let Some(path) = arg.strip_prefix("d2:") {
            return Ok(Self::D2 {
                backup: PathBuf::from(path),
                total: d1::D2_TOTAL,
            });
        }
        let n: usize = arg
            .parse()
            .map_err(|_| format!("dataset {arg:?} is not a count, d1:<backup> or d2:<backup>"))?;
        if n == 0 {
            return Err("n must be at least 1".into());
        }
        Ok(Self::Synthetic(n))
    }

    fn name(&self) -> String {
        match self {
            Self::Synthetic(n) => n.to_string(),
            Self::D1(_) => "d1".into(),
            Self::D2 { total, .. } => format!("d2-{total}"),
        }
    }
}

/// Run the benchmark the command line asks for, on one of `candidates`:
/// `<bench|open> <backend> <n|d1:<backup>|d2:<backup>> <dir>`. Prints one
/// JSON line.
pub fn run(candidates: &[Candidate]) -> Result<()> {
    let names: Vec<&str> = candidates.iter().map(|c| c.name).collect();
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The G7 child: `cold <backend> <run dir>`, its query on stdin.
    if let [mode, backend, dir] = args.as_slice() {
        if mode == "cold" {
            let candidate = find(candidates, backend)?;
            refuse_model_config()?;
            let dir = Path::new(dir);
            owned_run(dir)?;
            return cold::cold_child(candidate, dir);
        }
    }
    let [mode, backend, n, dir] = args.as_slice() else {
        return Err(format!(
            "usage: store_bench <bench|open> <{}> <n|d1:<backup>|d2:<backup>> <dir>",
            names.join("|")
        )
        .into());
    };
    let candidate = find(candidates, backend)?;
    // The G8 child (D52): `restore <backend> <package> <fresh dir>`.
    if mode == "restore" {
        refuse_model_config()?;
        let restored = restore(candidate, Path::new(n), Path::new(dir))?;
        println!("{}", serde_json::to_string(&restored)?);
        return Ok(());
    }
    let mode = Mode::parse(mode)?;
    let report = measure(
        candidate,
        mode,
        &Dataset::parse(n)?,
        Path::new(dir),
        Isolation::Child,
    )?;
    println!("{}", Value::Object(report));
    Ok(())
}

/// The candidate named `name`.
fn find(candidates: &[Candidate], name: &str) -> Result<Candidate> {
    let names: Vec<&str> = candidates.iter().map(|c| c.name).collect();
    candidates
        .iter()
        .find(|c| c.name == name)
        .copied()
        .ok_or_else(|| format!("unknown backend {name:?}; one of {names:?}").into())
}

/// Taken by every test that opens a store. A write stamps its context by
/// running `git`, and a child spawned while another test holds a store
/// lock inherits that lock's open file until it execs, so a reopen in the
/// other test can find the store "in use by another process". Tests that
/// open stores therefore run one at a time.
#[cfg(test)]
pub fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The file every run directory holds, so a child refuses any other.
const RUN_MARK: &str = ".store_bench_run";

/// Refuse `dir` unless a run made it: a child never opens another store.
fn owned_run(dir: &Path) -> Result<()> {
    if dir.join(RUN_MARK).is_file() {
        return Ok(());
    }
    Err(format!("{} is not a store_bench run directory", dir.display()).into())
}

/// One run of `candidate` on `dataset` in a new directory under `root`;
/// its report. The dataset and the environment are checked before the
/// filesystem is touched.
pub fn measure(
    candidate: Candidate,
    mode: Mode,
    dataset: &Dataset,
    root: &Path,
    isolation: Isolation,
) -> Result<Map<String, Value>> {
    match dataset {
        Dataset::Synthetic(0) => return Err("n must be at least 1".into()),
        Dataset::Synthetic(_) => {}
        Dataset::D1(backup) | Dataset::D2 { backup, .. } => {
            if mode == Mode::Open {
                return Err("open mode is synthetic only; bench reports the cold open".into());
            }
            if !package::PRIVACY_VERIFIABLE {
                return Err(
                    "real datasets need a run root verified private from creation, which this \
                     platform cannot check; run D1 and D2 on Unix"
                        .into(),
                );
            }
            if !backup.is_dir() {
                return Err(format!("backup {} is not a directory", backup.display()).into());
            }
            let backup = backup.canonicalize()?;
            let root = resolved(root)?;
            if backup.starts_with(&root) || root.starts_with(&backup) {
                return Err(format!(
                    "the run root {} and the backup {} overlap",
                    root.display(),
                    backup.display()
                )
                .into());
            }
        }
    }
    refuse_model_config()?;
    let stem = format!("{}-{}-{}", mode.name(), candidate.name, dataset.name());
    let dir = fresh_run_dir(root, &stem)?;

    let mut report = Map::new();
    report.insert("mode".into(), json!(mode.name()));
    report.insert("backend".into(), json!(candidate.name));
    report.insert("dataset".into(), json!(dataset.name()));
    report.insert("run_dir".into(), json!(dir));
    report.insert(
        "note".into(),
        json!("not a scoring run: no D3 ingest, crash or relevance workload yet"),
    );
    match mode {
        Mode::Bench => workload(candidate, &dir, dataset, &mut report, isolation)?,
        Mode::Open => {
            let Dataset::Synthetic(n) = dataset else {
                return Err("open mode is synthetic only".into());
            };
            reopen(candidate, &dir, *n, &mut report)?
        }
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

/// `path` with every symlink resolved, even where it does not exist yet:
/// its nearest existing ancestor canonicalized, the rest appended.
fn resolved(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path)?;
    let mut rest = Vec::new();
    let mut base = path.as_path();
    while !base.exists() {
        rest.push(
            base.file_name()
                .ok_or("the run root has no existing ancestor")?,
        );
        base = base
            .parent()
            .ok_or("the run root has no existing ancestor")?;
    }
    let mut out = base.canonicalize()?;
    for part in rest.iter().rev() {
        out.push(part);
    }
    Ok(out)
}

/// A new run directory, `run`, inside a private (0700) directory under
/// `root` that this run alone owns, so everything the run writes beside
/// it (the D1 copy, the package, the restore) is private too.
/// `create_dir` fails if a name is taken, and nothing is ever deleted.
fn fresh_run_dir(root: &Path, stem: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let owned = root.join(format!("{stem}-{}-{nanos}", std::process::id()));
    package::private_dir(&owned)?;
    let dir = owned.join("run");
    std::fs::create_dir(&dir)?;
    std::fs::write(dir.join(RUN_MARK), b"")?;
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
    dataset: &Dataset,
    report: &mut Map<String, Value>,
    isolation: Isolation,
) -> Result<()> {
    let corpus = Corpus::new();
    let mut rng = Rng(0x5eed);
    let bodies = Bodies::new(dir.join("blobs"));
    let Seeded {
        ids,
        entities,
        pairs,
        identity,
    } = {
        let db = candidate.open(dir)?;
        seed(&db, dataset, dir, &bodies, &corpus, &mut rng, report)?
    };
    let real = !matches!(dataset, Dataset::Synthetic(_));
    let n = ids.len();
    report.insert("memories".into(), json!(n));
    report.insert("disk_mb_after_fill".into(), disk(dir)?);
    // G7 and the idle baseline on the store exactly as the dataset left it.
    let cold = cold::cold_open(candidate, dir, &pairs[0], isolation)?;
    report.insert("cold_open".into(), cold);

    let started = Instant::now();
    let db = candidate.open(dir)?;
    report.insert("reopen_warm_ms".into(), json!(millis(started.elapsed())));
    let store = db.store();

    // Dialogs (#1616) and claims are append-only and refuse edits, so
    // updates and deletes take only the rows the store would let them
    // change, by the same check the edits make.
    let (mut editable, mut dialog) = (Vec::new(), None);
    for id in &ids {
        let memory = queries::get_memory_by_id(&store, id)?
            .ok_or_else(|| format!("memory {id} vanished after seeding"))?;
        if remind_me_core::dialog::refuse_protected_edit(&memory).is_ok() {
            editable.push(id.clone());
        } else if dialog.is_none() && memory.category == DIALOG {
            dialog = Some(id.clone());
        }
    }
    report.insert(
        "edit_pool".into(),
        json!({ "editable": editable.len(), "append_only": ids.len() - editable.len() }),
    );
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

    // Query vectors were made before any timed span (in `seed`), so the
    // searches time retrieval, not embedding.
    let queries_: Vec<String> = pairs.iter().map(|(t, _)| t.clone()).collect();
    let vectors_ = Precomputed(pairs.into_iter().collect());
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
        let walk = if entities.is_empty() {
            skipped("no entities")
        } else {
            timed(|_| {
                let seed = entities[rng.below(entities.len())].clone();
                entity::traverse_entities(&store, &[seed], hops, None, 1_000)?;
                Ok(())
            })?
        };
        report.insert(format!("entity_{hops}_hop"), walk);
    }

    // A small store can have no editable rows (n = 1 draws a dialog):
    // updates and deletes then report zero samples rather than failing.
    let update = if editable.is_empty() {
        skipped("no editable rows (dialogs and claims are append-only)")
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
        if real {
            grow(s, &bodies, 1, &corpus, rng)?;
        } else {
            fill(s, &bodies, 1, &corpus, rng)?;
        }
        Ok(())
    };
    if real {
        // Single adds on the real store, each with its vector written as
        // `grow` does, so the inherited embedding identity is kept.
        report.insert("add".into(), timed(|_| add_one(&store, &mut rng))?);
    }
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
        skipped("no editable rows (dialogs and claims are append-only)")
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
    // Reopen, so the identity check covers what the store persisted.
    drop(db);
    let db = candidate.open(dir)?;
    if real {
        let now = Vectors::new(&db.store()).meta()?;
        if now != identity {
            return Err(format!(
                "the embedding identity changed across the workloads and a reopen: \
                 {identity:?} became {now:?}"
            )
            .into());
        }
        report.insert(
            "embedding_identity".into(),
            json!({ "unchanged": true, "after_reopen": true, "dim": identity_dim(&identity) }),
        );
    }
    backup_and_restore(candidate, db, &bodies, dir, report, isolation)
}

/// What a seeded store holds that the workloads pick from.
struct Seeded {
    ids: Vec<String>,
    entities: Vec<String>,
    /// Query texts and their vectors, each text once.
    pairs: Vec<(String, Vec<f32>)>,
    /// The loaded embedding identity (empty for synthetic data).
    identity: Vec<(String, String)>,
}

/// `SAMPLES` distinct synthetic queries, each with its seeded vector, in
/// the order drawn.
fn synthetic_queries(corpus: &Corpus, rng: &mut Rng) -> Result<Vec<(String, Vec<f32>)>> {
    let mut seen = std::collections::HashSet::new();
    let mut pairs = Vec::with_capacity(SAMPLES);
    for i in 0..SAMPLES * 20 {
        if pairs.len() == SAMPLES {
            break;
        }
        let text = query(corpus, rng, i);
        if seen.insert(text.clone()) {
            let vector = unit_vector(&text);
            pairs.push((text, vector));
        }
    }
    if pairs.len() < SAMPLES {
        return Err(format!("only {} distinct queries drawn", pairs.len()).into());
    }
    Ok(pairs)
}

/// Refuse query vectors whose dimension is not the store's `dim`. The
/// error names the query by its ordinal, never its text (real content).
fn check_query_dims(pairs: &[(String, Vec<f32>)], dim: usize) -> Result<()> {
    match pairs.iter().position(|(_, v)| v.len() != dim) {
        Some(i) => Err(format!(
            "query {} of {} has a {}-dim vector; the store's are {dim}",
            i + 1,
            pairs.len(),
            pairs[i].1.len()
        )
        .into()),
        None => Ok(()),
    }
}

/// The dimension an embedding identity records, if it does.
fn identity_dim(identity: &[(String, String)]) -> Option<usize> {
    identity
        .iter()
        .find(|(k, _)| k == "dim")
        .and_then(|(_, v)| v.parse().ok())
}

/// Load `dataset` into `db`, reporting how long it took and what it wrote.
fn seed(
    db: &Database,
    dataset: &Dataset,
    dir: &Path,
    bodies: &Bodies,
    corpus: &Corpus,
    rng: &mut Rng,
    report: &mut Map<String, Value>,
) -> Result<Seeded> {
    let store = db.store();
    let started = Instant::now();
    let (backup, total) = match dataset {
        Dataset::Synthetic(n) => {
            let (ids, adds) = fill(&store, bodies, *n, corpus, rng)?;
            report.insert("fill_s".into(), json!(started.elapsed().as_secs_f64()));
            report.insert("add".into(), summary(adds));
            let count = n.div_ceil(MEMORIES_PER_PAGE);
            let took = pages(&store, bodies, count, corpus, rng)?;
            report.insert(
                "pages".into(),
                json!({ "count": count, "ms": millis(took) }),
            );
            drop(store);
            let entities = graph(db, &ids, rng)?;
            let pairs = synthetic_queries(corpus, rng)?;
            return Ok(Seeded {
                ids,
                entities,
                pairs,
                identity: Vec::new(),
            });
        }
        Dataset::D1(backup) => (backup, None),
        Dataset::D2 { backup, total } => (backup, Some(*total)),
    };
    let loaded = d1::load(backup, &dir.with_extension("d1-source"), &store, bodies)?;
    report.insert("load_s".into(), json!(started.elapsed().as_secs_f64()));
    report.insert("loaded".into(), Value::Object(loaded.counts));
    report.insert("parity".into(), loaded.parity);
    let dim = identity_dim(&loaded.identity);
    let mut ids = loaded.ids;
    if let Some(total) = total {
        if dim.is_some_and(|d| d != DIM) {
            return Err(
                format!("D2's synthetic vectors are {DIM}-dim; the store's are {dim:?}").into(),
            );
        }
        let started = Instant::now();
        let grown = grow(&store, bodies, total.saturating_sub(ids.len()), corpus, rng)?;
        report.insert(
            "grown".into(),
            json!({ "memories": grown.len(), "s": started.elapsed().as_secs_f64() }),
        );
        ids.extend(grown);
    }
    let pairs = d1::queries(&store, &ids, SAMPLES, |len| rng.below(len))?;
    if let Some(d) = dim {
        check_query_dims(&pairs, d)?;
    }
    report.insert(
        "queries".into(),
        json!("document-derived, each text once: timing only, not relevance (G2)"),
    );
    Ok(Seeded {
        ids,
        entities: loaded.entities,
        pairs,
        identity: loaded.identity,
    })
}

/// Add `n` synthetic memories to a loaded store (D2), each with a seeded
/// vector written straight to its chunk, so the store's recorded
/// embedding identity (the real model's) is left as it is. Their ids.
fn grow(
    store: &Store<'_>,
    bodies: &Bodies,
    n: usize,
    corpus: &Corpus,
    rng: &mut Rng,
) -> Result<Vec<String>> {
    let mut ids = Vec::with_capacity(n);
    for _ in 0..n {
        let mut input = corpus.memory(rng);
        if input.category == DIALOG {
            let stashed = bodies.stash(&input.content)?;
            input.metadata = stashed.metadata();
            input.content = stashed.head;
        }
        let vector = unit_vector(&input.content);
        let id = queries::add_memory(store, input)?.id;
        Vectors::new(store).put(&id, 0, &d1::to_bytes(&vector))?;
        ids.push(id);
    }
    Ok(ids)
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
/// the writer's error instead of hanging.
///
/// One monotonic clock times everything: each successful write's
/// completion, and each reader's span from the start of its first timed
/// query to the end of its last. The run fails as invalid when the writer
/// covers less than [`MIN_OVERLAP`] of any reader's span or no write
/// completed inside one (D54). The memory row counts each reader reads
/// just outside its span are reported as diagnostics only.
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
    let epoch = Instant::now();
    let rows_before = testing::count(store, testing::Table::Memories)?;
    let mut windows: Vec<Window> = Vec::new();
    let mut span = None;
    std::thread::scope(|scope| -> Result<()> {
        let writing = writer.map(|write| {
            scope.spawn(move || -> std::result::Result<Writes, String> {
                let store = source.store();
                let mut rng = Rng(0xbeef);
                let mut completed = Vec::new();
                let mut add = |completed: &mut Vec<f64>| -> std::result::Result<(), String> {
                    write(&store, &mut rng).map_err(|e| e.to_string())?;
                    completed.push(epoch.elapsed().as_secs_f64());
                    Ok(())
                };
                let first = epoch.elapsed().as_secs_f64();
                let started = add(&mut completed);
                writer_failed.store(started.is_err(), Ordering::SeqCst);
                start.wait();
                started?;
                while !readers_done.load(Ordering::SeqCst) {
                    add(&mut completed)?;
                }
                let last = epoch.elapsed().as_secs_f64();
                Ok(Writes {
                    first,
                    last,
                    completed,
                })
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
                    let mut span: Option<(Instant, Instant)> = None;
                    for i in (r..SAMPLES).step_by(READERS) {
                        let input: MemorySearchInput =
                            serde_json::from_value(json!({ "query": queries_[i] }))
                                .map_err(|e| e.to_string())?;
                        let t = Instant::now();
                        searched(&store, &input, Some(vectors_)).map_err(|e| e.to_string())?;
                        let done = Instant::now();
                        times.push(done - t);
                        span = Some((span.map_or(t, |(first, _)| first), done));
                    }
                    let (begin, end) = span.map_or((0.0, 0.0), |(first, last)| {
                        let since = |at: Instant| (at - epoch).as_secs_f64();
                        (since(first), since(last))
                    });
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
    let Some(writes) = span else {
        return Ok(value);
    };
    value["writer_adds"] = json!(writes.completed.len());
    value["writer_s"] = json!({ "first": writes.first, "last": writes.last });
    value["readers"] = windows
        .iter()
        .map(|w| {
            json!({
                "begin_s": w.begin,
                "end_s": w.end,
                "overlap": w.overlap(writes.first, writes.last),
                "writes_inside": w.writes_inside(&writes.completed),
                "rows_at_begin": w.rows_at_begin,
                "rows_at_end": w.rows_at_end,
            })
        })
        .collect();
    let invalid = invalid_readers(&windows, &writes);
    if !invalid.is_empty() {
        return Err(format!(
            "invalid run, re-run it: readers {invalid:?} had under {MIN_OVERLAP} of their \
             span covered or no write completed inside it: {}",
            value["readers"]
        )
        .into());
    }
    Ok(value)
}

/// The loaded run's writer: when it started and stopped, and when each
/// successful write completed, in seconds since the run's epoch.
struct Writes {
    first: f64,
    last: f64,
    completed: Vec<f64>,
}

/// The readers (by position) the writer did not load enough: under
/// [`MIN_OVERLAP`] of the span covered, or no write completed inside it.
fn invalid_readers(windows: &[Window], writes: &Writes) -> Vec<usize> {
    windows
        .iter()
        .enumerate()
        .filter(|(_, w)| {
            w.overlap(writes.first, writes.last) < MIN_OVERLAP
                || w.writes_inside(&writes.completed) == 0
        })
        .map(|(ix, _)| ix)
        .collect()
}

/// One reader's span, from the start of its first timed query to the end
/// of its last, in seconds since the run's epoch; the memory row count it
/// read just outside each end (a diagnostic); and its samples.
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

    /// How many of the write completion times `completed` fall inside
    /// this span.
    fn writes_inside(&self, completed: &[f64]) -> usize {
        completed
            .iter()
            .filter(|t| (self.begin..=self.end).contains(*t))
            .count()
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
    status_mb("VmHWM:")
}

/// The process's resident memory now, from Linux's `VmRSS`; null elsewhere.
fn rss_mb() -> Option<f64> {
    status_mb("VmRSS:")
}

/// A `/proc/self/status` field given in kB, in MB.
fn status_mb(field: &str) -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with(field))?;
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
        let _serial = serial();
        let dir = std::env::temp_dir().join(format!("store_bench_hang_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (sent, received) = mpsc::channel();
        let run_dir = dir.clone();
        let run = std::thread::spawn(move || {
            let tables = SqliteTables::open(&run_dir.join("store")).unwrap();
            let db =
                Database::open_backend(Box::new(tables), Some(run_dir.join("memory.db"))).unwrap();
            let store = db.store();
            let fail = |_: &Store<'_>, _: &mut Rng| -> Result<()> { Err("refused".into()) };
            let outcome = readers(&db, &store, &[], &Precomputed(HashMap::new()), Some(&fail));
            let _ = sent.send(outcome.map_err(|e| e.to_string()));
        });
        let outcome = received
            .recv_timeout(Duration::from_secs(60))
            .expect("the run returned instead of hanging");
        assert!(outcome.unwrap_err().contains("refused"));
        // The thread still holds the database until it ends; Windows will
        // not remove files that are open (OS error 32).
        run.join().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The cold child refuses a directory no run made, such as a live
    /// store, and accepts a fresh run directory.
    #[test]
    fn the_cold_child_opens_only_a_run_directory() {
        let root = std::env::temp_dir().join(format!("store_bench_owned_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("live")).unwrap();
        assert!(owned_run(&root.join("live")).is_err());
        let run = fresh_run_dir(&root, "t").unwrap();
        owned_run(&run).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A mismatched query is named by its ordinal, never by its text.
    #[test]
    fn a_query_dimension_error_carries_no_query_text() {
        let pairs = vec![
            ("first secret words".to_string(), vec![0.0; 4]),
            ("second secret words".to_string(), vec![0.0; 3]),
        ];
        check_query_dims(&pairs, 4).unwrap_err();
        let error = check_query_dims(&pairs, 4).unwrap_err().to_string();
        assert!(!error.contains("secret"), "{error}");
        assert!(
            error.contains("query 2 of 2") && error.contains("3-dim"),
            "{error}"
        );
        check_query_dims(&pairs[..1], 4).unwrap();
    }

    /// Off Unix, a real dataset is refused before anything is written.
    #[cfg(not(unix))]
    #[test]
    fn a_real_dataset_is_refused_where_privacy_cannot_be_verified() {
        let root = std::env::temp_dir().join(format!("store_bench_acl_{}", std::process::id()));
        let backup = std::env::temp_dir();
        let dataset = Dataset::D1(backup);
        let candidate = Candidate {
            name: "none",
            tables: |_| Err("unused".into()),
            restore: |_| Err("unused".into()),
            package: backup_package,
        };
        let error = measure(
            candidate,
            Mode::Bench,
            &dataset,
            &root,
            Isolation::InProcess,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("verified private"), "{error}");
        assert!(!root.exists());
    }

    fn window(begin: f64, end: f64) -> Window {
        Window {
            begin,
            end,
            rows_at_begin: 0,
            rows_at_end: 1,
            times: vec![],
        }
    }

    /// A write that completes after a reader's last timed query (before
    /// its closing row count would see it) does not load that reader.
    #[test]
    fn a_write_completing_after_the_last_query_is_rejected() {
        let writes = Writes {
            first: 0.0,
            last: 3.0,
            completed: vec![0.5, 2.5],
        };
        assert_eq!(invalid_readers(&[window(1.0, 2.0)], &writes), [0]);
    }

    #[test]
    fn a_write_completing_inside_the_span_is_accepted() {
        let writes = Writes {
            first: 0.0,
            last: 3.0,
            completed: vec![0.5, 1.5, 2.5],
        };
        assert!(invalid_readers(&[window(1.0, 2.0)], &writes).is_empty());
    }

    #[test]
    fn a_span_the_writer_barely_covers_is_rejected() {
        let writes = Writes {
            first: 1.5,
            last: 3.0,
            completed: vec![1.6],
        };
        assert_eq!(invalid_readers(&[window(1.0, 2.0)], &writes), [0]);
    }
}
