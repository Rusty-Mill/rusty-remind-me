//! Storage for store-wide statistics: counts over live memories, the
//! database's own size and location, and the daily `analytics_snapshots`.
//!
//! Every statement behind `stats.rs`, `status.rs` and `analytics.rs` lives
//! here (ADR-0022). Those modules keep the shapes they report and the rules
//! (one snapshot per calendar day, oldest-first trends, how sizes round).
//!
//! With the `engine-store` feature, a store that carries engine tables keeps
//! `analytics_snapshots` there (`db::engine::analytics`, ADR-0023 phase 4f),
//! and a store whose tables hold the memories core counts memories there
//! (`db::engine::stats`, core PR 2d) and chat imports there
//! (`db::engine::imports`, core PR 4a). The storage figures stay on SQLite
//! until the copy tool (ADR-0023 §5).

#[cfg(feature = "engine-store")]
use super::engine::{self, EngineTables};
use super::{Result, Store};
use crate::models::{AnalyticsSnapshot, DigestRecentMemory};
use crate::stats::RecentMemory;
#[cfg(feature = "engine-store")]
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// A column live memories can be grouped by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Category,
    Source,
}

impl GroupBy {
    fn column(self) -> &'static str {
        match self {
            GroupBy::Category => "category",
            GroupBy::Source => "source",
        }
    }
}

/// Where the database lives and how big it is, from SQLite's own accounting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageInfo {
    /// The main database file, or `None` for an in-memory database.
    pub path: Option<PathBuf>,
    /// Pages times page size, so an in-memory database has a size too.
    pub size_bytes: i64,
    /// What `PRAGMA user_version` reports.
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

/// The statistics queries, over one store.
pub struct StoreStats<'c> {
    conn: &'c Connection,
    #[cfg(feature = "engine-store")]
    engine: Option<&'c Mutex<EngineTables>>,
    /// The tables again when they hold the memories core.
    #[cfg(feature = "engine-store")]
    core: Option<&'c Mutex<EngineTables>>,
}

