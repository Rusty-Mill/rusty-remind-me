//! Storage for store-wide statistics: counts over live memories, the
//! store's own size and location, and the daily `analytics_snapshots`.
//!
//! Every read behind `stats.rs`, `status.rs` and `analytics.rs` goes
//! through here (ADR-0022), onto the engine (`db::engine::stats`,
//! `db::engine::analytics`, `db::engine::imports`). Those modules keep the
//! shapes they report and the rules (one snapshot per calendar day,
//! oldest-first trends, how sizes round).

use super::engine::{self, EngineLock};
use super::{Result, Store};
use crate::models::{AnalyticsSnapshot, DigestRecentMemory};
use crate::stats::RecentMemory;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// A column live memories can be grouped by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Category,
    Source,
    /// Memories with no project are left out of the counts.
    Project,
}

/// Where the store lives and how big it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageInfo {
    /// The database file the engine directory stands beside, or `None` for
    /// an in-memory database.
    pub path: Option<PathBuf>,
    /// The bytes of every file in the engine directory, so an in-memory
    /// database has a size too.
    pub size_bytes: i64,
    /// The schema version this build's records mirror: the one the last
    /// SQLite-storing build stamped (`db::legacy_sqlite::SCHEMA_VERSION`).
    /// The engine has no stamp of its own; every open brings its records to
    /// this build's layout.
    pub schema_version: i32,
}

/// A snapshot map as its column's JSON. Serialising a map of plain values
/// cannot fail; `{}` is the empty map if it somehow did.
pub(crate) fn encode_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".into())
}

/// A snapshot map from its column's JSON. A malformed value reads as empty,
/// so one bad row does not take the whole series down.
pub(crate) fn decode_json<T: serde::de::DeserializeOwned + Default>(json: &str) -> T {
    serde_json::from_str(json).unwrap_or_default()
}

/// The statistics queries, on the engine.
pub struct StoreStats<'c> {
    engine: &'c EngineLock,
    path: Option<PathBuf>,
}

impl<'c> StoreStats<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            engine: store.engine(),
            path: store.path().map(Path::to_path_buf),
        }
    }

    /// How many memories are not deleted.
    pub fn live_memories(&self) -> Result<i64> {
        engine::stats::live_memories(&self.engine.lock())
    }

    /// How many chat imports are recorded.
    pub fn imports(&self) -> Result<i64> {
        engine::imports::chat_import_count(&self.engine.lock())
    }

    /// Live memories counted by `group`.
    pub fn count_by(&self, group: GroupBy) -> Result<BTreeMap<String, i64>> {
        engine::stats::count_by(&self.engine.lock(), group)
    }

    /// Live memories counted by tag, through the tag index every write
    /// keeps in step with each row's JSON `tags`.
    pub fn count_by_tag(&self) -> Result<BTreeMap<String, i64>> {
        engine::stats::count_by_tag(&self.engine.lock())
    }

    /// How many memories are stored, tombstones included, and how many of
    /// them are tombstones.
    pub fn memory_totals(&self) -> Result<(i64, i64)> {
        engine::stats::memory_totals(&self.engine.lock())
    }

    /// How many tombstones still hold their text: any not yet emptied as
    /// ADR-0024 says.
    pub fn tombstones_holding_text(&self) -> Result<i64> {
        engine::stats::tombstones_holding_text(&self.engine.lock())
    }

    /// Every stored memory, tombstones included, counted by category, with
    /// an empty category counted as `(none)`.
    pub fn all_by_category(&self) -> Result<BTreeMap<String, i64>> {
        engine::stats::all_by_category(&self.engine.lock())
    }

    /// Live, non-sensitive memories created at or after `cutoff`, newest
    /// first (ties by id, descending), at most `limit`.
    pub fn shareable_since(&self, cutoff: &str, limit: usize) -> Result<Vec<DigestRecentMemory>> {
        engine::stats::shareable_since(&self.engine.lock(), cutoff, limit)
    }

    /// How many live, non-sensitive memories were created at or after
    /// `cutoff`.
    pub fn count_shareable_since(&self, cutoff: &str) -> Result<i64> {
        engine::stats::count_shareable_since(&self.engine.lock(), cutoff)
    }

    /// The `limit` newest live memories, content cut to 80 characters.
    pub fn recent(&self, limit: i64) -> Result<Vec<RecentMemory>> {
        engine::stats::recent(&self.engine.lock(), usize::try_from(limit).unwrap_or(0))
    }

    /// The store's file, size and schema version.
    pub fn storage_info(&self) -> Result<StorageInfo> {
        let size_bytes = self.engine.lock().size_on_disk()?;
        Ok(StorageInfo {
            path: self.path.clone(),
            size_bytes: i64::try_from(size_bytes).unwrap_or(i64::MAX),
            schema_version: crate::db::SCHEMA_VERSION,
        })
    }

    /// The id of the snapshot captured on `date` (`YYYY-MM-DD`), if any.
    pub fn snapshot_on(&self, date: &str) -> Result<Option<i64>> {
        Ok(engine::analytics::snapshot_on(&self.engine.lock(), date))
    }

    /// Store a snapshot and return its id.
    pub fn insert_snapshot(&self, snapshot: &AnalyticsSnapshot) -> Result<i64> {
        engine::analytics::insert(&mut self.engine.lock(), snapshot)
    }

    /// Every snapshot, oldest first. A malformed JSON column reads as an
    /// empty map, so one bad row does not take the whole series down.
    pub fn snapshots(&self) -> Result<Vec<AnalyticsSnapshot>> {
        Ok(engine::analytics::snapshots(&self.engine.lock()))
    }
}

