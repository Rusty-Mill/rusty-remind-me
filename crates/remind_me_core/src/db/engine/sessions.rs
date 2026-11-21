//! `sessions` on the engine core (schema v32): each client session's start
//! and end, and the engine's adapter for the [`SessionStore`] port
//! (ADR-0035).
//!
//! The merge rules live beside the row ([`SessionRecord::started`] and
//! [`SessionRecord::ended`]), so every backend answers the same way by
//! construction.

use super::core::{Change, CoreTables};
use super::{core_ref, engine_id, EngineTables};
use crate::db::sessions::{SessionRecord, SessionStart, SessionStore};
use crate::db::Result;
use rusty_multimodal_db_engine::generic::query::{AllIds, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Index marker: a session by its id, which is also its key.
pub struct BySession;
/// Slot marker: unused, always 0.
pub struct Slot;

pub(crate) type SessionTable = GenericMmapStore<SessionRow, BySession, Slot>;

/// One `sessions` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRow {
    engine_id: Uuid,
    session_id: String,
    client: String,
    project: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
    started_at: String,
    ended_at: Option<String>,
    start_sha: Option<String>,
    end_sha: Option<String>,
    summary_memory_id: Option<String>,
    work_log_memory_id: Option<String>,
    node_id: Option<String>,
    slot: i64,
}

impl SessionRow {
    fn new(row: &SessionRecord) -> Self {
        Self {
            engine_id: engine_id(&row.session_id),
            session_id: row.session_id.clone(),
            client: row.client.clone(),
            project: row.project.clone(),
            cwd: row.cwd.clone(),
            git_branch: row.git_branch.clone(),
            started_at: row.started_at.clone(),
            ended_at: row.ended_at.clone(),
            start_sha: row.start_sha.clone(),
            end_sha: row.end_sha.clone(),
            summary_memory_id: row.summary_memory_id.clone(),
            work_log_memory_id: row.work_log_memory_id.clone(),
            node_id: row.node_id.clone(),
            slot: 0,
        }
    }

    fn to_record(&self) -> SessionRecord {
        SessionRecord {
            session_id: self.session_id.clone(),
            client: self.client.clone(),
            project: self.project.clone(),
            cwd: self.cwd.clone(),
            git_branch: self.git_branch.clone(),
            started_at: self.started_at.clone(),
            ended_at: self.ended_at.clone(),
            start_sha: self.start_sha.clone(),
            end_sha: self.end_sha.clone(),
            summary_memory_id: self.summary_memory_id.clone(),
            work_log_memory_id: self.work_log_memory_id.clone(),
            node_id: self.node_id.clone(),
        }
    }
}

impl Record for SessionRow {
    type Id = Uuid;
    fn id(&self) -> Uuid {
        self.engine_id
    }
}

impl SchemaTag for SessionRow {
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::SessionRow@1";
}

impl IndexedField<BySession> for SessionRow {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.session_id
    }
}

impl ScannableField<Slot> for SessionRow {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.slot
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.slot = value;
    }
}

fn stored(core: &CoreTables, session_id: &str) -> Option<SessionRecord> {
    core.sessions
        .get(engine_id(session_id))
        .filter(|r| r.session_id == session_id)
        .map(|r| r.to_record())
}

fn put(tables: &mut EngineTables, record: &SessionRecord) -> Result<()> {
    let row = SessionRow::new(record);
    let id = row.engine_id;
    tables.commit(vec![Change::Session(id, Some(Box::new(row)))])
}

impl SessionStore for EngineTables {
    /// Record that a session started, or refresh what its row knows: the SQL's
    /// `INSERT … ON CONFLICT DO UPDATE`.
    fn upsert_start(&mut self, start: &SessionStart) -> Result<()> {
        let current = stored(core_ref(self)?, &start.session_id);
        put(self, &SessionRecord::started(current, start))
    }

    /// Record that a session ended. Whether there was a row to end.
    fn end(
        &mut self,
        session_id: &str,
        ended_at: &str,
        end_sha: Option<&str>,
        work_log_memory_id: Option<&str>,
    ) -> Result<bool> {
        let Some(mut current) = stored(core_ref(self)?, session_id) else {
            return Ok(false);
        };
        current.ended(ended_at, end_sha, work_log_memory_id);
        put(self, &current)?;
        Ok(true)
    }

    fn get(&self, session_id: &str) -> Result<Option<SessionRecord>> {
        Ok(stored(core_ref(self)?, session_id))
    }

    /// The most recently started sessions, of `project` when given, newest
    /// first (ties by id, descending), at most `limit`.
    fn recent(&self, limit: usize, project: Option<&str>) -> Result<Vec<SessionRecord>> {
        let core = core_ref(self)?;
        let mut rows: Vec<SessionRow> = core
            .sessions
            .all_ids()
            .into_iter()
            .filter_map(|id| core.sessions.get(id))
            .filter(|r| !project.is_some_and(|p| r.project.as_deref() != Some(p)))
            .collect();
        rows.sort_by(|a, b| (&b.started_at, &b.session_id).cmp(&(&a.started_at, &a.session_id)));
        Ok(rows.iter().take(limit).map(SessionRow::to_record).collect())
    }
}

#[cfg(test)]
mod conformance_tests {
    use super::EngineTables;
    crate::db::sessions::session_conformance!(EngineTables::open_temporary().unwrap());
}
