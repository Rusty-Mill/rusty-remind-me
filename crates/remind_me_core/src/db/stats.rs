//! Storage for store-wide statistics: counts over live memories, the
//! database's own size and location, and the daily `analytics_snapshots`.
//!
//! Every statement behind `stats.rs`, `status.rs` and `analytics.rs` lives
//! here (ADR-0022). Those modules keep the shapes they report and the rules
//! (one snapshot per calendar day, oldest-first trends, how sizes round).

use crate::models::AnalyticsSnapshot;
use crate::stats::RecentMemory;
use rusqlite::{params, Connection, OptionalExtension, Result};
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

/// The statistics queries, over one connection.
pub struct StoreStats<'c> {
    conn: &'c Connection,
}

impl<'c> StoreStats<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// How many memories are not deleted.
    pub fn live_memories(&self) -> Result<i64> {
        self.conn.query_row(
            "SELECT count(*) FROM memories WHERE deleted_at IS NULL",
            [],
            |r| r.get(0),
        )
    }

    /// How many chat imports are recorded.
    pub fn imports(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT count(*) FROM chat_imports", [], |r| r.get(0))
    }

    /// Live memories counted by `group`.
    pub fn count_by(&self, group: GroupBy) -> Result<BTreeMap<String, i64>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {0}, count(*) FROM memories WHERE deleted_at IS NULL GROUP BY {0}",
            group.column()
        ))?;
        let counts = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect();
        counts
    }

    /// Live memories counted by tag, through the `memory_tags` index every
    /// write keeps in step with each row's JSON `tags`.
    pub fn count_by_tag(&self) -> Result<BTreeMap<String, i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT mt.tag, count(*) FROM memory_tags mt
             JOIN memories m ON m.id = mt.memory_id
             WHERE m.deleted_at IS NULL
             GROUP BY mt.tag",
        )?;
        let counts = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect();
        counts
    }

    /// The `limit` newest live memories, content cut to 80 characters.
    pub fn recent(&self, limit: i64) -> Result<Vec<RecentMemory>> {
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
            .collect();
        rows
    }

    /// The database's file, size and schema version.
    pub fn storage_info(&self) -> Result<StorageInfo> {
        let path: String = self
            .conn
            .query_row("PRAGMA database_list", [], |r| r.get(2))?;
        let page_count: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let page_size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let schema_version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        Ok(StorageInfo {
            path: (!path.is_empty()).then(|| PathBuf::from(path)),
            size_bytes: page_count * page_size,
            schema_version,
        })
    }

    /// The id of the snapshot captured on `date` (`YYYY-MM-DD`), if any.
    pub fn snapshot_on(&self, date: &str) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT id FROM analytics_snapshots WHERE date(captured_at) = ?",
                params![date],
                |r| r.get(0),
            )
            .optional()
    }

    /// Store a snapshot and return its id.
    pub fn insert_snapshot(&self, snapshot: &AnalyticsSnapshot) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO analytics_snapshots
                 (captured_at, total_memories, vitality_buckets, category_counts)
             VALUES (?, ?, ?, ?)",
            params![
                snapshot.captured_at,
                snapshot.total_memories,
                serde_json::to_string(&snapshot.vitality_buckets).unwrap_or_else(|_| "{}".into()),
                serde_json::to_string(&snapshot.category_counts).unwrap_or_else(|_| "{}".into()),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Every snapshot, oldest first. A malformed JSON column reads as an
    /// empty map, so one bad row does not take the whole series down.
    pub fn snapshots(&self) -> Result<Vec<AnalyticsSnapshot>> {
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
                    vitality_buckets: serde_json::from_str(&buckets).unwrap_or_default(),
                    category_counts: serde_json::from_str(&categories).unwrap_or_default(),
                })
            })?
            .collect();
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    #[test]
    fn an_in_memory_database_has_no_path_but_a_size() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let info = StoreStats::new(&conn).storage_info().unwrap();
        assert_eq!(info.path, None);
        assert!(info.size_bytes > 0);
        assert_eq!(info.schema_version, crate::db::migrations::SCHEMA_VERSION);
    }

    #[test]
    fn snapshots_round_trip_oldest_first() {
        let db = Database::open_in_memory().unwrap();
        let conn = db.conn();
        let stats = StoreStats::new(&conn);
        let snap = |at: &str, total| AnalyticsSnapshot {
            captured_at: at.to_string(),
            total_memories: total,
            vitality_buckets: BTreeMap::from([("high".to_string(), 1)]),
            category_counts: BTreeMap::from([("fact".to_string(), total)]),
        };
        let second = stats
            .insert_snapshot(&snap("2026-09-25T10:00:00+00:00", 2))
            .unwrap();
        stats
            .insert_snapshot(&snap("2026-09-24T10:00:00+00:00", 1))
            .unwrap();
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
}
