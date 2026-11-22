//! Storage for the knowledge graph: `entities`, `entity_relations` and the
//! `memory_entities` mention links, behind the [`EntityStore`] port
//! (ADR-0035), which the engine's memories core implements in
//! `db::engine::graph`.
//!
//! Every read and write [`crate::entity`] makes goes through here. The
//! rules stay with it: how a name normalises into an id, how aliases merge,
//! which kind wins and how a traversal walks. A write queues its outbox
//! entry as it lands, in a store that syncs (see [`Origin`]).

use super::backend::BackendLock;
use super::derived::Origin;
use super::{Result, Store};
use crate::entity::{Entity, EntityFact, EntityLinkedMemory, EntityListItem, RelationEdge};
use std::collections::HashSet;

/// What a sync merge needs of the local copy of an entity.
#[derive(Debug, Clone, PartialEq)]
pub struct EntitySyncView {
    pub aliases: Vec<String>,
    pub updated_at: String,
}

/// A whole `entity_relations` row.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationRow<'a> {
    pub id: &'a str,
    pub subject_entity_id: &'a str,
    pub relation: &'a str,
    pub object_entity_id: &'a str,
    pub created_at: &'a str,
    pub updated_at: &'a str,
    pub node_id: Option<&'a str>,
}

/// A relation as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredRelation {
    pub id: String,
    pub subject_entity_id: String,
    pub relation: String,
    pub object_entity_id: String,
    pub created_at: String,
    pub updated_at: String,
}

/// The port for the knowledge-graph tables (ADR-0035). A write made with
/// [`Origin::Local`] queues its outbox entry, in a store that syncs.
/// [`conformance`] holds the behaviour every implementation must show.
pub trait EntityStore {
    /// The entity with id `id`, if there is one.
    fn get(&self, id: &str) -> Result<Option<Entity>>;

    /// Whether an entity with id `id` exists.
    fn exists(&self, id: &str) -> Result<bool>;

    /// Every entity, oldest first (ties by id).
    fn all_oldest_first(&self) -> Result<Vec<Entity>>;

    /// Every entity, by id.
    fn all(&self) -> Result<Vec<Entity>>;

    /// Insert `entity`, made on this node, created and updated at its own
    /// stamps, recording `node_id` as where it was made. An existing id is an
    /// error.
    fn insert(&mut self, entity: &Entity, node_id: Option<&str>) -> Result<()>;

    /// Set `id`'s kind and aliases, stamping `updated_at`.
    fn set_kind_and_aliases(
        &mut self,
        id: &str,
        kind: Option<&str>,
        aliases: &[String],
        updated_at: &str,
    ) -> Result<()>;

    /// How many entities there are.
    fn count(&self) -> Result<usize>;

    /// A page of entities with their mention counts, most-mentioned first,
    /// then by name, then id.
    fn page_by_mentions(&self, limit: usize, offset: usize) -> Result<Vec<EntityListItem>>;

    /// Rename the entity `from` to `to`, where `to` is free. Links and
    /// relations are repointed separately. Queued for sync as an update of
    /// `to`.
    fn rename(&mut self, from: &str, to: &str) -> Result<()>;

    /// Set `id`'s kind, aliases and `created_at`, without stamping
    /// `updated_at`: the merge of two rows that were always one entity.
    fn set_merged(
        &mut self,
        id: &str,
        kind: Option<&str>,
        aliases: &[String],
        created_at: &str,
    ) -> Result<()>;

    /// Delete the entity `id`. Its links and relations are not touched.
    fn delete(&mut self, id: &str) -> Result<()>;

    /// Repoint every mention link and relation end from the entity `from` to
    /// `to`. A link `to` already has is dropped rather than duplicated.
    fn repoint(&mut self, from: &str, to: &str) -> Result<()>;

    /// Every mention link as `(memory_id, entity_id, created_at)`, oldest
    /// first.
    fn links_oldest_first(&self) -> Result<Vec<(String, String, String)>>;

    /// Record that `memory_id` mentions `entity_id`. Returns whether the
    /// link is new: links are immutable, so an existing one is left as is.
    fn link(
        &mut self,
        memory_id: &str,
        entity_id: &str,
        created_at: &str,
        origin: Origin,
    ) -> Result<bool>;

