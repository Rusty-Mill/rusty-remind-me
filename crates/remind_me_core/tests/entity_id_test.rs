//! Coverage for entity identity: the derivation itself, and the migration that
//! rewrites ids written by earlier builds.

use remind_me_core::db::derived::Origin;
use remind_me_core::db::entities::{Entities, RelationRow, StoredRelation};
use remind_me_core::db::Store;
use remind_me_core::entity::{
    entity_id, get_entity_by_id, get_entity_by_name, normalize_entity_name, renormalize_entity_ids,
    upsert_entity, Entity,
};
use remind_me_core::testing::{self, Table};
use remind_me_core::{Database, EntityInput};

fn input(name: &str, aliases: &[&str]) -> EntityInput {
    EntityInput {
        name: name.to_string(),
        kind: None,
        aliases: aliases.iter().map(|a| a.to_string()).collect(),
    }
}

/// Insert a row the way an earlier build of this crate did: `ent_` plus the
/// full digest of a merely-trimmed name.
fn insert_legacy(store: &Store<'_>, name: &str, aliases: &str, created_at: &str) -> String {
    let id = format!("ent_{}", sha256::digest(name.trim().to_lowercase()));
    Entities::new(store)
        .insert(
            &Entity {
                id: id.clone(),
                name: name.trim().to_string(),
                kind: None,
                aliases: serde_json::from_str(aliases).unwrap(),
                created_at: created_at.to_string(),
                updated_at: created_at.to_string(),
            },
            None,
        )
        .unwrap();
    id
}

/// Link `memory_id` to `entity_id` as a raw `INSERT INTO memory_entities`
/// did: no memory row needed, nothing queued.
fn link(store: &Store<'_>, memory_id: &str, entity_id: &str) {
    Entities::new(store)
        .link(memory_id, entity_id, "2026-01-01T00:00:00Z", Origin::Sync)
        .unwrap();
}

/// Insert relation `rel_1` between two entity ids, nothing queued.
fn relate(store: &Store<'_>, subject: &str, relation: &str, object: &str) {
    let at = "2026-01-01T00:00:00Z";
    Entities::new(store)
        .insert_relation_or_ignore(
            &RelationRow {
                id: "rel_1",
                subject_entity_id: subject,
                relation,
                object_entity_id: object,
                created_at: at,
                updated_at: at,
                node_id: None,
            },
            Origin::Sync,
        )
        .unwrap();
}

/// Every mention link as `(memory_id, entity_id)`.
fn links(store: &Store<'_>) -> Vec<(String, String)> {
    Entities::new(store)
        .links_oldest_first()
        .unwrap()
        .into_iter()
        .map(|(memory, entity, _)| (memory, entity))
        .collect()
}

fn relation(store: &Store<'_>, id: &str) -> StoredRelation {
    Entities::new(store)
        .relations_oldest_first()
        .unwrap()
        .into_iter()
        .find(|r| r.id == id)
        .unwrap()
}

#[test]
fn the_id_matches_the_reference_derivation() {
    // sha256("bailey robertson")[:12], computed against remind_me's _entity_id.
    assert_eq!(entity_id("Bailey Robertson"), "494292a0dfb1");
    assert_eq!(entity_id("  Bailey   Robertson  "), "494292a0dfb1");
    assert_eq!(entity_id("bailey robertson"), "494292a0dfb1");
}

#[test]
fn the_id_is_unprefixed_and_twelve_hex_characters() {
    let id = entity_id("Tasmania");
    assert_eq!(id.len(), 12, "the reference truncates to 12");
    assert!(!id.starts_with("ent_"), "the reference has no prefix");
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn normalisation_collapses_internal_whitespace() {
    assert_eq!(
        normalize_entity_name(" Bailey\t Robertson\n"),
        "bailey robertson"
    );
    assert_eq!(normalize_entity_name(""), "");
}

#[test]
fn internal_whitespace_variants_are_one_entity() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();

    let first = upsert_entity(&store, &input("Bailey  Robertson", &["Bailey"])).unwrap();
    let second = upsert_entity(&store, &input("Bailey Robertson", &["BR"])).unwrap();

    assert_eq!(first.id, second.id);
    let count = testing::count(&store, Table::Entities).unwrap();
    assert_eq!(count, 1, "the two spellings must not create two rows");
    assert_eq!(second.aliases, vec!["Bailey".to_string(), "BR".to_string()]);
}

#[test]
fn lookup_by_name_tolerates_casing_and_spacing() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    upsert_entity(&store, &input("Tasmania", &[])).unwrap();

    assert!(get_entity_by_name(&store, "  TASMANIA ").unwrap().is_some());
}

