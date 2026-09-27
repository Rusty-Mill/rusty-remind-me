//! The sync outbox, its send markers and the sync flags on the engine
//! (ADR-0023, core PR 1, built dark). [`crate::db::outbox`] and
//! [`crate::db::sync_state`] call these when their store carries the core
//! tables.
//!
//! An outbox entry's id is an integer, as SQLite's `INTEGER PRIMARY KEY`
//! gives it, issued by the journal's `sync_outbox` sequence. An entry is
//! committed in the same journal batch as the write it records, so the two
//! land together or not at all.

use super::core::{Change, CoreTables};
use super::{core_ref, engine_id, pair_engine_id, EngineTables};
use crate::db::outbox::OutboxEntry;
use crate::db::Result;
use rusty_multimodal_db_engine::generic::query::{AllIds, FilterEq, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use uuid::Uuid;

/// The journal sequence outbox ids come from.
pub(crate) const SEQUENCE: &str = "sync_outbox";

/// The flag that turns the outbox on.
const SYNC_ENABLED: &str = "sync_enabled";

/// Index marker: a record by its string key.
pub struct ByKey;
/// Slot marker: the record's one integer column.
pub struct Slot;

pub(crate) type OutboxTable = GenericMmapStore<OutboxRecord, ByKey, Slot>;
pub(crate) type SendTable = GenericMmapStore<SendRecord, ByKey, Slot>;
pub(crate) type FlagTable = GenericMmapStore<FlagRecord, ByKey, Slot>;

/// One `sync_outbox` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboxRecord {
    pub(crate) id: i64,
    /// The key the entry was queued under: a memory's id, or for a link its
    /// memory's id.
    pub(crate) memory_id: String,
    pub(crate) operation: String,
    pub(crate) payload: String,
    pub(crate) created_at: String,
    /// Kept for the column's sake: nothing marks an entry sent any more,
    /// `sync_sends` records sends per remote.
    pub(crate) sent_at: String,
}

/// One `sync_sends` row: `outbox_id` went to `remote_id` at `sent_at`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendRecord {
    engine_id: Uuid,
    remote_id: String,
    outbox_id: i64,
    sent_at: String,
}

/// One `sync_flags` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlagRecord {
    engine_id: Uuid,
    key: String,
    value: String,
    slot: i64,
}

macro_rules! record {
    ($record:ty, $id:ty, $id_field:ident, $tag:literal, $key:ident, $slot:ident) => {
        impl Record for $record {
            type Id = $id;
            fn id(&self) -> $id {
                self.$id_field
            }
        }
        impl SchemaTag for $record {
            const SCHEMA_TAG: &'static str = $tag;
        }
        impl IndexedField<ByKey> for $record {
            type IndexValue = String;
            fn indexed_value(&self) -> &String {
                &self.$key
            }
        }
        impl ScannableField<Slot> for $record {
            type ScanValue = i64;
            fn scannable_value(&self) -> i64 {
                self.$slot
            }
            fn set_scannable_value(&mut self, value: i64) {
                self.$slot = value;
            }
        }
    };
}

record!(
    OutboxRecord,
    i64,
    id,
    "rusty_remind_me::node::OutboxRecord@1",
    memory_id,
    id
);
record!(
    SendRecord,
    Uuid,
    engine_id,
    "rusty_remind_me::node::SendRecord@1",
    remote_id,
    outbox_id
);
record!(
    FlagRecord,
    Uuid,
    engine_id,
    "rusty_remind_me::node::FlagRecord@1",
    key,
    slot
);

/// Now, in the shape SQLite's `strftime('%Y-%m-%dT%H:%M:%f000', 'now') ||
/// '+00:00'` gives every outbox stamp.
fn now_iso() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3f000+00:00")
        .to_string()
}

// --- flags ----------------------------------------------------------------

/// The flag `key`'s value, if set.
pub(crate) fn flag(core: &CoreTables, key: &str) -> Option<String> {
    core.flags
        .get(engine_id(key))
        .filter(|f| f.key == key)
        .map(|f| f.value)
}

/// Whether the outbox is on, as `db::derived`'s gate reads it.
pub(crate) fn sync_enabled(core: &CoreTables) -> bool {
    flag(core, SYNC_ENABLED).as_deref() == Some("1")
}