    /// Remove the link from `memory_id` to `entity_id`. Whether it existed.
    fn unlink(&mut self, memory_id: &str, entity_id: &str) -> Result<bool>;

    /// Remove every mention link from `memory_id`: part of deleting a
    /// memory.
    fn unlink_memory(&mut self, memory_id: &str) -> Result<()>;

    /// Live memories linked to `entity_id`, newest first (ties by id,
    /// descending), at most `limit`, with the first 300 characters of their
    /// content. A link to a memory not stored here is skipped.
    fn linked_memories(&self, entity_id: &str, limit: usize) -> Result<Vec<EntityLinkedMemory>>;

    /// Every live memory linked to `entity_id`, whole rows, newest first
    /// (ties by id, descending). What a rule needs: a snippet cannot carry
    /// `git_remote`, `cwd`, tags or kind.
    fn linked_memories_full(&self, entity_id: &str) -> Result<Vec<crate::models::Memory>>;

    /// How many live memories are linked to `entity_id`.
    fn linked_memory_count(&self, entity_id: &str) -> Result<usize>;

    /// How many live memories are linked to both `a` and `b`.
    fn shared_memory_count(&self, a: &str, b: &str) -> Result<usize>;

    /// Live memories whose subject or object, lowercased, is `canonical`,
    /// newest first (ties by id, descending), at most `limit`.
    fn facts_naming(&self, canonical: &str, limit: usize) -> Result<Vec<EntityFact>>;

    /// Every relation, oldest first (ties by id).
    fn relations_oldest_first(&self) -> Result<Vec<StoredRelation>>;

    /// Insert `row` unless its id is already taken. Returns whether it was
    /// inserted: relations are immutable.
    fn insert_relation_or_ignore(&mut self, row: &RelationRow<'_>, origin: Origin) -> Result<bool>;

    /// Every relation with either end in `entity_ids`, and `relation` as its
    /// label when one is given, oldest first (ties by id). Each comes with
    /// its id, and a relation whose ends are not both stored here is
    /// skipped. No ids touch nothing. `hop` is copied onto each edge.
    fn relations_touching(
        &self,
        entity_ids: &[String],
        relation: Option<&str>,
        hop: u32,
    ) -> Result<Vec<(String, RelationEdge)>>;

    /// The local copy of `id` as a sync merge sees it, if there is one.
    fn sync_view(&self, id: &str) -> Result<Option<EntitySyncView>>;

    /// Write an entity a peer sent: insert it, or overwrite the local row's
    /// name, kind, aliases, `updated_at` and `node_id`. The local
    /// `created_at` is kept. Never queued for sync: it came from there.
    fn upsert_synced(&mut self, entity: &Entity, node_id: Option<&str>) -> Result<()>;

    /// Replace `id`'s aliases without stamping `updated_at`: a sync merge
    /// that lost last-write-wins still keeps the union. Never queued for
    /// sync.
    fn set_aliases(&mut self, id: &str, aliases: &[String]) -> Result<()>;

    /// The ids of the memories linked to `entity_id`, live or not.
    fn linked_ids(&self, entity_id: &str) -> Result<HashSet<String>>;
}

/// The knowledge-graph tables, on the engine's memories core.
pub struct Entities<'c> {
    backend: &'c BackendLock,
}

