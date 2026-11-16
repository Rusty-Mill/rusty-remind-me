//! Reminders on the engine core (ADR-0023, core PR 2c): the
//! reminder reads over memories and the `reminder_deliveries` log, as the
//! engine's adapter for the [`ReminderStore`] port (ADR-0035).

use super::core::{Change, CoreTables};
use super::memories::{self, MemoryRow};
use super::{core_ref, pair_engine_id, EngineTables};
use crate::db::reminders::ReminderStore;
use crate::db::Result;
use crate::models::{Memory, ReminderWindow};
use rusty_multimodal_db_engine::generic::query::GetById;
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Index marker: a delivery by its memory.
pub struct ByMemory;
/// Slot marker: unused, always 0.
pub struct Slot;

pub(crate) type DeliveryTable = GenericMmapStore<DeliveryRecord, ByMemory, Slot>;

/// One `reminder_deliveries` row: the reminder `memory_id` had at
/// `remind_at` was delivered at `delivered_at`. Keyed by the pair, which is
/// the table's unique index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeliveryRecord {
    engine_id: Uuid,
    memory_id: String,
    remind_at: String,
    delivered_at: String,
    slot: i64,
}

impl Record for DeliveryRecord {
    type Id = Uuid;
    fn id(&self) -> Uuid {
        self.engine_id
    }
}

impl SchemaTag for DeliveryRecord {
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::DeliveryRecord@1";
}

impl IndexedField<ByMemory> for DeliveryRecord {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.memory_id
    }
}

impl ScannableField<Slot> for DeliveryRecord {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.slot
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.slot = value;
    }
}

/// Whether the reminder `memory_id` had at `remind_at` has been delivered.
fn delivered(core: &CoreTables, memory_id: &str, remind_at: &str) -> bool {
    core.deliveries
        .get(pair_engine_id(memory_id, remind_at))
        .is_some_and(|d| d.memory_id == memory_id && d.remind_at == remind_at)
}

impl ReminderStore for EngineTables {
    fn is_live(&self, memory_id: &str) -> Result<bool> {
        Ok(memories::get_live(self, memory_id)?.is_some())
    }

    fn set_remind_at(
        &mut self,
        memory_id: &str,
        remind_at: Option<&str>,
        updated_at: &str,
    ) -> Result<()> {
        memories::set_remind_at(self, memory_id, remind_at, updated_at)
    }

    /// Text comparison, as SQLite's.
    fn in_window(&self, window: ReminderWindow, now: &str, limit: usize) -> Result<Vec<Memory>> {
        let core = core_ref(self)?;
        let due = |row: &MemoryRow, at: &str| {
            let upcoming = at > now;
            let overdue = at <= now && !delivered(core, &row.id, at);
            match window {
                ReminderWindow::Upcoming => upcoming,
                ReminderWindow::Overdue => overdue,
                ReminderWindow::All => upcoming || overdue,
            }
        };
        let mut rows: Vec<MemoryRow> = memories::rows(core)
            .filter(|row| row.deleted_at.is_none())
            .filter(|row| row.remind_at.as_deref().is_some_and(|at| due(row, at)))
            .collect();
        rows.sort_by(|a, b| (&a.remind_at, &a.id).cmp(&(&b.remind_at, &b.id)));
        Ok(rows.iter().take(limit).map(MemoryRow::to_memory).collect())
    }

    /// Unless one is recorded for the same reminder: `INSERT OR IGNORE` on
    /// the unique pair.
    fn record_delivery(
        &mut self,
        memory_id: &str,
        remind_at: &str,
        delivered_at: &str,
    ) -> Result<()> {
        if delivered(core_ref(self)?, memory_id, remind_at) {
            return Ok(());
        }
        let engine_id = pair_engine_id(memory_id, remind_at);
        let record = DeliveryRecord {
            engine_id,
            memory_id: memory_id.to_string(),
            remind_at: remind_at.to_string(),
            delivered_at: delivered_at.to_string(),
            slot: 0,
        };
        self.commit(vec![Change::Delivery(engine_id, Some(record))])
    }
}

/// The port's shared behaviour, on fresh temporary engine tables.
#[cfg(test)]
mod conformance_tests {
    use super::EngineTables;

    crate::db::reminders::reminder_conformance!(EngineTables::open_temporary().unwrap());
}
