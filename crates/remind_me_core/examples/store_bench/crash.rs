//! Workload 7, the crash check (G6, with G4's memory; D56, D57).
//!
//! `crash <backend> d1:<backup> <dir>` loads D1 into one private store,
//! then kills a writer on it [`KILLS`] times in each of two streams,
//! accumulating: one of adds, updates and deletes of the writer's own
//! memories, and one of pages sized like D3's. Each round re-runs this
//! executable as `crash-child`, which opens the store, prints `ready`, and
//! writes until it is killed with SIGKILL after a seeded delay. Before each
//! write it prints a `begin` line naming the write and what it read; after
//! the write's transaction commits, an `ack` line with the state it then
//! reads. Every line is flushed, and an incomplete last line counts as
//! unacked.
//!
//! The parent keeps the expected model: for each memory the hash of its
//! tracked columns, its state, whether its text is empty, and the hashes
//! of its revisions, oldest first; for each page the blob its row links.
//! What a write should leave comes from its `begin` line, not from what
//! the store then holds: an add or an update names the hash of the exact
//! row it intends, built from its input before it writes. After each kill
//! the parent reopens the store through the candidate's normal open and
//! checks:
//! - every acked write left exactly what it intended: an add, its row and
//!   no revisions; an update, its row and the old revisions plus the old
//!   row; a delete, no live row and no revisions, and a tombstone in the
//!   canonical emptied form (ADR-0024);
//! - every acked memory still matches its model, revisions included, so
//!   none survives on a hard-deleted or tombstoned memory;
//! - the one in-flight write is all or nothing: the state it read before
//!   (unchanged), or exactly what an acked write of it would leave;
//! - no live memory of the writer's category is missing from the model;
//! - a keyword search for each live acked nonce finds its memory;
//! - every acked page's row links a blob that verifies; an in-flight page
//!   may leave an orphan blob (written before its row), which is counted;
//! - the backend's own integrity check passes on the closed store
//!   ([`Candidate::verify`]).
//!
//! A final reopen and check follows the last round. The report gives each
//! round's in-flight write, the child's peak memory before the kill and
//! the parent's peak over the reopen and check (both G4), and every
//! failure; the run passes with none. Per stream it counts where the kills
//! fell (`kills`): between writes, inside a write before its commit (the
//! write did not land), or after the commit and before its ack (it
//! landed), so a reader can judge how many kills hit a write. A stream
//! whose kills never caught a write in flight gives no evidence for the
//! all-or-nothing check: the run is then `insufficient` for it and does
//! not pass, whatever else it found.
//!
//! The child wraps each write in `Store::transaction`: G6 judges the
//! store's atomicity, not production's call sites (Q21). It also warms
//! its write context before it opens the store, so no `git` child is
//! spawned while it holds the store's lock (a child spawned then would
//! inherit the lock, and outlive a kill).

use remind_me_core::blobs::content_key;
use remind_me_core::db::backend::{Backend, TransactionState};
use remind_me_core::db::engine::EngineTables;
use remind_me_core::db::history::{Revisions, Tracked};
use remind_me_core::db::memories::{KeywordFilter, Memories};
use remind_me_core::db::{queries, Store};
use remind_me_core::models::{MemoryAddInput, MemoryListInput, MemoryUpdateInput, UpdateOutcome};
use remind_me_core::sync::TOMBSTONE_CONTENT;
use remind_me_core::{wiki, Database};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use super::bodies::PAGE_LINK;
use super::{
    check_real_input, d1, find, fresh_run_dir, g4, millis, owned_run, peak_rss_mb, proc_status_mb,
    refuse_model_config, Bodies, Candidate, Corpus, Result, Rng,
};

/// Kills in each stream (D57).
pub const KILLS: usize = 100;
/// The category the writer's memories take, so a check can list them.
const CATEGORY: &str = "crash_bench";
/// The project the writer's memories name, so none is detected.
const PROJECT: &str = "crash_bench";
/// How long the child may take to open the store and say `ready`.
const READY: Duration = Duration::from_secs(300);
/// How long the child's output may take to end once it is killed.
const DRAIN: Duration = Duration::from_secs(10);
/// The seeded delay from `ready` to the kill: at least this many ms...
const MIN_DELAY_MS: u64 = 50;
/// ...and less than this many more.
const DELAY_SPREAD_MS: u64 = 400;
/// A child never killed stops writing after this long, failing its round.
const CHILD_LIMIT: Duration = Duration::from_secs(120);
/// With fewer live memories than this, the writer only adds.
const MIN_LIVE: usize = 8;
/// The most revisions a snapshot reads of one memory.
const MAX_REVISIONS: usize = 10_000;
/// SQLite's file in its store directory (`db::sqlite`).
const SQLITE_FILE: &str = "remind_me.sqlite3";
/// The FTS5 tables SQLite's integrity check also covers (D43).
const SQLITE_FTS: [&str; 2] = ["memories_fts", "wiki_fts"];

/// Starts a `crash-child` with these arguments: this executable in a
/// scoring run ([`exe`]), the test runner in the example's tests.
pub type Launcher = fn(&[String]) -> Result<Command>;

/// This executable, re-run as `crash-child`.
pub fn exe(args: &[String]) -> Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command.arg("crash-child").args(args);
    Ok(command)
}

/// A stream of writes the child repeats (D57).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    /// Adds, updates and deletes of the writer's own memories.
    Writes,
    /// Pages of 10 to 30 KB, each body to the blob store first (D50).
    Pages,
}

impl Stream {
    fn parse(name: &str) -> Result<Self> {
        match name {
            "writes" => Ok(Self::Writes),
            "pages" => Ok(Self::Pages),
            other => Err(format!("unknown stream {other:?}; one of writes, pages").into()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Writes => "writes",
            Self::Pages => "pages",
        }
    }
}

/// Whether a memory is there, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum State {
    Live,
    /// Deleted in a store that syncs: the row stays, its text emptied.
    Tombstone,
    /// Deleted in a store that never synced: no row.
    Absent,
}