impl<'c> Entities<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self {
            backend: store.backend(),
        }
    }

    /// See [`EntityStore::get`].
    pub fn get(&self, id: &str) -> Result<Option<Entity>> {
        EntityStore::get(&*self.backend.lock()?, id)
    }

    /// See [`EntityStore::exists`].
    pub fn exists(&self, id: &str) -> Result<bool> {
        EntityStore::exists(&*self.backend.lock()?, id)
    }

    /// See [`EntityStore::all_oldest_first`].
    pub fn all_oldest_first(&self) -> Result<Vec<Entity>> {
        EntityStore::all_oldest_first(&*self.backend.lock()?)
    }

    /// See [`EntityStore::all`].
    pub fn all(&self) -> Result<Vec<Entity>> {
        EntityStore::all(&*self.backend.lock()?)
    }

    /// See [`EntityStore::insert`].
    pub fn insert(&self, entity: &Entity, node_id: Option<&str>) -> Result<()> {
        EntityStore::insert(&mut *self.backend.lock()?, entity, node_id)
    }

    /// See [`EntityStore::set_kind_and_aliases`].
    pub fn set_kind_and_aliases(
        &self,
        id: &str,
        kind: Option<&str>,
        aliases: &[String],
        updated_at: &str,
    ) -> Result<()> {
        EntityStore::set_kind_and_aliases(&mut *self.backend.lock()?, id, kind, aliases, updated_at)
    }

    /// See [`EntityStore::count`].
    pub fn count(&self) -> Result<usize> {
        EntityStore::count(&*self.backend.lock()?)
    }

    /// See [`EntityStore::page_by_mentions`].
    pub fn page_by_mentions(&self, limit: usize, offset: usize) -> Result<Vec<EntityListItem>> {
        EntityStore::page_by_mentions(&*self.backend.lock()?, limit, offset)
    }

    /// See [`EntityStore::rename`].
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        EntityStore::rename(&mut *self.backend.lock()?, from, to)
    }

    /// See [`EntityStore::set_merged`].
    pub fn set_merged(
        &self,
        id: &str,
        kind: Option<&str>,
        aliases: &[String],
        created_at: &str,
    ) -> Result<()> {
        EntityStore::set_merged(&mut *self.backend.lock()?, id, kind, aliases, created_at)
    }

    /// See [`EntityStore::delete`].
    pub fn delete(&self, id: &str) -> Result<()> {
        EntityStore::delete(&mut *self.backend.lock()?, id)
    }

    /// See [`EntityStore::repoint`].
    pub fn repoint(&self, from: &str, to: &str) -> Result<()> {
        EntityStore::repoint(&mut *self.backend.lock()?, from, to)
    }

    /// See [`EntityStore::links_oldest_first`].
    pub fn links_oldest_first(&self) -> Result<Vec<(String, String, String)>> {
        EntityStore::links_oldest_first(&*self.backend.lock()?)
    }

    /// See [`EntityStore::link`].
    pub fn link(
        &self,
        memory_id: &str,
        entity_id: &str,
        created_at: &str,
        origin: Origin,
    ) -> Result<bool> {
        EntityStore::link(
            &mut *self.backend.lock()?,
            memory_id,
            entity_id,
            created_at,
            origin,
        )
    }

    /// See [`EntityStore::unlink`].
    pub fn unlink(&self, memory_id: &str, entity_id: &str) -> Result<bool> {
        EntityStore::unlink(&mut *self.backend.lock()?, memory_id, entity_id)
    }

    /// See [`EntityStore::unlink_memory`].
    pub fn unlink_memory(&self, memory_id: &str) -> Result<()> {
        EntityStore::unlink_memory(&mut *self.backend.lock()?, memory_id)
    }

    /// See [`EntityStore::linked_memories`].
    pub fn linked_memories(
        &self,
        entity_id: &str,
        limit: usize,
    ) -> Result<Vec<EntityLinkedMemory>> {
        EntityStore::linked_memories(&*self.backend.lock()?, entity_id, limit)
    }

    /// See [`EntityStore::linked_memories_full`].
    pub fn linked_memories_full(&self, entity_id: &str) -> Result<Vec<crate::models::Memory>> {
        EntityStore::linked_memories_full(&*self.backend.lock()?, entity_id)
    }

    /// See [`EntityStore::linked_memory_count`].
    pub fn linked_memory_count(&self, entity_id: &str) -> Result<usize> {
        EntityStore::linked_memory_count(&*self.backend.lock()?, entity_id)
    }

    /// See [`EntityStore::shared_memory_count`].
    pub fn shared_memory_count(&self, a: &str, b: &str) -> Result<usize> {
        EntityStore::shared_memory_count(&*self.backend.lock()?, a, b)
    }

    /// See [`EntityStore::facts_naming`].
    pub fn facts_naming(&self, canonical: &str, limit: usize) -> Result<Vec<EntityFact>> {
        EntityStore::facts_naming(&*self.backend.lock()?, canonical, limit)
    }

    /// See [`EntityStore::relations_oldest_first`].
    pub fn relations_oldest_first(&self) -> Result<Vec<StoredRelation>> {
        EntityStore::relations_oldest_first(&*self.backend.lock()?)
    }

    /// See [`EntityStore::insert_relation_or_ignore`].
    pub fn insert_relation_or_ignore(&self, row: &RelationRow<'_>, origin: Origin) -> Result<bool> {
        EntityStore::insert_relation_or_ignore(&mut *self.backend.lock()?, row, origin)
    }

    /// See [`EntityStore::relations_touching`].
    pub fn relations_touching(
        &self,
        entity_ids: &[String],
        relation: Option<&str>,
        hop: u32,
    ) -> Result<Vec<(String, RelationEdge)>> {
        EntityStore::relations_touching(&*self.backend.lock()?, entity_ids, relation, hop)
    }

    /// See [`EntityStore::sync_view`].
    pub fn sync_view(&self, id: &str) -> Result<Option<EntitySyncView>> {
        EntityStore::sync_view(&*self.backend.lock()?, id)
    }

    /// See [`EntityStore::upsert_synced`].
    pub fn upsert_synced(&self, entity: &Entity, node_id: Option<&str>) -> Result<()> {
        EntityStore::upsert_synced(&mut *self.backend.lock()?, entity, node_id)
    }

    /// See [`EntityStore::set_aliases`].
    pub fn set_aliases(&self, id: &str, aliases: &[String]) -> Result<()> {
        EntityStore::set_aliases(&mut *self.backend.lock()?, id, aliases)
    }
}