impl<'c> StoreStats<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            conn: store.conn(),
            #[cfg(feature = "engine-store")]
            engine: store.engine(),
            #[cfg(feature = "engine-store")]
            core: store.core(),
        }
    }

    /// How many memories are not deleted.
    pub fn live_memories(&self) -> Result<i64> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::stats::live_memories(&core.lock());
        }
        Ok(self.conn.query_row(
            "SELECT count(*) FROM memories WHERE deleted_at IS NULL",
            [],
            |r| r.get(0),
        )?)
    }

    /// How many chat imports are recorded.
    pub fn imports(&self) -> Result<i64> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::imports::chat_import_count(&core.lock());
        }
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM chat_imports", [], |r| r.get(0))?)
    }

    /// Live memories counted by `group`.
    pub fn count_by(&self, group: GroupBy) -> Result<BTreeMap<String, i64>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::stats::count_by(&core.lock(), group);
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {0}, count(*) FROM memories WHERE deleted_at IS NULL GROUP BY {0}",
            group.column()
        ))?;
        let counts = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        counts
    }

    /// Live memories counted by tag, through the `memory_tags` index every
    /// write keeps in step with each row's JSON `tags`.
    pub fn count_by_tag(&self) -> Result<BTreeMap<String, i64>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::stats::count_by_tag(&core.lock());
        }
        let mut stmt = self.conn.prepare(
            "SELECT mt.tag, count(*) FROM memory_tags mt
             JOIN memories m ON m.id = mt.memory_id
             WHERE m.deleted_at IS NULL
             GROUP BY mt.tag",
        )?;
        let counts = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        counts
    }

    /// How many memories are stored, tombstones included, and how many of
    /// them are tombstones.
    pub fn memory_totals(&self) -> Result<(i64, i64)> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::stats::memory_totals(&core.lock());
        }
        Ok(self.conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE WHEN deleted_at IS NOT NULL THEN 1 ELSE 0 END), 0)
               FROM memories",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// How many tombstones were deleted before `cutoff`.
    pub fn tombstones_before(&self, cutoff: &str) -> Result<i64> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::stats::tombstones_before(&core.lock(), cutoff);
        }
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE deleted_at IS NOT NULL AND deleted_at < ?",
            params![cutoff],
            |r| r.get(0),
        )?)
    }

    /// Every stored memory, tombstones included, counted by category, with
    /// an empty category counted as `(none)`.
    pub fn all_by_category(&self) -> Result<BTreeMap<String, i64>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::stats::all_by_category(&core.lock());
        }
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(NULLIF(category, ''), '(none)'), COUNT(*)
               FROM memories GROUP BY 1",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// Live, non-sensitive memories created at or after `cutoff`, newest
    /// first (ties by id, descending), at most `limit`.
    pub fn shareable_since(&self, cutoff: &str, limit: usize) -> Result<Vec<DigestRecentMemory>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::stats::shareable_since(&core.lock(), cutoff, limit);
        }
        let mut stmt = self.conn.prepare(
            "SELECT id, content, category, created_at
               FROM memories
              WHERE deleted_at IS NULL AND sensitive = 0 AND created_at >= ?
              ORDER BY created_at DESC, id DESC
              LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![cutoff, limit as i64], |r| {
                Ok(DigestRecentMemory {
                    id: r.get(0)?,
                    content: r.get(1)?,
                    category: r.get(2)?,
                    created_at: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// How many live, non-sensitive memories were created at or after
    /// `cutoff`.
    pub fn count_shareable_since(&self, cutoff: &str) -> Result<i64> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::stats::count_shareable_since(&core.lock(), cutoff);
        }
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM memories
              WHERE deleted_at IS NULL AND sensitive = 0 AND created_at >= ?",
            params![cutoff],
            |r| r.get(0),
        )?)
    }

    /// The `limit` newest live memories, content cut to 80 characters.
    pub fn recent(&self, limit: i64) -> Result<Vec<RecentMemory>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::stats::recent(&core.lock(), usize::try_from(limit).unwrap_or(0));
        }
        let mut stmt = self.conn.prepare(
            "SELECT id, category, substr(content, 1, 80), created_at
             FROM memories WHERE deleted_at IS NULL
             ORDER BY created_at DESC, id DESC LIMIT ?",
        )?;
        let rows = stmt
            .query_map([limit], |r| {
                Ok(RecentMemory {
                    id: r.get(0)?,
                    category: r.get(1)?,
                    preview: r.get(2)?,
                    created_at: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }

    /// The database's file, size and schema version.
    pub fn storage_info(&self) -> Result<StorageInfo> {
        let path = super::database_path(&Store::over_sqlite(self.conn))?;
        let page_count: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let page_size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let schema_version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        Ok(StorageInfo {
            path,
            size_bytes: page_count * page_size,
            schema_version,
        })
    }

    /// The id of the snapshot captured on `date` (`YYYY-MM-DD`), if any.
    pub fn snapshot_on(&self, date: &str) -> Result<Option<i64>> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return Ok(engine::analytics::snapshot_on(&engine.lock(), date));
        }
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM analytics_snapshots WHERE date(captured_at) = ?",
                params![date],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Store a snapshot and return its id.
    pub fn insert_snapshot(&self, snapshot: &AnalyticsSnapshot) -> Result<i64> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return engine::analytics::insert(&mut engine.lock(), snapshot);
        }
        self.conn.execute(
            "INSERT INTO analytics_snapshots
                 (captured_at, total_memories, vitality_buckets, category_counts)
             VALUES (?, ?, ?, ?)",
            params![
                snapshot.captured_at,
                snapshot.total_memories,
                encode_json(&snapshot.vitality_buckets),
                encode_json(&snapshot.category_counts),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Every snapshot, oldest first. A malformed JSON column reads as an
    /// empty map, so one bad row does not take the whole series down.
    pub fn snapshots(&self) -> Result<Vec<AnalyticsSnapshot>> {
        #[cfg(feature = "engine-store")]
        if let Some(engine) = self.engine {
            return Ok(engine::analytics::snapshots(&engine.lock()));
        }
        let mut stmt = self.conn.prepare(
            "SELECT captured_at, total_memories, vitality_buckets, category_counts
               FROM analytics_snapshots
              ORDER BY captured_at ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let buckets: String = r.get(2)?;
                let categories: String = r.get(3)?;
                Ok(AnalyticsSnapshot {
                    captured_at: r.get(0)?,
                    total_memories: r.get(1)?,
                    vitality_buckets: decode_json(&buckets),
                    category_counts: decode_json(&categories),
                })
            })?
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{on_each_backend, Database};

    /// Memories of every kind on `db`, and every count over them.
    fn exercise(db: &Database) -> Vec<String> {
        use crate::db::memories::{Memories, NewMemory};
        const T1: &str = "2026-09-25T00:00:00+00:00";
        const T2: &str = "2026-09-26T00:00:00+00:00";
        const T3: &str = "2026-09-27T00:00:00+00:00";
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
        let recent: Vec<(String, usize)> = stats
            .recent(3)
            .unwrap()
            .into_iter()
            .map(|r| (r.id, r.preview.chars().count()))
            .collect();
        vec![
            format!("{}", stats.live_memories().unwrap()),
            format!("{:?}", stats.count_by(GroupBy::Category).unwrap()),
            format!("{:?}", stats.count_by(GroupBy::Source).unwrap()),
            format!("{:?}", stats.count_by_tag().unwrap()),
            format!("{:?}", stats.memory_totals().unwrap()),
            format!("{}", stats.tombstones_before(T3).unwrap()),
            format!("{:?}", stats.all_by_category().unwrap()),
            format!("{:?}", stats.shareable_since(T2, 10).unwrap()),
            format!("{:?}", stats.shareable_since(T1, 1).unwrap()),
            format!("{}", stats.count_shareable_since(T2).unwrap()),
            format!("{recent:?}"),
        ]
    }

    #[test]
    fn the_engine_core_counts_as_sqlite_does() {
        let mut observed = Vec::new();
        crate::db::on_each_backend(|db| observed.push(exercise(db)));
        let sqlite = &observed[0];
        assert_eq!(sqlite[0], "4");
        assert_eq!(sqlite[4], "(6, 2)");
        for other in &observed[1..] {
            assert_eq!(other, sqlite);
        }
    }

    #[test]
    fn an_in_memory_database_has_no_path_but_a_size() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let info = StoreStats::new(&store).storage_info().unwrap();
        assert_eq!(info.path, None);
        assert!(info.size_bytes > 0);
        assert_eq!(info.schema_version, crate::db::migrations::SCHEMA_VERSION);
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
        on_each_backend(|db| {
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
        });
    }

    /// `date(captured_at)` is the UTC day, so a capture early in the day
    /// east of UTC belongs to the day before.
    #[test]
    fn a_snapshot_belongs_to_its_utc_day() {
        on_each_backend(|db| {
            let store = db.store();
            let stats = StoreStats::new(&store);
            let id = stats
                .insert_snapshot(&snap("2026-09-27T01:00:00+05:00", 3))
                .unwrap();
            assert_eq!(stats.snapshot_on("2026-09-26").unwrap(), Some(id));
            assert_eq!(stats.snapshot_on("2026-09-27").unwrap(), None);
        });
    }

    #[test]
    fn a_malformed_map_reads_as_empty() {
        let buckets: BTreeMap<String, usize> = decode_json("not json");
        assert!(buckets.is_empty());
        assert_eq!(encode_json(&BTreeMap::<String, i64>::new()), "{}");
    }
}
