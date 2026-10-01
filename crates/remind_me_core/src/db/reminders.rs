//! Storage for reminders: `memories.remind_at` and the
//! `reminder_deliveries` rows that record which reminders have fired.
//!
//! Every statement for reminders lives here (ADR-0022). The rules stay in
//! [`crate::reminders`] and [`crate::scheduler`]: what a valid `remind_at`
//! is, that a reminder must be in the future, and when a delivery counts.

#[cfg(feature = "engine-store")]
use super::engine::{self, EngineLock};
use super::{Result, Store};
use crate::db::derived::{write_memory, Origin};
use crate::db::queries::{parse_memory_row, prefixed_memory_columns};
use crate::models::{Memory, ReminderWindow};
use rusqlite::{params, Connection, OptionalExtension};

/// The reminder columns and tables, over one connection, or on the
/// engine's memories core when the store's tables hold it
/// (`db::engine::reminders`).
pub struct Reminders<'c> {
    conn: &'c Connection,
    #[cfg(feature = "engine-store")]
    core: Option<&'c EngineLock>,
}

impl<'c> Reminders<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            conn: store.conn(),
            #[cfg(feature = "engine-store")]
            core: store.core(),
        }
    }

    /// Whether `memory_id` names a memory that is not deleted.
    pub fn is_live(&self, memory_id: &str) -> Result<bool> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return Ok(engine::memories::get_live(&core.lock(), memory_id)?.is_some());
        }
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
    /// and what LWW compares.
    pub fn set_remind_at(
        &self,
        memory_id: &str,
        remind_at: Option<&str>,
        updated_at: &str,
    ) -> Result<()> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::memories::set_remind_at(
                &mut core.lock(),
                memory_id,
                remind_at,
                updated_at,
            );
        }
        write_memory(self.conn, memory_id, Origin::Local, || {
            Ok(self.conn.execute(
                "UPDATE memories SET remind_at = ?, updated_at = ? WHERE id = ?",
                params![remind_at, updated_at, memory_id],
            )?)
        })?;
        Ok(())
    }

    /// Live memories with a reminder in `window` as of `now`, soonest first
    /// (ties by id), at most `limit`.
    ///
    /// - `Upcoming`: `remind_at` after `now`.
    /// - `Overdue`: `remind_at` at or before `now`, with no delivery recorded
    ///   for that exact `remind_at`, so a rescheduled reminder fires again.
    /// - `All`: either.
    pub fn in_window(&self, window: ReminderWindow, now: &str, limit: i64) -> Result<Vec<Memory>> {
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::reminders::in_window(
                &core.lock(),
                window,
                now,
                usize::try_from(limit).unwrap_or(0),
            );
        }
        let (condition, now_bindings) = window_sql(window);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM memories m
              WHERE m.remind_at IS NOT NULL
                AND m.deleted_at IS NULL
                AND {}
              ORDER BY m.remind_at ASC, m.id
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
            .collect::<rusqlite::Result<_>>()
            .map_err(crate::db::StoreError::from);
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
        #[cfg(feature = "engine-store")]
        if let Some(core) = self.core {
            return engine::reminders::record_delivery(
                &mut core.lock(),
                memory_id,
                remind_at,
                delivered_at,
            );
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::{on_each_backend, Database};

    const NOW: &str = "2026-09-27T12:00:00+00:00";

    /// Reminders set, cleared, delivered and rescheduled on `db`, and what
    /// each window shows after every step.
    fn exercise(db: &Database) -> Vec<Vec<String>> {
        let store = db.store();
        let memories = Memories::new(&store);
        let reminders = Reminders::new(&store);
        for id in ["past", "past2", "future", "none", "gone"] {
            memories.insert(&NewMemory::new(id, "x", NOW)).unwrap();
        }
        let set = |id: &str, at: Option<&str>| reminders.set_remind_at(id, at, NOW).unwrap();
        set("past", Some("2026-09-27T11:00:00+00:00"));
        set("past2", Some("2026-09-27T11:00:00+00:00"));
        set("future", Some("2026-09-28T00:00:00+00:00"));
        set("gone", Some("2026-09-27T10:00:00+00:00"));
        memories.delete_live("gone", Some(NOW)).unwrap();

        let mut seen = Vec::new();
        let look = |reminders: &Reminders<'_>| -> Vec<Vec<String>> {
            let mut seen = Vec::new();
            for window in [
                ReminderWindow::Upcoming,
                ReminderWindow::Overdue,
                ReminderWindow::All,
            ] {
                for limit in [1, 10] {
                    let ids = reminders
                        .in_window(window, NOW, limit)
                        .unwrap()
                        .into_iter()
                        .map(|m| format!("{}@{}", m.id, m.remind_at.unwrap_or_default()))
                        .collect();
                    seen.push(ids);
                }
            }
            seen
        };
        seen.extend(look(&reminders));
        reminders
            .record_delivery("past", "2026-09-27T11:00:00+00:00", NOW)
            .unwrap();
        reminders
            .record_delivery("past", "2026-09-27T11:00:00+00:00", "later")
            .unwrap();
        seen.extend(look(&reminders));
        // A rescheduled reminder fires again.
        set("past", Some("2026-09-27T11:30:00+00:00"));
        set("past2", None);
        seen.extend(look(&reminders));
        seen.push(
            ["past", "none", "gone", "missing"]
                .iter()
                .map(|id| format!("{id}:{}", reminders.is_live(id).unwrap()))
                .collect(),
        );
        seen
    }

    #[test]
    fn the_engine_core_keeps_reminders_as_sqlite_does() {
        let mut observed = Vec::new();
        on_each_backend(|db| observed.push(exercise(db)));
        let sqlite = &observed[0];
        assert_eq!(
            sqlite[3],
            [
                "past@2026-09-27T11:00:00+00:00",
                "past2@2026-09-27T11:00:00+00:00"
            ]
        );
        assert_eq!(
            sqlite[9],
            ["past2@2026-09-27T11:00:00+00:00"],
            "delivered once, no longer overdue"
        );
        for other in &observed[1..] {
            assert_eq!(other, sqlite);
        }
    }
}
