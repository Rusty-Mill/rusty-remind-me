//! Storage for the knowledge graph: `entities`, `entity_relations` and the
//! `memory_entities` mention links, on the engine's memories core
//! (`db::engine::graph`).
//!
//! Every read and write [`crate::entity`] and [`crate::sync::graph`] make
//! goes through here. The rules stay with them: how a name normalises into
//! an id, how aliases merge, which kind wins, how a traversal walks, and how
//! sync resolves a conflict. A local write queues its outbox entry as it
//! lands; a synced one never does (see [`Origin`]).

use super::derived::Origin;
use super::engine::{self, EngineLock};
use super::{Result, Store};
use crate::entity::{Entity, EntityFact, EntityLinkedMemory, EntityListItem, RelationEdge};

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

/// The knowledge-graph tables, on the engine's memories core.
pub struct Entities<'c> {
    core: &'c EngineLock,
}

impl<'c> Entities<'c> {
    pub fn new(store: &'c Store<'_>) -> Self {
        Self { core: store.core() }
    }

    // --- entities --------------------------------------------------------

    /// The entity with id `id`, if there is one.
    pub fn get(&self, id: &str) -> Result<Option<Entity>> {
        engine::graph::get(&self.core.lock(), id)
    }

    /// Whether an entity with id `id` exists.
    pub fn exists(&self, id: &str) -> Result<bool> {
        engine::graph::exists(&self.core.lock(), id)
    }

    /// Every entity, oldest first (ties by id).
    pub fn all_oldest_first(&self) -> Result<Vec<Entity>> {
        engine::graph::all_oldest_first(&self.core.lock())
    }

    /// Every entity, by id.
    pub fn all(&self) -> Result<Vec<Entity>> {
        engine::graph::all(&self.core.lock())
    }

    /// Insert `entity`, made on this node, created and updated at its own
    /// stamps, recording `node_id` as where it was made. An existing id is an
    /// error.
    pub fn insert(&self, entity: &Entity, node_id: Option<&str>) -> Result<()> {
        engine::graph::insert(&mut self.core.lock(), entity, node_id)
    }

    /// Set `id`'s kind and aliases, stamping `updated_at`.
    pub fn set_kind_and_aliases(
        &self,
        id: &str,
        kind: Option<&str>,
        aliases: &[String],
        updated_at: &str,
    ) -> Result<()> {
        engine::graph::set_kind_and_aliases(&mut self.core.lock(), id, kind, aliases, updated_at)
    }

    /// How many entities there are.
    pub fn count(&self) -> Result<usize> {
        engine::graph::count(&self.core.lock())
    }

    /// A page of entities with their mention counts, most-mentioned first,
    /// then by name, then id.
    pub fn page_by_mentions(&self, limit: usize, offset: usize) -> Result<Vec<EntityListItem>> {
        engine::graph::page_by_mentions(&self.core.lock(), limit, offset)
    }

    // --- id renormalisation ----------------------------------------------

