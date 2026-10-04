//! Storage for reminders: `memories.remind_at` and the
//! `reminder_deliveries` rows that record which reminders have fired.
//!
//! Every read and write for reminders goes through here (ADR-0022), onto
//! the engine's memories core (`db::engine::reminders`). The rules stay in
//! [`crate::reminders`] and [`crate::scheduler`]: what a valid `remind_at`
//! is, that a reminder must be in the future, and when a delivery counts.

use super::engine::{self, EngineLock};
use super::{Result, Store};
use crate::models::{Memory, ReminderWindow};

/// The reminder columns and tables, on the engine's memories core.
pub struct Reminders<'c> {
    core: &'c EngineLock,
}

impl<'c> Reminders<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self { core: store.core() }
    }

    /// Whether `memory_id` names a memory that is not deleted.
    pub fn is_live(&self, memory_id: &str) -> Result<bool> {
        Ok(engine::memories::get_live(&self.core.lock(), memory_id)?.is_some())
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
        engine::memories::set_remind_at(&mut self.core.lock(), memory_id, remind_at, updated_at)
    }

    /// Live memories with a reminder in `window` as of `now`, soonest first
    /// (ties by id), at most `limit`.
    ///
    /// - `Upcoming`: `remind_at` after `now`.
    /// - `Overdue`: `remind_at` at or before `now`, with no delivery recorded
    ///   for that exact `remind_at`, so a rescheduled reminder fires again.
    /// - `All`: either.
    pub fn in_window(&self, window: ReminderWindow, now: &str, limit: i64) -> Result<Vec<Memory>> {
        engine::reminders::in_window(
            &self.core.lock(),
            window,
            now,
            usize::try_from(limit).unwrap_or(0),
        )
    }

    /// Record that the reminder `memory_id` had at `remind_at` was delivered.
    /// A delivery already recorded is left as it is: two racing pollers must
    /// produce one delivery, not an error that aborts the pass.
    pub fn record_delivery(
        &self,
        memory_id: &str,
        remind_at: &str,
        delivered_at: &str,
    ) -> Result<()> {
        engine::reminders::record_delivery(&mut self.core.lock(), memory_id, remind_at, delivered_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    const NOW: &str = "2026-09-27T12:00:00+00:00";

    /// Reminders set, cleared, delivered and rescheduled, and what each
    /// window shows after every step.
    #[test]
    fn windows_follow_reminders_through_delivery_and_rescheduling() {
        let db = Database::open_in_memory().unwrap();
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

        let look = |window: ReminderWindow, limit: i64| -> Vec<String> {
            reminders
                .in_window(window, NOW, limit)
                .unwrap()
                .into_iter()
                .map(|m| format!("{}@{}", m.id, m.remind_at.unwrap_or_default()))
                .collect()
        };
        assert_eq!(
            look(ReminderWindow::Upcoming, 10),
            ["future@2026-09-28T00:00:00+00:00"]
        );
        assert_eq!(
            look(ReminderWindow::Overdue, 10),
            [
                "past@2026-09-27T11:00:00+00:00",
                "past2@2026-09-27T11:00:00+00:00"
            ],
            "a deleted memory's reminder does not show"
        );
        assert_eq!(look(ReminderWindow::Overdue, 1).len(), 1);
        assert_eq!(look(ReminderWindow::All, 10).len(), 3);

        reminders
            .record_delivery("past", "2026-09-27T11:00:00+00:00", NOW)
            .unwrap();
        reminders
            .record_delivery("past", "2026-09-27T11:00:00+00:00", "later")
            .unwrap();
        assert_eq!(
            look(ReminderWindow::Overdue, 10),
            ["past2@2026-09-27T11:00:00+00:00"],
            "delivered once, no longer overdue"
        );
        // A rescheduled reminder fires again.
        set("past", Some("2026-09-27T11:30:00+00:00"));
        set("past2", None);
        assert_eq!(
            look(ReminderWindow::Overdue, 10),
            ["past@2026-09-27T11:30:00+00:00"]
        );
        assert_eq!(look(ReminderWindow::All, 10).len(), 2);
        let live: Vec<String> = ["past", "none", "gone", "missing"]
            .iter()
            .map(|id| format!("{id}:{}", reminders.is_live(id).unwrap()))
            .collect();
        assert_eq!(live, ["past:true", "none:true", "gone:false", "missing:false"]);
    }
}