/// The behaviour every [`EntityStore`] must show, run against each backend
/// by [`entity_conformance`] (ADR-0035).
///
/// Mention links point at memories, so a backend under test implements
/// [`MemoryStore`] too, and the checks seed memories through it.
#[doc(hidden)]
pub mod conformance {
    use super::{EntityStore, EntitySyncView, RelationRow};
    use crate::db::derived::Origin;
    use crate::db::memories::{MemoryStore, NewMemory};
    use crate::entity::Entity;

    pub const T1: &str = "2026-09-26T00:00:00+00:00";
    pub const T2: &str = "2026-09-27T00:00:00+00:00";

    fn entity(id: &str, name: &str, created_at: &str) -> Entity {
        Entity {
            id: id.to_string(),
            name: name.to_string(),
            kind: None,
            aliases: vec!["alias".to_string()],
            created_at: created_at.to_string(),
            updated_at: created_at.to_string(),
        }
    }

    /// Every graph write and every read after. Run with sync on, the same
    /// writes are what [`tests`](super::tests) checks the outbox against.
    pub fn the_graph_keeps_entities_links_and_relations<S: EntityStore + MemoryStore + ?Sized>(
        store: &mut S,
    ) {
        use Origin::{Local, Sync};
        for (id, at, subject) in [
            ("m1", T1, Some("RUST")),
            ("m2", T2, None),
            ("m3", T2, Some("rust")),
            ("m4", T1, None),
            ("m5", T2, None),
        ] {
            MemoryStore::insert(
                store,
                &NewMemory {
                    subject: subject.map(String::from),
                    ..NewMemory::new(id, format!("{id} {}", "ß".repeat(400)), at)
                },
            )
            .unwrap();
        }
        MemoryStore::set_superseded_by(store, "m4", "m1", None).unwrap();
        EntityStore::insert(store, &entity("rust", "Rust", T1), None).unwrap();
        EntityStore::insert(store, &entity("tokio", "Tokio", T2), Some("n1")).unwrap();
        EntityStore::insert(store, &entity("old", "Old", T1), None).unwrap();
        assert!(
            EntityStore::insert(store, &entity("rust", "Again", T2), None).is_err(),
            "a taken entity id is refused"
        );
        EntityStore::set_kind_and_aliases(store, "tokio", Some("lib"), &["tk".into()], T2).unwrap();
        EntityStore::set_merged(store, "old", Some("x"), &["o".into()], T2).unwrap();
        let linked: Vec<bool> = [
            ("m1", "rust", Local),
            ("m2", "rust", Sync),
            ("m4", "rust", Local),
            ("m1", "old", Local),
            ("m3", "old", Local),
            ("gone", "old", Local),
        ]
        .into_iter()
        .map(|(m, e, origin)| EntityStore::link(store, m, e, T1, origin).unwrap())
        .collect();
        assert_eq!(linked, [true; 6]);
        assert!(
            !EntityStore::link(store, "m1", "rust", T2, Local).unwrap(),
            "a link already there is left as it is"
        );
        let inserted: Vec<bool> = [
            ("r1", "rust", "tokio", Local),
            ("r2", "tokio", "old", Sync),
            ("r3", "old", "nowhere", Local),
        ]
        .into_iter()
        .map(|(id, s, o, origin)| {
            let row = RelationRow {
                id,
                subject_entity_id: s,
                relation: "uses",
                object_entity_id: o,
                created_at: T1,
                updated_at: T1,
                node_id: None,
            };
            EntityStore::insert_relation_or_ignore(store, &row, origin).unwrap()
        })
        .collect();
        assert_eq!(inserted, [true; 3]);
        let page: Vec<(String, i64)> = EntityStore::page_by_mentions(store, 10, 0)
            .unwrap()
            .into_iter()
            .map(|e| (e.id, e.mention_count))
            .collect();
        assert_eq!(
            page,
            [
                ("old".to_string(), 3),
                ("rust".to_string(), 3),
                ("tokio".to_string(), 0)
            ],
            "most mentioned first, ties by name"
        );
        EntityStore::repoint(store, "old", "rust").unwrap();
        EntityStore::rename(store, "tokio", "tokio2").unwrap();
        EntityStore::delete(store, "old").unwrap();
        EntityStore::upsert_synced(store, &entity("rust", "Rust!", T2), Some("peer")).unwrap();
        EntityStore::upsert_synced(store, &entity("new", "New", T2), None).unwrap();
        EntityStore::set_aliases(store, "new", &["n".into(), "nn".into()]).unwrap();
        EntityStore::unlink_memory(store, "m3").unwrap();

        let rust = EntityStore::get(store, "rust").unwrap().unwrap();
        assert_eq!(
            (rust.name.as_str(), rust.created_at.as_str()),
            ("Rust!", T1)
        );
        assert!(
            EntityStore::get(store, "tokio").unwrap().is_none(),
            "renamed away"
        );
        assert!(EntityStore::exists(store, "tokio2").unwrap());
        assert_eq!(EntityStore::count(store).unwrap(), 3);
        let oldest: Vec<String> = EntityStore::all_oldest_first(store)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(oldest, ["rust", "new", "tokio2"]);
        let second_page: Vec<String> = EntityStore::page_by_mentions(store, 2, 1)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(second_page, ["new", "tokio2"]);
        let mut links: Vec<(String, String)> = EntityStore::links_oldest_first(store)
            .unwrap()
            .into_iter()
            .map(|(m, e, _)| (m, e))
            .collect();
        links.sort();
        assert_eq!(
            links,
            [
                ("gone".to_string(), "rust".to_string()),
                ("m1".to_string(), "rust".to_string()),
                ("m2".to_string(), "rust".to_string()),
                ("m4".to_string(), "rust".to_string()),
            ],
            "old's links moved to rust, m1's duplicate dropped, m3 unlinked"
        );
        let linked: Vec<(String, usize)> = EntityStore::linked_memories(store, "rust", 10)
            .unwrap()
            .into_iter()
            .map(|m| (m.id, m.content_snippet.chars().count()))
            .collect();
        assert_eq!(
            linked,
            [("m2".to_string(), 300), ("m1".to_string(), 300)],
            "live and unsuperseded, newest first, snippets cut by character"
        );
        assert_eq!(EntityStore::linked_memory_count(store, "rust").unwrap(), 2);
        let facts: Vec<String> = EntityStore::facts_naming(store, "rust", 10)
            .unwrap()
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(
            facts,
            ["m3", "m1"],
            "subject matched case-insensitively, newest first"
        );
        let relations: Vec<(String, String, String)> = EntityStore::relations_oldest_first(store)
            .unwrap()
            .into_iter()
            .map(|r| (r.id, r.subject_entity_id, r.object_entity_id))
            .collect();
        assert_eq!(
            relations,
            [
                ("r1".to_string(), "rust".to_string(), "tokio".to_string()),
                ("r2".to_string(), "tokio".to_string(), "rust".to_string()),
                ("r3".to_string(), "rust".to_string(), "nowhere".to_string()),
            ],
            "a repoint follows the ends; a rename does not"
        );
        assert!(
            EntityStore::relations_touching(store, &["rust".into(), "tokio2".into()], None, 2)
                .unwrap()
                .is_empty(),
            "a relation whose other end is not stored is skipped"
        );
        EntityStore::insert_relation_or_ignore(
            store,
            &RelationRow {
                id: "r4",
                subject_entity_id: "tokio2",
                relation: "owns",
                object_entity_id: "rust",
                created_at: T2,
                updated_at: T2,
                node_id: None,
            },
            Sync,
        )
        .unwrap();
        let touching: Vec<String> =
            EntityStore::relations_touching(store, &["rust".into()], None, 2)
                .unwrap()
                .into_iter()
                .map(|(id, edge)| {
                    format!(
                        "{id}:{}:{}:{}",
                        edge.subject_name, edge.object_name, edge.hop
                    )
                })
                .collect();
        assert_eq!(touching, ["r4:Tokio:Rust!:2"]);
        assert!(
            EntityStore::relations_touching(store, &["rust".into()], Some("uses"), 1)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            EntityStore::sync_view(store, "new").unwrap(),
            Some(EntitySyncView {
                aliases: vec!["n".into(), "nn".into()],
                updated_at: T2.into(),
            })
        );
        let (unannotated_total, unannotated) = MemoryStore::unannotated_page(store, 10).unwrap();
        let unannotated: Vec<String> = unannotated.into_iter().map(|m| m.id).collect();
        assert_eq!(
            (unannotated_total, unannotated),
            (1, vec!["m5".to_string()]),
            "m1 and m3 have subjects, m4 is superseded, m2 keeps a link"
        );
    }

