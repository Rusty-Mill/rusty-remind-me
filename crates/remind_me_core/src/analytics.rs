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

use crate::db::stats::{GroupBy, StoreStats};
use crate::models::{AnalyticsSnapshot, CapturedSnapshot};
use crate::vitality::build_vitality_report;
use rusqlite::{Connection, Result};

/// Record one snapshot for today, unless today already has one.
///
/// Returns [`CapturedSnapshot::AlreadyToday`] rather than an error when a row
/// exists: being called more than once a day is the expected case, not a
/// failure — the caller is a poll loop, not a user.
pub fn capture_snapshot(conn: &Connection) -> Result<CapturedSnapshot> {
    let now = chrono::Utc::now();
    let today = now.date_naive().to_string();

    let stats = StoreStats::new(conn);
    if let Some(id) = stats.snapshot_on(&today)? {
        return Ok(CapturedSnapshot::AlreadyToday { id });
    }

    let report = build_vitality_report(conn)?;
    let id = stats.insert_snapshot(&AnalyticsSnapshot {
        captured_at: now.to_rfc3339(),
        total_memories: report.total_memories as i64,
        vitality_buckets: report.vitality_buckets,
        category_counts: stats.count_by(GroupBy::Category)?,
    })?;
    Ok(CapturedSnapshot::Captured { id })
}

/// Every snapshot, **oldest first**.
///
/// Oldest-first because the only consumer is a chart, and a series that has to
/// be reversed before plotting is a trap the first caller falls into.
pub fn trend(conn: &Connection) -> Result<Vec<AnalyticsSnapshot>> {
    StoreStats::new(conn).snapshots()
}