pub(crate) fn set_flag(tables: &mut EngineTables, key: &str, value: &str) -> Result<()> {
    let record = FlagRecord {
        engine_id: engine_id(key),
        key: key.to_string(),
        value: value.to_string(),
        slot: 0,
    };
    tables.commit(vec![Change::Flag(record.engine_id, Some(record))])
}

// --- queueing -------------------------------------------------------------

/// A new outbox entry for `key`, under the next id, for the caller to
/// commit with the write it records.
pub(crate) fn entry(
    tables: &mut EngineTables,
    key: &str,
    operation: &str,
    payload: String,
) -> Result<Change> {
    let id = tables.allocate(SEQUENCE)?;
    Ok(Change::Outbox(
        id,
        Some(OutboxRecord {
            id,
            memory_id: key.to_string(),
            operation: operation.to_string(),
            payload,
            created_at: now_iso(),
            sent_at: String::new(),
        }),
    ))
}

/// Queue `payload` under `key` on its own, when sync is enabled: for the
/// graph's rows, which stay on SQLite for now.
pub(crate) fn queue(
    tables: &mut EngineTables,
    key: &str,
    operation: &str,
    payload: String,
) -> Result<()> {
    if !sync_enabled(core_ref(tables)?) {
        return Ok(());
    }
    let change = entry(tables, key, operation, payload)?;
    tables.commit(vec![change])
}

/// Queue every memory as an insert with the backfill's payload, then each of
/// `others` (key and payload), in one batch.
pub(crate) fn backfill(tables: &mut EngineTables, others: Vec<(String, String)>) -> Result<()> {
    let mut rows: Vec<_> = super::memories::rows(core_ref(tables)?).collect();
    rows.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    let queued = rows
        .into_iter()
        .map(|row| (row.id.clone(), row.backfill_payload().to_string()))
        .chain(others);
    let mut changes = Vec::new();
    for (key, payload) in queued {
        changes.push(entry(tables, &key, "insert", payload)?);
    }
    tables.commit(changes)
}

// --- reads ----------------------------------------------------------------

/// Every entry, oldest id first.
pub(crate) fn entries(core: &CoreTables) -> Vec<OutboxRecord> {
    let mut ids = core.outbox.all_ids();
    ids.sort_unstable();
    ids.into_iter()
        .filter_map(|id| core.outbox.get(id))
        .collect()
}

/// The outbox ids recorded as sent to `remote_id`.
fn sent_to(core: &CoreTables, remote_id: &str) -> HashSet<i64> {
    FilterEq::<SendRecord, ByKey>::filter_eq(&core.sends, &remote_id.to_string())
        .into_iter()
        .filter_map(|id| core.sends.get(id))
        .filter(|s| s.remote_id == remote_id)
        .map(|s| s.outbox_id)
        .collect()
}

pub(crate) fn unsent_to(
    tables: &EngineTables,
    remote_id: &str,
    after_id: i64,
    limit: usize,
) -> Result<Vec<OutboxEntry>> {
    let core = core_ref(tables)?;
    let sent = sent_to(core, remote_id);
    Ok(entries(core)
        .into_iter()
        .filter(|e| e.id > after_id && e.sent_at.is_empty() && !sent.contains(&e.id))
        .take(limit)
        .map(|e| OutboxEntry {
            id: e.id,
            key: e.memory_id,
            payload_json: e.payload,
        })
        .collect())
}

/// How many entries have no send to `remote_id`, and the oldest one's
/// `created_at` as SQLite's `MIN` compares the text.
pub(crate) fn pending_for(tables: &EngineTables, remote_id: &str) -> Result<(i64, Option<String>)> {
    let core = core_ref(tables)?;
    let sent = sent_to(core, remote_id);
    let pending: Vec<OutboxRecord> = entries(core)
        .into_iter()
        .filter(|e| !sent.contains(&e.id))
        .collect();
    let oldest = pending.iter().map(|e| e.created_at.clone()).min();
    Ok((count(pending.len()), oldest))
}

pub(crate) fn len(tables: &EngineTables) -> Result<i64> {
    Ok(count(core_ref(tables)?.outbox.all_ids().len()))
}

pub(crate) fn unsent_count(tables: &EngineTables) -> Result<i64> {
    let unsent = entries(core_ref(tables)?)
        .into_iter()
        .filter(|e| e.sent_at.is_empty())
        .count();
    Ok(count(unsent))
}

