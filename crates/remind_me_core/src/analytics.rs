//! Daily analytics snapshots and the trend read over them.
//!
//! `remind_me_stats` and the vitality report answer "what does the vault look
//! like *now*". Neither can answer "is it getting better or worse", because
//! nothing was ever recorded to compare against. A snapshot per day is the
//! cheapest thing that makes the second question answerable at all.
//!
//! # Idempotent per calendar day, by date rather than timestamp
//!
//! [`capture_snapshot`] checks whether a snapshot already exists for *today's
//! date* before inserting. Comparing dates rather than exact timestamps is the
//! point: a server restarted at a different second on the same day must not
//! produce a second row, or a day with three restarts shows three data points
//! and the trend reads as a spike that never happened.

use crate::models::{AnalyticsSnapshot, CapturedSnapshot};
use crate::vitality::build_vitality_report;
use rusqlite::{params, Connection, OptionalExtension, Result};
use std::collections::BTreeMap;

fn category_counts(conn: &Connection) -> Result<BTreeMap<String, i64>> {
    let mut stmt = conn.prepare(
        "SELECT category, COUNT(*) FROM memories
          WHERE deleted_at IS NULL GROUP BY category",
    )?;
    let counts = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect();
    counts
}

/// Record one snapshot for today, unless today already has one.
///
/// Returns [`CapturedSnapshot::AlreadyToday`] rather than an error when a row
/// exists: being called more than once a day is the expected case, not a
/// failure — the caller is a poll loop, not a user.
pub fn capture_snapshot(conn: &Connection) -> Result<CapturedSnapshot> {
    let now = chrono::Utc::now();
    let today = now.date_naive().to_string();

    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM analytics_snapshots WHERE date(captured_at) = ?",
            params![today],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(CapturedSnapshot::AlreadyToday { id });
    }

    let report = build_vitality_report(conn)?;
    let categories = category_counts(conn)?;

    conn.execute(
        "INSERT INTO analytics_snapshots
             (captured_at, total_memories, vitality_buckets, category_counts)
         VALUES (?, ?, ?, ?)",
        params![
            now.to_rfc3339(),
            report.total_memories as i64,
            serde_json::to_string(&report.vitality_buckets).unwrap_or_else(|_| "{}".into()),
            serde_json::to_string(&categories).unwrap_or_else(|_| "{}".into()),
        ],
    )?;

    Ok(CapturedSnapshot::Captured {
        id: conn.last_insert_rowid(),
    })
}

/// Every snapshot, **oldest first**.
///
/// Oldest-first because the only consumer is a chart, and a series that has to
/// be reversed before plotting is a trap the first caller falls into.
pub fn trend(conn: &Connection) -> Result<Vec<AnalyticsSnapshot>> {
    let mut stmt = conn.prepare(
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
                // Decoded here rather than handed to the caller as a string:
                // a malformed value becomes an empty map, because one bad row
                // should not take the whole chart down with it.
                vitality_buckets: serde_json::from_str(&buckets).unwrap_or_default(),
                category_counts: serde_json::from_str(&categories).unwrap_or_default(),
            })
        })?
        .collect();
    rows
}
