//! Memory revisions on the engine (ADR-0023, phase 4g): one row per edit,
//! holding the tracked columns the edit replaced. This is the engine's
//! adapter for the [`HistoryStore`] port (ADR-0035).
//!
//! A revision's id is an integer from the journal's `memory_revisions`
//! sequence, as SQLite's `INTEGER PRIMARY KEY` gives it: a caller passes it
//! back to revert, so it must never be issued twice.

use super::capture;
use super::{core_ref, deleted, engine_error, memories, micros, EngineTables};
use crate::db::history::{HistoryStore, Tracked};
use crate::db::Result;
use crate::models::MemoryRevision;
use rusty_multimodal_db_engine::generic::query::{AllIds, FilterEq, GetById};
use rusty_multimodal_db_engine::generic::traits::{
    IndexedField, Record, ScannableField, SchemaTag,
};
use rusty_multimodal_db_engine::generic::GenericMmapStore;
use serde::{Deserialize, Serialize};

/// The journal sequence revision ids come from.
pub(crate) const SEQUENCE: &str = "memory_revisions";

/// Index marker: a revision by the memory it belongs to.
pub struct ByMemory;
/// Slot marker: when the edit happened, in µs since the epoch.
pub struct EditedAt;

pub(crate) type RevisionTable = GenericMmapStore<RevisionRecord, ByMemory, EditedAt>;

/// One `memory_revisions` row.
///
/// Schema v33 added `project` (card #702), so the layout tag moved from `@1`
/// to `@2`; [`super::revisions_v1`] reads and upgrades a `@1` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevisionRecord {
    pub(super) id: i64,
    pub(super) memory_id: String,
    pub(super) content: String,
    pub(super) category: String,
    pub(super) tags: String,
    pub(super) metadata: String,
    pub(super) sensitive: Option<bool>,
    pub(super) edited_at: String,
    pub(super) edited_us: i64,
    pub(super) revision_reason: Option<String>,
    /// The project the edit replaced, or `None` when unknown (a revision
    /// from before v33, or copied from SQLite, which never stored it).
    #[serde(default, with = "known_project")]
    pub(super) project: Option<Option<String>>,
}

/// `RevisionRecord::project` in JSON (the undo log, the redo journal and
/// a change sink) without losing "known: no project" (Q22).
///
/// Plain JSON writes `Some(None)` and `None` both as `null`, and reads
/// `null` back as `None`, so a revision that cleared its project would
/// come back as one whose project is unknown, and revert would then leave
/// the project alone. JSON writes it as `{"known": null}` instead; a name
/// and an unknown project are written as before, so older JSON reads the
/// same. Bincode, the table's own format, is not human-readable and keeps
/// the derived encoding, so the table's layout does not change.
mod known_project {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum Json {
        Name(String),
        Known { known: Option<String> },
    }

