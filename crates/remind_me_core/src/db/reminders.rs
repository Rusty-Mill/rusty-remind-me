//! Storage for reminders: `memories.remind_at` and the
//! `reminder_deliveries` rows that record which reminders have fired.
//!
//! Every statement for reminders lives here (ADR-0022). The rules stay in
//! [`crate::reminders`] and [`crate::scheduler`]: what a valid `remind_at`
//! is, that a reminder must be in the future, and when a delivery counts.

use crate::db::queries::{parse_memory_row, prefixed_memory_columns};
use crate::models::{Memory, ReminderWindow};
use rusqlite::{params, Connection, OptionalExtension, Result};

/// The reminder columns and tables, over one connection.
pub struct Reminders<'c> {
    conn: &'c Connection,
}

impl<'c> Reminders<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Whether `memory_id` names a memory that is not deleted.
    pub fn is_live(&self, memory_id: &str) -> Result<bool> {
        let found: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM memories WHERE id = ? AND deleted_at IS NULL",
                params![memory_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Set `memory_id`'s `remind_at`, or clear it with `None`, stamping
    /// `updated_at`. The stamp is what puts the change in the sync outbox
    /// (by trigger) and what LWW compares.
    pub fn set_remind_at(
        &self,
        memory_id: &str,
        remind_at: Option<&str>,
        updated_at: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE memories SET remind_at = ?, updated_at = ? WHERE id = ?",
            params![remind_at, updated_at, memory_id],
        )?;
        Ok(())
    }

    /// Live memories with a reminder in `window` as of `now`, soonest first,
    /// at most `limit`.
    ///
    /// - `Upcoming`: `remind_at` after `now`.
    /// - `Overdue`: `remind_at` at or before `now`, with no delivery recorded
    ///   for that exact `remind_at`, so a rescheduled reminder fires again.
    /// - `All`: either.
    pub fn in_window(&self, window: ReminderWindow, now: &str, limit: i64) -> Result<Vec<Memory>> {
        let (condition, now_bindings) = window_sql(window);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM memories m
              WHERE m.remind_at IS NOT NULL
                AND m.deleted_at IS NULL
                AND {}
              ORDER BY m.remind_at ASC
              LIMIT ?",
            prefixed_memory_columns("m"),
            condition
        ))?;

        let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
        for _ in 0..now_bindings {
            bindings.push(rusqlite::types::Value::Text(now.to_string()));
        }
        bindings.push(rusqlite::types::Value::Integer(limit));

        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(bindings.iter()),
                parse_memory_row,
            )?
            .collect();
        rows
    }

    /// Record that the reminder `memory_id` had at `remind_at` was delivered.
    ///
    /// `INSERT OR IGNORE` because the unique index is the real guarantee: two
    /// racing pollers must produce one delivery, not an error that aborts
    /// the pass.
    pub fn record_delivery(
        &self,
        memory_id: &str,
        remind_at: &str,
        delivered_at: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO reminder_deliveries (memory_id, remind_at, delivered_at)
             VALUES (?, ?, ?)",
            params![memory_id, remind_at, delivered_at],
        )?;
        Ok(())
    }
}

/// The SQL condition for one window, and how many `now` bindings it takes.
fn window_sql(window: ReminderWindow) -> (String, usize) {
    let not_delivered = "NOT EXISTS (SELECT 1 FROM reminder_deliveries rd \
         WHERE rd.memory_id = m.id AND rd.remind_at = m.remind_at)";
    let upcoming = "m.remind_at > ?";
    let overdue = format!("(m.remind_at <= ? AND {not_delivered})");

    match window {
        ReminderWindow::Upcoming => (upcoming.to_string(), 1),
        ReminderWindow::Overdue => (overdue, 1),
        ReminderWindow::All => (format!("({upcoming} OR {overdue})"), 2),
    }
}