/// A memory as the check compares it (D56): its state, the hash of its
/// tracked columns, and the hash of each revision's, oldest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Snapshot {
    state: State,
    row: Option<String>,
    /// Whether the row is in the canonical emptied form a tombstone must
    /// take (ADR-0024): content [`TOMBSTONE_CONTENT`], no tags, and no
    /// subject, predicate or object.
    emptied: bool,
    revisions: Vec<String>,
}

/// One write, as its `begin` line names it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Op {
    /// `row`: the hash of the row the add intends.
    Add {
        nonce: String,
        row: String,
    },
    /// `row`: the hash of the row the update intends.
    Update {
        id: String,
        nonce: String,
        before: Snapshot,
        row: String,
    },
    Delete {
        id: String,
        before: Snapshot,
    },
    Page {
        slug: String,
        key: String,
    },
}

impl Op {
    fn kind(&self) -> &'static str {
        match self {
            Self::Add { .. } => "add",
            Self::Update { .. } => "update",
            Self::Delete { .. } => "delete",
            Self::Page { .. } => "page",
        }
    }
}

/// A memory a write committed, as the child read it straight after.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Landed {
    id: String,
    after: Snapshot,
}

/// One line of the child's output.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "lowercase")]
enum Line {
    Ready,
    Begin { n: usize, op: Op },
    Ack { n: usize, landed: Option<Landed> },
}

/// What the child is told when it starts.
#[derive(Serialize, Deserialize)]
struct Start {
    live: Vec<String>,
    next_page: usize,
}

/// What the parent expects of one memory.
struct Expected {
    snapshot: Snapshot,
    nonce: String,
}

/// The parent's expected model of everything the writers wrote.
#[derive(Default)]
struct Model {
    memories: BTreeMap<String, Expected>,
    /// Each acked page's slug and the blob its row links.
    pages: BTreeMap<String, String>,
    next_page: usize,
    orphan_blobs: usize,
}

/// G6 on `candidate`: [`KILLS`] (or `kills`) rounds in each stream on one
/// copy of D1 (`set` is `d1:<backup>`) under `root`; the report.
pub fn run(
    candidate: Candidate,
    set: &str,
    root: &Path,
    kills: usize,
    launch: Launcher,
) -> Result<Map<String, Value>> {
    let backup = set
        .strip_prefix("d1:")
        .map(Path::new)
        .ok_or("usage: store_bench crash <backend> d1:<backup> <dir>")?;
    check_real_input(backup, root)?;
    refuse_model_config()?;
    let dir = fresh_run_dir(root, &format!("crash-{}-d1", candidate.name))?;
    {
        let db = candidate.open(&dir)?;
        let bodies = Bodies::new(dir.join("blobs"));
        d1::load(backup, &dir.join("d1-source"), &db.store(), &bodies)?;
    }
    let mut report = Map::new();
    report.insert("mode".into(), json!("crash"));
    report.insert("backend".into(), json!(candidate.name));
    report.insert("dataset".into(), json!("d1"));
    report.insert("gates".into(), json!(["G6", "G4"]));
    report.insert("git_sha".into(), json!(std::env::var("R3_GIT_SHA").ok()));
    report.insert("run_dir".into(), json!(dir));
    report.insert("kills_per_stream".into(), json!(kills));

    let mut model = Model::default();
    let mut rounds = Vec::new();
    let mut failures = 0;
    let mut finished = || -> Result<Value> {
        for stream in [Stream::Writes, Stream::Pages] {
            for round in 0..kills {
                let (value, failed) =
                    kill_round(candidate, &dir, stream, round, &mut model, launch)?;
                failures += failed;
                rounds.push(value);
            }
        }
        let (value, failed) = reopen_and_check(candidate, &dir, &mut model, None)?;
        failures += failed.len();
        Ok(json!({ "check": value, "failures": failed }))
    };
    let last = finished();
    let max = |key: &str| g4::max_mb(rounds.iter().map(|r| r[key].as_f64()));
    report.insert("child_peak_max_mb".into(), json!(max("child_peak_mb")));
    report.insert("reopen_peak_max_mb".into(), json!(max("reopen_peak_mb")));
    report.insert("g4".into(), g4::crash(&rounds));
    report.insert("memories_tracked".into(), json!(model.memories.len()));
    report.insert("pages_tracked".into(), json!(model.pages.len()));
    report.insert("orphan_blobs".into(), json!(model.orphan_blobs));
    report.insert("failures".into(), json!(failures));
    let passed = match last {
        Ok(final_check) => {
            report.insert("final".into(), final_check);
            failures == 0
        }
        Err(e) => {
            report.insert("aborted".into(), json!(e.to_string()));
            false
        }
    };
    let kills = where_kills_fell(&rounds);
    let insufficient = insufficient(&kills);
    report.insert("kills".into(), kills);
    report.insert("insufficient".into(), json!(insufficient));
    report.insert("sufficient".into(), json!(insufficient.is_empty()));
    report.insert("rounds".into(), json!(rounds));
    report.insert("passed".into(), json!(passed && insufficient.is_empty()));
    // The parent's whole peak, D1's load included: reported, never G4.
    report.insert("peak_rss_mb".into(), json!(peak_rss_mb()));
    Ok(report)
}

/// Per stream, where its kills fell: between writes, inside a write
/// before its commit (it did not land), or after the commit and before
/// its ack (it landed).
fn where_kills_fell(rounds: &[Value]) -> Value {
    let mut by_stream = Map::new();
    for stream in [Stream::Writes, Stream::Pages] {
        let mine: Vec<&Value> = rounds
            .iter()
            .filter(|r| r["stream"] == stream.name())
            .collect();
        let count = |landed: Option<bool>| {
            mine.iter()
                .filter(|r| match landed {
                    None => r["in_flight"].is_null(),
                    Some(l) => r["check"]["in_flight_landed"] == l,
                })
                .count()
        };
        by_stream.insert(
            stream.name().into(),
            json!({
                "total": mine.len(),
                "between_writes": count(None),
                "inside_a_write_not_landed": count(Some(false)),
                "inside_a_write_landed": count(Some(true)),
            }),
        );
    }
    Value::Object(by_stream)
}