    /// Rename the entity `from` to `to`, where `to` is free. Links and
    /// relations are repointed separately. Queued for sync as an update of
    /// `to`.
    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        engine::graph::rename(&mut self.core.lock(), from, to)
    }

    /// Set `id`'s kind, aliases and `created_at`, without stamping
    /// `updated_at`: the merge of two rows that were always one entity.
    pub fn set_merged(
        &self,
        id: &str,
        kind: Option<&str>,
        aliases: &[String],
        created_at: &str,
    ) -> Result<()> {
        engine::graph::set_merged(&mut self.core.lock(), id, kind, aliases, created_at)
    }

    /// Delete the entity `id`. Its links and relations are not touched.
    pub fn delete(&self, id: &str) -> Result<()> {
        engine::graph::delete(&mut self.core.lock(), id)
    }

    /// Repoint every mention link and relation end from the entity `from` to
    /// `to`. A link `to` already has is dropped rather than duplicated.
    pub fn repoint(&self, from: &str, to: &str) -> Result<()> {
        engine::graph::repoint(&mut self.core.lock(), from, to)
    }

    // --- mention links ---------------------------------------------------

    /// Every mention link as `(memory_id, entity_id, created_at)`, oldest
    /// first.
    pub fn links_oldest_first(&self) -> Result<Vec<(String, String, String)>> {
        engine::graph::links_oldest_first(&self.core.lock())
    }

    /// Record that `memory_id` mentions `entity_id`. Returns whether the
    /// link is new: links are immutable, so an existing one is left as is.
    pub fn link(
        &self,
        memory_id: &str,
        entity_id: &str,
        created_at: &str,
        origin: Origin,
    ) -> Result<bool> {
        engine::graph::link(
            &mut self.core.lock(),
            memory_id,
            entity_id,
            created_at,
            origin,
        )
    }

    /// Remove every mention link from `memory_id`: part of deleting a
    /// memory.
    pub fn unlink_memory(&self, memory_id: &str) -> Result<()> {
        engine::graph::unlink_memory(&mut self.core.lock(), memory_id)
    }

    /// Live memories linked to `entity_id`, newest first (ties by id,
    /// descending), at most `limit`, with the first 300 characters of their
    /// content. A link to a memory not stored here is skipped.
    pub fn linked_memories(
        &self,
        entity_id: &str,
        limit: usize,
    ) -> Result<Vec<EntityLinkedMemory>> {
        engine::graph::linked_memories(&self.core.lock(), entity_id, limit)
    }

    /// How many live memories are linked to `entity_id`.
    pub fn linked_memory_count(&self, entity_id: &str) -> Result<usize> {
        engine::graph::linked_memory_count(&self.core.lock(), entity_id)
    }

    /// Live memories whose subject or object, lowercased, is `canonical`,
    /// newest first (ties by id, descending), at most `limit`.
    pub fn facts_naming(&self, canonical: &str, limit: usize) -> Result<Vec<EntityFact>> {
        engine::graph::facts_naming(&self.core.lock(), canonical, limit)
    }

    // --- relations -------------------------------------------------------

    /// Every relation, oldest first (ties by id).
    pub fn relations_oldest_first(&self) -> Result<Vec<StoredRelation>> {
        engine::graph::relations_oldest_first(&self.core.lock())
    }

    /// Insert `row` unless its id is already taken. Returns whether it was
    /// inserted: relations are immutable.
    pub fn insert_relation_or_ignore(&self, row: &RelationRow<'_>, origin: Origin) -> Result<bool> {
        engine::graph::insert_relation_or_ignore(&mut self.core.lock(), row, origin)
    }

    /// Every relation with either end in `entity_ids`, and `relation` as its
    /// label when one is given, oldest first (ties by id). Each comes with
    /// its id, and a relation whose ends are not both stored here is
    /// skipped. `hop` is copied onto each edge.
    pub fn relations_touching(
        &self,
        entity_ids: &[String],
        relation: Option<&str>,
        hop: u32,
    ) -> Result<Vec<(String, RelationEdge)>> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }
        engine::graph::relations_touching(&self.core.lock(), entity_ids, relation, hop)
    }

    // --- sync ------------------------------------------------------------

    /// The local copy of `id` as a sync merge sees it, if there is one.
    pub fn sync_view(&self, id: &str) -> Result<Option<EntitySyncView>> {
        engine::graph::sync_view(&self.core.lock(), id)
    }

    /// Write an entity a peer sent: insert it, or overwrite the local row's
    /// name, kind, aliases, `updated_at` and `node_id`. The local
    /// `created_at` is kept. Never queued for sync: it came from there.
    pub fn upsert_synced(&self, entity: &Entity, node_id: Option<&str>) -> Result<()> {
        engine::graph::upsert_synced(&mut self.core.lock(), entity, node_id)
    }

    /// Replace `id`'s aliases without stamping `updated_at`: a sync merge
    /// that lost last-write-wins still keeps the union. Never queued for
    /// sync.
    pub fn set_aliases(&self, id: &str, aliases: &[String]) -> Result<()> {
        engine::graph::set_aliases(&mut self.core.lock(), id, aliases)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    const T1: &str = "2026-09-26T00:00:00+00:00";
    const T2: &str = "2026-09-27T00:00:00+00:00";

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

    /// Every graph write with sync on, and every read after, with the
    /// outbox it queued.
    #[test]
    fn the_graph_keeps_entities_links_and_relations_and_queues_local_writes() {
        use crate::db::derived::Origin::{Local, Sync};
        use crate::db::memories::{Memories, NewMemory};
        use crate::db::outbox::Outbox;
        use crate::db::sync_state::SyncState;
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        SyncState::new(&store)
            .set_flag("sync_enabled", "1")
            .unwrap();
        let memories = Memories::new(&store);
        for (id, at, subject) in [
            ("m1", T1, Some("RUST")),
            ("m2", T2, None),
            ("m3", T2, Some("rust")),
            ("m4", T1, None),
            ("m5", T2, None),
        ] {
            memories
                .insert(&NewMemory {
                    subject: subject.map(String::from),
                    ..NewMemory::new(id, format!("{id} {}", "ß".repeat(400)), at)
                })
                .unwrap();
        }
        memories.set_superseded_by("m4", "m1", None).unwrap();
        let entities = Entities::new(&store);
        entities.insert(&entity("rust", "Rust", T1), None).unwrap();
        entities
            .insert(&entity("tokio", "Tokio", T2), Some("n1"))
            .unwrap();
        entities.insert(&entity("old", "Old", T1), None).unwrap();
        assert!(
            entities.insert(&entity("rust", "Again", T2), None).is_err(),
            "a taken entity id is refused"
        );
        entities
            .set_kind_and_aliases("tokio", Some("lib"), &["tk".into()], T2)
            .unwrap();
        entities
            .set_merged("old", Some("x"), &["o".into()], T2)
            .unwrap();
        let linked: Vec<bool> = [
            ("m1", "rust", Local),
            ("m2", "rust", Sync),
            ("m4", "rust", Local),
            ("m1", "old", Local),
            ("m3", "old", Local),
            ("gone", "old", Local),
        ]
        .into_iter()
        .map(|(m, e, origin)| entities.link(m, e, T1, origin).unwrap())
        .collect();
        assert_eq!(linked, [true; 6]);
        assert!(
            !entities.link("m1", "rust", T2, Local).unwrap(),
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
            entities.insert_relation_or_ignore(&row, origin).unwrap()
        })
        .collect();
        assert_eq!(inserted, [true; 3]);
        let page: Vec<(String, i64)> = entities
            .page_by_mentions(10, 0)
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
        entities.repoint("old", "rust").unwrap();
        entities.rename("tokio", "tokio2").unwrap();
        entities.delete("old").unwrap();
        entities
            .upsert_synced(&entity("rust", "Rust!", T2), Some("peer"))
            .unwrap();
        entities
            .upsert_synced(&entity("new", "New", T2), None)
            .unwrap();
        entities
            .set_aliases("new", &["n".into(), "nn".into()])
            .unwrap();
        Entities::new(&store).unlink_memory("m3").unwrap();

        let rust = entities.get("rust").unwrap().unwrap();
        assert_eq!(
            (rust.name.as_str(), rust.created_at.as_str()),
            ("Rust!", T1)
        );
        assert!(entities.get("tokio").unwrap().is_none(), "renamed away");
        assert!(entities.exists("tokio2").unwrap());
        assert_eq!(entities.count().unwrap(), 3);
        let oldest: Vec<String> = entities
            .all_oldest_first()
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(oldest, ["rust", "new", "tokio2"]);
        let second_page: Vec<String> = entities
            .page_by_mentions(2, 1)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(second_page, ["new", "tokio2"]);
        let mut links: Vec<(String, String)> = entities
            .links_oldest_first()
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
        let linked: Vec<(String, usize)> = entities
            .linked_memories("rust", 10)
            .unwrap()
            .into_iter()
            .map(|m| (m.id, m.content_snippet.chars().count()))
            .collect();
        assert_eq!(
            linked,
            [("m2".to_string(), 300), ("m1".to_string(), 300)],
            "live and unsuperseded, newest first, snippets cut by character"
        );
        assert_eq!(entities.linked_memory_count("rust").unwrap(), 2);
        let facts: Vec<String> = entities
            .facts_naming("rust", 10)
            .unwrap()
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(
            facts,
            ["m3", "m1"],
            "subject matched case-insensitively, newest first"
        );
        let relations: Vec<(String, String, String)> = entities
            .relations_oldest_first()
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
            entities
                .relations_touching(&["rust".into(), "tokio2".into()], None, 2)
                .unwrap()
                .is_empty(),
            "a relation whose other end is not stored is skipped"
        );
        entities
            .insert_relation_or_ignore(
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
        let touching: Vec<String> = entities
            .relations_touching(&["rust".into()], None, 2)
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
        assert!(entities
            .relations_touching(&["rust".into()], Some("uses"), 1)
            .unwrap()
            .is_empty());
        assert_eq!(
            entities.sync_view("new").unwrap(),
            Some(EntitySyncView {
                aliases: vec!["n".into(), "nn".into()],
                updated_at: T2.into(),
            })
        );
        let (unannotated_total, unannotated) = memories.unannotated_page(10).unwrap();
        let unannotated: Vec<String> = unannotated.into_iter().map(|m| m.id).collect();
        assert_eq!(
            (unannotated_total, unannotated),
            (1, vec!["m5".to_string()]),
            "m1 and m3 have subjects, m4 is superseded, m2 keeps a link"
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

    #[test]
    fn a_synced_overwrite_keeps_created_at() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let entities = Entities::new(&store);
        entities.insert(&entity("e1", "Local", T1), None).unwrap();
        entities
            .upsert_synced(&entity("e1", "Remote", T2), Some("peer"))
            .unwrap();

        let stored = entities.get("e1").unwrap().unwrap();
        assert_eq!(stored.name, "Remote");
        assert_eq!(stored.created_at, T1);
        assert_eq!(stored.updated_at, T2);
    }

    #[test]
    fn repoint_drops_a_link_the_target_already_has() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        let entities = Entities::new(&store);
        assert!(entities.link("m1", "old", T1, Origin::Local).unwrap());
        assert!(entities.link("m1", "new", T1, Origin::Local).unwrap());
        assert!(entities.link("m2", "old", T1, Origin::Local).unwrap());
        assert!(!entities.link("m2", "old", T2, Origin::Local).unwrap());

        entities.repoint("old", "new").unwrap();

        let mut links: Vec<(String, String)> = entities
            .links_oldest_first()
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

    #[test]
    fn relations_touching_nothing_is_empty() {
        let db = Database::open_in_memory().unwrap();
        let store = db.store();
        assert!(Entities::new(&store)
            .relations_touching(&[], None, 1)
            .unwrap()
            .is_empty());
    }
}
