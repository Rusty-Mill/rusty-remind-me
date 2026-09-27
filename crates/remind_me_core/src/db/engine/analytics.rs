//! Analytics snapshots on the engine (ADR-0023, phase 4f): one row per day
//! of vitality and category counts. [`crate::db::stats`] calls these when
//! its store carries engine tables; every other read it makes stays on
//! SQLite until memories move.
//!
//! A snapshot's id is an integer, as SQLite's `INTEGER PRIMARY KEY` gives
//! it, issued by the journal's `analytics_snapshots` sequence so an id is
//! never issued twice, even across a crash.

use super::{engine_error, EngineTables};
use crate::db::stats::{decode_json, encode_json};
use crate::db::Result;
use crate::models::AnalyticsSnapshot;
use rusty_multimodal_db_engine::generic::query::{AllIds, FilterEq, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};

/// The journal sequence snapshot ids come from.
pub(crate) const SEQUENCE: &str = "analytics_snapshots";

/// Index marker: a snapshot by the UTC day it was captured on.
pub struct ByDay;
/// Slot marker: how many memories the snapshot counted.
pub struct Total;

pub(crate) type SnapshotTable = GenericMmapStore<SnapshotRecord, ByDay, Total>;

/// One `analytics_snapshots` row. The two maps stay JSON text, as in
/// SQLite, so a malformed value reads back the same way on both backends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotRecord {
    id: i64,
    /// `date(captured_at)` as SQLite computes it, or empty when it cannot.
    day: String,
    captured_at: String,
    total_memories: i64,
    vitality_buckets: String,
    category_counts: String,
}

impl SnapshotRecord {
    fn to_snapshot(&self) -> AnalyticsSnapshot {
        AnalyticsSnapshot {
            captured_at: self.captured_at.clone(),
            total_memories: self.total_memories,
            vitality_buckets: decode_json(&self.vitality_buckets),
            category_counts: decode_json(&self.category_counts),
        }
    }
}

impl Record for SnapshotRecord {
    type Id = i64;
    fn id(&self) -> i64 {
        self.id
    }
}

impl SchemaTag for SnapshotRecord {
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::SnapshotRecord@1";
}

impl IndexedField<ByDay> for SnapshotRecord {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.day
    }
}

impl ScannableField<Total> for SnapshotRecord {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.total_memories
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.total_memories = value;
    }
}

/// The id of a snapshot captured on `date` (`YYYY-MM-DD`, UTC), if any.
/// When several were, the lowest id, which is the first captured.
pub(crate) fn snapshot_on(tables: &EngineTables, date: &str) -> Option<i64> {
    FilterEq::<SnapshotRecord, ByDay>::filter_eq(&tables.snapshots, &date.to_string())
        .into_iter()
        .min()
}

/// Store `snapshot` under the next id from the journal, and return it.
///
/// The counter is made durable before the record is written, so a crash
/// between the two leaves a gap in the ids, never a reissued one.
pub(crate) fn insert(tables: &mut EngineTables, snapshot: &AnalyticsSnapshot) -> Result<i64> {
    let id = tables.next_id(SEQUENCE)?;
    let record = SnapshotRecord {
        id,
        day: utc_day(&snapshot.captured_at),
        captured_at: snapshot.captured_at.clone(),
        total_memories: snapshot.total_memories,
        vitality_buckets: encode_json(&snapshot.vitality_buckets),
        category_counts: encode_json(&snapshot.category_counts),
    };
    tables.snapshots.insert(record).map_err(engine_error)?;
    Ok(id)
}

/// Every snapshot, `captured_at` oldest first as SQLite compares the text,
/// ties in id order.
pub(crate) fn snapshots(tables: &EngineTables) -> Vec<AnalyticsSnapshot> {
    let mut records: Vec<SnapshotRecord> = tables
        .snapshots
        .all_ids()
        .into_iter()
        .filter_map(|id| tables.snapshots.get(id))
        .collect();
    records.sort_by(|a, b| (a.captured_at.as_str(), a.id).cmp(&(b.captured_at.as_str(), b.id)));
    records.iter().map(SnapshotRecord::to_snapshot).collect()
}

/// The highest snapshot id stored, or 0: the floor for the sequence when
/// the tables open.
pub(crate) fn max_id(table: &SnapshotTable) -> u64 {
    table
        .all_ids()
        .into_iter()
        .max()
        .map_or(0, |id| u64::try_from(id).unwrap_or(0))
}

/// `date(timestamp)` as SQLite computes it: the UTC calendar day of an RFC
/// 3339 timestamp, or of a plain `YYYY-MM-DD[ T]HH:MM:SS` or `YYYY-MM-DD`
/// one taken as UTC. Empty when it is none of those, as SQLite's is `NULL`,
/// which no day matches.
fn utc_day(timestamp: &str) -> String {
    use chrono::{NaiveDate, NaiveDateTime};
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(timestamp) {
        return t.naive_utc().date().to_string();
    }
    for format in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(timestamp, format) {
            return t.date().to_string();
        }
    }
    NaiveDate::parse_from_str(timestamp, "%Y-%m-%d")
        .map(|d| d.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(captured_at: &str, total: i64) -> AnalyticsSnapshot {
        AnalyticsSnapshot {
            captured_at: captured_at.to_string(),
            total_memories: total,
            vitality_buckets: Default::default(),
            category_counts: Default::default(),
        }
    }

    #[test]
    fn the_day_is_the_utc_day_sqlite_would_compute() {
        assert_eq!(utc_day("2026-09-27T01:00:00+05:00"), "2026-09-26");
        assert_eq!(utc_day("2026-09-27T23:30:00-02:00"), "2026-09-28");
        assert_eq!(utc_day("2026-09-27 10:00:00"), "2026-09-27");
        assert_eq!(utc_day("2026-09-27"), "2026-09-27");
        assert_eq!(utc_day("not a time"), "");
    }

    #[test]
    fn ids_keep_rising_across_a_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "remind_me_engine_snapshots_reopen_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let first = {
            let mut tables = EngineTables::open(&dir).unwrap();
            insert(&mut tables, &snap("2026-09-25T10:00:00+00:00", 1)).unwrap()
        };
        let mut tables = EngineTables::open(&dir).unwrap();
        let second = insert(&mut tables, &snap("2026-09-26T10:00:00+00:00", 2)).unwrap();
        assert!(second > first, "{second} must follow {first}");
        assert_eq!(snapshot_on(&tables, "2026-09-25"), Some(first));
        assert_eq!(snapshots(&tables).len(), 2);
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
