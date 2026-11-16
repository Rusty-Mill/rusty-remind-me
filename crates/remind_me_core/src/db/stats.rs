//! Storage for store-wide statistics: counts over live memories, the
//! store's own size and location, and the daily `analytics_snapshots`.
//!
//! [`StatsStore`] (counts and size) and [`SnapshotStore`] (the snapshot
//! table) are the ports (ADR-0035); the engine's adapters are in
//! `db::engine::stats` and `db::engine::analytics`. [`StoreStats`] is what
//! callers take: it locks the store's tables for each call and goes through
//! the ports. `stats.rs`, `status.rs` and `analytics.rs` keep the shapes
//! they report and the rules (one snapshot per calendar day, oldest-first
//! trends, how sizes round).

use super::engine::EngineLock;
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

/// Store-wide counts over memories, and the store's size, as a backend
/// computes them (ADR-0035). [`conformance`] holds the behaviour every
/// implementation must show.
pub trait StatsStore {
    /// How many memories are not deleted.
    fn live_memories(&self) -> Result<i64>;

    /// How many chat imports are recorded.
    fn imports(&self) -> Result<i64>;

    /// Live memories counted by `group`.
    fn count_by(&self, group: GroupBy) -> Result<BTreeMap<String, i64>>;

    /// Live memories counted by tag. A tag no live memory carries is left
    /// out.
    fn count_by_tag(&self) -> Result<BTreeMap<String, i64>>;

    /// How many memories are stored, tombstones included, and how many of
    /// them are tombstones.
    fn memory_totals(&self) -> Result<(i64, i64)>;

    /// How many tombstones still hold their text: any not yet emptied as
    /// ADR-0024 says.
    fn tombstones_holding_text(&self) -> Result<i64>;

    /// Every stored memory, tombstones included, counted by category, with
    /// an empty category counted as `(none)`.
    fn all_by_category(&self) -> Result<BTreeMap<String, i64>>;

    /// Live, non-sensitive memories created at or after `cutoff` (compared
    /// as text), newest first (ties by id, descending), at most `limit`.
    fn shareable_since(&self, cutoff: &str, limit: usize) -> Result<Vec<DigestRecentMemory>>;

    /// How many live, non-sensitive memories were created at or after
    /// `cutoff`.
    fn count_shareable_since(&self, cutoff: &str) -> Result<i64>;

    /// The `limit` newest live memories (ties by id, descending), content
    /// cut to 80 characters.
    fn recent(&self, limit: usize) -> Result<Vec<RecentMemory>>;

    /// The bytes the store takes on disk, which an in-memory store has too.
    fn size_on_disk(&self) -> Result<u64>;
}

/// The daily `analytics_snapshots`, as a backend stores them (ADR-0035).
pub trait SnapshotStore {
    /// The id of a snapshot captured on `date` (`YYYY-MM-DD`, UTC), if any:
    /// the first captured, when there are several.
    fn snapshot_on(&self, date: &str) -> Result<Option<i64>>;

    /// Store a snapshot under a new id, never one issued before, and
    /// return it.
    fn insert_snapshot(&mut self, snapshot: &AnalyticsSnapshot) -> Result<i64>;

    /// Every snapshot, `captured_at` oldest first (compared as text), ties
    /// in id order. A malformed map reads as empty.
    fn snapshots(&self) -> Result<Vec<AnalyticsSnapshot>>;
}

/// The statistics of a [`Store`], one locked call at a time.
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

    /// See [`StatsStore::live_memories`].
    pub fn live_memories(&self) -> Result<i64> {
        StatsStore::live_memories(&*self.engine.lock())
    }

    /// See [`StatsStore::imports`].
    pub fn imports(&self) -> Result<i64> {
        StatsStore::imports(&*self.engine.lock())
    }

    /// See [`StatsStore::count_by`].
    pub fn count_by(&self, group: GroupBy) -> Result<BTreeMap<String, i64>> {
        StatsStore::count_by(&*self.engine.lock(), group)
    }

    /// See [`StatsStore::count_by_tag`].
    pub fn count_by_tag(&self) -> Result<BTreeMap<String, i64>> {
        StatsStore::count_by_tag(&*self.engine.lock())
    }

    /// See [`StatsStore::memory_totals`].
    pub fn memory_totals(&self) -> Result<(i64, i64)> {
        StatsStore::memory_totals(&*self.engine.lock())
    }

    /// See [`StatsStore::tombstones_holding_text`].
    pub fn tombstones_holding_text(&self) -> Result<i64> {
        StatsStore::tombstones_holding_text(&*self.engine.lock())
    }

    /// See [`StatsStore::all_by_category`].
    pub fn all_by_category(&self) -> Result<BTreeMap<String, i64>> {
        StatsStore::all_by_category(&*self.engine.lock())
    }

    /// See [`StatsStore::shareable_since`].
    pub fn shareable_since(&self, cutoff: &str, limit: usize) -> Result<Vec<DigestRecentMemory>> {
        StatsStore::shareable_since(&*self.engine.lock(), cutoff, limit)
    }

    /// See [`StatsStore::count_shareable_since`].
    pub fn count_shareable_since(&self, cutoff: &str) -> Result<i64> {
        StatsStore::count_shareable_since(&*self.engine.lock(), cutoff)
    }

    /// See [`StatsStore::recent`]. A negative `limit` returns nothing.
    pub fn recent(&self, limit: i64) -> Result<Vec<RecentMemory>> {
        StatsStore::recent(&*self.engine.lock(), usize::try_from(limit).unwrap_or(0))
    }

    /// The store's file, size and schema version.
    pub fn storage_info(&self) -> Result<StorageInfo> {
        let size_bytes = StatsStore::size_on_disk(&*self.engine.lock())?;
        Ok(StorageInfo {
            path: self.path.clone(),
            size_bytes: i64::try_from(size_bytes).unwrap_or(i64::MAX),
            schema_version: crate::db::SCHEMA_VERSION,
        })
    }

    /// See [`SnapshotStore::snapshot_on`].
    pub fn snapshot_on(&self, date: &str) -> Result<Option<i64>> {
        SnapshotStore::snapshot_on(&*self.engine.lock(), date)
    }

    /// See [`SnapshotStore::insert_snapshot`].
    pub fn insert_snapshot(&self, snapshot: &AnalyticsSnapshot) -> Result<i64> {
        SnapshotStore::insert_snapshot(&mut *self.engine.lock(), snapshot)
    }

    /// See [`SnapshotStore::snapshots`].
    pub fn snapshots(&self) -> Result<Vec<AnalyticsSnapshot>> {
        SnapshotStore::snapshots(&*self.engine.lock())
    }
}

