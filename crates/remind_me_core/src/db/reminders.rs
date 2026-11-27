//! Storage for reminders: `memories.remind_at` and the
//! `reminder_deliveries` rows that record which reminders have fired.
//!
//! [`ReminderStore`] is the port (ADR-0035); the engine's adapter is in
//! `db::engine::reminders`. [`Reminders`] is what callers take: it locks the
//! store's tables for each call and goes through the port. The rules stay in
//! [`crate::reminders`] and [`crate::scheduler`]: what a valid `remind_at`
//! is, that a reminder must be in the future, and when a delivery counts.

use super::backend::BackendLock;
use super::{Result, Store};
use crate::models::{Memory, ReminderWindow};

/// The reminder columns and the delivery log, as a backend stores them
/// (ADR-0035). [`conformance`] holds the behaviour every implementation
/// must show.
pub trait ReminderStore {
    /// Whether `memory_id` names a memory that is not deleted.
    fn is_live(&self, memory_id: &str) -> Result<bool>;

    /// Set `memory_id`'s `remind_at`, or clear it with `None`, stamping
    /// `updated_at`.
    fn set_remind_at(
        &mut self,
        memory_id: &str,
        remind_at: Option<&str>,
        updated_at: &str,
    ) -> Result<()>;

    /// Live memories with a reminder in `window` as of `now`, soonest first
    /// (ties by id), at most `limit`. Times compare as text.
    ///
    /// - `Upcoming`: `remind_at` after `now`.
    /// - `Overdue`: `remind_at` at or before `now`, with no delivery recorded
    ///   for that exact `remind_at`, so a rescheduled reminder fires again.
    /// - `All`: either.
    fn in_window(&self, window: ReminderWindow, now: &str, limit: usize) -> Result<Vec<Memory>>;

    /// [`ReminderStore::in_window`] restricted to the memories `keep`
    /// accepts, in the same order, at most `limit`. `keep` runs before the
    /// limit, so rejected memories never take a slot. A backend reads its
    /// reminders once and holds no more than a bounded batch of the kept
    /// ones; this default, for a backend that has not done so, reads the
    /// whole window.
    fn in_window_where(
        &self,
        window: ReminderWindow,
        now: &str,
        limit: usize,
        keep: &dyn Fn(&Memory) -> bool,
    ) -> Result<Vec<Memory>> {
        let all = self.in_window(window, now, usize::MAX)?;
        Ok(all.into_iter().filter(|m| keep(m)).take(limit).collect())
    }

    /// Record that the reminder `memory_id` had at `remind_at` was
    /// delivered. A delivery already recorded is left as it is.
    fn record_delivery(
        &mut self,
        memory_id: &str,
        remind_at: &str,
        delivered_at: &str,
    ) -> Result<()>;
}

/// The reminder columns and tables of a [`Store`], one locked call at a
/// time.
pub struct Reminders<'c> {
    backend: &'c BackendLock,
}

impl<'c> Reminders<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            backend: store.backend(),
        }
    }

    /// See [`ReminderStore::is_live`].
    pub fn is_live(&self, memory_id: &str) -> Result<bool> {
        ReminderStore::is_live(&*self.backend.lock()?, memory_id)
    }

    /// See [`ReminderStore::set_remind_at`]. The stamp is what LWW compares.
    pub fn set_remind_at(
        &self,
        memory_id: &str,
        remind_at: Option<&str>,
        updated_at: &str,
    ) -> Result<()> {
        ReminderStore::set_remind_at(&mut *self.backend.lock()?, memory_id, remind_at, updated_at)
    }

    /// See [`ReminderStore::in_window`]. A negative `limit` returns nothing.
    pub fn in_window(&self, window: ReminderWindow, now: &str, limit: i64) -> Result<Vec<Memory>> {
        let limit = usize::try_from(limit).unwrap_or(0);
        ReminderStore::in_window(&*self.backend.lock()?, window, now, limit)
    }

    /// See [`ReminderStore::in_window_where`]. A negative `limit` returns
    /// nothing.
    pub fn in_window_where(
        &self,
        window: ReminderWindow,
        now: &str,
        limit: i64,
        keep: &dyn Fn(&Memory) -> bool,
    ) -> Result<Vec<Memory>> {
        let limit = usize::try_from(limit).unwrap_or(0);
        ReminderStore::in_window_where(&*self.backend.lock()?, window, now, limit, keep)
    }

    /// See [`ReminderStore::record_delivery`]: two racing pollers must
    /// produce one delivery, not an error that aborts the pass.
    pub fn record_delivery(
        &self,
        memory_id: &str,
        remind_at: &str,
        delivered_at: &str,
    ) -> Result<()> {
        ReminderStore::record_delivery(
            &mut *self.backend.lock()?,
            memory_id,
            remind_at,
            delivered_at,
        )
    }
}

