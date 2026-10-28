//! `sync_log` on the engine (ADR-0023, phase 4e): each remote's pull
//! cursors and liveness stamps. [`crate::db::sync_state`] calls these when
//! its store carries engine tables; `sync_flags` and `sync_sends` move with
//! the outbox, in the memories core (`db::engine::outbox`).
//!
//! Every write is a read-modify-write of the remote's whole row, which is
//! what the SQL's `INSERT … ON CONFLICT DO UPDATE` does column by column. A
//! remote with no row starts from the schema's defaults.

use super::{engine_error, engine_id, ensure_same_id, EngineTables};
use crate::db::sync_state::{RemoteLog, SyncLogRow};
use crate::db::Result;
use rusty_multimodal_db_engine::generic::query::{AllIds, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Index marker: a row by its remote id, which is also its key.
pub struct ByRemote;
/// Slot marker: the `hub_seq` pull cursor.
pub struct PullSeq;

pub(crate) type SyncLogTable = GenericMmapStore<SyncLogRecord, ByRemote, PullSeq>;

/// One `sync_log` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncLogRecord {
    engine_id: Uuid,
    remote_id: String,
    last_pull: String,
    last_push: String,
    last_pull_id: String,
    last_attempt_at: String,
    last_push_at: String,
    last_pull_at: String,
    last_pull_seq: i64,
}

impl SyncLogRecord {
    fn new(row: &SyncLogRow) -> Self {
        Self {
            engine_id: engine_id(&row.remote_id),
            remote_id: row.remote_id.clone(),
            last_pull: row.last_pull.clone(),
            last_push: row.last_push.clone(),
            last_pull_id: row.last_pull_id.clone(),
            last_attempt_at: row.last_attempt_at.clone(),
            last_push_at: row.last_push_at.clone(),
            last_pull_at: row.last_pull_at.clone(),
            last_pull_seq: row.last_pull_seq,
        }
    }

    fn to_row(&self) -> SyncLogRow {
        SyncLogRow {
            remote_id: self.remote_id.clone(),
            last_pull: self.last_pull.clone(),
            last_push: self.last_push.clone(),
            last_pull_id: self.last_pull_id.clone(),
            last_attempt_at: self.last_attempt_at.clone(),
            last_push_at: self.last_push_at.clone(),
            last_pull_at: self.last_pull_at.clone(),
            last_pull_seq: self.last_pull_seq,
        }
    }
}

impl Record for SyncLogRecord {
    type Id = Uuid;
    fn id(&self) -> Uuid {
        self.engine_id
    }
}

impl SchemaTag for SyncLogRecord {
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::SyncLogRecord@1";
}

impl IndexedField<ByRemote> for SyncLogRecord {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.remote_id
    }
}

impl ScannableField<PullSeq> for SyncLogRecord {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.last_pull_seq
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.last_pull_seq = value;
    }
}

/// `remote_id`'s row, if it has one.
pub(crate) fn row(tables: &EngineTables, remote_id: &str) -> Option<SyncLogRow> {
    tables
        .sync_log
        .get(engine_id(remote_id))
        .filter(|r| r.remote_id == remote_id)
        .map(|r| r.to_row())
}

/// Store `row` whole, replacing any row for its remote.
pub(crate) fn put(tables: &mut EngineTables, row: &SyncLogRow) -> Result<()> {
    let record = SyncLogRecord::new(row);
    match tables.sync_log.get(record.engine_id) {
        Some(stored) => {
            ensure_same_id(&stored.remote_id, &row.remote_id)?;
            tables.sync_log.replace(record).map_err(engine_error)
        }
        None => tables.sync_log.insert(record).map_err(engine_error),
    }
}

/// Apply `change` to `remote_id`'s row, starting from the schema's defaults
/// if it has none: an `INSERT … ON CONFLICT DO UPDATE`.
pub(crate) fn upsert(
    tables: &mut EngineTables,
    remote_id: &str,
    change: impl FnOnce(&mut SyncLogRow),
) -> Result<()> {
    let mut current = row(tables, remote_id).unwrap_or_else(|| SyncLogRow::new(remote_id));
    change(&mut current);
    put(tables, &current)
}

/// Apply `change` to `remote_id`'s row if it has one: an `UPDATE`. Whether
/// there was a row.
pub(crate) fn update(
    tables: &mut EngineTables,
    remote_id: &str,
    change: impl FnOnce(&mut SyncLogRow),
) -> Result<bool> {
    let Some(mut current) = row(tables, remote_id) else {
        return Ok(false);
    };
    change(&mut current);
    put(tables, &current)?;
    Ok(true)
}

/// Every remote's liveness stamps, by remote id in byte order.
pub(crate) fn remotes(tables: &EngineTables) -> Vec<RemoteLog> {
    let mut rows: Vec<SyncLogRecord> = tables
        .sync_log
        .all_ids()
        .into_iter()
        .filter_map(|id| tables.sync_log.get(id))
        .collect();
    rows.sort_by(|a, b| a.remote_id.cmp(&b.remote_id));
    rows.into_iter()
        .map(|r| RemoteLog {
            remote_id: r.remote_id,
            last_attempt_at: r.last_attempt_at,
            last_push_at: r.last_push_at,
            last_pull_at: r.last_pull_at,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_survive_a_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "remind_me_engine_sync_log_reopen_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let written = SyncLogRow {
            last_pull_seq: 42,
            last_pull_id: "m9".to_string(),
            ..SyncLogRow::new("hub")
        };
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            put(&mut tables, &written).unwrap();
        }
        let tables = crate::db::engine::reopen(&dir);
        assert_eq!(row(&tables, "hub"), Some(written));
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
