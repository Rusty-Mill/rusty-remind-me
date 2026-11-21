//! Store statistics and analytics snapshots on SQLite: the [`StatsStore`]
//! and [`SnapshotStore`] adapters.

use super::memories::{rows_where, text};
use super::SqliteTables;
use crate::db::engine::analytics::utc_day;
use crate::db::stats::{decode_json, encode_json, GroupBy, SnapshotStore, StatsStore};
use crate::db::Result;
use crate::models::{AnalyticsSnapshot, DigestRecentMemory};
use crate::stats::RecentMemory;
use rusqlite::params;
use std::collections::BTreeMap;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS analytics_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT, day TEXT NOT NULL, captured_at TEXT NOT NULL,
    total_memories INTEGER NOT NULL, vitality_buckets TEXT NOT NULL, category_counts TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS analytics_snapshots_day ON analytics_snapshots (day);
";

/// A `limit` as SQL binds it.
pub(super) fn limit(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

impl SqliteTables {
    fn count(&self, sql: &str, params: impl rusqlite::Params) -> Result<i64> {
        Ok(self.one(sql, params, |r| r.get(0))?.unwrap_or(0))
    }

    fn counts(&self, sql: &str) -> Result<BTreeMap<String, i64>> {
        Ok(self
            .all(sql, [], |r| Ok((r.get(0)?, r.get(1)?)))?
            .into_iter()
            .collect())
    }
}

impl StatsStore for SqliteTables {
    fn live_memories(&self) -> Result<i64> {
        self.count("SELECT count(*) FROM memories WHERE deleted_at IS NULL", [])
    }

    fn imports(&self) -> Result<i64> {
        self.count("SELECT count(*) FROM chat_imports", [])
    }

    fn count_by(&self, group: GroupBy) -> Result<BTreeMap<String, i64>> {
        let column = match group {
            GroupBy::Category => "category",
            GroupBy::Source => "source",
            GroupBy::Project => "project",
        };
        self.counts(&format!(
            "SELECT {column}, count(*) FROM memories WHERE deleted_at IS NULL \
             AND {column} IS NOT NULL GROUP BY {column}"
        ))
    }

    fn count_by_tag(&self) -> Result<BTreeMap<String, i64>> {
        self.counts(
            "SELECT t.tag, count(*) FROM memory_tags t JOIN memories m ON m.id = t.memory_id \
             WHERE m.deleted_at IS NULL GROUP BY t.tag",
        )
    }

    fn memory_totals(&self) -> Result<(i64, i64)> {
        Ok(self
            .one(
                "SELECT count(*), count(deleted_at) FROM memories",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?
            .unwrap_or((0, 0)))
    }

    fn tombstones_holding_text(&self) -> Result<i64> {
        let rows = rows_where(&self.conn, "deleted_at IS NOT NULL", "id", &[])?;
        Ok(limit(rows.iter().filter(|row| !row.is_emptied()).count()))
    }

    fn all_by_category(&self) -> Result<BTreeMap<String, i64>> {
        self.counts(
            "SELECT CASE category WHEN '' THEN '(none)' ELSE category END, count(*) \
             FROM memories GROUP BY 1",
        )
    }

    fn shareable_since(&self, cutoff: &str, n: usize) -> Result<Vec<DigestRecentMemory>> {
        let order = format!("created_at DESC, id DESC LIMIT {}", limit(n));
        let rows = rows_where(
            &self.conn,
            "deleted_at IS NULL AND sensitive = 0 AND created_at >= ?1",
            &order,
            &[text(cutoff)],
        )?;
        Ok(rows
            .into_iter()
            .map(|row| DigestRecentMemory {
                id: row.id,
                content: row.content,
                category: row.category,
                created_at: row.created_at,
            })
            .collect())
    }

    fn count_shareable_since(&self, cutoff: &str) -> Result<i64> {
        self.count(
            "SELECT count(*) FROM memories WHERE deleted_at IS NULL AND sensitive = 0 AND created_at >= ?1",
            [cutoff],
        )
    }

    fn recent(&self, n: usize) -> Result<Vec<RecentMemory>> {
        self.all(
            "SELECT id, category, substr(content, 1, 80), created_at FROM memories \
             WHERE deleted_at IS NULL ORDER BY created_at DESC, id DESC LIMIT ?1",
            [limit(n)],
            |r| {
                Ok(RecentMemory {
                    id: r.get(0)?,
                    category: r.get(1)?,
                    preview: r.get(2)?,
                    created_at: r.get(3)?,
                })
            },
        )
    }

    fn size_on_disk(&self) -> Result<u64> {
        let bytes = self.count(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
        )?;
        Ok(u64::try_from(bytes).unwrap_or(0))
    }
}

impl SnapshotStore for SqliteTables {
    fn snapshot_on(&self, date: &str) -> Result<Option<i64>> {
        self.one(
            "SELECT min(id) FROM analytics_snapshots WHERE day = ?1",
            [date],
            |r| r.get(0),
        )
        .map(Option::flatten)
    }

    fn insert_snapshot(&mut self, s: &AnalyticsSnapshot) -> Result<i64> {
        self.exec(
            "INSERT INTO analytics_snapshots (day, captured_at, total_memories, vitality_buckets, \
             category_counts) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                utc_day(&s.captured_at),
                s.captured_at,
                s.total_memories,
                encode_json(&s.vitality_buckets),
                encode_json(&s.category_counts)
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    fn snapshots(&self) -> Result<Vec<AnalyticsSnapshot>> {
        self.all(
            "SELECT captured_at, total_memories, vitality_buckets, category_counts \
             FROM analytics_snapshots ORDER BY captured_at, id",
            [],
            |r| {
                Ok(AnalyticsSnapshot {
                    captured_at: r.get(0)?,
                    total_memories: r.get(1)?,
                    vitality_buckets: decode_json(&r.get::<_, String>(2)?),
                    category_counts: decode_json(&r.get::<_, String>(3)?),
                })
            },
        )
    }
}

#[cfg(test)]
mod conformance_tests {
    mod stats {
        use crate::db::sqlite::SqliteTables;
        crate::db::stats::stats_conformance!(SqliteTables::open_in_memory().unwrap());
    }
    mod snapshots {
        use crate::db::sqlite::SqliteTables;
        crate::db::stats::snapshot_conformance!(SqliteTables::open_in_memory().unwrap());
    }
}
