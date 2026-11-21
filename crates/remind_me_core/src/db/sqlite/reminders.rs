//! Reminders on SQLite: the [`ReminderStore`] adapter. `remind_at` is a
//! memories column; deliveries are their own table.

use super::memories::{rows_where, text};
use super::SqliteTables;
use crate::db::derived::Origin;
use crate::db::memories::MemoryStore;
use crate::db::reminders::ReminderStore;
use crate::db::Result;
use crate::models::{Memory, ReminderWindow};
use rusqlite::params;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS reminder_deliveries (
    memory_id TEXT NOT NULL, remind_at TEXT NOT NULL, delivered_at TEXT NOT NULL,
    PRIMARY KEY (memory_id, remind_at)) WITHOUT ROWID;
";

impl ReminderStore for SqliteTables {
    fn is_live(&self, memory_id: &str) -> Result<bool> {
        Ok(self.get_live(memory_id)?.is_some())
    }

    fn set_remind_at(&mut self, id: &str, remind_at: Option<&str>, updated_at: &str) -> Result<()> {
        self.update_memory(id, Origin::Local, |row| {
            row.remind_at = remind_at.map(str::to_string);
            row.updated_at = updated_at.to_string();
            Ok(())
        })
    }

    fn in_window(&self, window: ReminderWindow, now: &str, limit: usize) -> Result<Vec<Memory>> {
        let overdue = "(remind_at <= ?1 AND NOT EXISTS (SELECT 1 FROM reminder_deliveries d \
                       WHERE d.memory_id = memories.id AND d.remind_at = memories.remind_at))";
        let due = match window {
            ReminderWindow::Upcoming => "remind_at > ?1".to_string(),
            ReminderWindow::Overdue => overdue.to_string(),
            ReminderWindow::All => format!("(remind_at > ?1 OR {overdue})"),
        };
        let clause = format!("deleted_at IS NULL AND remind_at IS NOT NULL AND {due}");
        let order = format!(
            "remind_at, id LIMIT {}",
            i64::try_from(limit).unwrap_or(i64::MAX)
        );
        let rows = rows_where(&self.conn, &clause, &order, &[text(now)])?;
        Ok(rows.iter().map(|r| r.to_memory()).collect())
    }

    fn record_delivery(
        &mut self,
        memory_id: &str,
        remind_at: &str,
        delivered_at: &str,
    ) -> Result<()> {
        self.exec(
            "INSERT OR IGNORE INTO reminder_deliveries VALUES (?1, ?2, ?3)",
            params![memory_id, remind_at, delivered_at],
        )
        .map(drop)
    }
}

#[cfg(test)]
mod conformance_tests {
    use super::SqliteTables;
    crate::db::reminders::reminder_conformance!(SqliteTables::open_in_memory().unwrap());
}