    pub fn a_synced_overwrite_keeps_created_at(store: &mut dyn EntityStore) {
        EntityStore::insert(store, &entity("e1", "Local", T1), None).unwrap();
        EntityStore::upsert_synced(store, &entity("e1", "Remote", T2), Some("peer")).unwrap();

        let stored = EntityStore::get(store, "e1").unwrap().unwrap();
        assert_eq!(stored.name, "Remote");
        assert_eq!(stored.created_at, T1);
        assert_eq!(stored.updated_at, T2);
    }

    pub fn repoint_drops_a_link_the_target_already_has(store: &mut dyn EntityStore) {
        assert!(EntityStore::link(store, "m1", "old", T1, Origin::Local).unwrap());
        assert!(EntityStore::link(store, "m1", "new", T1, Origin::Local).unwrap());
        assert!(EntityStore::link(store, "m2", "old", T1, Origin::Local).unwrap());
        assert!(!EntityStore::link(store, "m2", "old", T2, Origin::Local).unwrap());

        EntityStore::repoint(store, "old", "new").unwrap();

        let mut links: Vec<(String, String)> = EntityStore::links_oldest_first(store)
            .unwrap()
            .into_iter()
            .map(|(memory_id, entity_id, _)| (memory_id, entity_id))
            .collect();
        links.sort();
        assert_eq!(
            links,
            vec![
                ("m1".to_string(), "new".to_string()),
                ("m2".to_string(), "new".to_string())
            ]
        );
    }