/// The behaviour every [`ReminderStore`] must show, run against each backend
/// by [`reminder_conformance`] (ADR-0035).
///
/// Reminders hang off memories, so a backend under test implements
/// [`MemoryStore`](crate::db::memories::MemoryStore) too, and the checks seed
/// memories through it.
#[doc(hidden)]
pub mod conformance {
    use super::ReminderStore;
    use crate::db::memories::{MemoryStore, NewMemory};
    use crate::models::ReminderWindow;

    pub const NOW: &str = "2026-09-27T12:00:00+00:00";

    /// Add a live memory `id` with content `"x"`.
    fn add<S: MemoryStore>(store: &mut S, id: &str) {
        MemoryStore::insert(store, &NewMemory::new(id, "x", NOW)).unwrap();
    }

    /// Tombstone memory `id`.
    fn delete<S: MemoryStore>(store: &mut S, id: &str) {
        MemoryStore::delete_live(store, id, Some(NOW)).unwrap();
    }

    fn look<S: ReminderStore>(store: &S, window: ReminderWindow, limit: usize) -> Vec<String> {
        store
            .in_window(window, NOW, limit)
            .unwrap()
            .into_iter()
            .map(|m| format!("{}@{}", m.id, m.remind_at.unwrap_or_default()))
            .collect()
    }

    /// Reminders set, cleared, delivered and rescheduled, and what each
    /// window shows after every step.
    pub fn windows_follow_delivery_and_rescheduling<S: ReminderStore + MemoryStore>(store: &mut S) {
        for id in ["past", "past2", "future", "none", "gone"] {
            add(store, id);
        }
        let set = |store: &mut S, id: &str, at: Option<&str>| {
            store.set_remind_at(id, at, NOW).unwrap();
        };
        set(store, "past", Some("2026-09-27T11:00:00+00:00"));
        set(store, "past2", Some("2026-09-27T11:00:00+00:00"));
        set(store, "future", Some("2026-09-28T00:00:00+00:00"));
        set(store, "gone", Some("2026-09-27T10:00:00+00:00"));
        delete(store, "gone");

        assert_eq!(
            look(store, ReminderWindow::Upcoming, 10),
            ["future@2026-09-28T00:00:00+00:00"]
        );
        assert_eq!(
            look(store, ReminderWindow::Overdue, 10),
            [
                "past@2026-09-27T11:00:00+00:00",
                "past2@2026-09-27T11:00:00+00:00"
            ],
            "a deleted memory's reminder does not show"
        );
        assert_eq!(look(store, ReminderWindow::Overdue, 1).len(), 1);
        assert!(look(store, ReminderWindow::All, 0).is_empty());
        assert_eq!(look(store, ReminderWindow::All, 10).len(), 3);

        store
            .record_delivery("past", "2026-09-27T11:00:00+00:00", NOW)
            .unwrap();
        store
            .record_delivery("past", "2026-09-27T11:00:00+00:00", "later")
            .unwrap();
        assert_eq!(
            look(store, ReminderWindow::Overdue, 10),
            ["past2@2026-09-27T11:00:00+00:00"],
            "delivered once, no longer overdue"
        );
        // A rescheduled reminder fires again.
        set(store, "past", Some("2026-09-27T11:30:00+00:00"));
        set(store, "past2", None);
        assert_eq!(
            look(store, ReminderWindow::Overdue, 10),
            ["past@2026-09-27T11:30:00+00:00"]
        );
        assert_eq!(look(store, ReminderWindow::All, 10).len(), 2);
    }

