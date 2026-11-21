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

/// Engine tables over an Attest log in `dir`: replays the log onto fresh
/// temporary tables, then hands every later write to the log.
///
/// # Errors
///
/// The log's error (locked, corrupt) or the engine's.
pub fn open(dir: &Path) -> Result<EngineTables> {
    let log = FileStore::open(dir, StoreConfig::default()).map_err(log_error)?;
    let records = log.scan().map_err(log_error)?;
    let mut tables = EngineTables::open_temporary()?;
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
    let malformed = || StoreError::Engine(format!("attest: record {} is malformed", record.envelope.lsn));
    let body = record.content.as_ref().ok_or_else(malformed)?;
    let field = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_string);
    Ok(CapturedChange {
        table: field("table").ok_or_else(malformed)?,
        key: field("key").ok_or_else(malformed)?.into_bytes(),
        value: field("value").map(String::into_bytes),
    })
}

fn log_error(e: LogError) -> StoreError {
    StoreError::Engine(format!("attest: {e}"))
}