    pub fn relations_touching_nothing_is_empty(store: &mut dyn EntityStore) {
        assert!(store.relations_touching(&[], None, 1).unwrap().is_empty());
    }
}

/// One `#[test]` per [`conformance`] check, each on a fresh store from
/// `$make`.
#[doc(hidden)]
#[macro_export]
macro_rules! entity_conformance {
    ($make:expr) => {
        use $crate::db::entities::conformance;

        #[test]
        fn the_graph_keeps_entities_links_and_relations() {
            conformance::the_graph_keeps_entities_links_and_relations(&mut $make);
        }
        #[test]
        fn a_synced_overwrite_keeps_created_at() {
            conformance::a_synced_overwrite_keeps_created_at(&mut $make);
        }
        #[test]
        fn repoint_drops_a_link_the_target_already_has() {
            conformance::repoint_drops_a_link_the_target_already_has(&mut $make);
        }
        #[test]
        fn relations_touching_nothing_is_empty() {
            conformance::relations_touching_nothing_is_empty(&mut $make);
        }
    };
}
#[doc(hidden)]
pub use crate::entity_conformance;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memories::Memories;
    use crate::db::outbox::Outbox;
    use crate::db::sync_state::SyncState;
    use crate::db::Database;

    /// With sync on, local graph writes queue in order and synced ones do
    /// not; a backfill queues the graph as stored. The writes are the
    /// conformance scenario, run on the engine behind the store.
    #[test]
    fn local_graph_writes_queue_for_sync() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        SyncState::new(&store)
            .set_flag("sync_enabled", "1")
            .unwrap();
        conformance::the_graph_keeps_entities_links_and_relations(
            &mut *store.backend().lock().unwrap(),
        );

        let queued = |outbox: &Outbox<'_>| -> Vec<(String, serde_json::Value)> {
            outbox
                .unsent_to("hub", 0, 1000)
                .unwrap()
                .into_iter()
                .filter(|e| !e.key.starts_with('m') || e.payload_json.contains("record_type"))
                .map(|e| (e.key, serde_json::from_str(&e.payload_json).unwrap()))
                .collect()
        };
        let outbox = Outbox::new(&store);
        let graph_entries = queued(&outbox);
        let kinds: Vec<String> = graph_entries
            .iter()
            .map(|(key, payload)| format!("{key}:{}", payload["record_type"]))
            .collect();
        assert_eq!(
            kinds,
            [
                "rust:\"entity\"",
                "tokio:\"entity\"",
                "old:\"entity\"",
                "tokio:\"entity\"",
                "old:\"entity\"",
                "m1:\"memory_entity\"",
                "m4:\"memory_entity\"",
                "m1:\"memory_entity\"",
                "m3:\"memory_entity\"",
                "gone:\"memory_entity\"",
                "r1:\"entity_relation\"",
                "r3:\"entity_relation\"",
                "tokio2:\"entity\"",
            ],
            "local writes queue in order; synced links, relations and upserts do not"
        );
        outbox.clear().unwrap();
        outbox.backfill_everything().unwrap();
        let mut backfilled: Vec<String> = queued(&outbox)
            .into_iter()
            .map(|(key, payload)| format!("{key}:{}", payload["record_type"]))
            .collect();
        backfilled.sort();
        assert_eq!(
            backfilled,
            [
                "gone:\"memory_entity\"",
                "m1:\"memory_entity\"",
                "m2:\"memory_entity\"",
                "m4:\"memory_entity\"",
                "new:\"entity\"",
                "rust:\"entity\"",
                "tokio2:\"entity\"",
            ],
            "a backfill queues every entity and link as stored now"
        );
    }

    /// The facade locks per call and reaches the engine through the port.
    #[test]
    fn the_facade_reads_back_what_it_writes() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let entities = Entities::new(&store);
        let rust = Entity {
            id: "rust".into(),
            name: "Rust".into(),
            kind: None,
            aliases: vec![],
            created_at: conformance::T1.into(),
            updated_at: conformance::T1.into(),
        };
        entities.insert(&rust, None).unwrap();
        assert!(entities
            .link("m1", "rust", conformance::T1, Origin::Local)
            .unwrap());
        assert_eq!(
            entities.get("rust").unwrap().map(|e| e.name),
            Some(rust.name)
        );
        assert_eq!(
            Memories::new(&store).linked_ids("rust").unwrap(),
            HashSet::from(["m1".to_string()])
        );
        assert!(entities
            .relations_touching(&[], None, 1)
            .unwrap()
            .is_empty());
    }
}
