//! Storage for memory rows: every insert, sync upsert and field update of
//! `memories`, on the engine's memories core (`db::engine::memories`).
//!
//! Every write keeps the derived data (the full-text index, the tag index
//! and the sync outbox) in step, as triggers did up to schema v30. The rules
//! stay with their modules: what a capture, a promotion or a consolidation
//! writes, and how vitality is seeded, are decided there and handed over as
//! a [`NewMemory`] or a field value.

use super::engine::{self, EngineLock};
use super::{Result, Store};
use crate::models::{Memory, UnannotatedMemory, UnclassifiedMemory};
use crate::sync::TOMBSTONE_CONTENT;
use serde_json::Value;
use std::collections::HashSet;

/// A whole `memories` row, as a writer supplies it.
///
/// [`NewMemory::new`] fills every column with the schema's own default, so a
/// writer names only the columns it sets.
#[derive(Debug, Clone, PartialEq)]
pub struct NewMemory {
    pub id: String,
    pub content: String,
    pub category: String,
    pub tags: Vec<String>,
    pub source: String,
    pub metadata: Value,
    pub created_at: String,
    pub updated_at: String,
    pub capture_id: Option<String>,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    pub superseded_by: Option<String>,
    pub decay_rate: f64,
    pub vitality: f64,
    pub base_weight: f64,
    pub access_count: i64,
    pub accessed_at: Option<String>,
    pub doc_id: Option<String>,
    pub chunk_index: Option<i64>,
    pub remind_at: Option<String>,
    pub sensitive: bool,
    pub memory_type: String,
    pub status: String,
    pub node_id: Option<String>,
    pub client: String,
    pub source_capture_id: Option<String>,
    pub deleted_at: Option<String>,
    // Schema v32 (see `models::Memory` for what each holds).
    pub project: Option<String>,
    pub session_id: Option<String>,
    pub git_remote: Option<String>,
    pub git_branch: Option<String>,
    pub git_sha: Option<String>,
    pub cwd: Option<String>,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    pub confidence: f64,
    pub verified_at: Option<String>,
    pub outcome: Option<String>,
    pub written_by: String,
    pub capture_method: String,
}

impl NewMemory {
    /// Drop a tombstone's text, keeping what last-write-wins and re-imports
    /// read (ADR-0024). A live memory is left as it is.
    pub fn empty_if_tombstone(&mut self) {
        if self.deleted_at.is_none() {
            return;
        }
        self.content = TOMBSTONE_CONTENT.to_string();
        self.tags.clear();
        self.subject = None;
        self.predicate = None;
        self.object = None;
    }

    /// A row holding `content` under `id`, created and updated at `now`, with
    /// every other column at the schema's default (what the SQLite store's
    /// `DEFAULT` clauses gave a row; the engine record keeps them).
    pub fn new(id: impl Into<String>, content: impl Into<String>, now: &str) -> Self {
        Self {
            id: id.into(),
            content: content.into(),
            category: "general".to_string(),
            tags: Vec::new(),
            source: "manual".to_string(),
            metadata: Value::Object(serde_json::Map::new()),
            created_at: now.to_string(),
            updated_at: now.to_string(),
            capture_id: None,
            subject: None,
            predicate: None,
            object: None,
            superseded_by: None,
            decay_rate: 0.1,
            vitality: 1.0,
            base_weight: 1.0,
            access_count: 0,
            accessed_at: None,
            doc_id: None,
            chunk_index: None,
            remind_at: None,
            sensitive: false,
            memory_type: "unclassified".to_string(),
            status: "active".to_string(),
            node_id: None,
            client: "unknown".to_string(),
            source_capture_id: None,
            deleted_at: None,
            project: None,
            session_id: None,
            git_remote: None,
            git_branch: None,
            git_sha: None,
            cwd: None,
            valid_from: None,
            valid_until: None,
            confidence: crate::models::default_confidence(),
            verified_at: None,
            outcome: None,
            written_by: crate::models::default_written_by(),
            capture_method: crate::models::default_capture_method(),
        }
    }
}

/// What a sync merge needs of the local copy of a memory.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncView {
    pub tags: Vec<String>,
    pub metadata: Value,
    pub updated_at: String,
    /// Whether the local copy is a tombstone.
    pub deleted: bool,
}

/// What recording an access needs of a memory.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessInputs {
    pub id: String,
    pub access_count: i64,
    pub decay_rate: f64,
    pub base_weight: f64,
}

/// A memory's subject-predicate-object triple, all three present.
#[derive(Debug, Clone, PartialEq)]
pub struct Triple {
    pub id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
}

/// A live memory as the wiki's compile brief shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatedMemory {
    pub id: String,
    pub category: String,
    pub content: String,
    pub created_at: String,
}

/// Which memories a listing takes: live ones, of `category` and `source`
/// when given, carrying every one of `tags`, and sensitive ones only when
/// asked.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListFilter {
    pub include_sensitive: bool,
    pub category: Option<String>,
    pub source: Option<String>,
    pub tags: Vec<String>,
    pub scope: crate::context::ScopeFilter,
}

/// An edit to a memory's fields: each `Some` field is written, the rest keep
/// their value, and `updated_at` is always stamped.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemoryEdit {
    pub content: Option<String>,
    pub category: Option<String>,
    pub tags: Option<Vec<String>>,
    pub metadata: Option<Value>,
    pub sensitive: Option<bool>,
    /// Clear `superseded_by`. There is no way to set it here: superseding is
    /// [`Memories::set_superseded_by`]'s job.
    pub clear_superseded: bool,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    pub memory_type: Option<String>,
    pub decay_rate: Option<f64>,
    // Schema v32. Each sets its column; none clears one to NULL, which no
    // caller needs yet.
    pub project: Option<String>,
    pub session_id: Option<String>,
    pub git_remote: Option<String>,
    pub git_branch: Option<String>,
    pub git_sha: Option<String>,
    pub cwd: Option<String>,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    pub confidence: Option<f64>,
    pub verified_at: Option<String>,
    pub outcome: Option<String>,
    pub written_by: Option<String>,
    pub capture_method: Option<String>,
    pub updated_at: String,
}