/// The streams whose kills never caught a write in flight: they give no
/// evidence for the all-or-nothing check (D57), so the run cannot score
/// G6 however few failures it found. Their counts stay in the report.
fn insufficient(kills: &Value) -> Vec<String> {
    let Some(streams) = kills.as_object() else {
        return vec!["no kill counts".into()];
    };
    streams
        .iter()
        .filter(|(_, k)| {
            let n = |key: &str| k[key].as_u64().unwrap_or(0);
            n("inside_a_write_not_landed") + n("inside_a_write_landed") == 0
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// One kill: start a child on `stream`, kill it a seeded delay after it
/// is ready, then reopen and check. The round's report and how many
/// failures it found.
fn kill_round(
    candidate: Candidate,
    dir: &Path,
    stream: Stream,
    round: usize,
    model: &mut Model,
    launch: Launcher,
) -> Result<(Value, usize)> {
    let seed = seed_for(stream, round);
    let delay = Duration::from_millis(MIN_DELAY_MS + mix(seed) % DELAY_SPREAD_MS);
    let start = Start {
        live: model.live(),
        next_page: model.next_page,
    };
    let args = [
        candidate.name.to_string(),
        dir.display().to_string(),
        stream.name().to_string(),
        seed.to_string(),
    ];
    let mut child = launch(&args)?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let driven = drive(&mut child, &start, delay);
    if driven.is_err() {
        let _ = child.kill();
    }
    child.wait()?;
    let (lines, child_peak) = driven?;
    let mut failures = Vec::new();
    let acked = lines
        .iter()
        .filter(|l| matches!(l, Line::Ack { .. }))
        .count();
    let in_flight = model.replay(lines, &mut failures);
    let in_flight_kind = in_flight.as_ref().map(Op::kind);
    let (checked, found) = reopen_and_check(candidate, dir, model, in_flight)?;
    failures.extend(found);
    let value = json!({
        "stream": stream.name(),
        "round": round,
        "delay_ms": millis(delay),
        "acked": acked,
        "in_flight": in_flight_kind,
        "child_peak_mb": child_peak,
        "reopen_ms": checked["reopen_ms"],
        "reopen_peak_mb": checked["reopen_peak_mb"],
        "check": checked,
        "failures": failures,
    });
    Ok((value, failures.len()))
}

/// Send the child its start, wait for `ready`, sleep `delay`, read its
/// peak memory and kill it; every complete line it printed, and the peak.
fn drive(child: &mut Child, start: &Start, delay: Duration) -> Result<(Vec<Line>, Option<f64>)> {
    let mut stdin = child.stdin.take().ok_or("crash child has no stdin")?;
    stdin.write_all(&serde_json::to_vec(start)?)?;
    drop(stdin);
    let stdout = child.stdout.take().ok_or("crash child has no stdout")?;
    let (sent, raw) = mpsc::channel::<Vec<u8>>();
    let reader = std::thread::spawn(move || {
        let mut stdout = BufReader::new(stdout);
        loop {
            let mut line = Vec::new();
            match stdout.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) if sent.send(line).is_err() => return,
                Ok(_) => {}
            }
        }
    });
    let ready_by = Instant::now() + READY;
    loop {
        let wait = ready_by.saturating_duration_since(Instant::now());
        let line = raw
            .recv_timeout(wait)
            .map_err(|_| format!("crash child was not ready within {READY:?}"))?;
        match parse(&line)? {
            Some(Line::Ready) => break,
            Some(other) => return Err(format!("crash child wrote before ready: {other:?}").into()),
            None => {}
        }
    }
    std::thread::sleep(delay);
    if let Some(status) = child.try_wait()? {
        return Err(format!("crash child exited before its kill: {status}").into());
    }
    let peak = proc_status_mb(&child.id().to_string(), "VmHWM:");
    child.kill()?;
    let mut lines = Vec::new();
    loop {
        match raw.recv_timeout(DRAIN) {
            Ok(line) => lines.extend(parse(&line)?),
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                return Err(format!("crash child output did not end within {DRAIN:?}").into())
            }
        }
    }
    reader.join().map_err(|_| "crash child reader panicked")?;
    Ok((lines, peak))
}

/// A protocol line, or `None` for one the kill cut short (no newline) or
/// one that is not a JSON object (a test runner's own output).
fn parse(raw: &[u8]) -> Result<Option<Line>> {
    if raw.last() != Some(&b'\n') || raw.first() != Some(&b'{') {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(raw)?))
}

/// The seed of `round` in `stream`.
fn seed_for(stream: Stream, round: usize) -> u64 {
    let stream = match stream {
        Stream::Writes => 1,
        Stream::Pages => 2,
    };
    mix(0x6a09_e667_f3bc_c909 ^ (stream << 32) ^ round as u64) | 1
}

/// A 64-bit mix (splitmix64's finaliser).
fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Reopen the store, settle `in_flight`, check the whole model, close,
/// then run the backend's integrity check; the check's report and its
/// failures. A store that cannot reopen ends the run.
fn reopen_and_check(
    candidate: Candidate,
    dir: &Path,
    model: &mut Model,
    in_flight: Option<Op>,
) -> Result<(Value, Vec<String>)> {
    // Writing 5 resets this process's peak (VmHWM) to its current size,
    // so the peak read after the check is the reopen's.
    let reset = std::fs::write("/proc/self/clear_refs", "5").is_ok();
    let mut failures = Vec::new();
    let t = Instant::now();
    let (reopen_ms, settled) = {
        let db = candidate
            .open(dir)
            .map_err(|e| format!("the store did not reopen: {e}"))?;
        let reopen_ms = millis(t.elapsed());
        let store = db.store();
        let bodies = Bodies::new(dir.join("blobs"));
        let settled = match in_flight {
            Some(op) => Some(model.settle(&store, &bodies, op, &mut failures)?),
            None => None,
        };
        model.check(&store, &bodies, &mut failures)?;
        (reopen_ms, settled)
    };
    let peak = reset.then(|| proc_status_mb("self", "VmHWM:")).flatten();
    let integrity = match (candidate.verify)(&dir.join("store")) {
        Ok(v) => v,
        Err(e) => {
            failures.push(format!("integrity: {e}"));
            Value::Null
        }
    };
    let value = json!({
        "reopen_ms": reopen_ms,
        "reopen_peak_mb": peak,
        "in_flight_landed": settled,
        "integrity": integrity,
    });
    Ok((value, failures))
}

