//! Feedback on the engine core (ADR-0023, core PR 2c): the
//! `memory_feedback` event log and the review reads over memories.
//! [`crate::db::feedback`] calls these when its store carries the core.

use super::core::{Change, CoreTables};
use super::memories::{self, MemoryRow};
use super::{core_ref, engine_error, engine_id, EngineTables};
use crate::db::feedback::{FeedbackEvent, ReviewFilter};
use crate::db::Result;
use crate::models::RecalibrateCandidate;
use rusty_multimodal_db_engine::generic::query::{FilterEq, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Index marker: an event by its memory.
pub struct ByMemory;
/// Slot marker: unused, always 0.
pub struct Slot;

pub(crate) type FeedbackTable = GenericMmapStore<FeedbackRecord, ByMemory, Slot>;

/// One `memory_feedback` row, keyed by its text id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeedbackRecord {
    engine_id: Uuid,
    id: String,
    memory_id: String,
    query: String,
    query_tokens: String,
    signal: String,
    magnitude: f64,
    created_at: String,
    slot: i64,
}

impl Record for FeedbackRecord {
    type Id = Uuid;
    fn id(&self) -> Uuid {
        self.engine_id
    }
}

impl SchemaTag for FeedbackRecord {
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::FeedbackRecord@1";
}

impl IndexedField<ByMemory> for FeedbackRecord {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.memory_id
    }
}

impl ScannableField<Slot> for FeedbackRecord {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.slot
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.slot = value;
    }
}

fn events_of(core: &CoreTables, memory_id: &str) -> Vec<FeedbackRecord> {
    let mut events: Vec<FeedbackRecord> =
        FilterEq::<FeedbackRecord, ByMemory>::filter_eq(&core.feedback, &memory_id.to_string())
            .into_iter()
            .filter_map(|id| core.feedback.get(id))
            .filter(|e| e.memory_id == memory_id)
            .collect();
    events.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
    events
}

/// Log an event under `id`. As the table's `PRIMARY KEY` and `CHECK` do, a
/// taken id and a signal other than `helpful` or `unhelpful` are refused.
pub(crate) fn log_event(
    tables: &mut EngineTables,
    id: &str,
    memory_id: &str,
    query: &str,
    event: &FeedbackEvent,
    created_at: &str,
) -> Result<()> {
    if !matches!(event.signal.as_str(), "helpful" | "unhelpful") {
        return Err(engine_error(format!(
            "feedback signal {:?} is neither helpful nor unhelpful",
            event.signal
        )));
    }
    let engine_id = engine_id(id);
    if core_ref(tables)?.feedback.get(engine_id).is_some() {
        return Err(engine_error(format!("feedback {id:?} already exists")));
    }
    let record = FeedbackRecord {
        engine_id,
        id: id.to_string(),
        memory_id: memory_id.to_string(),
        query: query.to_string(),
        query_tokens: event.query_tokens.clone(),
        signal: event.signal.clone(),
        magnitude: event.magnitude,
        created_at: created_at.to_string(),
        slot: 0,
    };
    tables.commit(vec![Change::Feedback(engine_id, Some(record))])
}

/// Every event logged for `memory_id`, oldest first, ties by id.
pub(crate) fn events(tables: &EngineTables, memory_id: &str) -> Result<Vec<FeedbackEvent>> {
    Ok(events_of(core_ref(tables)?, memory_id)
        .into_iter()
        .map(|e| FeedbackEvent {
            query_tokens: e.query_tokens,
            signal: e.signal,
            magnitude: e.magnitude,
        })
        .collect())
}

/// Remove every event logged for `memory_id`, in one batch.
pub(crate) fn delete_for(tables: &mut EngineTables, memory_id: &str) -> Result<()> {
    let changes = events_of(core_ref(tables)?, memory_id)
        .into_iter()
        .map(|e| Change::Feedback(e.engine_id, None))
        .collect();
    tables.commit(changes)
}

/// Days from `timestamp` to `now`, as `julianday('now') - julianday(ts)`
/// computes them, or `None` where SQLite's `julianday` would be `NULL`.
fn days_since(timestamp: &str, now: chrono::DateTime<chrono::Utc>) -> Option<f64> {
    use chrono::{NaiveDate, NaiveDateTime};
    let at = chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|t| t.naive_utc())
        .ok()
        .or_else(|| {
            ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"]
                .iter()
                .find_map(|f| NaiveDateTime::parse_from_str(timestamp, f).ok())
        })
        .or_else(|| {
            NaiveDate::parse_from_str(timestamp, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
        })?;
    let micros = (now.naive_utc() - at).num_microseconds()?;
    Some(micros as f64 / 86_400_000_000.0)
}

/// The memories `filter` makes due for review: live, unsuperseded, weighty
/// or of a durable type, unread for `stale_days`, and never given feedback.
fn due(core: &CoreTables, filter: &ReviewFilter<'_>) -> Vec<MemoryRow> {
    let now = chrono::Utc::now();
    memories::rows(core)
        .filter(|row| row.deleted_at.is_none() && row.superseded_by.is_none())
        .filter(|row| {
            row.base_weight >= filter.min_base_weight
                || filter.durable_types.contains(&row.memory_type.as_str())
        })
        .filter(|row| {
            let last = row.accessed_at.as_deref().unwrap_or(&row.created_at);
            days_since(last, now).is_some_and(|days| days >= filter.stale_days as f64)
        })
        .filter(|row| events_of(core, &row.id).is_empty())
        .collect()
}

pub(crate) fn review_count(tables: &EngineTables, filter: &ReviewFilter<'_>) -> Result<i64> {
    Ok(i64::try_from(due(core_ref(tables)?, filter).len()).unwrap_or(i64::MAX))
}

/// What `Feedback::review_batch` selects: heaviest first, then longest
/// unread (a never-read memory first, as SQLite sorts `NULL`), ties by id.
pub(crate) fn review_batch(
    tables: &EngineTables,
    filter: &ReviewFilter<'_>,
    snippet_chars: usize,
    limit: usize,
) -> Result<Vec<RecalibrateCandidate>> {
    let mut rows = due(core_ref(tables)?, filter);
    rows.sort_by(|a, b| {
        b.base_weight
            .total_cmp(&a.base_weight)
            .then_with(|| a.accessed_at.cmp(&b.accessed_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(rows
        .into_iter()
        .take(limit)
        .map(|row| RecalibrateCandidate {
            content_snippet: row.content.chars().take(snippet_chars).collect(),
            memory_type: Some(row.memory_type),
            id: row.id,
            category: row.category,
            base_weight: row.base_weight,
            access_count: row.access_count,
            accessed_at: row.accessed_at,
            created_at: row.created_at,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_are_what_julianday_computes() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-27T12:00:00+00:00")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(days_since("2026-09-26T12:00:00+00:00", now), Some(1.0));
        assert_eq!(days_since("2026-09-27T14:00:00+02:00", now), Some(0.0));
        assert_eq!(days_since("2026-09-25 12:00:00", now), Some(2.0));
        assert_eq!(days_since("2026-09-27", now), Some(0.5));
        assert_eq!(days_since("never", now), None);
    }
}