impl MemoryEdit {
    /// An edit that only stamps `updated_at`; set the fields to write.
    pub fn at(updated_at: impl Into<String>) -> Self {
        Self {
            updated_at: updated_at.into(),
            ..Self::default()
        }
    }
}

/// Which live, unsuperseded memories a ranked keyword search takes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeywordFilter {
    /// Only memories whose effective vitality, as of now, is at least this.
    pub min_effective_vitality: Option<f64>,
    pub category: Option<String>,
    pub include_sensitive: bool,
    pub scope: crate::context::ScopeFilter,
}

/// Which live, unsuperseded memories a paged search takes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageFilter {
    pub category: Option<String>,
    /// All-of.
    pub tags: Vec<String>,
    /// Only memories linked to this entity, or whose subject or object is its
    /// canonical name.
    pub entity: Option<EntityScope>,
}

/// An entity a paged search is narrowed to.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityScope {
    pub id: String,
    /// The entity's normalized name, compared with `lower(subject)` and
    /// `lower(object)`.
    pub canonical: String,
}

/// `tags` as the JSON array the `tags` column holds.
pub(crate) fn tags_json(tags: &[String]) -> String {
    serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_string())
}

/// The `memories` table, on the engine's memories core.
pub struct Memories<'c> {
    core: &'c EngineLock,
}