impl Model {
    /// The live memories the writer may edit.
    fn live(&self) -> Vec<String> {
        self.memories
            .iter()
            .filter(|(_, e)| e.snapshot.state == State::Live)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Apply a killed child's lines: every acked write, checked against
    /// what its write should do. The write begun and never acked, if any.
    fn replay(&mut self, lines: Vec<Line>, failures: &mut Vec<String>) -> Option<Op> {
        let mut begun: Option<(usize, Op)> = None;
        for line in lines {
            match line {
                Line::Ready => failures.push("the child said ready twice".into()),
                Line::Begin { n, op } => {
                    if let Some((m, _)) = &begun {
                        failures.push(format!("write {m} had no ack before write {n} began"));
                    }
                    if let Op::Update { id, before, .. } | Op::Delete { id, before } = &op {
                        let modelled = self.memories.get(id).map(|e| &e.snapshot);
                        if modelled != Some(before) {
                            failures.push(format!("{id}: read before write {n} is not the model"));
                        }
                    }
                    if matches!(op, Op::Page { .. }) {
                        self.next_page += 1;
                    }
                    begun = Some((n, op));
                }
                Line::Ack { n, landed } => match begun.take() {
                    Some((m, op)) if m == n => self.acked(op, landed, failures),
                    _ => failures.push(format!("ack {n} matches no begun write")),
                },
            }
        }
        begun.map(|(_, op)| op)
    }

    /// Take an acked write into the model, checking what it left against
    /// what it intended. The model takes what the write left, so a wrong
    /// write is reported once, not again each round.
    fn acked(&mut self, op: Op, landed: Option<Landed>, failures: &mut Vec<String>) {
        let kind = op.kind();
        let problem = match (op, landed) {
            (Op::Page { slug, key }, None) => {
                self.pages.insert(slug, key);
                None
            }
            (Op::Add { nonce, row }, Some(Landed { id, after })) => {
                let problem =
                    unlike(&added(&row), &after).map(|why| format!("{id}: acked add {why}"));
                if self.memories.contains_key(&id) {
                    failures.push(format!("{id}: an add reused an id"));
                }
                self.adopt(id, after, nonce);
                problem
            }
            (
                Op::Update {
                    id,
                    nonce,
                    before,
                    row,
                },
                Some(landed),
            ) if landed.id == id => {
                let problem = unlike(&updated(&before, &row), &landed.after)
                    .map(|why| format!("{id}: acked update {why}"));
                self.adopt(id, landed.after, nonce);
                problem
            }
            (Op::Delete { id, .. }, Some(landed)) if landed.id == id => {
                let problem = deleted(&landed.after).map(|why| format!("{id}: acked delete {why}"));
                self.set(&id, landed.after);
                problem
            }
            _ => Some(format!("an ack does not match its {kind} write")),
        };
        failures.extend(problem);
    }

    fn adopt(&mut self, id: String, snapshot: Snapshot, nonce: String) {
        self.memories.insert(id, Expected { snapshot, nonce });
    }

    /// A memory's new snapshot, keeping its nonce.
    fn set(&mut self, id: &str, snapshot: Snapshot) {
        if let Some(expected) = self.memories.get_mut(id) {
            expected.snapshot = snapshot;
        }
    }

    /// Settle the in-flight write on the reopened store: it landed whole
    /// (taken into the model) or not at all. Whether it landed; a partial
    /// write is a failure, and the model takes what the store holds, so
    /// later rounds do not report it again.
    fn settle(
        &mut self,
        store: &Store<'_>,
        bodies: &Bodies,
        op: Op,
        failures: &mut Vec<String>,
    ) -> Result<bool> {
        match op {
            Op::Add { nonce, row } => {
                let phantoms = self.phantoms(store)?;
                let [id] = phantoms.as_slice() else {
                    if !phantoms.is_empty() {
                        failures.push(format!("an in-flight add left {} memories", phantoms.len()));
                    }
                    return Ok(false);
                };
                let snapshot = snapshot(store, id)?;
                if let Some(why) = unlike(&added(&row), &snapshot) {
                    failures.push(format!("{id}: the in-flight add is partial: {why}"));
                }
                self.adopt(id.clone(), snapshot, nonce);
                Ok(true)
            }
            Op::Update {
                id,
                nonce,
                before,
                row,
            } => {
                let now = snapshot(store, &id)?;
                if now == before {
                    return Ok(false);
                }
                let intended = updated(&before, &row);
                let whole = now == intended;
                if let Some(why) = unlike(&intended, &now) {
                    failures.push(format!("{id}: the in-flight update is partial: {why}"));
                }
                self.set(&id, now);
                if whole {
                    self.memories.entry(id).and_modify(|e| e.nonce = nonce);
                }
                Ok(true)
            }
            Op::Delete { id, before } => {
                let now = snapshot(store, &id)?;
                if now == before {
                    return Ok(false);
                }
                if let Some(why) = deleted(&now) {
                    failures.push(format!("{id}: the in-flight delete is partial: {why}"));
                }
                self.set(&id, now);
                Ok(true)
            }
            Op::Page { slug, key } => {
                let Some(page) = wiki::get_wiki_page(store, &slug)? else {
                    if bodies.blobs().get(&key)?.is_some() {
                        self.orphan_blobs += 1;
                    }
                    return Ok(false);
                };
                if page.summary != link(&key) || !bodies.blobs().verify(&key)? {
                    failures.push(format!("{slug}: the in-flight page is partial"));
                }
                self.pages.insert(slug, key);
                Ok(true)
            }
        }
    }

    /// Check every memory and page in the model against the store.
    fn check(&self, store: &Store<'_>, bodies: &Bodies, failures: &mut Vec<String>) -> Result<()> {
        for (id, expected) in &self.memories {
            let found = snapshot(store, id)?;
            if found != expected.snapshot {
                failures.push(format!("{id}: {}", differ(&expected.snapshot, &found)));
            }
            if expected.snapshot.state == State::Live && !searchable(store, id, &expected.nonce)? {
                failures.push(format!("{id}: a keyword search for its nonce misses it"));
            }
        }
        for id in self.phantoms(store)? {
            failures.push(format!("{id}: a live memory the model does not hold"));
        }
        for (slug, key) in &self.pages {
            match wiki::get_wiki_page(store, slug)? {
                None => failures.push(format!("{slug}: an acked page is missing")),
                Some(page) if page.summary != link(key) => {
                    failures.push(format!("{slug}: the page links another blob"));
                }
                Some(_) if !bodies.blobs().verify(key)? => {
                    failures.push(format!("{slug}: the page's blob does not verify"));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// Live memories of the writer's category that the model does not
    /// hold as live.
    fn phantoms(&self, store: &Store<'_>) -> Result<Vec<String>> {
        let mut phantoms = Vec::new();
        for id in listed(store, Some(CATEGORY))? {
            let live = self
                .memories
                .get(&id)
                .is_some_and(|e| e.snapshot.state == State::Live);
            if !live {
                phantoms.push(id);
            }
        }
        Ok(phantoms)
    }
}

/// What an add of `row` leaves: that row, live, with no revisions.
fn added(row: &str) -> Snapshot {
    Snapshot {
        state: State::Live,
        row: Some(row.to_string()),
        emptied: false,
        revisions: Vec::new(),
    }
}

/// What an update of `before` to `row` leaves: that row, live, with
/// `before`'s revisions plus one holding `before`'s row.
fn updated(before: &Snapshot, row: &str) -> Snapshot {
    let mut revisions = before.revisions.clone();
    revisions.extend(before.row.clone());
    Snapshot {
        revisions,
        ..added(row)
    }
}

/// How `found` differs from what the write intended, if it does.
fn unlike(intended: &Snapshot, found: &Snapshot) -> Option<String> {
    (found != intended).then(|| differ(intended, found))
}

/// Why `after` is not what a delete leaves: no live row, no revisions,
/// and a tombstone in the canonical emptied form. Whether the row stays
/// as a tombstone (a store that syncs) or goes is the store's setting.
fn deleted(after: &Snapshot) -> Option<String> {
    if after.state == State::Live {
        return Some("left the memory live".into());
    }
    if after.state == State::Tombstone && !after.emptied {
        return Some("left a tombstone not in the emptied form".into());
    }
    (!after.revisions.is_empty()).then(|| format!("left {} revisions", after.revisions.len()))
}

/// How `found` differs from `expected`, without any content.
fn differ(expected: &Snapshot, found: &Snapshot) -> String {
    format!(
        "expected {:?} with {} revisions, found {:?} with {}{}",
        expected.state,
        expected.revisions.len(),
        found.state,
        found.revisions.len(),
        if expected.row == found.row && expected.emptied == found.emptied {
            ""
        } else {
            ", and its row differs"
        }
    )
}

/// A page's summary linking blob `key` (D50).
fn link(key: &str) -> String {
    format!("{PAGE_LINK}{key}")
}

/// `id` as the check compares it.
fn snapshot(store: &Store<'_>, id: &str) -> Result<Snapshot> {
    let history = Revisions::new(store);
    let mut listed = history.list(id, MAX_REVISIONS)?;
    listed.reverse();
    let revisions = listed
        .into_iter()
        .map(|r| {
            hash(&Tracked {
                content: r.content,
                category: r.category,
                tags: r.tags,
                metadata: r.metadata,
                sensitive: r.sensitive,
                project: r.project,
            })
        })
        .collect();
    let Some(current) = history.current(id)? else {
        return Ok(Snapshot {
            state: State::Absent,
            row: None,
            emptied: false,
            revisions,
        });
    };
    let state = if history.is_live(id)? {
        State::Live
    } else {
        State::Tombstone
    };
    Ok(Snapshot {
        state,
        emptied: emptied(store, id)?,
        row: Some(hash(&current)),
        revisions,
    })
}

/// Whether `id`'s row is in the canonical emptied form (ADR-0024).
fn emptied(store: &Store<'_>, id: &str) -> Result<bool> {
    let rows = Memories::new(store).get_many(&[id.to_string()])?;
    Ok(rows.first().is_some_and(|m| {
        m.content == TOMBSTONE_CONTENT
            && m.tags.is_empty()
            && m.subject.is_none()
            && m.predicate.is_none()
            && m.object.is_none()
    }))
}

/// The hash of tracked columns, stable within a run.
fn hash(tracked: &Tracked) -> String {
    content_key(format!("{tracked:?}").as_bytes())
}

/// Whether a keyword search for `nonce` returns `id`.
fn searchable(store: &Store<'_>, id: &str, nonce: &str) -> Result<bool> {
    let filter = KeywordFilter {
        include_sensitive: true,
        ..KeywordFilter::default()
    };
    let hits = Memories::new(store).keyword_hits(&[nonce.to_string()], &filter, 10)?;
    Ok(hits.iter().any(|(m, _)| m.id == id))
}

/// The ids of every live memory, or of one category's.
fn listed(store: &Store<'_>, category: Option<&str>) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    loop {
        let input = MemoryListInput {
            category: category.map(str::to_string),
            limit: remind_me_core::models::LIST_LIMIT_MAX,
            offset: ids.len(),
            include_sensitive: true,
            ..MemoryListInput::default()
        };
        let page = queries::list_memories(store, &input)?;
        if page.memories.is_empty() {
            return Ok(ids);
        }
        ids.extend(page.memories.into_iter().map(|m| m.id));
    }
}

/// The `crash-child` for `args`: `<backend> <run dir> <stream> <seed>`.
pub fn child_from(candidates: &[Candidate], args: &[String]) -> Result<()> {
    let [backend, dir, stream, seed] = args else {
        return Err("usage: store_bench crash-child <backend> <run dir> <stream> <seed>".into());
    };
    let candidate = find(candidates, backend)?;
    child(
        candidate,
        Path::new(dir),
        Stream::parse(stream)?,
        seed.parse()?,
    )
}

/// The `crash-child`: read the start from stdin, open the store, say
/// `ready`, then write `stream` until killed, each write between its
/// `begin` and `ack` lines.
fn child(candidate: Candidate, dir: &Path, stream: Stream, seed: u64) -> Result<()> {
    owned_run(dir)?;
    refuse_model_config()?;
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let start: Start = serde_json::from_slice(&input)?;
    // Detect the write context (it runs `git`) before the store is open.
    let _ = remind_me_core::context::current();
    let db = candidate.open(dir)?;
    let store = db.store();
    let bodies = Bodies::new(dir.join("blobs"));
    let corpus = Corpus::new();
    let mut rng = Rng(seed);
    let mut live = start.live;
    let mut out = std::io::stdout().lock();
    emit(&mut out, &Line::Ready)?;
    let started = Instant::now();
    for n in 0.. {
        if started.elapsed() > CHILD_LIMIT {
            return Err(format!("the crash child was not killed within {CHILD_LIMIT:?}").into());
        }
        let landed = match stream {
            Stream::Writes => {
                let (op, content) = next_write(&store, &live, seed, n, &corpus, &mut rng)?;
                emit(&mut out, &Line::Begin { n, op: op.clone() })?;
                let landed = write(&store, &op, content)?;
                match &op {
                    Op::Add { .. } => live.push(landed.id.clone()),
                    Op::Delete { id, .. } => live.retain(|l| l != id),
                    _ => {}
                }
                Some(landed)
            }
            Stream::Pages => {
                let i = start.next_page + n;
                let body = corpus.page(&mut Rng(mix(i as u64) | 1));
                let slug = format!("crash-page-{i}");
                let key = content_key(body.as_bytes());
                let op = Op::Page {
                    slug: slug.clone(),
                    key,
                };
                emit(&mut out, &Line::Begin { n, op })?;
                let stashed = bodies.stash(&body)?;
                store.transaction(|s| {
                    wiki::write_wiki_page(s, &slug, &slug, &stashed.head, &stashed.page_summary())
                })?;
                None
            }
        };
        emit(&mut out, &Line::Ack { n, landed })?;
    }
    Ok(())
}

/// The writer's next write: an add while few memories are live, else an
/// add, update or delete (4:4:2) of a live one. With it, the content an
/// add or update writes; its `begin` line names the hash of the exact row
/// it intends, built from that content and, for an update, the row it
/// replaces.
fn next_write(
    store: &Store<'_>,
    live: &[String],
    seed: u64,
    n: usize,
    corpus: &Corpus,
    rng: &mut Rng,
) -> Result<(Op, Option<String>)> {
    let nonce = format!("crashnonce{seed:016x}{n:08x}");
    let roll = rng.below(10);
    if live.len() < MIN_LIVE || roll < 4 {
        let content = format!("{nonce} {}", corpus.content(rng));
        let row = hash(&add_row(&content));
        return Ok((Op::Add { nonce, row }, Some(content)));
    }
    let id = live[rng.below(live.len())].clone();
    let before = snapshot(store, &id)?;
    if roll >= 8 {
        return Ok((Op::Delete { id, before }, None));
    }
    let content = format!("{nonce} {}", corpus.content(rng));
    let current = Revisions::new(store)
        .current(&id)?
        .ok_or_else(|| format!("{id} is live but has no row"))?;
    let row = hash(&Tracked {
        content: content.clone(),
        ..current
    });
    let op = Op::Update {
        id,
        nonce,
        before,
        row,
    };
    Ok((op, Some(content)))
}

/// The row an add of `content` intends: its input sets every tracked
/// column, so none is defaulted or detected.
fn add_row(content: &str) -> Tracked {
    Tracked {
        content: content.to_string(),
        category: CATEGORY.into(),
        tags: "[]".into(),
        metadata: "{}".into(),
        sensitive: Some(false),
        project: Some(Some(PROJECT.into())),
    }
}

/// Make `op`, a memory write, in one transaction, writing `content` for
/// an add or update; the memory as it then reads.
fn write(store: &Store<'_>, op: &Op, content: Option<String>) -> Result<Landed> {
    let text = || content.clone().ok_or("an add or update needs its content");
    let id = match op {
        Op::Add { .. } => {
            let input = MemoryAddInput {
                content: text()?,
                category: CATEGORY.into(),
                tags: Vec::new(),
                metadata: json!({}),
                sensitive: false,
                project: Some(PROJECT.into()),
                extract: false,
                ..MemoryAddInput::default()
            };
            store.transaction(|s| queries::add_memory(s, input))?.id
        }
        Op::Update { id, .. } => {
            let input: MemoryUpdateInput = serde_json::from_value(json!({
                "memory_id": id,
                "content": text()?,
            }))?;
            let outcome = store.transaction(|s| queries::update_memory(s, &input))?;
            if !matches!(outcome, UpdateOutcome::Updated(_)) {
                return Err(format!("the update of {id} wrote nothing: {outcome:?}").into());
            }
            id.clone()
        }
        Op::Delete { id, .. } => {
            if !store.transaction(|s| queries::delete_memory(s, id))? {
                return Err(format!("the delete of {id} found no live memory").into());
            }
            id.clone()
        }
        Op::Page { .. } => return Err("a page is not a memory write".into()),
    };
    Ok(Landed {
        after: snapshot(store, &id)?,
        id,
    })
}

/// Print `line` and flush it.
fn emit(out: &mut impl Write, line: &Line) -> Result<()> {
    serde_json::to_writer(&mut *out, line)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

/// SQLite's integrity check on the closed store in `dir`: `PRAGMA
/// integrity_check`, then FTS5's `'integrity-check'` on each index, which
/// the PRAGMA does not cover for external content (D43, D56).
pub fn verify_sqlite(dir: &Path) -> Result<Value> {
    use rusqlite::{Connection, OpenFlags};
    let conn =
        Connection::open_with_flags(dir.join(SQLITE_FILE), OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let rows = conn
        .prepare("PRAGMA integrity_check")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    if rows != ["ok"] {
        return Err(format!("PRAGMA integrity_check: {}", rows.join("; ")).into());
    }
    for table in SQLITE_FTS {
        conn.execute(
            &format!("INSERT INTO {table}({table}) VALUES ('integrity-check')"),
            [],
        )
        .map_err(|e| format!("{table} integrity-check: {e}"))?;
    }
    Ok(json!({ "check": "sqlite integrity_check, fts5 integrity-check", "fts": SQLITE_FTS }))
}

/// The engine's check on the closed store in `dir`: it has no verify, so
/// a clean open (not needing recovery) and a read of every memory and its
/// revisions through the ports (D56).
pub fn verify_engine(dir: &Path) -> Result<Value> {
    let tables = EngineTables::open(dir)?;
    if tables.transaction_state() == TransactionState::NeedsRecovery {
        return Err("the engine opened needing recovery".into());
    }
    let db = Database::open_backend(Box::new(tables), None)?;
    let store = db.store();
    let ids = listed(&store, None)?;
    let history = Revisions::new(&store);
    let mut revisions = 0;
    for id in &ids {
        queries::get_memory_by_id(&store, id)?.ok_or_else(|| format!("{id} listed, not read"))?;
        revisions += history.list(id, MAX_REVISIONS)?.len();
    }
    Ok(json!({
        "check": "engine clean open, full read",
        "memories": ids.len(),
        "revisions": revisions,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bodies(name: &str) -> (std::path::PathBuf, Bodies) {
        let dir =
            std::env::temp_dir().join(format!("store_bench_crash_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (dir.clone(), Bodies::new(dir))
    }

    /// An acked add, as the child reports it, from the in-memory store.
    fn add(store: &Store<'_>, model: &mut Model, n: usize) -> String {
        let (op, content) = next_write(store, &[], 7, n, &Corpus::new(), &mut Rng(3)).unwrap();
        let landed = write(store, &op, content).unwrap();
        let id = landed.id.clone();
        let mut failures = Vec::new();
        model.acked(op, Some(landed), &mut failures);
        assert!(failures.is_empty(), "{failures:?}");
        id
    }

    #[test]
    fn a_cut_or_foreign_line_is_not_a_protocol_line() {
        assert_eq!(parse(b"{\"ev\":\"ready\"}\n").unwrap(), Some(Line::Ready));
        assert_eq!(parse(b"{\"ev\":\"ready\"}").unwrap(), None);
        assert_eq!(parse(b"running 1 test\n").unwrap(), None);
        assert!(parse(b"{\"ev\":\"nonsense\"}\n").is_err());
    }

    #[test]
    fn an_unacked_write_is_in_flight_and_acks_must_pair() {
        let mut model = Model::default();
        let mut failures = Vec::new();
        let page = |n: usize| Line::Begin {
            n,
            op: Op::Page {
                slug: format!("p{n}"),
                key: "k".into(),
            },
        };
        let lines = vec![
            Line::Ready,
            page(0),
            Line::Ack { n: 0, landed: None },
            page(1),
        ];
        let lines = lines.into_iter().skip(1).collect();
        let in_flight = model.replay(lines, &mut failures);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(model.pages.len(), 1);
        assert_eq!(model.next_page, 2);
        assert_eq!(in_flight.map(|op| op.kind()), Some("page"));
        let in_flight = model.replay(vec![Line::Ack { n: 9, landed: None }], &mut failures);
        assert!(in_flight.is_none());
        assert_eq!(failures.len(), 1, "{failures:?}");
    }

    /// The check passes on the writes it modelled, and finds an edit made
    /// behind its back, a revision left without its row change, and a
    /// page whose row never landed (its blob an orphan).
    #[test]
    fn the_check_finds_partial_and_unmodelled_writes() {
        let _serial = super::super::serial();
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let (dir, bodies) = bodies("check");
        let mut model = Model::default();
        let first = add(&store, &mut model, 0);
        let second = add(&store, &mut model, 1);
        let mut failures = Vec::new();
        model.check(&store, &bodies, &mut failures).unwrap();
        assert!(failures.is_empty(), "{failures:?}");

        // An in-flight update that wrote its revision and not its row.
        let before = model.memories[&first].snapshot.clone();
        let current = Revisions::new(&store).current(&first).unwrap().unwrap();
        Revisions::new(&store)
            .insert(&first, &current, "2026-11-23T00:00:00Z", None)
            .unwrap();
        let op = Op::Update {
            id: first.clone(),
            nonce: "crashnonceneverwritten".into(),
            before,
            row: "the row it never wrote".into(),
        };
        assert!(model.settle(&store, &bodies, op, &mut failures).unwrap());
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("partial"), "{failures:?}");

        // An unacked edit made behind the model's back.
        failures.clear();
        let input: MemoryUpdateInput =
            serde_json::from_value(json!({ "memory_id": second, "content": "rewritten" })).unwrap();
        queries::update_memory(&store, &input).unwrap();
        model.check(&store, &bodies, &mut failures).unwrap();
        assert!(
            failures.iter().any(|f| f.starts_with(&second)),
            "{failures:?}"
        );

        // A page whose blob was written and whose row was not.
        failures.clear();
        let key = bodies.stash("a page body").unwrap().key;
        let op = Op::Page {
            slug: "crash-page-0".into(),
            key,
        };
        assert!(!model.settle(&store, &bodies, op, &mut failures).unwrap());
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(model.orphan_blobs, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_update_must_add_exactly_the_old_row_as_a_revision() {
        let before = Snapshot {
            revisions: vec!["x".into()],
            ..added("a")
        };
        let mut after = Snapshot {
            revisions: vec!["x".into(), "a".into()],
            ..added("b")
        };
        assert_eq!(unlike(&updated(&before, "b"), &after), None);
        assert!(
            unlike(&updated(&before, "c"), &after).is_some(),
            "another row"
        );
        after.revisions.push("a".into());
        assert!(
            unlike(&updated(&before, "b"), &after).is_some(),
            "a revision too many"
        );
    }

    /// A stream with no in-flight kill makes the run insufficient; its
    /// counts stay as they were.
    #[test]
    fn a_stream_without_an_in_flight_kill_is_insufficient() {
        let round = |stream: &str, in_flight: Option<&str>, landed: Option<bool>| {
            json!({
                "stream": stream,
                "in_flight": in_flight,
                "check": { "in_flight_landed": landed },
            })
        };
        let rounds = vec![
            round("writes", Some("update"), Some(false)),
            round("writes", None, None),
            round("pages", None, None),
            round("pages", None, None),
        ];
        let kills = where_kills_fell(&rounds);
        assert_eq!(kills["pages"]["total"], 2);
        assert_eq!(kills["pages"]["between_writes"], 2);
        assert_eq!(kills["writes"]["inside_a_write_not_landed"], 1);
        assert_eq!(insufficient(&kills), ["pages"]);
        let landed = vec![
            round("writes", Some("add"), Some(true)),
            round("pages", Some("page"), Some(false)),
        ];
        assert!(insufficient(&where_kills_fell(&landed)).is_empty());
    }

    #[test]
    fn a_delete_leaves_no_live_row_no_revisions_and_no_text() {
        let gone = Snapshot {
            state: State::Absent,
            row: None,
            emptied: false,
            revisions: vec![],
        };
        assert_eq!(deleted(&gone), None);
        let tombstone = Snapshot {
            state: State::Tombstone,
            row: Some("t".into()),
            emptied: true,
            revisions: vec![],
        };
        assert_eq!(deleted(&tombstone), None);
        let holding_text = Snapshot {
            emptied: false,
            ..tombstone
        };
        assert!(deleted(&holding_text).is_some());
        assert!(deleted(&Snapshot {
            revisions: vec!["x".into()],
            ..gone.clone()
        })
        .is_some());
        assert!(deleted(&added("a")).is_some());
    }

    /// On a store that syncs, a delete leaves a tombstone. Through the
    /// crash stream's own delete (`queries::delete_memory`) it is in the
    /// canonical emptied form on the engine and on SQLite, the acked
    /// delete passes and the model checks clean; a tombstone still holding
    /// text or tags is refused.
    #[test]
    fn a_synced_delete_leaves_a_canonical_tombstone() {
        let _serial = super::super::serial();
        let (dir, bodies) = bodies("tombstone");
        let engine = Database::open_backend(
            Box::new(remind_me_core::db::engine::EngineTables::open(&dir.join("engine")).unwrap()),
            None,
        )
        .unwrap();
        // Not `Database::open_in_memory`: that is the engine again.
        let sqlite = Database::open_backend(
            Box::new(remind_me_core::db::sqlite::SqliteTables::open_in_memory().unwrap()),
            None,
        )
        .unwrap();
        for (backend, db) in [("engine", &engine), ("sqlite", &sqlite)] {
            let store = db.store();
            remind_me_core::testing::mark_store_syncing(&store).unwrap();
            let mut model = Model::default();
            let id = add(&store, &mut model, 0);
            let op = Op::Delete {
                id: id.clone(),
                before: snapshot(&store, &id).unwrap(),
            };
            let landed = write(&store, &op, None).unwrap();
            assert_eq!(landed.after.state, State::Tombstone, "{backend}");
            assert!(landed.after.emptied, "{backend}: {:?}", landed.after);
            assert_eq!(deleted(&landed.after), None, "{backend}");
            let mut failures = Vec::new();
            model.acked(op, Some(landed), &mut failures);
            model.check(&store, &bodies, &mut failures).unwrap();
            assert!(failures.is_empty(), "{backend}: {failures:?}");
            for (column, value) in [("content", "still here"), ("tags", "[\"kept\"]")] {
                remind_me_core::testing::set_memory_column(&store, &id, column, value).unwrap();
                let after = snapshot(&store, &id).unwrap();
                assert!(
                    deleted(&after).is_some(),
                    "{backend}: a tombstone with {column}"
                );
                let canonical = if column == "content" {
                    TOMBSTONE_CONTENT
                } else {
                    "[]"
                };
                remind_me_core::testing::set_memory_column(&store, &id, column, canonical).unwrap();
                assert_eq!(deleted(&snapshot(&store, &id).unwrap()), None, "{backend}");
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An add's row is exactly the one its `begin` line names; a write
    /// that lands another row, or an in-flight one left with another
    /// row, fails though its nonce is there.
    #[test]
    fn an_add_must_land_exactly_its_intended_row() {
        let _serial = super::super::serial();
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let (dir, bodies) = bodies("exact");
        let mut model = Model::default();
        let (op, content) = next_write(&store, &[], 7, 0, &Corpus::new(), &mut Rng(3)).unwrap();
        let Op::Add { nonce, .. } = op.clone() else {
            panic!("an empty store starts with an add");
        };
        let landed = write(&store, &op, content.clone()).unwrap();
        let mut failures = Vec::new();
        model.acked(op.clone(), Some(landed), &mut failures);
        assert!(failures.is_empty(), "{failures:?}");

        // The same nonce, another row.
        let wrong = Op::Add {
            nonce: nonce.clone(),
            row: hash(&add_row(&format!("{nonce} other text"))),
        };
        let landed = write(&store, &wrong, content).unwrap();
        model.acked(wrong, Some(landed), &mut failures);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("acked add"), "{failures:?}");
        let _ = std::fs::remove_dir_all(dir);
        drop(bodies);
    }
}