use std::path::Path;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    /// Memories of every kind, and every count over them.
    #[test]
    fn counts_cover_live_memories_tombstones_and_tags() {
        use crate::db::memories::{Memories, NewMemory};
        const T1: &str = "2026-09-25T00:00:00+00:00";
        const T2: &str = "2026-09-26T00:00:00+00:00";
        const T3: &str = "2026-09-27T00:00:00+00:00";
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let memories = Memories::new(&store);
        for (id, category, source, tags, at, sensitive) in [
            ("a", "fact", "manual", vec!["red", "blue"], T1, false),
            ("b", "fact", "import", vec!["red"], T2, false),
            ("c", "", "manual", vec![], T2, true),
            ("d", "note", "manual", vec!["red"], T3, false),
            ("e", "note", "import", vec!["blue"], T3, false),
            ("f", "note", "manual", vec!["red"], T1, false),
        ] {
            memories
                .insert(&NewMemory {
                    category: category.into(),
                    source: source.into(),
                    tags: tags.into_iter().map(String::from).collect(),
                    sensitive,
                    ..NewMemory::new(id, format!("{id} {}", "é".repeat(100)), at)
                })
                .unwrap();
        }
        memories.delete_live("f", Some(T2)).unwrap();
        memories.delete_live("e", Some(T3)).unwrap();
        let stats = StoreStats::new(&store);
        assert_eq!(stats.live_memories().unwrap(), 4);
        assert_eq!(
            stats.count_by(GroupBy::Category).unwrap(),
            BTreeMap::from([
                ("".to_string(), 1),
                ("fact".to_string(), 2),
                ("note".to_string(), 1)
            ])
        );
        assert_eq!(
            stats.count_by(GroupBy::Source).unwrap(),
            BTreeMap::from([("import".to_string(), 1), ("manual".to_string(), 3)])
        );
        assert_eq!(
            stats.count_by_tag().unwrap(),
            BTreeMap::from([("blue".to_string(), 1), ("red".to_string(), 3)])
        );
        assert_eq!(stats.memory_totals().unwrap(), (6, 2));
        assert_eq!(stats.tombstones_holding_text().unwrap(), 0);
        assert_eq!(
            stats.all_by_category().unwrap(),
            BTreeMap::from([
                ("(none)".to_string(), 1),
                ("fact".to_string(), 2),
                ("note".to_string(), 3)
            ])
        );
        let shareable: Vec<String> = stats
            .shareable_since(T2, 10)
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(
            shareable,
            ["d", "b"],
            "newest first, no sensitive, no tombstones"
        );
        assert_eq!(stats.shareable_since(T1, 1).unwrap().len(), 1);
        assert_eq!(stats.count_shareable_since(T2).unwrap(), 2);
        let recent: Vec<(String, usize)> = stats
            .recent(3)
            .unwrap()
            .into_iter()
            .map(|r| (r.id, r.preview.chars().count()))
            .collect();
        assert_eq!(
            recent,
            [
                ("d".to_string(), 80),
                ("c".to_string(), 80),
                ("b".to_string(), 80)
            ],
            "previews are cut by character"
        );
    }

    #[test]
    fn an_in_memory_database_has_no_path_but_a_size() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let info = StoreStats::new(&store).storage_info().unwrap();
        assert_eq!(info.path, None);
        assert!(info.size_bytes > 0);
        assert_eq!(info.schema_version, crate::db::SCHEMA_VERSION);
    }

    fn snap(at: &str, total: i64) -> AnalyticsSnapshot {
        AnalyticsSnapshot {
            captured_at: at.to_string(),
            total_memories: total,
            vitality_buckets: BTreeMap::from([("high".to_string(), 1)]),
            category_counts: BTreeMap::from([("fact".to_string(), total)]),
        }
    }

    #[test]
    fn snapshots_round_trip_oldest_first() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let stats = StoreStats::new(&store);
        let second = stats
            .insert_snapshot(&snap("2026-09-25T10:00:00+00:00", 2))
            .unwrap();
        let first = stats
            .insert_snapshot(&snap("2026-09-24T10:00:00+00:00", 1))
            .unwrap();
        assert_ne!(first, second, "every snapshot gets its own id");
        assert_eq!(stats.snapshot_on("2026-09-25").unwrap(), Some(second));
        assert_eq!(stats.snapshot_on("2026-09-23").unwrap(), None);
        assert_eq!(
            stats.snapshots().unwrap(),
            vec![
                snap("2026-09-24T10:00:00+00:00", 1),
                snap("2026-09-25T10:00:00+00:00", 2)
            ]
        );
    }

    /// A snapshot's day is its UTC day, so a capture early in the day east
    /// of UTC belongs to the day before.
    #[test]
    fn a_snapshot_belongs_to_its_utc_day() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let stats = StoreStats::new(&store);
        let id = stats
            .insert_snapshot(&snap("2026-09-27T01:00:00+05:00", 3))
            .unwrap();
        assert_eq!(stats.snapshot_on("2026-09-26").unwrap(), Some(id));
        assert_eq!(stats.snapshot_on("2026-09-27").unwrap(), None);
    }

    #[test]
    fn a_malformed_map_reads_as_empty() {
        let buckets: BTreeMap<String, usize> = decode_json("not json");
        assert!(buckets.is_empty());
        assert_eq!(encode_json(&BTreeMap::<String, i64>::new()), "{}");
    }
}