impl<'c> Memories<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self { core: store.core() }
    }

    /// Insert `row`, made on this node. An existing id is an error.
    pub fn insert(&self, row: &NewMemory) -> Result<()> {
        engine::memories::insert(&mut self.core.lock(), row)
    }

    /// Insert `row` unless its id is already taken. Returns whether it was
    /// inserted.
    pub fn insert_or_ignore(&self, row: &NewMemory) -> Result<bool> {
        engine::memories::insert_or_ignore(&mut self.core.lock(), row)
    }

    /// Write a record a peer sent: insert it, or overwrite the local row.
    ///
    /// An overwrite keeps the local `created_at`, `doc_id` and `chunk_index`:
    /// a peer's record does not carry the last two, and the first never
    /// changes. Never queued for sync: it came from there.
    pub fn upsert_synced(&self, row: &NewMemory) -> Result<()> {
        engine::memories::upsert_synced(&mut self.core.lock(), row)
    }

    /// The local copy of `id` as a sync merge sees it, if there is one.
    /// Unparseable `tags` read as none and unparseable `metadata` as `{}`.
    pub fn sync_view(&self, id: &str) -> Result<Option<SyncView>> {
        engine::memories::sync_view(&self.core.lock(), id)
    }

    /// Replace `id`'s tags and metadata without stamping `updated_at`: a sync
    /// merge that lost last-write-wins still keeps the union, and must not
    /// look like a newer local edit.
    pub fn set_tags_and_metadata(&self, id: &str, tags: &[String], metadata: &Value) -> Result<()> {
        engine::memories::set_tags_and_metadata(&mut self.core.lock(), id, tags, metadata)
    }

    /// Point `id` at the memory that replaces it. `updated_at`, when given,
    /// is stamped too, which is what puts the change in the sync outbox.
    pub fn set_superseded_by(
        &self,
        id: &str,
        superseded_by: &str,
        updated_at: Option<&str>,
    ) -> Result<()> {
        engine::memories::set_superseded_by(&mut self.core.lock(), id, superseded_by, updated_at)
    }

    /// Supersede every live, not yet superseded chunk of the import
    /// `old_import_id` with `new_import_id`, stamping `updated_at`. Returns
    /// how many were superseded.
    pub fn supersede_import(
        &self,
        old_import_id: &str,
        new_import_id: &str,
        updated_at: &str,
    ) -> Result<usize> {
        engine::memories::supersede_import(
            &mut self.core.lock(),
            old_import_id,
            new_import_id,
            updated_at,
        )
    }

    /// Hard-delete every memory of `category` belonging to the capture
    /// `capture_id`. Returns how many went.
    pub fn delete_capture_category(&self, capture_id: &str, category: &str) -> Result<usize> {
        engine::memories::delete_capture_category(&mut self.core.lock(), capture_id, category)
    }

    /// Rewrite `id` as the merge of its cluster: content, summed access
    /// count and tags, stamping `updated_at`.
    pub fn set_merged(
        &self,
        id: &str,
        content: &str,
        access_count: i64,
        tags: &[String],
        updated_at: &str,
    ) -> Result<()> {
        engine::memories::set_merged(
            &mut self.core.lock(),
            id,
            content,
            access_count,
            tags,
            updated_at,
        )
    }

    /// Set `id`'s vitality and status, without stamping `updated_at`: both
    /// are local scores, not edits.
    pub fn set_vitality(&self, id: &str, vitality: f64, status: &str) -> Result<()> {
        engine::memories::set_vitality(&mut self.core.lock(), id, vitality, status)
    }

    /// The access-tracking inputs of each of `ids` that exists, in no
    /// particular order.
    pub fn access_inputs(&self, ids: &[String]) -> Result<Vec<AccessInputs>> {
        engine::memories::access_inputs(&self.core.lock(), ids)
    }

    /// Record an access to `id`: when, the new count, and the vitality and
    /// status it leads to. Not an edit, so `updated_at` is left alone.
    pub fn record_access(
        &self,
        id: &str,
        accessed_at: &str,
        access_count: i64,
        vitality: f64,
        status: &str,
    ) -> Result<()> {
        engine::memories::record_access(
            &mut self.core.lock(),
            id,
            accessed_at,
            access_count,
            vitality,
            status,
        )
    }

    /// The triple of every live, unsuperseded memory other than `except_id`
    /// that has all three parts.
    pub fn live_triples_except(&self, except_id: &str) -> Result<Vec<Triple>> {
        engine::memories::live_triples_except(&self.core.lock(), except_id)
    }

    /// Live, unsuperseded memories created after `cutoff`, oldest first, at
    /// most `limit`.
    pub fn live_created_after(&self, cutoff: &str, limit: usize) -> Result<Vec<CreatedMemory>> {
        engine::memories::live_created_after(&self.core.lock(), cutoff, limit)
    }

    /// How many live, unsuperseded memories were created after `cutoff`.
    pub fn count_live_created_after(&self, cutoff: &str) -> Result<usize> {
        engine::memories::count_live_created_after(&self.core.lock(), cutoff)
    }

    /// The memories `ids` names that exist, in no particular order.
    pub fn get_many(&self, ids: &[String]) -> Result<Vec<Memory>> {
        engine::memories::get_many(&self.core.lock(), ids)
    }

    /// Whether `id` exists.
    pub fn exists(&self, id: &str) -> Result<bool> {
        engine::memories::exists(&self.core.lock(), id)
    }

    /// Whether `id` is marked sensitive, or `None` when there is no such
    /// memory.
    pub fn sensitivity(&self, id: &str) -> Result<Option<bool>> {
        engine::memories::sensitivity(&self.core.lock(), id)
    }

    /// The memories an export takes, oldest first (ties by id): of
    /// `category` when given, carrying every one of `tags`, and only the live,
    /// unsuperseded ones unless `include_deleted`.
    pub fn exportable(
        &self,
        include_deleted: bool,
        category: Option<&str>,
        tags: &[String],
    ) -> Result<Vec<Memory>> {
        engine::memories::exportable(&self.core.lock(), include_deleted, category, tags)
    }

    /// Every live memory, in no particular order.
    pub fn all_live(&self) -> Result<Vec<Memory>> {
        engine::memories::all_live(&self.core.lock())
    }

    /// Memory `id`, unless it is missing or deleted.
    pub fn get_live(&self, id: &str) -> Result<Option<Memory>> {
        engine::memories::get_live(&self.core.lock(), id)
    }

    /// Live memory `id`'s category, or `None` when it is missing or deleted.
    pub fn live_category(&self, id: &str) -> Result<Option<String>> {
        Ok(engine::memories::get_live(&self.core.lock(), id)?.map(|m| m.category))
    }

    /// The memories `filter` takes, newest first (ties by id, descending):
    /// how many there are in all, and the page at `offset` of at most
    /// `limit`.
    pub fn list_page(
        &self,
        filter: &ListFilter,
        limit: usize,
        offset: usize,
    ) -> Result<(usize, Vec<Memory>)> {
        engine::memories::list_page(&self.core.lock(), filter, limit, offset)
    }

    /// Apply `edit` to memory `id`, made on this node. A missing id is a
    /// no-op.
    pub fn apply_edit(&self, id: &str, edit: &MemoryEdit) -> Result<()> {
        engine::memories::apply_edit(&mut self.core.lock(), id, edit)
    }

    /// Drop the text of every tombstone that still holds it (ADR-0024), as
    /// storage rather than an edit: `updated_at` stays and nothing is
    /// queued, since every node empties its own copy. How many changed; 0
    /// once they all have, so it is cheap to run at every open.
    pub fn empty_tombstones(&self) -> Result<usize> {
        engine::memories::empty_tombstones(&mut self.core.lock())
    }

    /// Delete live memory `id`: tombstone it at `tombstone_at` (stamping
    /// `deleted_at` and `updated_at`, and dropping its text as ADR-0024
    /// says), or remove the row when that is `None`. Whether there was a
    /// live memory to delete.
    pub fn delete_live(&self, id: &str, tombstone_at: Option<&str>) -> Result<bool> {
        engine::memories::delete_live(&mut self.core.lock(), id, tombstone_at)
    }

    /// Live memories of `memory_type`, oldest first (ties by id): how many
    /// there are, and the first `limit` with the first 500 characters of
    /// their content.
    pub fn of_type_page(
        &self,
        memory_type: &str,
        limit: usize,
    ) -> Result<(usize, Vec<UnclassifiedMemory>)> {
        engine::memories::of_type_page(&self.core.lock(), memory_type, limit)
    }

    /// Live, unsuperseded memories matching any of `phrases` in the full-text
    /// index, best BM25 first (ties by id), at most `limit`, each with its
    /// BM25 score. No phrases match nothing.
    pub fn keyword_hits(
        &self,
        phrases: &[String],
        filter: &KeywordFilter,
        limit: usize,
    ) -> Result<Vec<(Memory, f64)>> {
        if phrases.is_empty() {
            return Ok(Vec::new());
        }
        engine::memories::keyword_hits(&self.core.lock(), phrases, filter, limit)
    }

    /// The ids of every memory marked sensitive.
    pub fn sensitive_ids(&self) -> Result<HashSet<String>> {
        engine::memories::sensitive_ids(&self.core.lock())
    }

    /// A page of the live, unsuperseded memories `filter` takes: matching
    /// any of `phrases`, best BM25 first, or with no phrases newest first
    /// (ties by id either way). How many there are, and the page.
    pub fn keyword_page(
        &self,
        phrases: &[String],
        filter: &PageFilter,
        limit: usize,
        offset: usize,
    ) -> Result<(usize, Vec<Memory>)> {
        let tables = self.core.lock();
        let linked = match &filter.entity {
            Some(scope) => engine::graph::linked_ids(&tables, &scope.id)?,
            None => HashSet::new(),
        };
        engine::memories::keyword_page(&tables, phrases, filter, &linked, limit, offset)
    }

    /// Memories awaiting extraction, newest first (ties by id, descending):
    /// how many there are, and the first `limit` with the first 500
    /// characters of their content. A memory qualifies when it is live, is
    /// **not a raw dialog or skeleton**, has no SPO triple at all, and has
    /// no entity links at all: one that has entities but no triple is
    /// already annotated, and a captured transcript's facts come out through
    /// `decompose`, so dialogs would otherwise flood this backlog.
    pub fn unannotated_page(&self, limit: usize) -> Result<(usize, Vec<UnannotatedMemory>)> {
        engine::memories::unannotated_page(&self.core.lock(), limit)
    }

    /// The id, content and metadata JSON of every live, unsuperseded,
    /// non-sensitive memory that records code references.
    pub fn with_code_refs(&self) -> Result<Vec<(String, String, String)>> {
        engine::memories::with_code_refs(&self.core.lock())
    }

    /// Set `metadata.ingest` to `marker` on every chunk of the import
    /// `doc_id`. Returns how many were stamped.
    pub fn set_ingest_marker(&self, doc_id: &str, marker: &str) -> Result<usize> {
        engine::memories::set_ingest_marker(&mut self.core.lock(), doc_id, marker)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::outbox::Outbox;
    use crate::db::sync_state::SyncState;
    use crate::db::derived::Origin;
    use crate::db::Database;

    const NOW: &str = "2026-09-26T00:00:00+00:00";

    /// Run `test` on a fresh in-memory database.
    fn on_engine(mut test: impl FnMut(&Database)) {
        test(&Database::open_in_memory().unwrap());
    }

    /// The one memory `id`, which must exist.
    fn get(memories: &Memories<'_>, id: &str) -> Memory {
        let mut found = memories.get_many(&[id.to_string()]).unwrap();
        assert_eq!(found.len(), 1, "memory {id}");
        found.remove(0)
    }

    #[test]
    fn insert_refuses_a_taken_id_and_insert_or_ignore_reports_it() {
        on_engine(|db| {
            let store = db.store();
            let memories = Memories::new(&store);
            let row = NewMemory::new("a", "first", NOW);

            assert!(memories.insert_or_ignore(&row).unwrap());
            assert!(memories.insert(&row).is_err());
            let second = NewMemory::new("a", "second", NOW);
            assert!(!memories.insert_or_ignore(&second).unwrap());
            assert_eq!(get(&memories, "a").content, "first");
        });
    }

    #[test]
    fn a_synced_overwrite_keeps_created_at_and_the_chunk_position() {
        on_engine(|db| {
            let store = db.store();
            let memories = Memories::new(&store);
            memories
                .insert(&NewMemory {
                    doc_id: Some("imp_1".to_string()),
                    chunk_index: Some(3),
                    ..NewMemory::new("a", "local", NOW)
                })
                .unwrap();

            let later = "2026-09-27T00:00:00+00:00";
            memories
                .upsert_synced(&NewMemory {
                    created_at: later.to_string(),
                    tags: vec!["peer".to_string()],
                    ..NewMemory::new("a", "remote", later)
                })
                .unwrap();

            let a = get(&memories, "a");
            assert_eq!(a.content, "remote");
            assert_eq!(a.created_at, NOW);
            assert_eq!(a.doc_id.as_deref(), Some("imp_1"));
            assert_eq!(a.chunk_index, Some(3));
            assert_eq!(a.tags, ["peer"]);
        });
    }

    #[test]
    fn sync_view_reads_tags_that_are_not_an_array_as_none() {
        on_engine(|db| {
            let store = db.store();
            Memories::new(&store)
                .insert(&NewMemory::new("a", "x", NOW))
                .unwrap();
            crate::testing::set_memory_column(&store, "a", "tags", r#""not an array""#).unwrap();
            let view = Memories::new(&store).sync_view("a").unwrap().unwrap();
            assert!(view.tags.is_empty());
            assert_eq!(view.metadata, serde_json::json!({}));
            assert!(Memories::new(&store)
                .sync_view("missing")
                .unwrap()
                .is_none());
        });
    }

    #[test]
    fn access_inputs_skips_unknown_ids_and_takes_an_empty_list() {
        on_engine(|db| {
            let store = db.store();
            let memories = Memories::new(&store);
            memories.insert(&NewMemory::new("a", "x", NOW)).unwrap();

            assert!(memories.access_inputs(&[]).unwrap().is_empty());
            let found = memories
                .access_inputs(&["a".to_string(), "missing".to_string(), "a".to_string()])
                .unwrap();
            assert_eq!(
                found,
                vec![AccessInputs {
                    id: "a".to_string(),
                    access_count: 0,
                    decay_rate: 0.1,
                    base_weight: 1.0,
                }]
            );
        });
    }

    /// Everything a backend answers after [`exercise`]'s writes, in a form
    /// two backends can be compared in.
    #[derive(Debug, PartialEq)]
    struct Observed {
        counts: Vec<usize>,
        exported: Vec<Value>,
        live: Vec<Value>,
        red_live: Vec<String>,
        code_refs: Vec<(String, String, String)>,
        triples: Vec<Triple>,
        created: Vec<CreatedMemory>,
        created_count: usize,
        sensitivity: Vec<Option<bool>>,
        views: Vec<Option<SyncView>>,
        outbox: Vec<(String, Value)>,
    }

    fn as_values(mut memories: Vec<Memory>) -> Vec<Value> {
        memories.sort_by(|a, b| a.id.cmp(&b.id));
        memories
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect()
    }

    /// Every write the repository makes, with sync on, then every read.
    fn exercise(db: &Database) -> Observed {
        const T2: &str = "2026-09-27T00:00:00+00:00";
        const T3: &str = "2026-09-28T00:00:00+00:00";
        let store = db.store();
        SyncState::new(&store)
            .set_flag("sync_enabled", "1")
            .unwrap();
        let m = Memories::new(&store);
        let text = |s: &str| Some(s.to_string());
        let mut counts = Vec::new();

        m.insert(&NewMemory {
            category: "fact".into(),
            tags: vec!["red".into(), "blue".into()],
            capture_id: text("cap"),
            subject: text("sky"),
            predicate: text("is"),
            object: text("blue"),
            ..NewMemory::new("a", "alpha quokka", NOW)
        })
        .unwrap();
        counts.push(usize::from(
            m.insert_or_ignore(&NewMemory {
                tags: vec!["red".into()],
                metadata: serde_json::json!({"code_refs": ["src/x.rs"]}),
                ..NewMemory::new("b", "beta", T2)
            })
            .unwrap(),
        ));
        counts.push(usize::from(
            m.insert_or_ignore(&NewMemory::new("b", "ignored", T2))
                .unwrap(),
        ));
        m.upsert_synced(&NewMemory {
            sensitive: true,
            ..NewMemory::new("c", "gamma", T2)
        })
        .unwrap();
        for (id, chunk) in [("d1", 0), ("d2", 1)] {
            m.insert(&NewMemory {
                metadata: serde_json::json!({"import_id": "imp1"}),
                doc_id: text("doc1"),
                chunk_index: Some(chunk),
                capture_id: text("cap"),
                category: "chunk".into(),
                ..NewMemory::new(id, "chunk text", T2)
            })
            .unwrap();
        }
        m.set_tags_and_metadata("c", &["green".into()], &serde_json::json!({"k": "v"}))
            .unwrap();
        m.set_superseded_by("b", "c", None).unwrap();
        m.set_merged("c", "gamma merged", 5, &["green".into(), "red".into()], T3)
            .unwrap();
        m.set_vitality("a", 0.25, "fading").unwrap();
        m.record_access("a", T3, 3, 0.75, "active").unwrap();
        counts.push(m.set_ingest_marker("doc1", "done").unwrap());
        counts.push(m.supersede_import("imp1", "imp2", T3).unwrap());
        m.upsert_synced(&NewMemory {
            created_at: T3.into(),
            ..NewMemory::new("a", "alpha remote", T3)
        })
        .unwrap();
        m.insert(&NewMemory {
            capture_id: text("cap"),
            category: "chunk".into(),
            ..NewMemory::new("e", "doomed", T3)
        })
        .unwrap();
        counts.push(m.delete_capture_category("cap", "chunk").unwrap());
        m.insert(&with_every_v32_column(NewMemory::new("ctx", "in context", T3)))
            .unwrap();
        m.upsert_synced(&NewMemory {
            created_at: T3.into(),
            ..with_every_v32_column(NewMemory::new("b", "beta remote", T3))
        })
        .unwrap();

        let ids: Vec<String> = ["a", "b", "c", "d1", "e", "missing"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut triples = m.live_triples_except("none").unwrap();
        triples.sort_by(|a, b| a.id.cmp(&b.id));
        let mut code_refs = m.with_code_refs().unwrap();
        code_refs.sort();
        let outbox = Outbox::new(&store)
            .unsent_to("hub", 0, 100)
            .unwrap()
            .into_iter()
            .map(|e| (e.key, serde_json::from_str(&e.payload_json).unwrap()))
            .collect();
        Observed {
            counts,
            exported: m
                .exportable(true, None, &[])
                .unwrap()
                .iter()
                .map(|m| serde_json::to_value(m).unwrap())
                .collect(),
            live: as_values(m.all_live().unwrap()),
            red_live: m
                .exportable(false, None, &["red".into()])
                .unwrap()
                .into_iter()
                .map(|m| m.id)
                .collect(),
            code_refs,
            triples,
            created: m.live_created_after(NOW, 10).unwrap(),
            created_count: m.count_live_created_after(NOW).unwrap(),
            sensitivity: ids.iter().map(|id| m.sensitivity(id).unwrap()).collect(),
            views: ids.iter().map(|id| m.sync_view(id).unwrap()).collect(),
            outbox,
        }
    }

    /// `row` with every schema v32 column set to a value no default has.
    fn with_every_v32_column(row: NewMemory) -> NewMemory {
        let text = |s: &str| Some(s.to_string());
        NewMemory {
            project: text("quokka"),
            session_id: text("sess-1"),
            git_remote: text("github.com/o/r"),
            git_branch: text("feature/x"),
            git_sha: text("0123abcd"),
            cwd: text("/work/quokka"),
            valid_from: text("2026-09-01T00:00:00+00:00"),
            valid_until: text("2026-12-01T00:00:00+00:00"),
            confidence: 0.75,
            verified_at: text("2026-09-27T00:00:00+00:00"),
            outcome: text("done"),
            written_by: "model:test".into(),
            capture_method: "auto".into(),
            ..row
        }
    }

    /// Every schema v32 column of `memory`, as JSON, in a fixed order.
    fn v32_columns(memory: &Memory) -> Value {
        serde_json::json!([
            memory.project,
            memory.session_id,
            memory.git_remote,
            memory.git_branch,
            memory.git_sha,
            memory.cwd,
            memory.valid_from,
            memory.valid_until,
            memory.confidence,
            memory.verified_at,
            memory.outcome,
            memory.written_by,
            memory.capture_method,
        ])
    }

    /// A row with every v32 column set reads back with every one as
    /// written, and one with none set reads back the schema's defaults.
    #[test]
    fn every_v32_column_reads_back_as_written_on_both_stores() {
        on_engine(|db| {
            let store = db.store();
            let m = Memories::new(&store);
            let written = with_every_v32_column(NewMemory::new("ctx", "x", NOW));
            m.insert(&written).unwrap();
            m.insert(&NewMemory::new("plain", "y", NOW)).unwrap();

            let read = get(&m, "ctx");
            assert_eq!(
                v32_columns(&read),
                serde_json::json!([
                    "quokka",
                    "sess-1",
                    "github.com/o/r",
                    "feature/x",
                    "0123abcd",
                    "/work/quokka",
                    "2026-09-01T00:00:00+00:00",
                    "2026-12-01T00:00:00+00:00",
                    0.75,
                    "2026-09-27T00:00:00+00:00",
                    "done",
                    "model:test",
                    "auto",
                ])
            );
            let plain = get(&m, "plain");
            assert_eq!(
                v32_columns(&plain),
                serde_json::json!([
                    null, null, null, null, null, null, null, null, 1.0, null, null, "unknown",
                    "manual"
                ])
            );

            // The edit's setters write each column; the rest stay.
            m.apply_edit(
                "plain",
                &MemoryEdit {
                    project: Some("edited".into()),
                    confidence: Some(0.5),
                    outcome: Some("abandoned".into()),
                    written_by: Some("human".into()),
                    ..MemoryEdit::at(NOW)
                },
            )
            .unwrap();
            let edited = get(&m, "plain");
            assert_eq!(edited.project.as_deref(), Some("edited"));
            assert_eq!(edited.confidence, 0.5);
            assert_eq!(edited.outcome.as_deref(), Some("abandoned"));
            assert_eq!(edited.written_by, "human");
            assert_eq!(edited.capture_method, "manual");
            assert_eq!(edited.session_id, None);
        });
    }

    /// Lists, edits and deletes on `db`, and what each read returns after.
    fn exercise_reads(db: &Database) -> Vec<Value> {
        const T2: &str = "2026-09-27T00:00:00+00:00";
        let store = db.store();
        let m = Memories::new(&store);
        let text = |s: &str| Some(s.to_string());
        for (id, at, category, tags, sensitive) in [
            ("a", NOW, "fact", vec!["red"], false),
            ("b", T2, "fact", vec!["red", "blue"], false),
            ("c", T2, "note", vec![], true),
            ("d", NOW, "note", vec!["blue"], false),
            ("e", T2, "fact", vec![], false),
        ] {
            m.insert(&NewMemory {
                category: category.into(),
                tags: tags.into_iter().map(String::from).collect(),
                sensitive,
                source: if id == "d" {
                    "import".into()
                } else {
                    "manual".into()
                },
                ..NewMemory::new(id, format!("content of {id} {}", "é".repeat(600)), at)
            })
            .unwrap();
        }
        let mut seen = Vec::new();
        let page = |filter: ListFilter, limit, offset| {
            let (total, memories) = m.list_page(&filter, limit, offset).unwrap();
            let ids: Vec<String> = memories.into_iter().map(|m| m.id).collect();
            serde_json::json!([total, ids])
        };
        seen.push(page(ListFilter::default(), 10, 0));
        seen.push(page(ListFilter::default(), 2, 1));
        seen.push(page(
            ListFilter {
                include_sensitive: true,
                ..ListFilter::default()
            },
            10,
            0,
        ));
        seen.push(page(
            ListFilter {
                category: text("fact"),
                tags: vec!["red".into()],
                ..ListFilter::default()
            },
            10,
            0,
        ));
        seen.push(page(
            ListFilter {
                source: text("import"),
                ..ListFilter::default()
            },
            10,
            0,
        ));

        m.apply_edit(
            "a",
            &MemoryEdit {
                content: text("edited"),
                tags: Some(vec!["green".into()]),
                metadata: Some(serde_json::json!({"k": 1})),
                sensitive: Some(true),
                subject: text("s"),
                memory_type: text("decision"),
                decay_rate: Some(0.02),
                project: text("p"),
                session_id: text("s1"),
                git_remote: text("r"),
                git_branch: text("b"),
                git_sha: text("sha"),
                cwd: text("/w"),
                valid_from: text(T2),
                valid_until: text(T2),
                confidence: Some(0.25),
                verified_at: text(T2),
                outcome: text("done"),
                written_by: text("hook"),
                capture_method: text("auto"),
                ..MemoryEdit::at(T2)
            },
        )
        .unwrap();
        m.set_superseded_by("b", "a", None).unwrap();
        m.apply_edit(
            "b",
            &MemoryEdit {
                clear_superseded: true,
                ..MemoryEdit::at(T2)
            },
        )
        .unwrap();
        m.apply_edit("missing", &MemoryEdit::at(T2)).unwrap();
        seen.push(serde_json::json!([
            m.delete_live("c", Some(T2)).unwrap(),
            m.delete_live("c", Some(T2)).unwrap(),
            m.delete_live("d", None).unwrap(),
            m.delete_live("missing", None).unwrap(),
        ]));
        for id in ["a", "b", "c", "d"] {
            seen.push(serde_json::to_value(m.get_live(id).unwrap()).unwrap());
            seen.push(serde_json::to_value(m.live_category(id).unwrap()).unwrap());
        }
        let (total, page) = m.of_type_page("unclassified", 2).unwrap();
        seen.push(serde_json::json!([
            total,
            serde_json::to_value(page).unwrap()
        ]));
        seen.push(serde_json::to_value(as_values(m.exportable(true, None, &[]).unwrap())).unwrap());
        seen
    }

    #[test]
    fn lists_edits_and_deletes_read_back_as_the_rules_say() {
        let mut observed = Vec::new();
        on_engine(|db| observed.push(exercise_reads(db)));
        let seen = &observed[0];
        assert_eq!(seen[0], serde_json::json!([4, ["e", "b", "d", "a"]]));
        assert_eq!(seen[1], serde_json::json!([4, ["b", "d"]]), "a page at an offset");
        assert_eq!(
            seen[2],
            serde_json::json!([5, ["e", "c", "b", "d", "a"]]),
            "sensitive rows show only when asked"
        );
        assert_eq!(seen[3], serde_json::json!([2, ["b", "a"]]), "category and all-of tags");
        assert_eq!(seen[4], serde_json::json!([1, ["d"]]), "by source");
        assert_eq!(
            seen[5],
            serde_json::json!([true, false, true, false]),
            "a delete reports whether there was a live memory"
        );
        let a = &seen[6];
        assert_eq!(a["content"], "edited");
        assert_eq!(a["tags"], serde_json::json!(["green"]));
        assert_eq!(a["sensitive"], true);
        assert_eq!(a["memory_type"], "decision");
        assert_eq!(a["decay_rate"], 0.02);
        assert_eq!(a["project"], "p");
        assert_eq!(a["confidence"], 0.25);
        assert_eq!(a["written_by"], "hook");
        assert_eq!(seen[7], "fact");
        assert_eq!(seen[8]["superseded_by"], serde_json::Value::Null, "cleared");
        assert_eq!(seen[10], serde_json::Value::Null, "c is deleted");
        assert_eq!(seen[11], serde_json::Value::Null);
        assert_eq!(seen[12], serde_json::Value::Null, "d was removed outright");
        let (total, page) = (&seen[14][0], &seen[14][1]);
        assert_eq!(total, 2, "b and e are live and unclassified; a became a decision");
        assert_eq!(page.as_array().map(Vec::len), Some(2));
        assert_eq!(
            page[0]["content_snippet"].as_str().map(|s| s.chars().count()),
            Some(500),
            "snippets are cut by character"
        );
        let exported = seen[15].as_array().unwrap();
        let ids: Vec<&str> = exported.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["a", "b", "c", "e"], "an export with deleted rows keeps the tombstone");
    }

    /// A corpus searched through `db`'s full-text index: every ranked and
    /// paged answer, scores to ten significant digits so float noise cannot
    /// fail the comparison but a real difference in ranking does.
    /// Every search the node makes, on a fixed corpus, as comparable values.
    /// With `tombstones`, one row is superseded and one deleted, and hits
    /// are compared as sets of ids: FTS5's BM25 counts those rows, the
    /// engine's does not, so only the matches must agree.
    fn exercise_search(db: &Database, tombstones: bool) -> Vec<Value> {
        const T2: &str = "2026-09-27T00:00:00+00:00";
        let store = db.store();
        let m = Memories::new(&store);
        let text = |s: &str| Some(s.to_string());
        let corpus = [
            (
                "a",
                "the quokka eats leaves at night",
                "fact",
                vec!["animal"],
                NOW,
            ),
            (
                "b",
                "a quokka and a wombat share a burrow",
                "fact",
                vec!["animal", "burrow"],
                T2,
            ),
            (
                "c",
                "wombat burrows are deep; quokka burrows are not",
                "note",
                vec![],
                T2,
            ),
            (
                "d",
                "rust ownership rules and the borrow checker",
                "note",
                vec!["rust"],
                NOW,
            ),
            ("e", "quokka quokka quokka everywhere", "fact", vec![], NOW),
            ("f", "a sensitive quokka secret", "fact", vec![], NOW),
            ("g", "a dormant quokka nobody reads", "fact", vec![], NOW),
            ("h", "a superseded quokka note", "fact", vec![], NOW),
            ("i", "a deleted quokka note", "fact", vec![], NOW),
            (
                "j",
                "the borrow checker loves wombats",
                "note",
                vec!["quokka"],
                T2,
            ),
        ];
        for (id, content, category, tags, at) in corpus {
            m.insert(&NewMemory {
                category: category.into(),
                tags: tags.into_iter().map(String::from).collect(),
                sensitive: id == "f",
                base_weight: if id == "g" { 0.001 } else { 1.0 },
                subject: if id == "d" { text("Rust") } else { None },
                ..NewMemory::new(id, content, at)
            })
            .unwrap();
        }
        if tombstones {
            m.set_superseded_by("h", "a", None).unwrap();
            m.delete_live("i", Some(T2)).unwrap();
        }
        crate::db::entities::Entities::new(&store)
            .insert(
                &crate::entity::Entity {
                    id: "ent_w".into(),
                    name: "Wombat".into(),
                    kind: None,
                    aliases: Vec::new(),
                    created_at: NOW.into(),
                    updated_at: NOW.into(),
                },
                None,
            )
            .unwrap();
        crate::db::entities::Entities::new(&store)
            .link("c", "ent_w", NOW, Origin::Local)
            .unwrap();

        let phrases = crate::fts::query_phrases;
        let mut seen = Vec::new();
        let queries = [
            "quokka",
            "quokka burrow",
            "\"borrow checker\"",
            "what's a wombat?",
            "animal",
            "note",
            "nothing matches this",
        ];
        let filters = [
            KeywordFilter::default(),
            KeywordFilter {
                include_sensitive: true,
                ..KeywordFilter::default()
            },
            KeywordFilter {
                min_effective_vitality: Some(crate::vitality::VITALITY_FLOOR),
                category: text("fact"),
                include_sensitive: true,
            },
        ];
        for query in queries {
            for filter in &filters {
                for limit in [2, 20] {
                    let hits = m.keyword_hits(&phrases(query), filter, limit).unwrap();
                    if !tombstones {
                        let hits: Vec<Value> = hits
                            .into_iter()
                            .map(|(memory, score)| {
                                serde_json::json!([memory.id, format!("{score:.9e}")])
                            })
                            .collect();
                        seen.push(serde_json::json!([query, limit, hits]));
                    } else if limit == 20 {
                        let mut ids: Vec<String> =
                            hits.into_iter().map(|(memory, _)| memory.id).collect();
                        ids.sort();
                        seen.push(serde_json::json!([query, ids]));
                    }
                }
            }
        }
        let pages = [
            ("quokka", PageFilter::default(), 3, 0),
            ("quokka", PageFilter::default(), 3, 3),
            (
                "quokka",
                PageFilter {
                    category: text("fact"),
                    tags: vec!["animal".into()],
                    entity: None,
                },
                10,
                0,
            ),
            ("", PageFilter::default(), 4, 1),
            (
                "",
                PageFilter {
                    entity: Some(EntityScope {
                        id: "ent_w".into(),
                        canonical: "rust".into(),
                    }),
                    ..PageFilter::default()
                },
                10,
                0,
            ),
            (
                "burrows",
                PageFilter {
                    entity: Some(EntityScope {
                        id: "ent_w".into(),
                        canonical: "wombat".into(),
                    }),
                    ..PageFilter::default()
                },
                10,
                0,
            ),
        ];
        for (query, filter, limit, offset) in pages {
            let (total, page) = m
                .keyword_page(&phrases(query), &filter, limit, offset)
                .unwrap();
            let ids: Vec<String> = page.into_iter().map(|m| m.id).collect();
            if tombstones {
                seen.push(serde_json::json!([query, total]));
            } else {
                seen.push(serde_json::json!([query, total, ids]));
            }
        }
        let mut sensitive: Vec<String> = m.sensitive_ids().unwrap().into_iter().collect();
        sensitive.sort();
        seen.push(serde_json::json!(sensitive));
        seen
    }

    #[test]
    fn keyword_search_ranks_filters_and_pages_the_corpus() {
        let mut observed = Vec::new();
        on_engine(|db| observed.push(exercise_search(db, false)));
        let seen = &observed[0];
        // [query, limit, [[id, score], ...]] per (query, filter, limit).
        let hits = |i: usize| -> Vec<String> {
            seen[i][2]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h[0].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(seen[0][0], "quokka");
        assert_eq!(hits(0).len(), 2, "the limit caps the ranking: {:?}", seen[0]);
        assert_eq!(hits(0)[0], "e", "the memory that says it most ranks first");
        let all_quokka = hits(1);
        assert!(!all_quokka.contains(&"f".to_string()), "sensitive rows are out by default");
        assert!(all_quokka.contains(&"j".to_string()), "a tag is indexed too");
        assert!(all_quokka.contains(&"g".to_string()), "no vitality floor by default");
        assert!(hits(3).contains(&"f".to_string()), "include_sensitive lets f through");
        let faded = hits(5);
        assert!(faded.contains(&"f".to_string()) && !faded.contains(&"g".to_string()));
        assert!(!faded.contains(&"c".to_string()), "the category filter holds");
        let scores: Vec<f64> = seen[1][2]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h[1].as_str().unwrap().parse().unwrap())
            .collect();
        assert!(scores.windows(2).all(|w| w[0] <= w[1]), "best (lowest) BM25 first: {scores:?}");
        let last_query = seen.iter().rposition(|v| v[0] == "nothing matches this").unwrap();
        assert!(hits(last_query).is_empty());
        // The pages come after the ranked hits.
        let pages: Vec<&Value> = seen.iter().filter(|v| v.as_array().is_some_and(|a| a.len() == 3 && a[1].is_u64() && a[2].is_array() && a[2][0].is_string())).collect();
        assert_eq!(pages.len(), 6, "{pages:?}");
        assert_eq!(pages[0][1], pages[1][1], "two pages of one query share a total");
        assert_eq!(pages[0][2].as_array().map(Vec::len), Some(3));
        assert_eq!(pages[3][2], serde_json::json!(["c", "b", "i", "h"]), "no phrases: newest first, by id");
        assert_eq!(pages[4][2], serde_json::json!(["c", "d"]), "linked to the entity, or naming it");
        assert_eq!(pages[5][2], serde_json::json!(["c"]));
        let sensitive = seen.last().unwrap();
        assert_eq!(sensitive, &serde_json::json!(["f"]));
    }

    /// Superseded and deleted rows are out of the index: no search finds
    /// them, and the totals do not count them.
    #[test]
    fn with_tombstones_search_finds_only_live_rows() {
        let mut observed = Vec::new();
        on_engine(|db| observed.push(exercise_search(db, true)));
        let seen = &observed[0];
        let mut any_several = false;
        for entry in seen {
            let Some(ids) = entry.get(1).and_then(Value::as_array) else {
                continue;
            };
            any_several |= ids.len() > 2;
            for gone in ["h", "i"] {
                assert!(!ids.contains(&Value::from(gone)), "{entry:?}");
            }
        }
        assert!(any_several, "the corpus gives some query several hits: {seen:?}");
        let quokka_page = seen.iter().find(|v| v[0] == "quokka" && v[1].is_u64()).unwrap();
        assert_eq!(quokka_page[1], 7, "a, b, c, e, f, g and j; not the superseded h or deleted i");
    }

    #[test]
    fn every_write_lands_as_the_rules_say() {
        let mut observed = Vec::new();
        on_engine(|db| observed.push(exercise(db)));
        let seen = &observed[0];
        assert_eq!(seen.counts, [1, 0, 2, 2, 3]);
        let live: Vec<&str> = seen.live.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(live, ["a", "b", "c", "ctx"], "the capture's chunks were removed outright");
        assert_eq!(seen.red_live, ["c"], "red-tagged, live and unsuperseded: only the merged c");
        assert_eq!(seen.code_refs.len(), 0, "b's refs went with its synced overwrite");
        assert_eq!(seen.triples.len(), 0, "a's triple went with its synced overwrite");
        let created: Vec<&str> = seen.created.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(created, ["b", "c", "ctx"], "a kept its first created_at");
        assert_eq!(seen.created_count, 3);
        assert_eq!(
            seen.sensitivity,
            [Some(false), Some(false), Some(true), None, None, None]
        );
        assert_eq!(seen.views[5], None);
        assert_eq!(
            seen.views[2].as_ref().map(|v| v.tags.clone()),
            Some(vec!["green".to_string(), "red".to_string()])
        );
        let queued_keys: Vec<&str> = seen.outbox.iter().map(|(k, _)| k.as_str()).collect();
        assert!(
            queued_keys.contains(&"a") && queued_keys.contains(&"ctx"),
            "local writes queue: {queued_keys:?}"
        );
        for (key, payload) in &seen.outbox {
            assert_eq!(&payload["id"], key, "a payload names its row");
        }
        let exported: Vec<&str> = seen.exported.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(exported, ["a", "b", "c", "ctx"], "oldest first, ties by id");
    }
}