#[test]
fn the_migration_rewrites_a_legacy_id_and_its_links() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let legacy = insert_legacy(&store, "Tasmania", "[]", "2026-01-01T00:00:00Z");
    link(&store, "mem_1", &legacy);
    relate(&store, &legacy, "located_in", "other");

    assert_eq!(renormalize_entity_ids(&store).unwrap(), 1);

    let want = entity_id("Tasmania");
    assert!(get_entity_by_id(&store, &want).unwrap().is_some());
    assert!(get_entity_by_id(&store, &legacy).unwrap().is_none());

    // Nothing cascades — there is no foreign key — so a link left pointing at
    // the old id would simply dangle, silently.
    let linked = links(&store)
        .into_iter()
        .find(|(memory, _)| memory == "mem_1")
        .unwrap()
        .1;
    assert_eq!(linked, want);
    let subject = relation(&store, "rel_1").subject_entity_id;
    assert_eq!(subject, want);
}

#[test]
fn the_migration_rewrites_object_side_relations_too() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let legacy = insert_legacy(&store, "Hobart", "[]", "2026-01-01T00:00:00Z");
    relate(&store, "other", "capital_of", &legacy);

    renormalize_entity_ids(&store).unwrap();

    let object = relation(&store, "rel_1").object_entity_id;
    assert_eq!(object, entity_id("Hobart"));
}

#[test]
fn the_migration_merges_rows_that_normalise_together() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    // Two rows only because the old derivation did not collapse internal runs.
    let spaced = insert_legacy(
        &store,
        "Bailey  Robertson",
        r#"["Bailey"]"#,
        "2026-01-02T00:00:00Z",
    );
    let single = insert_legacy(
        &store,
        "Bailey Robertson",
        r#"["BR"]"#,
        "2026-01-01T00:00:00Z",
    );
    for (memory, id) in [("mem_1", &spaced), ("mem_2", &single)] {
        link(&store, memory, id);
    }

    renormalize_entity_ids(&store).unwrap();

    let count = testing::count(&store, Table::Entities).unwrap();
    assert_eq!(
        count, 1,
        "colliding rows must merge, not fail the migration"
    );

    let merged = get_entity_by_id(&store, &entity_id("Bailey Robertson"))
        .unwrap()
        .unwrap();
    let mut aliases = merged.aliases.clone();
    aliases.sort();
    assert_eq!(aliases, vec!["BR".to_string(), "Bailey".to_string()]);
    assert_eq!(
        merged.created_at, "2026-01-01T00:00:00Z",
        "the earliest creation wins"
    );

    // Both memories keep their link, repointed at the survivor.
    let all = links(&store);
    let linked = all
        .iter()
        .filter(|(_, entity)| *entity == merged.id)
        .count();
    assert_eq!(linked, 2);
    let orphans = all
        .iter()
        .filter(|(_, entity)| *entity != merged.id)
        .count();
    assert_eq!(orphans, 0, "no link may be left pointing at a dead id");
}

#[test]
fn a_duplicate_link_across_a_merge_collapses_rather_than_erroring() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    let spaced = insert_legacy(&store, "Bailey  Robertson", "[]", "2026-01-02T00:00:00Z");
    let single = insert_legacy(&store, "Bailey Robertson", "[]", "2026-01-01T00:00:00Z");
    // The same memory mentions both — after the merge that is one link, and
    // `memory_entities` is keyed (memory_id, entity_id).
    for id in [&spaced, &single] {
        link(&store, "mem_1", id);
    }

    renormalize_entity_ids(&store).unwrap();

    let links = testing::count(&store, Table::MemoryEntities).unwrap();
    assert_eq!(links, 1);
}

#[test]
fn the_migration_is_idempotent() {
    let db = Database::open_in_memory().unwrap();
    let store = db.store();
    upsert_entity(&store, &input("Tasmania", &["Tas"])).unwrap();

    assert_eq!(
        renormalize_entity_ids(&store).unwrap(),
        0,
        "rows already on the current derivation must not be touched"
    );
    assert_eq!(renormalize_entity_ids(&store).unwrap(), 0);
}

#[test]
fn opening_an_existing_database_migrates_it() {
    let dir = std::env::temp_dir().join(format!("rrm_entity_id_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("memories.db");
    let _ = std::fs::remove_file(&path);

    let legacy = {
        let db = Database::open(&path).unwrap();
        let store = db.store();
        insert_legacy(&store, "Tasmania", "[]", "2026-01-01T00:00:00Z")
    };

    // Reopening runs the reconciler, which is where the rewrite lives.
    let db = Database::open(&path).unwrap();
    let store = db.store();
    assert!(get_entity_by_id(&store, &legacy).unwrap().is_none());
    assert!(get_entity_by_name(&store, "tasmania").unwrap().is_some());

    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}