    pub(super) fn serialize<S: Serializer>(
        project: &Option<Option<String>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        if !serializer.is_human_readable() {
            return project.serialize(serializer);
        }
        match project {
            None => serializer.serialize_none(),
            Some(Some(name)) => serializer.serialize_some(&Json::Name(name.clone())),
            Some(None) => serializer.serialize_some(&Json::Known { known: None }),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Option<String>>, D::Error> {
        if !deserializer.is_human_readable() {
            return Option::<Option<String>>::deserialize(deserializer);
        }
        Ok(match Option::<Json>::deserialize(deserializer)? {
            None => None,
            Some(Json::Name(name)) => Some(Some(name)),
            Some(Json::Known { known }) => Some(known),
        })
    }
}

impl RevisionRecord {
    fn to_revision(&self) -> MemoryRevision {
        MemoryRevision {
            id: self.id,
            memory_id: self.memory_id.clone(),
            content: self.content.clone(),
            category: self.category.clone(),
            tags: self.tags.clone(),
            metadata: self.metadata.clone(),
            sensitive: self.sensitive,
            edited_at: self.edited_at.clone(),
            revision_reason: self.revision_reason.clone(),
            project: self.project.clone(),
        }
    }

    fn tracked(&self) -> Tracked {
        Tracked {
            content: self.content.clone(),
            category: self.category.clone(),
            tags: self.tags.clone(),
            metadata: self.metadata.clone(),
            sensitive: self.sensitive,
            project: self.project.clone(),
        }
    }
}

impl Record for RevisionRecord {
    type Id = i64;
    fn id(&self) -> i64 {
        self.id
    }
}

impl SchemaTag for RevisionRecord {
    const SCHEMA_TAG: &'static str = "rusty_remind_me::node::RevisionRecord@2";
}

impl IndexedField<ByMemory> for RevisionRecord {
    type IndexValue = String;
    fn indexed_value(&self) -> &String {
        &self.memory_id
    }
}

impl ScannableField<EditedAt> for RevisionRecord {
    type ScanValue = i64;
    fn scannable_value(&self) -> i64 {
        self.edited_us
    }
    fn set_scannable_value(&mut self, value: i64) {
        self.edited_us = value;
    }
}

impl HistoryStore for EngineTables {
    fn is_live(&self, memory_id: &str) -> Result<bool> {
        Ok(memories::get_live(self, memory_id)?.is_some())
    }

    fn current(&self, memory_id: &str) -> Result<Option<Tracked>> {
        memories::tracked(self, memory_id)
    }

    fn delete_for(&mut self, memory_id: &str) -> Result<()> {
        self.logged(|t| {
            for record in of_memory(t, memory_id) {
                let noted = t.note(capture::REVISIONS, &record.id, None::<&RevisionRecord>)?;
                deleted(t.revisions.delete(record.id))?;
                t.emit(noted)?;
            }
            Ok(())
        })
    }

    fn delete_of_tombstones(&mut self) -> Result<usize> {
        self.logged(|t| {
            let core = core_ref(t)?;
            let doomed: Vec<i64> = t
                .revisions
                .all_ids()
                .into_iter()
                .filter_map(|id| t.revisions.get(id))
                .filter(|r| memories::is_tombstone(core, &r.memory_id))
                .map(|r| r.id)
                .collect();
            for id in &doomed {
                let noted = t.note(capture::REVISIONS, id, None::<&RevisionRecord>)?;
                deleted(t.revisions.delete(*id))?;
                t.emit(noted)?;
            }
            Ok(doomed.len())
        })
    }

    fn insert(
        &mut self,
        memory_id: &str,
        values: &Tracked,
        edited_at: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        let id = self.next_id(SEQUENCE)?;
        let record = RevisionRecord {
            id,
            memory_id: memory_id.to_string(),
            content: values.content.clone(),
            category: values.category.clone(),
            tags: values.tags.clone(),
            metadata: values.metadata.clone(),
            sensitive: values.sensitive,
            edited_at: edited_at.to_string(),
            edited_us: micros(edited_at),
            revision_reason: reason.map(str::to_string),
            project: values.project.clone(),
        };
        let noted = self.note(capture::REVISIONS, &id, Some(&record))?;
        self.revisions.insert(record).map_err(engine_error)?;
        self.emit(noted)
    }

    /// `edited_at` compared as SQLite compares the text.
    fn list(&self, memory_id: &str, limit: usize) -> Result<Vec<MemoryRevision>> {
        let mut records = of_memory(self, memory_id);
        records.sort_by(|a, b| (b.edited_at.as_str(), b.id).cmp(&(a.edited_at.as_str(), a.id)));
        Ok(records
            .iter()
            .take(limit)
            .map(RevisionRecord::to_revision)
            .collect())
    }

    /// Ids from the memory index, newest first; each record is read only
    /// when the walk reaches it.
    fn content_revised_at(&self, memory_id: &str, content: &str) -> Result<Option<String>> {
        let mut ids = FilterEq::<RevisionRecord, ByMemory>::filter_eq(
            &self.revisions,
            &memory_id.to_string(),
        );
        ids.sort_unstable_by(|a, b| b.cmp(a));
        let mut replaced_by = content.to_string();
        for id in ids {
            let Some(record) = self.revisions.get(id) else {
                continue;
            };
            if record.content != replaced_by {
                return Ok(Some(record.edited_at));
            }
            replaced_by = record.content;
        }
        Ok(None)
    }

    fn revision(&self, memory_id: &str, revision_id: i64) -> Result<Option<Tracked>> {
        Ok(self
            .revisions
            .get(revision_id)
            .filter(|r| r.memory_id == memory_id)
            .map(|r| r.tracked()))
    }

    fn restore(&mut self, memory_id: &str, values: &Tracked, updated_at: &str) -> Result<()> {
        memories::restore_tracked(self, memory_id, values, updated_at)
    }
}

/// The highest revision id stored, or 0: the floor for the sequence when
/// the tables open.
pub(crate) fn max_id(table: &RevisionTable) -> u64 {
    table
        .all_ids()
        .into_iter()
        .max()
        .map_or(0, |id| u64::try_from(id).unwrap_or(0))
}

fn of_memory(tables: &EngineTables, memory_id: &str) -> Vec<RevisionRecord> {
    FilterEq::<RevisionRecord, ByMemory>::filter_eq(&tables.revisions, &memory_id.to_string())
        .into_iter()
        .filter_map(|id| tables.revisions.get(id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(project: Option<Option<String>>) -> RevisionRecord {
        RevisionRecord {
            id: 1,
            memory_id: "m".into(),
            content: "c".into(),
            category: "fact".into(),
            tags: "[]".into(),
            metadata: "{}".into(),
            sensitive: None,
            edited_at: "2026-09-27T00:00:00+00:00".into(),
            edited_us: 0,
            revision_reason: None,
            project,
        }
    }

    #[test]
    fn json_keeps_a_known_empty_project_apart_from_an_unknown_one() {
        for project in [None, Some(None), Some(Some("p".to_string()))] {
            let json = serde_json::to_vec(&record(project.clone())).unwrap();
            let back: RevisionRecord = serde_json::from_slice(&json).unwrap();
            assert_eq!(back.project, project);
        }
        let known_none = serde_json::to_value(record(Some(None))).unwrap();
        assert_eq!(known_none["project"], serde_json::json!({ "known": null }));
    }

    #[test]
    fn json_from_before_the_known_form_reads_as_it_did() {
        let mut json = serde_json::to_value(record(None)).unwrap();
        json["project"] = serde_json::json!("p");
        let named: RevisionRecord = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(named.project, Some(Some("p".to_string())));
        json["project"] = serde_json::Value::Null;
        let unknown: RevisionRecord = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(unknown.project, None);
        json.as_object_mut().unwrap().remove("project");
        let absent: RevisionRecord = serde_json::from_value(json).unwrap();
        assert_eq!(absent.project, None);
    }

    #[test]
    fn every_project_state_survives_the_tables_own_format() {
        let dir = std::env::temp_dir().join(format!(
            "remind_me_engine_revisions_project_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let states = [None, Some(None), Some(Some("p".to_string()))];
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            for (i, project) in states.iter().enumerate() {
                let tracked = Tracked {
                    project: project.clone(),
                    ..values("a")
                };
                HistoryStore::insert(
                    &mut tables,
                    &format!("m{i}"),
                    &tracked,
                    "2026-09-27T00:00:00+00:00",
                    None,
                )
                .unwrap();
            }
        }
        let tables = crate::db::engine::reopen(&dir);
        for (i, project) in states.iter().enumerate() {
            let listed = HistoryStore::list(&tables, &format!("m{i}"), 10).unwrap();
            assert_eq!(&listed[0].project, project);
        }
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn values(content: &str) -> Tracked {
        Tracked {
            content: content.to_string(),
            category: "fact".to_string(),
            tags: "[]".to_string(),
            metadata: "{}".to_string(),
            sensitive: None,
            project: None,
        }
    }

    #[test]
    fn revisions_and_their_ids_survive_a_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "remind_me_engine_revisions_reopen_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut tables = EngineTables::open(&dir).unwrap();
            HistoryStore::insert(
                &mut tables,
                "m1",
                &values("a"),
                "2026-09-27T00:00:00+00:00",
                None,
            )
            .unwrap();
        }
        let mut tables = crate::db::engine::reopen(&dir);
        HistoryStore::insert(
            &mut tables,
            "m1",
            &values("b"),
            "2026-09-27T00:00:01+00:00",
            Some("why"),
        )
        .unwrap();
        let listed = HistoryStore::list(&tables, "m1", 10).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(
            listed[0].id > listed[1].id,
            "newest first, ids never reused"
        );
        assert_eq!(listed[0].revision_reason.as_deref(), Some("why"));
        assert_eq!(
            tables.revision("m1", listed[1].id).unwrap(),
            Some(values("a"))
        );
        drop(tables);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// The port's shared behaviour, on fresh temporary engine tables.
#[cfg(test)]
mod conformance_tests {
    use super::EngineTables;

    crate::db::history::history_conformance!(EngineTables::open_temporary().unwrap());
}