fn count(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

// --- removal --------------------------------------------------------------

/// Remove every sent entry and every one created before `cutoff`, and the
/// send markers left pointing at nothing, in one batch. How many entries
/// went.
pub(crate) fn prune(tables: &mut EngineTables, cutoff: &str) -> Result<usize> {
    let core = core_ref(tables)?;
    let (gone, kept): (Vec<OutboxRecord>, Vec<OutboxRecord>) = entries(core)
        .into_iter()
        .partition(|e| !e.sent_at.is_empty() || e.created_at.as_str() < cutoff);
    let kept: HashSet<i64> = kept.iter().map(|e| e.id).collect();
    let mut changes: Vec<Change> = gone.iter().map(|e| Change::Outbox(e.id, None)).collect();
    changes.extend(
        sends(core)
            .into_iter()
            .filter(|s| !kept.contains(&s.outbox_id))
            .map(|s| Change::Send(s.engine_id, None)),
    );
    tables.commit(changes)?;
    Ok(gone.len())
}

/// Remove every entry and send marker, in one batch.
pub(crate) fn clear(tables: &mut EngineTables) -> Result<()> {
    let core = core_ref(tables)?;
    let mut changes: Vec<Change> = core
        .outbox
        .all_ids()
        .into_iter()
        .map(|id| Change::Outbox(id, None))
        .collect();
    changes.extend(
        sends(core)
            .into_iter()
            .map(|s| Change::Send(s.engine_id, None)),
    );
    tables.commit(changes)
}

// --- sends ----------------------------------------------------------------

fn sends(core: &CoreTables) -> Vec<SendRecord> {
    core.sends
        .all_ids()
        .into_iter()
        .filter_map(|id| core.sends.get(id))
        .collect()
}

/// Record that `outbox_ids` went to `remote_id` at `at`, replacing any
/// earlier record of the same send, in one batch.
pub(crate) fn record_sends(
    tables: &mut EngineTables,
    remote_id: &str,
    outbox_ids: &[i64],
    at: &str,
) -> Result<()> {
    let changes = outbox_ids
        .iter()
        .map(|&outbox_id| {
            let engine_id = pair_engine_id(remote_id, &outbox_id.to_string());
            Change::Send(
                engine_id,
                Some(SendRecord {
                    engine_id,
                    remote_id: remote_id.to_string(),
                    outbox_id,
                    sent_at: at.to_string(),
                }),
            )
        })
        .collect();
    tables.commit(changes)
}

/// Every send marker as (outbox id, sent at), by outbox id: for tests.
#[cfg(test)]
pub(crate) fn sent(tables: &EngineTables) -> Vec<(i64, String)> {
    let mut sent: Vec<(i64, String)> = core_ref(tables)
        .map(sends)
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.outbox_id, s.sent_at))
        .collect();
    sent.sort();
    sent
}

/// The highest outbox id stored, or 0: the floor for the sequence when the
/// tables open.
pub(crate) fn max_id(table: &OutboxTable) -> u64 {
    table
        .all_ids()
        .into_iter()
        .max()
        .map_or(0, |id| u64::try_from(id).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stamp_has_the_shape_sqlite_gives_it() {
        let stamp = now_iso();
        assert_eq!(
            stamp.len(),
            "2026-09-27T10:00:00.123000+00:00".len(),
            "{stamp}"
        );
        assert!(stamp.ends_with("000+00:00"), "{stamp}");
    }

    #[test]
    fn prune_drops_old_and_sent_entries_and_their_markers() {
        let mut tables = EngineTables::open_temporary_with_core().unwrap();
        set_flag(&mut tables, SYNC_ENABLED, "1").unwrap();
        queue(&mut tables, "old", "insert", "{}".to_string()).unwrap();
        queue(&mut tables, "new", "insert", "{}".to_string()).unwrap();
        let first = entries(core_ref(&tables).unwrap())[0].id;
        record_sends(&mut tables, "hub", &[first], "t").unwrap();

        // Everything is older than a cutoff in the far future.
        assert_eq!(prune(&mut tables, "9999").unwrap(), 2);
        assert_eq!(len(&tables).unwrap(), 0);
        assert!(sent(&tables).is_empty());
    }
}