use std::path::Path;

/// The behaviour every [`StatsStore`] and [`SnapshotStore`] must show, run
/// against each backend by [`stats_conformance`] and
/// [`snapshot_conformance`] (ADR-0035).
///
/// The counts are over memories, so a backend under [`stats_conformance`]
/// implements [`MemoryStore`](crate::db::memories::MemoryStore) too, and the
/// check seeds and tombstones memories through it.
#[cfg(test)]
pub(crate) mod conformance {
    use super::{GroupBy, SnapshotStore, StatsStore};
    use crate::db::memories::{MemoryStore, NewMemory};
    use crate::models::AnalyticsSnapshot;
    use std::collections::BTreeMap;

    pub(crate) const T1: &str = "2026-09-25T00:00:00+00:00";
    pub(crate) const T2: &str = "2026-09-26T00:00:00+00:00";
    pub(crate) const T3: &str = "2026-09-27T00:00:00+00:00";

    /// Add memory `new`.
    fn add<S: MemoryStore>(store: &mut S, new: &NewMemory) {
        MemoryStore::insert(store, new).unwrap();
    }

    /// Tombstone memory `id` at `at`.
    fn delete<S: MemoryStore>(store: &mut S, id: &str, at: &str) {
        MemoryStore::delete_live(store, id, Some(at)).unwrap();
    }

    /// Memories of every kind, and every count over them.
    pub(crate) fn counts_cover_live_memories_tombstones_and_tags<S: StatsStore + MemoryStore>(
        store: &mut S,
    ) {
        for (id, category, source, tags, at, sensitive, project) in [
            (
                "a",
                "fact",
                "manual",
                vec!["red", "blue"],
                T1,
                false,
                Some("p"),
            ),
            ("b", "fact", "import", vec!["red"], T2, false, None),
            ("c", "", "manual", vec![], T2, true, Some("p")),
            ("d", "note", "manual", vec!["red"], T3, false, Some("q")),
            ("e", "note", "import", vec!["blue"], T3, false, Some("q")),
            ("f", "note", "manual", vec!["red"], T1, false, None),
        ] {
            let new = NewMemory {
                category: category.into(),
                source: source.into(),
                tags: tags.into_iter().map(String::from).collect(),
                sensitive,
                project: project.map(String::from),
                ..NewMemory::new(id, format!("{id} {}", "é".repeat(100)), at)
            };
            add(store, &new);
        }
        delete(store, "f", T2);
        delete(store, "e", T3);
        assert_eq!(store.live_memories().unwrap(), 4);
        assert_eq!(store.imports().unwrap(), 0);
        assert_eq!(
            store.count_by(GroupBy::Category).unwrap(),
            BTreeMap::from([
                ("".to_string(), 1),
                ("fact".to_string(), 2),
                ("note".to_string(), 1)
            ])
        );
        assert_eq!(
            store.count_by(GroupBy::Source).unwrap(),
            BTreeMap::from([("import".to_string(), 1), ("manual".to_string(), 3)])
        );
        assert_eq!(
            store.count_by(GroupBy::Project).unwrap(),
            BTreeMap::from([("p".to_string(), 2), ("q".to_string(), 1)]),
            "no project is left out"
        );
        assert_eq!(
            store.count_by_tag().unwrap(),
            BTreeMap::from([("blue".to_string(), 1), ("red".to_string(), 3)])
        );
        assert_eq!(store.memory_totals().unwrap(), (6, 2));
        assert_eq!(store.tombstones_holding_text().unwrap(), 0);
        assert_eq!(
            store.all_by_category().unwrap(),
            BTreeMap::from([
                ("(none)".to_string(), 1),
                ("fact".to_string(), 2),
                ("note".to_string(), 3)
            ])
        );
        let shareable: Vec<String> = store
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
        assert_eq!(store.shareable_since(T1, 1).unwrap().len(), 1);
        assert_eq!(store.count_shareable_since(T2).unwrap(), 2);
        let recent: Vec<(String, usize)> = store
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
        assert!(store.size_on_disk().unwrap() > 0);
    }

