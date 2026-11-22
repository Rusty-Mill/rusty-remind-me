//! R3's Attest candidate (D47): Attest's append-only log is the durable
//! store; the engine's own tables, full-text and vector indexes live in a
//! temporary directory and are rebuilt from the log at open. Every write
//! the engine makes durable reaches the log through the engine's change
//! hook (D48) as one Attest transaction, before the write is acknowledged.
//!
//! What this costs is what R3 measures: the log append on every write, the
//! replay at open, and log growth (updates and deletes are superseding
//! records; Attest reclaims nothing). The temporary engine's own files
//! count against Attest too, which the write-up says.

use attest::domain::log::{Envelope, Record, COMMIT};
use attest::store::file::{FileStore, StoreConfig};
use attest::{Log, LogError};
use remind_me_core::db::engine::capture::{CapturedChange, ChangeSink};
use remind_me_core::db::engine::EngineTables;
use remind_me_core::db::{Result, StoreError};
use rusty_serde::Value;
use std::path::Path;

/// The record kind every captured write is stored under.
pub const KIND: &str = "remind_me.write";
const WRITER: &str = "remind-me-r3";
/// The file [`open_at`] leaves in an engine directory it made, naming the
/// log it was made for; it clears only a directory whose mark names the
/// same log.
pub const SCRATCH_MARK: &str = "ATTEST_R3_SCRATCH";
/// The lock file Attest keeps in its directory.
const LOCK: &str = "LOCK";

/// Engine tables over an Attest log in `dir`: replays the log onto fresh
/// temporary tables, then hands every later write to the log.
///
/// # Errors
///
/// The log's error (locked, corrupt) or the engine's.
pub fn open(dir: &Path) -> Result<EngineTables> {
    replay(dir, EngineTables::open_temporary()?)
}

/// As [`open`], with the engine's tables in `engine` instead of a hidden
/// temporary directory, so the benchmark can count their disk (D48, D53).
/// The tables are always rebuilt from the log alone, so `engine` is
/// cleared first, but only scratch this function made for this same log:
/// a directory whose [`SCRATCH_MARK`] is missing or names another log
/// (another run's) is refused and left untouched.
///
/// # Errors
///
/// As [`open`]; `engine` existing without a mark naming this log; or the
/// filesystem's
/// error clearing or creating it.
pub fn open_at(dir: &Path, engine: &Path) -> Result<EngineTables> {
    // Take the log's lock before touching `engine`, so a second opener of
    // the same log is refused before it can clear the first one's tables.
    let log = FileStore::open(dir, StoreConfig::default()).map_err(log_error)?;
    let owner = dir.canonicalize().map_err(io_error)?.display().to_string();
    if engine.exists() {
        let mark = std::fs::read_to_string(engine.join(SCRATCH_MARK)).ok();
        if mark.as_deref() != Some(owner.as_str()) {
            return Err(StoreError::Engine(format!(
                "attest: {} exists and is not scratch made for {owner}; refusing to clear it",
                engine.display()
            )));
        }
        std::fs::remove_dir_all(engine).map_err(io_error)?;
    }
    std::fs::create_dir_all(engine).map_err(io_error)?;
    std::fs::write(engine.join(SCRATCH_MARK), owner).map_err(io_error)?;
    replay_into(log, EngineTables::open(engine)?)
}

/// Copy the log in `dir` (its segment directories) into `dest`, a new
/// directory, leaving out the lock file, which only names its holder.
/// With no write in flight (the caller holds the store), the copy is the
/// whole durable store; the engine's tables are not in it, since an open
/// rebuilds them (D52).
///
/// # Errors
///
/// The filesystem's error.
pub fn package(dir: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir(dest).map_err(io_error)?;
    for entry in std::fs::read_dir(dir).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let target = dest.join(entry.file_name());
        if entry.file_name() == LOCK {
            continue;
        }
        if entry.file_type().map_err(io_error)?.is_dir() {
            package(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target).map_err(io_error)?;
        }
    }
    Ok(())
}

fn replay(dir: &Path, tables: EngineTables) -> Result<EngineTables> {
    let log = FileStore::open(dir, StoreConfig::default()).map_err(log_error)?;
    replay_into(log, tables)
}

/// Replay `log` onto empty `tables`, then hand every later write to it.
fn replay_into(log: FileStore, mut tables: EngineTables) -> Result<EngineTables> {
    let records = log.scan().map_err(log_error)?;
    let mut changes = Vec::with_capacity(records.len());
    for record in &records {
        if record.envelope.kind == KIND {
            changes.push(captured(record)?);
        }
    }
    tables.apply_captured(changes)?;
    let next = records.last().map_or(1, |r| r.envelope.lsn + 1);
    let txn = records.last().map_or(1, |r| r.envelope.txn + 1);
    tables.set_change_sink(Some(Box::new(AttestSink { log, next, txn })));
    Ok(tables)
}

/// Appends each hand-over as one transaction: a record per write, then
/// the commit record.
struct AttestSink {
    log: FileStore,
    next: u64,
    txn: u64,
}

impl ChangeSink for AttestSink {
    fn committed(&mut self, changes: Vec<CapturedChange>) -> Result<()> {
        let mut records = Vec::with_capacity(changes.len() + 1);
        for change in &changes {
            records.push(self.record(KIND, Some(content(change)?)));
        }
        records.push(self.record(COMMIT, None));
        self.log.append(records).map_err(log_error)?;
        self.txn += 1;
        Ok(())
    }
}

impl AttestSink {
    fn record(&mut self, kind: &str, content: Option<Value>) -> Record {
        let lsn = self.next;
        self.next += 1;
        Record {
            envelope: Envelope {
                lsn,
                txn: self.txn,
                kind: kind.to_string(),
                id: format!("w{lsn}"),
                body: None,
                writer: WRITER.to_string(),
                at: String::new(),
                idem: None,
                req: None,
            },
            content,
        }
    }
}

/// A write as a record body: its table, and its key and record as the
/// JSON text the engine gave.
fn content(change: &CapturedChange) -> Result<Value> {
    let text = |bytes: &[u8]| {
        String::from_utf8(bytes.to_vec()).map_err(|e| StoreError::Engine(format!("attest: {e}")))
    };
    let value = match &change.value {
        Some(v) => Value::String(text(v)?),
        None => Value::Null,
    };
    Ok(Value::Map(vec![
        ("table".into(), Value::String(change.table.clone())),
        ("key".into(), Value::String(text(&change.key)?)),
        ("value".into(), value),
    ]))
}

/// A stored record back as the write it holds.
fn captured(record: &Record) -> Result<CapturedChange> {
    let malformed = || {
        StoreError::Engine(format!(
            "attest: record {} is malformed",
            record.envelope.lsn
        ))
    };
    let body = record.content.as_ref().ok_or_else(malformed)?;
    let field = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_string);
    Ok(CapturedChange {
        table: field("table").ok_or_else(malformed)?,
        key: field("key").ok_or_else(malformed)?.into_bytes(),
        value: field("value").map(String::into_bytes),
    })
}

fn io_error(e: std::io::Error) -> StoreError {
    StoreError::Engine(format!("attest: {e}"))
}

fn log_error(e: LogError) -> StoreError {
    StoreError::Engine(format!("attest: {e}"))
}