    /// `keep` filters before the limit and the order is `in_window`'s.
    pub fn a_filter_runs_before_the_limit<S: ReminderStore + MemoryStore>(store: &mut S) {
        for (id, at) in [
            ("a", "2026-09-28T01:00:00+00:00"),
            ("b", "2026-09-28T02:00:00+00:00"),
            ("c", "2026-09-28T03:00:00+00:00"),
            ("d", "2026-09-28T04:00:00+00:00"),
            ("e", "2026-09-28T05:00:00+00:00"),
        ] {
            add(store, id);
            store.set_remind_at(id, Some(at), NOW).unwrap();
        }
        let ids = |limit: usize, keep: &dyn Fn(&crate::models::Memory) -> bool| -> Vec<String> {
            store
                .in_window_where(ReminderWindow::All, NOW, limit, keep)
                .unwrap()
                .into_iter()
                .map(|m| m.id)
                .collect()
        };
        assert_eq!(ids(2, &|m| m.id != "a" && m.id != "b"), ["c", "d"]);
        assert_eq!(ids(10, &|m| m.id != "c"), ["a", "b", "d", "e"]);
        assert_eq!(ids(1, &|_| true), ["a"]);
        assert!(ids(0, &|_| true).is_empty());
        assert!(ids(10, &|_| false).is_empty());
    }

    pub fn only_undeleted_memories_are_live<S: ReminderStore + MemoryStore>(store: &mut S) {
        add(store, "kept");
        add(store, "gone");
        delete(store, "gone");
        let live: Vec<String> = ["kept", "gone", "missing"]
            .iter()
            .map(|id| format!("{id}:{}", store.is_live(id).unwrap()))
            .collect();
        assert_eq!(live, ["kept:true", "gone:false", "missing:false"]);
    }
}

/// One `#[test]` per [`conformance`] check, each on a fresh store from
/// `$make`.
#[doc(hidden)]
#[macro_export]
macro_rules! reminder_conformance {
    ($make:expr) => {
        use $crate::db::reminders::conformance;

        #[test]
        fn windows_follow_delivery_and_rescheduling() {
            conformance::windows_follow_delivery_and_rescheduling(&mut $make);
        }
        #[test]
        fn a_filter_runs_before_the_limit() {
            conformance::a_filter_runs_before_the_limit(&mut $make);
        }
        #[test]
        fn only_undeleted_memories_are_live() {
            conformance::only_undeleted_memories_are_live(&mut $make);
        }
    };
}
#[doc(hidden)]
pub use crate::reminder_conformance;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::{Memories, NewMemory};
    use crate::db::Database;

    const NOW: &str = conformance::NOW;

    #[test]
    fn the_repository_reaches_the_store_through_the_port() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        Memories::new(&store)
            .insert(&NewMemory::new("m", "x", NOW))
            .unwrap();
        let reminders = Reminders::new(&store);
        reminders
            .set_remind_at("m", Some("2026-09-27T11:00:00+00:00"), NOW)
            .unwrap();
        assert!(reminders.is_live("m").unwrap());
        assert_eq!(
            reminders
                .in_window(ReminderWindow::Overdue, NOW, 10)
                .unwrap()
                .len(),
            1
        );
        assert!(
            reminders
                .in_window(ReminderWindow::Overdue, NOW, -1)
                .unwrap()
                .is_empty(),
            "a negative limit returns nothing"
        );
        reminders
            .record_delivery("m", "2026-09-27T11:00:00+00:00", NOW)
            .unwrap();
        assert!(reminders
            .in_window(ReminderWindow::Overdue, NOW, 10)
            .unwrap()
            .is_empty());
    }
}