    fn snap(at: &str, total: i64) -> AnalyticsSnapshot {
        AnalyticsSnapshot {
            captured_at: at.to_string(),
            total_memories: total,
            vitality_buckets: BTreeMap::from([("high".to_string(), 1)]),
            category_counts: BTreeMap::from([("fact".to_string(), total)]),
        }
    }

    pub(crate) fn snapshots_round_trip_oldest_first(store: &mut dyn SnapshotStore) {
        let second = store
            .insert_snapshot(&snap("2026-09-25T10:00:00+00:00", 2))
            .unwrap();
        let first = store
            .insert_snapshot(&snap("2026-09-24T10:00:00+00:00", 1))
            .unwrap();
        assert_ne!(first, second, "every snapshot gets its own id");
        assert_eq!(store.snapshot_on("2026-09-25").unwrap(), Some(second));
        assert_eq!(store.snapshot_on("2026-09-23").unwrap(), None);
        assert_eq!(
            store.snapshots().unwrap(),
            vec![
                snap("2026-09-24T10:00:00+00:00", 1),
                snap("2026-09-25T10:00:00+00:00", 2)
            ]
        );
    }

    /// A snapshot's day is its UTC day, so a capture early in the day east
    /// of UTC belongs to the day before.
    pub(crate) fn a_snapshot_belongs_to_its_utc_day(store: &mut dyn SnapshotStore) {
        let id = store
            .insert_snapshot(&snap("2026-09-27T01:00:00+05:00", 3))
            .unwrap();
        assert_eq!(store.snapshot_on("2026-09-26").unwrap(), Some(id));
        assert_eq!(store.snapshot_on("2026-09-27").unwrap(), None);
    }

    pub(crate) fn the_first_snapshot_of_a_day_is_found(store: &mut dyn SnapshotStore) {
        let first = store
            .insert_snapshot(&snap("2026-09-25T10:00:00+00:00", 1))
            .unwrap();
        store
            .insert_snapshot(&snap("2026-09-25T11:00:00+00:00", 2))
            .unwrap();
        assert_eq!(store.snapshot_on("2026-09-25").unwrap(), Some(first));
    }
}

/// One `#[test]` per [`StatsStore`] check in [`conformance`], each on a
/// fresh store from `$make`.
#[cfg(test)]
macro_rules! stats_conformance {
    ($make:expr) => {
        #[test]
        fn counts_cover_live_memories_tombstones_and_tags() {
            $crate::db::stats::conformance::counts_cover_live_memories_tombstones_and_tags(
                &mut $make,
            );
        }
    };
}
#[cfg(test)]
pub(crate) use stats_conformance;

/// One `#[test]` per [`SnapshotStore`] check in [`conformance`], each on a
/// fresh store from `$make`.
#[cfg(test)]
macro_rules! snapshot_conformance {
    ($make:expr) => {
        use $crate::db::stats::conformance;

        #[test]
        fn snapshots_round_trip_oldest_first() {
            conformance::snapshots_round_trip_oldest_first(&mut $make);
        }
        #[test]
        fn a_snapshot_belongs_to_its_utc_day() {
            conformance::a_snapshot_belongs_to_its_utc_day(&mut $make);
        }
        #[test]
        fn the_first_snapshot_of_a_day_is_found() {
            conformance::the_first_snapshot_of_a_day_is_found(&mut $make);
        }
    };
}
#[cfg(test)]
pub(crate) use snapshot_conformance;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    #[test]
    fn the_repository_reaches_the_store_through_the_ports() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        Memories::new(&store)
            .insert(&NewMemory::new("m", "content", conformance::T1))
            .unwrap();
        let stats = StoreStats::new(&store);
        assert_eq!(stats.live_memories().unwrap(), 1);
        assert_eq!(stats.recent(5).unwrap().len(), 1);
        assert!(
            stats.recent(-1).unwrap().is_empty(),
            "a negative limit returns nothing"
        );
        let snapshot = AnalyticsSnapshot {
            captured_at: conformance::T1.to_string(),
            total_memories: 1,
            vitality_buckets: BTreeMap::new(),
            category_counts: BTreeMap::new(),
        };
        let id = stats.insert_snapshot(&snapshot).unwrap();
        assert_eq!(stats.snapshot_on("2026-09-25").unwrap(), Some(id));
        assert_eq!(stats.snapshots().unwrap(), [snapshot]);
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

    #[test]
    fn a_malformed_map_reads_as_empty() {
        let buckets: BTreeMap<String, usize> = decode_json("not json");
        assert!(buckets.is_empty());
        assert_eq!(encode_json(&BTreeMap::<String, i64>::new()), "{}");
    }
}
